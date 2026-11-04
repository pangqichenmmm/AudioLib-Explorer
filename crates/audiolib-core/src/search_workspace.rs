// crates/audiolib-core/src/search_workspace.rs
//! Owned client-local search tasks and explicitly saved search definitions.
use crate::{Error, Result, SearchFilters, SearchRequest, SortOrder};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

/// Persistent workspace encoding, independent of the library and snapshot schema.
pub const SEARCH_WORKSPACE_VERSION: u32 = 2;
/// Hard limit on simultaneously open search tasks; favorites do not count toward it.
pub const MAX_SEARCH_TASKS: usize = 30;

/// A user-data directory and immutable library scope captured before worker dispatch.
/// Writers hold the desktop instance lease and serialize saves for this data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchWorkspaceTarget {
    pub data_root: PathBuf,
    pub library_scope: String,
}

impl SearchWorkspaceTarget {
    /// Construct a target; providers validate it before accessing any file.
    pub fn new(data_root: impl Into<PathBuf>, library_scope: impl Into<String>) -> Self {
        Self {
            data_root: data_root.into(),
            library_scope: library_scope.into(),
        }
    }

    /// Validate a bounded scope without interpreting it as a filesystem component.
    pub fn validate(&self) -> Result<()> {
        if self.data_root.as_os_str().is_empty() {
            return Err(invalid("Search workspace needs a user-data directory"));
        }
        validate_scope(&self.library_scope)
    }
}

/// Folder identity includes its path so a reused numeric ID cannot broaden a search.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchFolderRef {
    pub id: i64,
    pub path: String,
}

/// Audio identity includes its path for safe selection restoration after a restore.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchAudioRef {
    pub id: i64,
    pub file_path: String,
}

/// An album's semantic owner within one identity namespace. Numeric IDs alone
/// cannot survive an independent import or a legacy server's native upgrade.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlbumFilterBinding {
    pub namespace: String,
    pub kind: crate::AlbumKind,
    pub metadata_key: Option<String>,
    pub folder_path: Option<String>,
    /// Preserve an unresolved filter visibly instead of searching another album
    /// which happens to reuse its numeric ID. Restoring may retry the binding.
    #[serde(default)]
    pub unavailable: bool,
}

/// Live search conditions. Saving a favorite stores conditions, never result rows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchTaskDefinition {
    pub query: String,
    pub folder: Option<SearchFolderRef>,
    pub include_subfolders: bool,
    pub filters: SearchFilters,
    #[serde(default)]
    pub album_binding: Option<AlbumFilterBinding>,
    pub order_by: SortOrder,
    pub descending: bool,
}

impl Default for SearchTaskDefinition {
    fn default() -> Self {
        Self {
            query: String::new(),
            folder: None,
            include_subfolders: true,
            filters: SearchFilters::default(),
            album_binding: None,
            order_by: SortOrder::Relevance,
            descending: false,
        }
    }
}

impl SearchTaskDefinition {
    /// Build a request for the caller's page. Execution still validates the request.
    /// Resolve `album_binding` against the chosen library namespace before running
    /// an album-ID filter; an unresolved binding must never execute its numeric ID.
    pub fn to_request(&self, limit: usize, offset: usize) -> SearchRequest {
        SearchRequest {
            query: self.query.clone(),
            folder_id: self.folder.as_ref().map(|folder| folder.id),
            include_subfolders: self.include_subfolders,
            filters: self.filters.clone(),
            limit,
            offset,
            order_by: self.order_by,
            descending: self.descending,
        }
    }

    /// Validate stored conditions, retaining an unfinished single-letter query.
    /// Invalid text drafts belong to `SearchTaskView::raw_inputs` instead.
    pub fn validate(&self) -> Result<()> {
        if self.query.chars().count() > 500 || self.query.contains([';', '\0']) {
            return Err(invalid(
                "Stored query exceeds 500 characters or contains a forbidden delimiter",
            ));
        }
        let mut request = self.to_request(200, 0);
        if request.needs_more_search_text() {
            request.query.clear();
        }
        request.validate()?;
        if let Some(folder) = &self.folder {
            validate_reference(&folder.path)?;
        }
        if let Some(binding) = &self.album_binding {
            if self.filters.album_id.is_none() {
                return Err(invalid("An album binding requires its album filter"));
            }
            validate_name(&binding.namespace, 128, "album namespace")?;
            match binding.kind {
                crate::AlbumKind::Metadata => {
                    let key = binding
                        .metadata_key
                        .as_deref()
                        .ok_or_else(|| invalid("Metadata album binding needs its key"))?;
                    validate_reference(key)?;
                    if binding.folder_path.is_some() {
                        return Err(invalid(
                            "Metadata album binding cannot contain a folder path",
                        ));
                    }
                }
                crate::AlbumKind::Folder => {
                    if binding.metadata_key.is_some() {
                        return Err(invalid(
                            "Folder album binding cannot contain a Metadata key",
                        ));
                    }
                    if let Some(path) = &binding.folder_path {
                        validate_reference(path)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Per-task navigation and raw editor drafts, separate from executable conditions.
/// Scroll positions are signed because Slint viewports use negative translations.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchTaskView {
    pub offset: usize,
    pub expanded_folders: Vec<SearchFolderRef>,
    pub folder_scroll_y: f32,
    pub list_scroll_y: f32,
    pub list_scroll_x: f32,
    pub filter_scroll_y: f32,
    pub inspector_scroll_y: f32,
    pub list_top_row: usize,
    pub list_row_offset: f32,
    pub selected_audio: Option<SearchAudioRef>,
    pub checked_audio: Vec<SearchAudioRef>,
    pub selection_anchor: Option<SearchAudioRef>,
    pub raw_inputs: BTreeMap<String, String>,
    pub sample_filter: i32,
    pub bit_filter: i32,
    pub channel_filter: i32,
    pub visibility_index: i32,
    pub sidebar_page: i32,
    pub favorites_category: i32,
    pub filters_visible: bool,
}

impl SearchTaskView {
    fn validate(&self) -> Result<()> {
        for value in [
            self.folder_scroll_y,
            self.list_scroll_y,
            self.list_scroll_x,
            self.filter_scroll_y,
            self.inspector_scroll_y,
            self.list_row_offset,
        ] {
            if !value.is_finite() {
                return Err(invalid("Search workspace scroll positions must be finite"));
            }
        }
        for folder in &self.expanded_folders {
            validate_reference(&folder.path)?;
        }
        for audio in self
            .selected_audio
            .iter()
            .chain(self.checked_audio.iter())
            .chain(self.selection_anchor.iter())
        {
            validate_reference(&audio.file_path)?;
        }
        if self.raw_inputs.iter().any(|(key, value)| {
            key.len() > 128 || key.contains('\0') || value.len() > 1_048_576 || value.contains('\0')
        }) {
            return Err(invalid(
                "Search workspace editor drafts exceed their size bound",
            ));
        }
        Ok(())
    }
}

/// An open task; its optional favorite link never updates that favorite implicitly.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchTask {
    pub id: String,
    pub title: String,
    /// Explicit user names bypass automatic/default title localization. Legacy
    /// workspaces omit this field and retain automatic-name handling (`false`).
    #[serde(default)]
    pub title_is_custom: bool,
    pub definition: SearchTaskDefinition,
    pub view: SearchTaskView,
    pub saved_search_id: Option<String>,
}

/// Every open task and their order, including tasks which have never been favorited.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchSession {
    pub tasks: Vec<SearchTask>,
    pub active_task_id: Option<String>,
}

/// An explicitly saved definition, independent of any subsequently edited task.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSearchTask {
    pub id: String,
    pub title: String,
    /// Retain explicit-name provenance when reopening a favorite. Legacy
    /// favorites omit this field and retain automatic-name handling (`false`).
    #[serde(default)]
    pub title_is_custom: bool,
    pub definition: SearchTaskDefinition,
}

/// One personal library workspace, separate from settings and replaceable snapshots.
/// The version and scope are required when reading a file; unknown formats are refused.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchWorkspace {
    pub format_version: u32,
    pub library_scope: String,
    #[serde(default)]
    pub session: SearchSession,
    #[serde(default)]
    pub favorites: Vec<SavedSearchTask>,
}

impl SearchWorkspace {
    /// Empty workspace for a validated scope; the desktop creates its first task.
    pub fn for_scope(library_scope: impl Into<String>) -> Self {
        Self {
            format_version: SEARCH_WORKSPACE_VERSION,
            library_scope: library_scope.into(),
            session: SearchSession::default(),
            favorites: Vec::new(),
        }
    }

    /// Admit a fully decoded legacy workspace without changing any conditions.
    /// Storage readers must reject unknown fields before invoking this conversion.
    /// The next ordered save writes v2, which v1 readers refuse before execution.
    pub fn upgrade_legacy_version(&mut self) -> Result<()> {
        if self.format_version == 1 {
            self.format_version = SEARCH_WORKSPACE_VERSION;
        }
        self.validate()
    }

    /// Validate persistent identities, search conditions and the hard open-task limit.
    pub fn validate(&self) -> Result<()> {
        if self.format_version != SEARCH_WORKSPACE_VERSION {
            return Err(invalid("Unsupported search workspace version"));
        }
        validate_scope(&self.library_scope)?;
        if self.session.tasks.len() > MAX_SEARCH_TASKS {
            return Err(invalid(format!(
                "At most {MAX_SEARCH_TASKS} search tasks may be open"
            )));
        }
        let mut tasks = std::collections::HashSet::new();
        for task in &self.session.tasks {
            validate_name(&task.id, 128, "task identity")?;
            validate_name(&task.title, 256, "task title")?;
            if !tasks.insert(&task.id) {
                return Err(invalid("Search task identities must be unique"));
            }
            task.definition.validate()?;
            task.view.validate()?;
            if let Some(id) = &task.saved_search_id {
                validate_name(id, 128, "favorite identity")?;
            }
        }
        if let Some(active) = &self.session.active_task_id
            && !tasks.contains(active)
        {
            return Err(invalid("The active search task does not exist"));
        }
        let mut favorites = std::collections::HashSet::new();
        for favorite in &self.favorites {
            validate_name(&favorite.id, 128, "favorite identity")?;
            validate_name(&favorite.title, 256, "favorite title")?;
            if !favorites.insert(&favorite.id) {
                return Err(invalid("Search favorite identities must be unique"));
            }
            favorite.definition.validate()?;
        }
        Ok(())
    }
}

fn validate_scope(scope: &str) -> Result<()> {
    if scope.is_empty() || scope.len() > 150 || scope.contains('\0') {
        return Err(invalid(
            "Search workspace scope must contain 1..=150 UTF-8 bytes without NUL",
        ));
    }
    Ok(())
}

fn validate_reference(path: &str) -> Result<()> {
    // SQLite identities span the full signed i64 domain, including zero. Optional
    // references already distinguish an absent selection from every valid row ID.
    if path.is_empty() || path.len() > 1_048_576 || path.contains('\0') {
        return Err(invalid(
            "Search workspace references need a nonempty bounded path without NUL",
        ));
    }
    Ok(())
}

fn validate_name(value: &str, max_chars: usize, kind: &str) -> Result<()> {
    if value.trim().is_empty() || value.chars().count() > max_chars || value.contains('\0') {
        return Err(invalid(format!("Invalid search {kind}")));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Validation(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_provenance_defaults_for_legacy_json_and_round_trips_explicit_names() {
        let mut task: SearchTask =
            serde_json::from_str(r#"{"id":"task","title":"Search 2"}"#).unwrap();
        let mut saved: SavedSearchTask =
            serde_json::from_str(r#"{"id":"saved","title":"default"}"#).unwrap();
        assert!(!task.title_is_custom);
        assert!(!saved.title_is_custom);
        task.title_is_custom = true;
        saved.title_is_custom = true;
        let restored_task: SearchTask =
            serde_json::from_slice(&serde_json::to_vec(&task).unwrap()).unwrap();
        let restored_saved: SavedSearchTask =
            serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
        assert_eq!(restored_task, task);
        assert_eq!(restored_saved, saved);
        assert!(restored_task.title_is_custom);
        assert!(restored_saved.title_is_custom);
    }

    #[test]
    fn definition_round_trip_retains_complete_filters_and_partial_query() {
        let mut definition = SearchTaskDefinition {
            query: "s".into(),
            folder: Some(SearchFolderRef {
                id: 8,
                path: "/sounds/wood".into(),
            }),
            descending: true,
            ..Default::default()
        };
        definition.filters.include_tags = vec!["wood".into()];
        definition.filters.exclude_tags = vec!["metal".into()];
        definition.filters.favorites_only = true;
        definition.validate().unwrap();
        let restored: SearchTaskDefinition =
            serde_json::from_slice(&serde_json::to_vec(&definition).unwrap()).unwrap();
        assert_eq!(restored, definition);
        let request = restored.to_request(50, 100);
        assert_eq!(
            (request.folder_id, request.limit, request.offset),
            (Some(8), 50, 100)
        );
        assert!(request.validate().is_err());
    }

    #[test]
    fn album_binding_round_trips_and_legacy_name_filters_remain_readable() {
        let mut legacy = serde_json::to_value(SearchTaskDefinition {
            filters: SearchFilters {
                album: Some("Legacy Library".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        legacy.as_object_mut().unwrap().remove("album_binding");
        let old: SearchTaskDefinition = serde_json::from_value(legacy).unwrap();
        assert!(old.album_binding.is_none());
        old.validate().unwrap();
        let mut definition = SearchTaskDefinition {
            filters: SearchFilters {
                album_id: Some(7),
                ..Default::default()
            },
            album_binding: Some(AlbumFilterBinding {
                namespace: "namespace-one".into(),
                kind: crate::AlbumKind::Metadata,
                metadata_key: Some("Library A".into()),
                folder_path: None,
                unavailable: true,
            }),
            ..Default::default()
        };
        definition.validate().unwrap();
        let restored: SearchTaskDefinition =
            serde_json::from_slice(&serde_json::to_vec(&definition).unwrap()).unwrap();
        assert_eq!(restored, definition);
        definition.album_binding.as_mut().unwrap().folder_path = Some("/unrelated".into());
        assert!(definition.validate().is_err());
        definition.album_binding.as_mut().unwrap().folder_path = None;
        definition.filters.album_id = None;
        assert!(definition.validate().is_err());
    }

    #[test]
    fn workspace_v1_is_explicitly_admitted_and_newer_versions_remain_protected() {
        let mut old = SearchWorkspace::for_scope("server-library");
        old.session.tasks.push(SearchTask {
            id: "task".into(),
            title: "Original".into(),
            ..Default::default()
        });
        old.favorites.push(SavedSearchTask {
            id: "saved".into(),
            title: "Favorite".into(),
            ..Default::default()
        });
        old.format_version = 1;
        assert!(old.validate().is_err());
        let original = old.clone();
        old.upgrade_legacy_version().unwrap();
        assert_eq!(old.format_version, 2);
        assert_eq!(old.session, original.session);
        assert_eq!(old.favorites, original.favorites);
        old.format_version = 3;
        assert!(old.upgrade_legacy_version().is_err());
        assert_eq!(old.format_version, 3);
    }

    #[test]
    fn session_validates_identity_and_limit_without_changing_favorites() {
        let mut workspace = SearchWorkspace::for_scope("server-library");
        workspace.favorites.push(SavedSearchTask {
            id: "saved".into(),
            title: "Wood".into(),
            ..Default::default()
        });
        workspace.session.tasks.push(SearchTask {
            id: "task".into(),
            title: "Search".into(),
            saved_search_id: Some("saved".into()),
            ..Default::default()
        });
        workspace.session.active_task_id = Some("task".into());
        workspace.session.tasks[0].definition.query = "metal".into();
        workspace.session.tasks[0]
            .view
            .raw_inputs
            .insert("duration_min".into(), "unfinished".into());
        workspace.validate().unwrap();
        assert_eq!(workspace.favorites[0].definition.query, "");
        workspace.session.active_task_id = Some("missing".into());
        assert!(workspace.validate().is_err());
        workspace.session.active_task_id = None;
        workspace.session.tasks = (0..=MAX_SEARCH_TASKS)
            .map(|index| SearchTask {
                id: index.to_string(),
                title: "Search".into(),
                ..Default::default()
            })
            .collect();
        assert!(workspace.validate().is_err());
    }

    #[test]
    fn thirty_task_session_round_trip_retains_order_and_last_active_task() {
        let mut workspace = SearchWorkspace::for_scope("server-library");
        workspace.session.tasks = (0..30)
            .map(|index| SearchTask {
                id: format!("task-{index}"),
                title: format!("Search {}", index + 1),
                definition: SearchTaskDefinition {
                    query: format!("sound {index}"),
                    ..Default::default()
                },
                ..Default::default()
            })
            .collect();
        workspace.session.active_task_id = Some("task-29".into());
        workspace.validate().unwrap();
        let restored: SearchWorkspace =
            serde_json::from_slice(&serde_json::to_vec(&workspace).unwrap()).unwrap();
        restored.validate().unwrap();
        assert_eq!(restored, workspace);
        workspace.session.tasks.push(SearchTask {
            id: "task-30".into(),
            title: "Search 31".into(),
            ..Default::default()
        });
        assert!(workspace.validate().is_err());
    }
}
