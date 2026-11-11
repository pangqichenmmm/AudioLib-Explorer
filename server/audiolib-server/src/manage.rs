// server/audiolib-server/src/manage.rs
//! Library management that removes records: `POST /audio/remove` and
//! `DELETE /folders/{id}` (doc/api/README.md §6.8), plus `GET /capabilities`.
//!
//! Removal is the library's own `remove_audio` / `remove_folder` (local-mode semantics):
//! it deletes `audio_files` rows and the rows that reference them (`playlist_items`,
//! `project_references`, `waveform_cache`), and for a folder the folder
//! rows of its subtree and their `folder_closure` entries, then refreshes the folder
//! statistics, all in one transaction. **No file is touched**: files still on disk come
//! back with the next scan. Both endpoints are refused with 409 `BUSY` while a scan (of
//! this server, or a CLI / local-mode desktop scan or administration operation holding
//! the library lease) runs or is paused, and are gated by `manage_mode`
//! (`crate::auth::ManageMode`, 403 `MANAGE_FORBIDDEN`) like scans.
use crate::{
    ApiError, AppState,
    auth::ManageMode,
    state::{folder_key, is_ancestor},
};
use audiolib_core::{DataProvider, Error};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Largest accepted `ids` list of `POST /audio/remove` (the library's own bound).
pub const MAX_REMOVE_IDS: usize = 10_000;
/// Ids per existence query (well below SQLite's bound-parameter limit).
const ID_QUERY_CHUNK: usize = 500;

/// Body of `POST /audio/remove`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoveAudioRequest {
    /// 1..=10000 positive audio ids; duplicates are ignored.
    pub ids: Vec<i64>,
}

/// Reply of `POST /audio/remove`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoveAudioResponse {
    /// Records removed from the library.
    pub removed: u64,
    /// Requested ids without a record (not an error), in request order without duplicates.
    pub not_found: Vec<i64>,
    /// Library revision covering every commit before this request.
    pub revision_before: u64,
    /// `revision_before + 1` when something was removed, otherwise `revision_before`.
    pub revision: u64,
}

/// Reply of `DELETE /folders/{id}`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoveFolderResponse {
    /// Audio records removed (the whole subtree).
    pub removed: u64,
    /// Folder records removed: the folder itself and every folder below it.
    pub folders_removed: u64,
    /// Library revision covering every commit before this request.
    pub revision_before: u64,
    /// `revision_before + 1`.
    pub revision: u64,
}

/// Reply of `GET /capabilities`: what this client may do on this server.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilitiesResponse {
    /// Shared application/version and database-format compatibility catalog.
    pub compatibility: audiolib_core::compatibility::CompatibilityManifest,
    /// Actual source-library schema and formats offered during the transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_schema: Option<audiolib_core::server_upgrade::LibrarySchemaInfo>,
    /// Explicit migration readiness and its managed endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_migration: Option<audiolib_core::server_upgrade::SchemaMigrationInfo>,
    /// Real importer handlers and enforced resource limits, absent while not ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_import: Option<audiolib_core::server_upgrade::VendorImportInfo>,
    /// Upgrade hint for the representation selected for this client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_upgrade: Option<audiolib_core::server_upgrade::ClientUpgradeInfo>,
    /// Query scopes implemented by this server, including strict filename matching.
    #[serde(default)]
    pub search_scopes: Vec<String>,
    /// Effective `manage_mode`: `all`, `lan` or `off`.
    pub manage_mode: ManageMode,
    /// Whether this request was classified as external (through a reverse proxy or from
    /// a non-private peer, doc/api/README.md §2.2).
    pub external: bool,
    /// Whether this client may start/control scans and remove records or folders now.
    pub can_manage: bool,
    /// This API supports the stable server directory browser and root mutations.
    pub root_management: bool,
    /// `POST /waveforms` support (doc/api/README.md §9.1); absent on older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_waveforms: Option<crate::list_waveform::ListWaveformsCapability>,
    /// Effective `pregenerate_list_waveforms` (read-only here; see `PATCH /settings`).
    #[serde(default)]
    pub pregenerate_list_waveforms: bool,
    /// List waveform cache jobs and the `list_waveform_rate` setting (§9.5); absent on
    /// older servers (= unsupported).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_waveform_jobs: Option<crate::wf_job::ListWaveformJobsCapability>,
}

impl AppState {
    /// Blocking. 409 `BUSY` while a scan of this server runs or is paused, or while any
    /// process holds the library's scan lease (CLI or local-mode desktop scans,
    /// restore, maintenance, ...).
    pub fn ensure_no_scan(&self) -> Result<(), ApiError> {
        if let Some(scan) = self.scans().current() {
            let id = scan.progress.session_id;
            return Err(Error::Busy(if id > 0 {
                format!(
                    "A scan is running for this library (session {id}); retry when it has finished"
                )
            } else {
                "A scan is starting for this library; retry when it has finished".into()
            })
            .into());
        }
        Ok(self.provider().ensure_library_idle()?)
    }

    /// Blocking. Ids of `ids` that have an `audio_files` record.
    fn existing_audio_ids(&self, ids: &[i64]) -> audiolib_core::Result<HashSet<i64>> {
        let mut found = HashSet::new();
        for chunk in ids.chunks(ID_QUERY_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!("SELECT id FROM audio_files WHERE id IN ({placeholders})");
            self.read_query(|conn| {
                let mut statement = conn.prepare(&sql)?;
                let rows = statement.query_map(rusqlite::params_from_iter(chunk), |row| {
                    row.get::<_, i64>(0)
                })?;
                for id in rows {
                    found.insert(id?);
                }
                Ok(())
            })?;
        }
        Ok(found)
    }

    /// Blocking. `POST /audio/remove`: remove the records of `ids` (never any file).
    /// Unknown ids are reported in `not_found`; the revision is bumped once only when a
    /// record was removed. When none of the ids exists the library is not written at all.
    pub fn remove_audio(&self, ids: &[i64]) -> Result<RemoveAudioResponse, ApiError> {
        if ids.is_empty() || ids.len() > MAX_REMOVE_IDS {
            return Err(ApiError::validation(format!(
                "ids must contain 1..={MAX_REMOVE_IDS} audio ids, got {}",
                ids.len()
            )));
        }
        if let Some(bad) = ids.iter().find(|id| **id <= 0) {
            return Err(ApiError::validation(format!(
                "ids must be positive integers, got {bad}"
            )));
        }
        let mut seen = HashSet::new();
        let ids: Vec<i64> = ids.iter().copied().filter(|id| seen.insert(*id)).collect();
        self.ensure_no_scan()?;
        let outcome = self.write_if(
            |provider| {
                let existing = self.existing_audio_ids(&ids)?;
                let (present, not_found): (Vec<i64>, Vec<i64>) =
                    ids.iter().copied().partition(|id| existing.contains(id));
                let removed = if present.is_empty() {
                    0
                } else {
                    provider.remove_audio(&present)?
                };
                Ok((removed, not_found))
            },
            |(removed, _)| *removed > 0,
        )?;
        let (removed, not_found) = outcome.value;
        if outcome.revision != outcome.revision_before {
            self.invalidate_snapshot();
        }
        if removed > 0 {
            self.list_waveforms().request_prune();
        }
        tracing::info!(
            requested = ids.len(),
            removed,
            not_found = not_found.len(),
            revision = outcome.revision,
            "Removed audio records (files untouched)"
        );
        Ok(RemoveAudioResponse {
            removed,
            not_found,
            revision_before: outcome.revision_before,
            revision: outcome.revision,
        })
    }

    /// Blocking. Path of folder `id` and the number of folders in its subtree (itself
    /// included); `None` when there is no such folder.
    fn folder_subtree(&self, id: i64) -> audiolib_core::Result<Option<(String, u64)>> {
        self.read_query(|conn| {
            let Some(path) = conn
                .query_row("SELECT folder_path FROM folders WHERE id=?1", [id], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?
            else {
                return Ok(None);
            };
            let count: i64 = conn.query_row(
                "WITH RECURSIVE tree(id) AS (SELECT ?1 UNION \
                 SELECT f.id FROM folders f JOIN tree ON f.parent_id=tree.id) \
                 SELECT COUNT(*) FROM tree",
                [id],
                |row| row.get(0),
            )?;
            Ok(Some((path, u64::try_from(count).unwrap_or_default())))
        })
    }

    /// Name of the configured root whose top-level folder is `path`, or that lies below
    /// `path` (removing `path` would remove that root's whole tree).
    fn root_covered_by(&self, path: &str) -> Option<String> {
        let folder = folder_key(path);
        let monitor = self.root_monitor();
        monitor
            .roots()
            .iter()
            .enumerate()
            .find(|(index, root)| {
                let configured = Some(root.server_path.clone());
                let canonical = monitor.status(*index).canonical;
                [configured, canonical].into_iter().flatten().any(|root| {
                    let key = folder_key(&root.to_string_lossy());
                    !key.is_empty() && (key == folder || is_ancestor(&folder, &key))
                })
            })
            .map(|(_, root)| root.name.clone())
    }

    /// Blocking. `DELETE /folders/{id}`: remove the records of the folder's whole subtree
    /// (never any file). Unknown folders are 404; a root's top-level folder (or a folder
    /// containing a root) is refused with 400 `VALIDATION`.
    pub fn remove_folder(&self, id: i64) -> Result<RemoveFolderResponse, ApiError> {
        self.remove_folder_tree(id, true)
    }

    /// Blocking. Remove folder `id`'s subtree; `protect_roots` refuses a root's top-level
    /// folder (the admin page's root removal passes `false`).
    fn remove_folder_tree(
        &self,
        id: i64,
        protect_roots: bool,
    ) -> Result<RemoveFolderResponse, ApiError> {
        self.ensure_no_scan()?;
        let Some((path, _)) = self.folder_subtree(id)? else {
            return Err(Error::NotFound(format!("Folder {id}")).into());
        };
        if let Some(root) = protect_roots.then(|| self.root_covered_by(&path)).flatten() {
            return Err(ApiError::validation(format!(
                "Folder {id} is the top-level folder of library root {root:?} (or contains it) and cannot be removed through the API; remove the library root on the server's admin page (管理页 /admin → 库根目录)"
            )));
        }
        let outcome = self.write_if(
            |provider| {
                let folders = self
                    .folder_subtree(id)?
                    .map(|(_, count)| count)
                    .ok_or_else(|| Error::NotFound(format!("Folder {id}")))?;
                Ok((provider.remove_folder(id)?, folders))
            },
            |_| true,
        )?;
        let (removed, folders_removed) = outcome.value;
        self.invalidate_snapshot();
        if removed > 0 {
            self.list_waveforms().request_prune();
        }
        tracing::info!(
            folder = id,
            removed,
            folders_removed,
            revision = outcome.revision,
            "Removed a folder's records (files untouched)"
        );
        Ok(RemoveFolderResponse {
            removed,
            folders_removed,
            revision_before: outcome.revision_before,
            revision: outcome.revision,
        })
    }
}

/// `POST /admin/api/roots`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddRootRequest {
    /// Client-visible root name (unique, case-insensitively).
    pub name: String,
    /// Absolute directory on the server.
    pub path: String,
}

/// Body of `PATCH /api/v1/roots/{name}`. The root name remains stable.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootPathRequest {
    /// Absolute replacement directory on the server.
    pub path: String,
}

/// Durable recovery between an atomic SQLite relocation and root-settings storage.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootRelinkJournal {
    /// Top-level folder whose stable identity determines committed state.
    pub root_id: i64,
    /// Folder path stored before the SQLite transaction.
    pub source: String,
    /// Folder path stored after the SQLite transaction.
    pub target: String,
    /// Complete root list before relocation.
    pub before: Vec<crate::config::RootConfig>,
    /// Complete root list after relocation.
    pub after: Vec<crate::config::RootConfig>,
}

/// Recoverable pairing of one atomic folder removal with the root-list update.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootRemovalJournal {
    /// Folder identity removed in the SQLite transaction.
    pub root_id: i64,
    /// Original folder path; a different reused identity is never accepted.
    pub source: String,
    /// Complete root list before removal.
    pub before: Vec<crate::config::RootConfig>,
    /// Complete root list after removal.
    pub after: Vec<crate::config::RootConfig>,
}

/// Resolve interrupted root relocation from its preserved folder identity. The
/// database transaction is authoritative: before commit use the old root list,
/// after commit use the new one. Never guess if the folder no longer matches.
pub(crate) fn recover_root_relink(
    provider: &audiolib_db::SqliteProvider,
    security: &crate::Security,
) -> audiolib_core::Result<Option<Vec<crate::config::RootConfig>>> {
    let lease = security.settings_lease()?;
    recover_root_relink_locked(provider, security, &lease, |_| {})
}

/// Apply the database's selected root list before persistence, so storage failure
/// cannot leave an already-running server monitoring the pre-commit directories.
fn recover_root_relink_locked(
    provider: &audiolib_db::SqliteProvider,
    security: &crate::Security,
    lease: &crate::auth::SettingsLease,
    apply: impl FnOnce(&[crate::config::RootConfig]),
) -> audiolib_core::Result<Option<Vec<crate::config::RootConfig>>> {
    if let Some(journal) = security.root_removal() {
        let roots = match provider.get_folder(journal.root_id) {
            Ok(root) if same_root_path(&root.path, &journal.source) => journal.before,
            Err(Error::NotFound(_)) => journal.after,
            Ok(_) => {
                return Err(Error::Validation(
                    "Interrupted root removal has an unrelated folder identity".into(),
                ));
            }
            Err(error) => return Err(error),
        };
        apply(&roots);
        security
            .update_settings_locked(lease, |settings| {
                settings.roots = Some(roots.clone());
                settings.root_removal = None;
            })
            .map_err(|error| {
                Error::Database(format!("Cannot complete root removal recovery: {error:?}"))
            })?;
        return Ok(Some(roots));
    }
    let Some(journal) = security.root_relink() else {
        return Ok(None);
    };
    let root = provider.get_folder(journal.root_id)?;
    let roots = if same_root_path(&root.path, &journal.target) {
        journal.after
    } else if same_root_path(&root.path, &journal.source) {
        journal.before
    } else {
        return Err(Error::Validation(
            "Interrupted root relocation folder path does not match either saved state".into(),
        ));
    };
    apply(&roots);
    security
        .update_settings_locked(lease, |settings| {
            settings.roots = Some(roots.clone());
            settings.root_relink = None;
        })
        .map_err(|error| {
            Error::Database(format!(
                "Cannot complete root relocation recovery: {error:?}"
            ))
        })?;
    Ok(Some(roots))
}

// Folded search keys are not filesystem identities on a POSIX server: a case-only
// directory rename must still update paths and choose the correct journal state.
fn same_root_path(left: &str, right: &str) -> bool {
    #[cfg(windows)]
    {
        folder_key(left) == folder_key(right)
    }
    #[cfg(not(windows))]
    {
        left.trim_end_matches('/') == right.trim_end_matches('/')
    }
}

/// `DELETE /admin/api/roots/{name}`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoveRootResponse {
    pub name: String,
    /// Audio records removed with the root.
    pub removed: u64,
    /// Folder records removed with the root.
    pub folders_removed: u64,
    pub revision_before: u64,
    pub revision: u64,
}

impl AppState {
    /// Reload root settings and reconcile interrupted writes while the caller holds
    /// both the local root-edit lock and the cross-instance settings lease. Scans
    /// retain that lease until the scanner acquires the database's scan lease.
    pub(crate) fn refresh_roots_locked(
        &self,
        lease: &crate::auth::SettingsLease,
    ) -> Result<(), ApiError> {
        let apply = |roots: &[crate::config::RootConfig]| {
            if self.root_monitor().roots() != roots {
                self.replace_roots(roots);
            }
        };
        if let Some(roots) = self.security().roots_override() {
            apply(&roots);
        }
        recover_root_relink_locked(self.provider(), self.security(), lease, apply)?;
        Ok(())
    }

    /// Blocking. Add a library root (admin page): the directory must exist, be absolute
    /// and neither lie inside nor contain another root. It is stored in
    /// `admin-settings.json` (replacing the `config.toml` list from then on) and applies
    /// at once.
    pub fn add_root(&self, request: AddRootRequest) -> Result<crate::RootResponse, ApiError> {
        self.add_root_cancellable(request, Arc::new(AtomicBool::new(false)))
    }

    /// Add a root only while the requesting client still wants the result; a slow
    /// filesystem check cannot register a root after the browser reports a timeout.
    pub fn add_root_cancellable(
        &self,
        request: AddRootRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<crate::RootResponse, ApiError> {
        let _edit = self.roots_edit_lock();
        let lease = self.security().settings_lease()?;
        self.refresh_roots_locked(&lease)?;
        self.ensure_no_scan()?;
        let name = request.name;
        crate::config::check_root_name(&name).map_err(|e| ApiError::validation(e.to_string()))?;
        let path = crate::browse::resolve_directory(&request.path)?;
        let monitor = self.root_monitor();
        let mut roots = monitor.roots().to_vec();
        if roots
            .iter()
            .any(|root| root.name.eq_ignore_ascii_case(&name))
        {
            return Err(ApiError::validation(format!(
                "A library root named {name:?} already exists"
            )));
        }
        let new_key = folder_key(&path.to_string_lossy());
        for (index, root) in roots.iter().enumerate() {
            let canonical = monitor.status(index).canonical;
            for existing in [Some(root.server_path.clone()), canonical]
                .into_iter()
                .flatten()
            {
                let key = folder_key(&existing.to_string_lossy());
                if key == new_key || is_ancestor(&key, &new_key) || is_ancestor(&new_key, &key) {
                    return Err(ApiError::validation(format!(
                        "{} overlaps the library root {:?} ({}): a root may neither lie inside nor contain another",
                        path.display(),
                        root.name,
                        root.server_path.display()
                    )));
                }
            }
        }
        roots.push(crate::config::RootConfig {
            name: name.clone(),
            server_path: path,
            webdav_url: None,
        });
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled.into());
        }
        self.security()
            .update_settings_locked(&lease, |settings| settings.roots = Some(roots.clone()))?;
        self.replace_roots(&roots);
        tracing::info!(root = %name, "Library root added on the admin page");
        let monitor = self.root_monitor();
        let index = roots.len() - 1;
        Ok(self.root_response(&monitor, index, monitor.status(index))?)
    }

    /// Folder ids of the library holding exactly `root`'s top-level directory.
    fn root_folder_ids(
        &self,
        root: &crate::config::RootConfig,
        canonical: Option<std::path::PathBuf>,
    ) -> Result<Vec<i64>, ApiError> {
        let mut texts: Vec<String> = [Some(root.server_path.clone()), canonical]
            .into_iter()
            .flatten()
            .map(|path| {
                let text = audiolib_db::folder_path_text(&path);
                if path.file_name().is_none() {
                    text
                } else {
                    text.trim_end_matches('/').to_string()
                }
            })
            .collect();
        texts.sort();
        texts.dedup();
        let mut ids = Vec::new();
        for text in texts {
            let found = self.read_query(|conn| {
                conn.query_row(
                    "SELECT id FROM folders WHERE folder_path = ?1 COLLATE NOCASE",
                    [&text],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
            })?;
            ids.extend(found);
        }
        ids.dedup();
        Ok(ids)
    }

    /// Blocking. Remove library root `name` (admin page): its folder and audio records go
    /// (like `DELETE /folders/{id}`; no file is touched) and it disappears from the root
    /// list, persisted and applied at once. 409 `BUSY` while a scan runs, 404 when unknown.
    pub fn remove_root(&self, name: &str) -> Result<RemoveRootResponse, ApiError> {
        self.remove_root_cancellable(name, Arc::new(AtomicBool::new(false)))
    }

    /// Removal queued behind another root edit stops before starting any writes if
    /// its client has disconnected. Once removal begins, drain the requested write.
    pub fn remove_root_cancellable(
        &self,
        name: &str,
        cancel: Arc<AtomicBool>,
    ) -> Result<RemoveRootResponse, ApiError> {
        let _edit = self.roots_edit_lock();
        let lease = self.security().settings_lease()?;
        self.refresh_roots_locked(&lease)?;
        self.ensure_no_scan()?;
        let monitor = self.root_monitor();
        let index = monitor
            .roots()
            .iter()
            .position(|root| root.name == name)
            .ok_or_else(|| ApiError::not_found(format!("No library root named {name:?}")))?;
        let root = monitor.roots()[index].clone();
        let ids = self.root_folder_ids(&root, monitor.status(index).canonical)?;
        if ids.len() > 1 {
            return Err(ApiError::validation(
                "The root has multiple stored path identities; repair it before removal",
            ));
        }
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled.into());
        }
        let roots: Vec<_> = monitor
            .roots()
            .iter()
            .filter(|other| other.name != name)
            .cloned()
            .collect();
        if let Some(id) = ids.first() {
            let source = self.provider().get_folder(*id)?.path;
            // The rollback snapshot retains annotations and the complete tree even
            // if the process or settings storage fails after the requested deletion.
            let backup_dir = self
                .provider()
                .database_path()
                .parent()
                .unwrap_or(Path::new("."))
                .join("backups");
            std::fs::create_dir_all(&backup_dir).map_err(Error::from)?;
            let backup = backup_dir.join(format!("before-root-remove-{}.db", uuid::Uuid::new_v4()));
            self.provider().backup_library(&backup)?;
            if cancel.load(Ordering::Acquire) {
                return Err(Error::Cancelled.into());
            }
            let journal = RootRemovalJournal {
                root_id: *id,
                source,
                before: monitor.roots().to_vec(),
                after: roots.clone(),
            };
            self.security()
                .update_settings_locked(&lease, |settings| settings.root_removal = Some(journal))?;
        }
        let (mut removed, mut folders_removed) = (0, 0);
        let (mut before, mut after) = {
            let info = self.revision()?;
            (info.revision, info.revision)
        };
        for (position, id) in ids.iter().enumerate() {
            match self.remove_folder_tree(*id, false) {
                Ok(done) => {
                    if position == 0 {
                        before = done.revision_before;
                    }
                    removed += done.removed;
                    folders_removed += done.folders_removed;
                    after = done.revision;
                }
                Err(error) if error.status() == axum::http::StatusCode::NOT_FOUND => {}
                Err(error) => {
                    recover_root_relink_locked(
                        self.provider(),
                        self.security(),
                        &lease,
                        |roots| {
                            self.replace_roots(roots);
                            self.invalidate_snapshot();
                        },
                    )?;
                    return Err(error);
                }
            }
        }
        let saved = self.security().update_settings_locked(&lease, |settings| {
            settings.roots = Some(roots.clone());
            settings.root_removal = None;
        });
        self.replace_roots(&roots);
        saved?;
        tracing::info!(root = %name, removed, folders_removed, "Library root removed on the admin page (files untouched)");
        Ok(RemoveRootResponse {
            name: name.to_string(),
            removed,
            folders_removed,
            revision_before: before,
            revision: after,
        })
    }

    /// Blocking. Validate the replacement directory and relocate every indexed WAV
    /// atomically. Missing/invalid targets reject the whole operation. IDs, Metadata
    /// and annotations survive; the scanner subsequently reuses the relocated tree.
    pub fn update_root_path(
        &self,
        name: &str,
        request: RootPathRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<crate::RootResponse, ApiError> {
        let _edit = self.roots_edit_lock();
        let lease = self.security().settings_lease()?;
        self.refresh_roots_locked(&lease)?;
        self.ensure_no_scan()?;
        let monitor = self.root_monitor();
        let index = monitor
            .roots()
            .iter()
            .position(|root| root.name == name)
            .ok_or_else(|| ApiError::not_found(format!("No library root named {name:?}")))?;
        let path = crate::browse::resolve_directory(&request.path)?;
        let new_key = folder_key(&path.to_string_lossy());
        for (position, root) in monitor.roots().iter().enumerate() {
            if position == index {
                continue;
            }
            for existing in [
                Some(root.server_path.clone()),
                monitor.status(position).canonical,
            ]
            .into_iter()
            .flatten()
            {
                let key = folder_key(&existing.to_string_lossy());
                if key == new_key || is_ancestor(&key, &new_key) || is_ancestor(&new_key, &key) {
                    return Err(ApiError::validation(format!(
                        "Replacement path overlaps the library root {:?}",
                        root.name
                    )));
                }
            }
        }
        let before = monitor.roots().to_vec();
        let mut after = before.clone();
        after[index].server_path = path.clone();
        // A WebDAV location for the old directory would serve the wrong resources.
        if before[index].server_path != path {
            after[index].webdav_url = None;
        }
        let source = &before[index].server_path;
        if source.file_name().is_none() || path.file_name().is_none() {
            return Err(ApiError::validation(
                "File system or drive roots cannot be relocated",
            ));
        }
        if same_root_path(&source.to_string_lossy(), &path.to_string_lossy()) {
            return Ok(self.root_response(&monitor, index, monitor.status(index))?);
        }
        let ids = self.root_folder_ids(&before[index], monitor.status(index).canonical)?;
        if ids.len() > 1 {
            return Err(ApiError::validation(
                "The root has multiple stored path identities; repair it before relocation",
            ));
        }
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled.into());
        }
        if let Some(id) = ids.first() {
            let stored_source = self
                .folder_subtree(*id)?
                .ok_or_else(|| ApiError::not_found("Root folder disappeared"))?
                .0;
            let journal = RootRelinkJournal {
                root_id: *id,
                source: stored_source.clone(),
                target: audiolib_db::folder_path_text(&path),
                before: before.clone(),
                after: after.clone(),
            };
            self.security()
                .update_settings_locked(&lease, |settings| settings.root_relink = Some(journal))?;
            let done = self.write_if(
                |provider| {
                    provider.relink_folder_cancellable(Path::new(&stored_source), &path, cancel)
                },
                |_| true,
            );
            match done {
                Ok(_) => self.invalidate_snapshot(),
                Err(error) => {
                    // Recovery also covers a revision-sidecar failure after SQLite
                    // committed; inspect the folder instead of assuming rollback.
                    recover_root_relink_locked(
                        self.provider(),
                        self.security(),
                        &lease,
                        |roots| {
                            self.replace_roots(roots);
                            self.invalidate_snapshot();
                        },
                    )?;
                    return Err(error.into());
                }
            }
        }
        // The durable journal remains if final storage fails. A restart or the next
        // root edit completes it from SQLite without losing any annotations.
        let saved = self.security().update_settings_locked(&lease, |settings| {
            settings.roots = Some(after.clone());
            settings.root_relink = None;
        });
        self.replace_roots(&after);
        saved?;
        let monitor = self.root_monitor();
        Ok(self.root_response(&monitor, index, monitor.status(index))?)
    }
}

#[cfg(test)]
mod root_recovery_tests {
    use super::*;
    use crate::{BearerToken, ServerConfig};
    use std::{fs, path::PathBuf};

    fn fixture() -> (tempfile::TempDir, ServerConfig, AppState, i64) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("sounds");
        fs::create_dir(&root).unwrap();
        let database = dir.path().join("library.db");
        drop(audiolib_db::SqliteProvider::open(&database).unwrap());
        let conn = rusqlite::Connection::open(&database).unwrap();
        conn.execute(
            "INSERT INTO folders(folder_name,folder_path) VALUES('sounds',?1)",
            [audiolib_db::folder_path_text(&root)],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        drop(conn);
        let config = ServerConfig::from_toml_str(
            &format!(
                "bind = '127.0.0.1:0'\nlibrary_db = 'library.db'\n[[roots]]\nname = 'SFX'\nserver_path = '{}'\n",
                root.display()
            ),
            dir.path(),
        )
        .unwrap();
        let state = AppState::open(&config, None::<BearerToken>).unwrap();
        (dir, config, state, id)
    }

    #[test]
    fn active_root_journal_blocks_independent_startup_and_settings_changes() {
        let (_dir, config, state, id) = fixture();
        let lease = state.security().settings_lease().unwrap();
        let journal = RootRemovalJournal {
            root_id: id,
            source: state.provider().get_folder(id).unwrap().path,
            before: config.roots.clone(),
            after: Vec::new(),
        };
        state
            .security()
            .update_settings_locked(&lease, |settings| settings.root_removal = Some(journal))
            .unwrap();
        let independent = crate::Security::from_config(&config, None);
        assert!(matches!(
            AppState::open_with_security(&config, independent.clone()),
            Err(Error::Busy(_))
        ));
        assert_eq!(
            independent
                .update_settings(|settings| settings.lan_url = Some("http://localhost".into()))
                .unwrap_err()
                .status(),
            axum::http::StatusCode::CONFLICT
        );
        let persisted: crate::auth::AdminSettings = serde_json::from_slice(
            &fs::read(config.admin_dir().join(crate::auth::SETTINGS_FILE)).unwrap(),
        )
        .unwrap();
        assert!(persisted.root_removal.is_some());
        drop(lease);
        let restarted = AppState::open_with_security(&config, independent).unwrap();
        assert_eq!(restarted.root_monitor().roots().len(), 1);
        assert!(restarted.security().root_removal().is_none());
        assert!(config.admin_dir().join(".admin-settings.lock").is_file());
    }

    #[test]
    fn independent_root_edits_reload_the_latest_configured_root_list() {
        let (_dir, config, first, _) = fixture();
        let second = AppState::open(&config, None::<BearerToken>).unwrap();
        for (state, name) in [(&first, "First"), (&second, "Second")] {
            let path = config.admin_dir().join(name);
            fs::create_dir(&path).unwrap();
            state
                .add_root(AddRootRequest {
                    name: name.into(),
                    path: path.display().to_string(),
                })
                .unwrap();
        }
        let names: Vec<_> = second
            .root_monitor()
            .roots()
            .iter()
            .map(|root| root.name.clone())
            .collect();
        assert_eq!(names, ["SFX", "First", "Second"]);
        let restarted = AppState::open(&config, None::<BearerToken>).unwrap();
        assert_eq!(restarted.root_monitor().roots().len(), 3);
    }

    #[test]
    fn committed_root_recovery_applies_monitor_even_when_settings_cannot_persist() {
        for relocation in [false, true] {
            let (_dir, config, state, id) = fixture();
            let lease = state.security().settings_lease().unwrap();
            let source = state.provider().get_folder(id).unwrap().path;
            let mut after = config.roots.clone();
            if relocation {
                let target: PathBuf = config.admin_dir().join("moved");
                fs::create_dir(&target).unwrap();
                after[0].server_path = target.clone();
                state
                    .security()
                    .update_settings_locked(&lease, |settings| {
                        settings.root_relink = Some(RootRelinkJournal {
                            root_id: id,
                            source,
                            target: audiolib_db::folder_path_text(&target),
                            before: config.roots.clone(),
                            after: after.clone(),
                        });
                    })
                    .unwrap();
                let conn = rusqlite::Connection::open(&config.library_db).unwrap();
                conn.execute(
                    "UPDATE folders SET folder_path=?1 WHERE id=?2",
                    rusqlite::params![audiolib_db::folder_path_text(&target), id],
                )
                .unwrap();
            } else {
                after.clear();
                state
                    .security()
                    .update_settings_locked(&lease, |settings| {
                        settings.root_removal = Some(RootRemovalJournal {
                            root_id: id,
                            source,
                            before: config.roots.clone(),
                            after: after.clone(),
                        });
                    })
                    .unwrap();
                state.provider().remove_folder(id).unwrap();
            }
            // An owned directory at the file destination makes the final atomic
            // persist fail on Windows and Unix without changing filesystem ACLs.
            let settings_path = config.admin_dir().join(crate::auth::SETTINGS_FILE);
            fs::rename(&settings_path, settings_path.with_extension("saved.json")).unwrap();
            fs::create_dir(&settings_path).unwrap();
            assert!(
                recover_root_relink_locked(state.provider(), state.security(), &lease, |roots| {
                    state.replace_roots(roots)
                })
                .is_err()
            );
            let actual: Vec<_> = state
                .root_monitor()
                .roots()
                .iter()
                .map(|root| root.server_path.clone())
                .collect();
            assert_eq!(
                actual,
                after
                    .into_iter()
                    .map(|root| root.server_path)
                    .collect::<Vec<_>>()
            );
            assert!(
                state.security().root_relink().is_some()
                    || state.security().root_removal().is_some()
            );
        }
    }
}
