// app/audiolib-desktop/src/search_task_album_bindings.rs
//! Bind personal album filters by semantic identity before trusting numeric IDs.
use super::*;
use audiolib_core::search_workspace::AlbumFilterBinding;

#[cfg(test)]
#[path = "search_task_album_binding_tests.rs"]
mod tests;

fn binding(namespace: &str, album: &Album) -> AlbumFilterBinding {
    AlbumFilterBinding {
        namespace: namespace.into(),
        kind: album.kind,
        metadata_key: album.metadata_key.clone(),
        folder_path: album.folder_path.clone(),
        unavailable: false,
    }
}

fn matches(binding: &AlbumFilterBinding, album: &Album) -> bool {
    binding.kind == album.kind
        && match binding.kind {
            AlbumKind::Metadata => binding.metadata_key == album.metadata_key,
            AlbumKind::Folder => match (&binding.folder_path, &album.folder_path) {
                (Some(left), Some(right)) => audiolib_db::indexed_paths_equal(left, right),
                (None, None) => true,
                _ => false,
            },
        }
}

pub(super) fn binding_label(binding: &AlbumFilterBinding, chinese: bool) -> String {
    let label = match binding.kind {
        AlbumKind::Metadata => binding.metadata_key.as_deref().unwrap_or(if chinese {
            "未知专辑"
        } else {
            "Unknown album"
        }),
        AlbumKind::Folder => binding.folder_path.as_deref().unwrap_or(if chinese {
            "未分配音效库"
        } else {
            "Unassigned library"
        }),
    };
    if binding.unavailable {
        format!(
            "{}: {label}",
            if chinese {
                "专辑已失效"
            } else {
                "Unavailable album"
            }
        )
    } else {
        label.into()
    }
}

pub(super) fn album_filter_unavailable(definition: &SearchTaskDefinition) -> bool {
    definition.filters.album_id.is_some()
        && definition
            .album_binding
            .as_ref()
            .is_none_or(|binding| binding.unavailable)
}

/// Only a worker calls this method; failed resolution retains the old condition.
pub(super) fn resolve_album_filter(
    provider: &SqliteProvider,
    definition: &mut SearchTaskDefinition,
) -> Result<bool, String> {
    let Some(id) = definition.filters.album_id else {
        definition.album_binding = None;
        return Ok(false);
    };
    let Some(previous) = definition.album_binding.as_ref() else {
        return Ok(true);
    };
    let namespace = provider.album_namespace();
    let same_namespace = previous.namespace == namespace;
    // An unassigned folder is resolved by its reserved key, never a retired
    // folder whose deleted path also became NULL.
    if same_namespace && (previous.kind != AlbumKind::Folder || previous.folder_path.is_some()) {
        match provider.get_album(id) {
            Ok(album)
                if matches(previous, &album)
                    || (previous.kind == AlbumKind::Folder
                        && album.kind == AlbumKind::Folder
                        && previous.folder_path.is_some()
                        && album.folder_path.is_some()) =>
            {
                // Within one namespace the album allocator never reuses IDs.
                // A live folder keeps that ID when its root is relocated.
                definition.album_binding = Some(binding(&namespace, &album));
                return Ok(false);
            }
            Ok(_) | Err(audiolib_core::Error::NotFound(_)) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    let resolved = provider
        .find_album_by_semantic(
            previous.kind,
            previous.metadata_key.as_deref(),
            previous.folder_path.as_deref(),
        )
        .map_err(|error| error.to_string())?;
    if let Some(album) = resolved {
        definition.filters.album_id = Some(album.id);
        definition.album_binding = Some(binding(&namespace, &album));
        Ok(false)
    } else {
        definition.album_binding.as_mut().unwrap().unavailable = true;
        Ok(true)
    }
}

pub(super) fn reconcile_workspace_albums(
    provider: &SqliteProvider,
    workspace: &mut SearchWorkspace,
) -> Result<(), String> {
    for task in &mut workspace.session.tasks {
        resolve_album_filter(provider, &mut task.definition)?;
    }
    for favorite in &mut workspace.favorites {
        resolve_album_filter(provider, &mut favorite.definition)?;
    }
    Ok(())
}

impl Controller {
    /// A facet click names a current owned entity, replacing any blocked binding.
    pub(in crate::controller) fn admit_task_album_filter(&self, id: Option<i64>) -> bool {
        let namespace = self.provider().album_namespace();
        let mut state = self.task_workspace.lock().unwrap();
        if state.loading || state.restoring {
            return false;
        }
        let next = match id {
            Some(id) => match state
                .album_bindings
                .get(&id)
                .filter(|binding| binding.namespace == namespace && !binding.unavailable)
                .cloned()
            {
                Some(binding) => Some(binding),
                None => return false,
            },
            None => None,
        };
        let active = state.workspace.session.active_task_id.clone();
        if let Some(task) = state
            .workspace
            .session
            .tasks
            .iter_mut()
            .find(|task| Some(&task.id) == active.as_ref())
        {
            task.definition.filters.album = None;
            task.definition.filters.album_id = id;
            task.definition.album_binding = next;
        } else {
            return false;
        }
        state.unavailable_album = None;
        state.restore_failed = false;
        true
    }

    pub(in crate::controller) fn task_album_choices_only(&self, request: &SearchRequest) -> bool {
        let state = self.task_workspace.lock().unwrap();
        !state.loading
            && !state.restoring
            && !state.restore_failed
            && state.unavailable_folder.is_none()
            && state
                .unavailable_album
                .is_some_and(|id| request.filters.album_id == Some(id))
    }

    /// The existing bounded facet worker may refresh choices while results stay blocked.
    pub(in crate::controller) fn queue_unavailable_album_choices(
        &self,
        ui: &AppWindow,
        generation: u64,
    ) {
        let mut request = self.request.lock().unwrap().clone();
        if !ui.get_album_filter_unavailable() || !self.task_album_choices_only(&request) {
            return;
        }
        request.query = ui.get_query().to_string();
        let cancel = Arc::new(AtomicBool::new(false));
        std::mem::replace(&mut *self.search_cancel.lock().unwrap(), cancel.clone())
            .store(true, Ordering::Release);
        *self.facet_job.lock().unwrap() = Some(SearchJob {
            request,
            generation,
            not_before: Instant::now(),
            cancel,
            facet_revision: self.facet_revision.load(Ordering::Acquire),
            provider: self.provider(),
        });
        self.facet_wake.notify_one();
    }

    /// Accept owned facet identities after the publication generation check.
    pub(in crate::controller) fn remember_task_album_bindings(
        &self,
        namespace: String,
        albums: impl IntoIterator<Item = Album>,
    ) {
        let bindings = albums
            .into_iter()
            .map(|album| (album.id, binding(&namespace, &album)))
            .collect();
        self.task_workspace.lock().unwrap().album_bindings = bindings;
    }

    /// Event-loop capture uses owned facets/rows and cached namespace getters.
    pub(super) fn capture_task_album_binding(
        &self,
        request: &SearchRequest,
        previous: Option<&SearchTask>,
        records: &[AudioFile],
    ) -> Option<AlbumFilterBinding> {
        let id = request.filters.album_id?;
        let prior = previous
            .filter(|task| task.definition.filters.album_id == Some(id))
            .and_then(|task| task.definition.album_binding.clone());
        let namespace = self.provider().album_namespace();
        if prior
            .as_ref()
            .is_some_and(|binding| binding.unavailable || binding.namespace != namespace)
        {
            return prior;
        }
        let state = self.task_workspace.lock().unwrap();
        if state.unavailable_album == Some(id) {
            return prior;
        }
        let owned = state
            .album_bindings
            .get(&id)
            .filter(|binding| binding.namespace == namespace)
            .cloned();
        drop(state);
        owned
            .or_else(|| {
                records
                    .iter()
                    .filter_map(|file| file.album_entity.as_ref())
                    .find(|album| album.id == id)
                    .map(|album| binding(&namespace, album))
            })
            .or(prior)
    }
}
