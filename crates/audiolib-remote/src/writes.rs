// crates/audiolib-remote/src/writes.rs
//! Server-mode metadata writes (API §6.3) and the revision rule that keeps the
//! local snapshot current without a full download.
//!
//! Every write reply carries `revision_before` and `revision`. When the cached snapshot
//! is exactly at `revision_before`, nothing else changed on the server in between, so the
//! returned records are written into the snapshot and the cache moves to `revision`.
//! Otherwise (another client wrote meanwhile, no snapshot yet, apply failed) the caller
//! runs a full snapshot sync. A write whose outcome is unknown (timeout / `408`, dropped
//! connection, unreadable reply) also needs a sync: the server may have committed it.
use crate::client::{BatchWrite, FileWrite, RemoteClient};
use crate::error::{RemoteError, Result};
use crate::manage::{RemoveReport, manage_error_text};
use crate::sync::RemoteCache;
use audiolib_core::waveform_cache::{
    WaveformCacheCapabilities, WaveformCacheResult, WaveformCacheSettings,
    WaveformCacheSettingsPatch, WaveformCacheStatus, WaveformJobControl, WaveformJobMode,
};
use audiolib_core::{
    Album, AlbumFacet, AudioFile, BackupReport, BatchMetadataReport, BatchMetadataRequest,
    DataProvider, Error, Folder, IntegrityReport, LibraryExportFormat, LibraryInfo, LibraryStats,
    ListWaveformBatch, ListWaveformSource, MaintenanceAction, MaintenanceReport, MetadataPatch,
    RestoreReport, ScanSession, SearchPage, SearchRequest, TransferReport,
};
use audiolib_db::SqliteProvider;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
#[cfg(test)]
use std::time::Duration;

/// Serializes read-modify-write of `cache.json` + snapshot between concurrent writes
/// from different Metadata operations.
static APPLY_LOCK: Mutex<()> = Mutex::new(());

/// Short cache publication lease shared by snapshot installs and annotation replies.
/// The file is deliberately retained after unlocking, including across processes.
pub(crate) struct CachePublicationLease {
    _file: std::fs::File,
    _process: Option<MutexGuard<'static, ()>>,
}

impl Drop for CachePublicationLease {
    fn drop(&mut self) {
        // Unlock before releasing the process mutex: Windows may defer lock release
        // when relying only on CloseHandle, making an immediate cleanup look busy.
        let _ = self._file.unlock();
    }
}

pub(crate) fn cache_publication_lease(
    cache: &RemoteCache,
    library_id: &str,
) -> Result<CachePublicationLease> {
    let process = APPLY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    publication_file_lease(cache, library_id, Some(process))
}

/// Cleanup uses the per-library OS lock only: independent open handles exclude both
/// local and other-process publishers without being stalled by an unrelated library.
pub(crate) fn try_cache_publication_lease(
    cache: &RemoteCache,
    library_id: &str,
) -> Result<CachePublicationLease> {
    publication_file_lease(cache, library_id, None)
}

fn publication_file_lease(
    cache: &RemoteCache,
    library_id: &str,
    process: Option<MutexGuard<'static, ()>>,
) -> Result<CachePublicationLease> {
    // Keep locks outside library payloads: swap cleanup can remove an old library.
    let directory = cache.root().join(".publication-locks");
    if std::fs::symlink_metadata(&directory).is_ok_and(|metadata| !metadata.file_type().is_dir()) {
        return Err(RemoteError::Validation(
            "Cache publication directory must not be linked".into(),
        ));
    }
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{}.lock", crate::sync::directory_name(library_id)));
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.file_type().is_file()) {
        return Err(RemoteError::Validation(
            "Cache publication lock must be a regular file".into(),
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock().map_err(|_| RemoteError::Busy {
        message: "Another process is publishing this library cache".into(),
        retry_after: None,
    })?;
    Ok(CachePublicationLease {
        _process: process,
        _file: file,
    })
}

/// Order this process's non-idempotent write requests through their cache publication.
/// Locking only application permits a faster later reply to outrun an earlier one and
/// falsely request a full snapshot. Separate `RemoteWriter`s share this gate; network
/// reads, UI work and audio callbacks never acquire it. Other clients' writes still
/// produce an ordinary revision mismatch and require a validated snapshot sync.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// What happened to the local snapshot after a successful server write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheUpdate {
    /// The returned records were written into the snapshot; the cache is at `revision`.
    Applied { revision: u64 },
    /// The snapshot could not be advanced safely: run a full snapshot sync.
    Resync { reason: String },
}

/// A successful server write and its effect on the local cache.
#[derive(Clone, Debug)]
pub struct WriteOutcome<T> {
    pub value: T,
    pub revision_before: u64,
    pub revision: u64,
    pub cache: CacheUpdate,
}

/// Apply the revision rule for one write reply: if the cached snapshot of `library_id`
/// is at `revision_before`, write `files` into it and record `revision` (which may equal
/// `revision_before` for a play, so no full sync is needed); otherwise ask
/// for a full sync. Never downloads anything itself.
pub fn apply_server_write(
    cache: &RemoteCache,
    library_id: &str,
    files: &[AudioFile],
    revision_before: u64,
    revision: u64,
) -> CacheUpdate {
    let _guard = match cache_publication_lease(cache, library_id) {
        Ok(guard) => guard,
        Err(error) => {
            return CacheUpdate::Resync {
                reason: format!("cache publication is unavailable: {error}"),
            };
        }
    };
    let Some(mut state) = cache.state(library_id) else {
        return CacheUpdate::Resync {
            reason: "no cached snapshot for this library".into(),
        };
    };
    // `revision == revision_before` is a write that changed no snapshot content the
    // server versions (plays, no-op patches): apply the records, keep the revision.
    if state.revision != revision_before || revision < revision_before {
        return CacheUpdate::Resync {
            reason: format!(
                "cached revision {} does not match revision_before {revision_before} (server now {revision})",
                state.revision
            ),
        };
    }
    if let Err(error) = SqliteProvider::apply_snapshot_records(&cache.database_path(&state), files)
    {
        return CacheUpdate::Resync {
            reason: format!("could not update the cached snapshot: {error}"),
        };
    }
    state.revision = revision;
    match cache.save_state(&state) {
        Ok(()) => CacheUpdate::Applied { revision },
        Err(error) => CacheUpdate::Resync {
            reason: format!("could not record the new revision: {error}"),
        },
    }
}

/// Metadata writes for one server library, applied to its local cache.
#[derive(Clone, Debug)]
pub struct RemoteWriter {
    client: RemoteClient,
    cache: RemoteCache,
    library_id: String,
}

impl RemoteWriter {
    /// Bind every mutation, including management through [`Self::client`], to the
    /// identity of the cached snapshot. A fresh client is required after a library switch.
    pub fn new(client: RemoteClient, cache: RemoteCache, library_id: impl Into<String>) -> Self {
        let library_id = library_id.into();
        Self {
            client: client.bind_library(&library_id),
            cache,
            library_id,
        }
    }

    pub fn library_id(&self) -> &str {
        &self.library_id
    }

    /// The API client (capabilities, root checks and server scans, which are not
    /// `DataProvider` operations).
    pub fn client(&self) -> &RemoteClient {
        &self.client
    }

    /// The snapshot after a server removal (§6.8). The reply carries no records to apply,
    /// so a removal that moved the revision always means a full sync; removed ids may come
    /// back with new ids after a rescan, so callers also drop ids they cached (selection,
    /// waveform/preview keys). An unchanged revision removed nothing: the snapshot stays
    /// valid when it is at that revision.
    fn removed(&self, report: RemoveReport) -> WriteOutcome<RemoveReport> {
        let (before, after) = (report.revision_before, report.revision);
        let cached = self
            .cache
            .state(&self.library_id)
            .map(|state| state.revision);
        let cache = if !report.changed() && cached == Some(after) {
            CacheUpdate::Applied { revision: after }
        } else {
            let reason = format!(
                "{} records removed on the server (revision {before} -> {after})",
                report.removed
            );
            tracing::info!(%reason, "server removal needs a full snapshot sync");
            CacheUpdate::Resync { reason }
        };
        WriteOutcome {
            value: report,
            revision_before: before,
            revision: after,
            cache,
        }
    }

    /// `POST /audio/remove`: database records only (files untouched), then a sync.
    pub fn remove_audio(&self, ids: &[i64]) -> Result<WriteOutcome<RemoveReport>> {
        let _write = WRITE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(self.removed(self.client.remove_audio(ids)?))
    }

    /// `DELETE /folders/{id}`: database records only (files untouched), then a sync.
    pub fn remove_folder(&self, id: i64) -> Result<WriteOutcome<RemoveReport>> {
        let _write = WRITE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(self.removed(self.client.remove_folder(id)?))
    }

    fn finish<T>(&self, value: T, files: &[AudioFile], before: u64, after: u64) -> WriteOutcome<T> {
        let cache = apply_server_write(&self.cache, &self.library_id, files, before, after);
        if let CacheUpdate::Resync { reason } = &cache {
            tracing::info!(%reason, before, after, "server write needs a full snapshot sync");
        }
        WriteOutcome {
            value,
            revision_before: before,
            revision: after,
            cache,
        }
    }

    fn enrich_album_projection(&self, files: &mut [AudioFile]) {
        if !files.iter().any(|file| file.album_id.is_none()) {
            return;
        }
        let Some(state) = self.cache.state(&self.library_id) else {
            return;
        };
        if !state
            .format
            .as_ref()
            .is_some_and(|format| format.local.schema_version >= 8)
        {
            return;
        }
        let Ok(snapshot) = SqliteProvider::open_read_only(&self.cache.database_path(&state)) else {
            return;
        };
        for file in files {
            if file.album_id.is_none()
                && let Ok(local) = snapshot.get_audio(file.id)
            {
                file.album_id = local.album_id;
                file.album_entity = local.album_entity;
            }
        }
    }

    /// `PATCH /audio/{id}/metadata`, then the revision rule.
    pub fn update_metadata(
        &self,
        id: i64,
        patch: &MetadataPatch,
    ) -> Result<WriteOutcome<AudioFile>> {
        let _write = WRITE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let FileWrite {
            file,
            revision_before,
            revision,
        } = self.client.patch_metadata(id, patch)?;
        let mut outcome = self.finish(
            file.clone(),
            std::slice::from_ref(&file),
            revision_before,
            revision,
        );
        if matches!(outcome.cache, CacheUpdate::Applied { .. }) {
            self.enrich_album_projection(std::slice::from_mut(&mut outcome.value));
        }
        Ok(outcome)
    }

    /// `POST /audio/metadata/batch`, then the revision rule. The value carries the
    /// server's report and the written records (request order, de-duplicated).
    pub fn update_metadata_batch(
        &self,
        request: &BatchMetadataRequest,
    ) -> Result<WriteOutcome<(BatchMetadataReport, Vec<AudioFile>)>> {
        let _write = WRITE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let BatchWrite {
            report,
            mut files,
            revision_before,
            revision,
        } = self.client.batch_metadata(request)?;
        let outcome = self.finish((), &files, revision_before, revision);
        if matches!(outcome.cache, CacheUpdate::Applied { .. }) {
            self.enrich_album_projection(&mut files);
        }
        Ok(WriteOutcome {
            value: (report, files),
            revision_before: outcome.revision_before,
            revision: outcome.revision,
            cache: outcome.cache,
        })
    }
}

/// Cache effect of a write, reported to the GUI through [`RemoteProvider`]'s listener.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteEvent {
    /// The snapshot now reflects the write and is at `revision`.
    Applied { revision: u64 },
    /// Run a full snapshot sync (revision mismatch, apply failure or uncertain outcome).
    Resync { reason: String },
    /// No write was sent/applied: the snapshot's IDs belong to a replaced library.
    /// Disable writes and management until a new snapshot/provider has been opened.
    LibraryChanged { reason: String },
}

/// Listener for [`WriteEvent`]s (called on the writing thread).
pub type WriteListener = Arc<dyn Fn(WriteEvent) + Send + Sync>;

/// `DataProvider` for server mode: reads come from the read-only snapshot, metadata
/// writes go to the server ([`RemoteWriter`]) and are applied to the snapshot
/// by the revision rule. Record and folder removal go to the server's management
/// endpoints (database records only; servers without them answer "unsupported") and are
/// followed by a full sync when they changed the revision. Restore, maintenance, relink and import are not offered by
/// the server API and are refused. The desktop removes server records from the
/// 服务器音效库 page and its removal menus, only for the active server with `can_manage`.
pub struct RemoteProvider {
    snapshot: Arc<SqliteProvider>,
    writer: RemoteWriter,
    chinese: bool,
    listener: Option<WriteListener>,
    /// List thumbnail Waveforms from `POST /waveforms` (never from the snapshot's
    /// `waveform_cache` rows, §9.3); `None` hides the list's Waveform column.
    list_waveforms: Option<Arc<crate::list_waveforms::RemoteListWaveforms>>,
}

impl RemoteProvider {
    pub fn new(
        snapshot: Arc<SqliteProvider>,
        writer: RemoteWriter,
        chinese: bool,
        listener: Option<WriteListener>,
    ) -> Self {
        Self {
            snapshot,
            writer,
            chinese,
            listener,
            list_waveforms: None,
        }
    }

    /// Answer list Waveform lookups through `service` (its library must be the
    /// snapshot's).
    pub fn with_list_waveforms(
        mut self,
        service: Arc<crate::list_waveforms::RemoteListWaveforms>,
    ) -> Self {
        self.list_waveforms = Some(service);
        self
    }

    pub fn writer(&self) -> &RemoteWriter {
        &self.writer
    }

    fn emit(&self, event: WriteEvent) {
        if let Some(listener) = &self.listener {
            listener(event);
        }
    }

    fn settle<T>(&self, result: Result<WriteOutcome<T>>) -> audiolib_core::Result<T> {
        match result {
            Ok(outcome) => {
                self.emit(match outcome.cache {
                    CacheUpdate::Applied { revision } => WriteEvent::Applied { revision },
                    CacheUpdate::Resync { reason } => WriteEvent::Resync { reason },
                });
                Ok(outcome.value)
            }
            Err(error) => {
                if matches!(error, RemoteError::LibraryChanged(_)) {
                    self.emit(WriteEvent::LibraryChanged {
                        reason: error.localized(self.chinese),
                    });
                } else if error.write_may_have_committed() {
                    self.emit(WriteEvent::Resync {
                        reason: format!("write outcome uncertain: {error}"),
                    });
                }
                Err(core_error(error, self.chinese))
            }
        }
    }

    /// Like `settle` for management writes, with the management error wording
    /// (`403 MANAGE_FORBIDDEN`, `409 BUSY`, `503 ROOT_OFFLINE`, older server).
    fn settle_manage<T>(&self, result: Result<WriteOutcome<T>>) -> audiolib_core::Result<T> {
        match result {
            Err(error) if !error.write_may_have_committed() => {
                if matches!(error, RemoteError::LibraryChanged(_)) {
                    self.emit(WriteEvent::LibraryChanged {
                        reason: error.localized(self.chinese),
                    });
                }
                let text = manage_error_text(&error, self.chinese);
                Err(match error {
                    RemoteError::NotFound(_) => Error::NotFound(text),
                    RemoteError::Busy { .. }
                    | RemoteError::Starting { .. }
                    | RemoteError::RootOffline { .. } => Error::Busy(text),
                    RemoteError::Cancelled => Error::Cancelled,
                    RemoteError::Validation(_)
                    | RemoteError::JobValidation { .. }
                    | RemoteError::Forbidden(_)
                    | RemoteError::Unsupported(_) => Error::Validation(text),
                    _ => Error::Database(text),
                })
            }
            other => self.settle(other),
        }
    }

    fn refused(&self) -> Error {
        Error::Validation(if self.chinese {
            "服务器模式不支持此写操作（服务器 API 只提供标注编辑和播放计数）".into()
        } else {
            "Not available in server mode (the server API only offers annotation edits and play counts)".into()
        })
    }
}

/// A classified remote failure as a core error carrying the localized text.
pub fn core_error(error: RemoteError, chinese: bool) -> Error {
    let text = error.localized(chinese);
    match error {
        RemoteError::NotFound(_) => Error::NotFound(text),
        RemoteError::Busy { .. } | RemoteError::Starting { .. } => Error::Busy(text),
        RemoteError::Validation(_)
        | RemoteError::JobValidation { .. }
        | RemoteError::PayloadTooLarge(_) => Error::Validation(text),
        RemoteError::Cancelled => Error::Cancelled,
        _ => Error::Database(text),
    }
}

impl DataProvider for RemoteProvider {
    fn load_search_workspace(
        &self,
        target: &audiolib_core::SearchWorkspaceTarget,
    ) -> audiolib_core::Result<audiolib_core::SearchWorkspace> {
        self.snapshot.load_search_workspace(target)
    }

    fn save_search_workspace(
        &self,
        target: &audiolib_core::SearchWorkspaceTarget,
        workspace: &audiolib_core::SearchWorkspace,
    ) -> audiolib_core::Result<()> {
        self.snapshot.save_search_workspace(target, workspace)
    }

    fn search(&self, request: &SearchRequest) -> audiolib_core::Result<SearchPage> {
        self.snapshot.search(request)
    }
    fn search_folders(&self, request: &SearchRequest) -> audiolib_core::Result<Vec<Folder>> {
        self.snapshot.search_folders(request)
    }
    fn get_audio(&self, id: i64) -> audiolib_core::Result<AudioFile> {
        self.snapshot.get_audio(id)
    }
    fn album_facets(&self, request: &SearchRequest) -> audiolib_core::Result<Vec<AlbumFacet>> {
        self.snapshot.album_facets(request)
    }
    fn album_facets_cancellable(
        &self,
        request: &SearchRequest,
        cancel: Arc<AtomicBool>,
    ) -> audiolib_core::Result<Vec<AlbumFacet>> {
        self.snapshot.album_facets_cancellable(request, cancel)
    }
    fn get_album(&self, id: i64) -> audiolib_core::Result<Album> {
        self.snapshot.get_album(id)
    }
    fn album_members(
        &self,
        id: i64,
        limit: usize,
        cancel: Arc<AtomicBool>,
    ) -> audiolib_core::Result<Vec<AudioFile>> {
        self.snapshot.album_members(id, limit, cancel)
    }
    fn album_members_in_folder(
        &self,
        id: i64,
        folder_id: i64,
        limit: usize,
        cancel: Arc<AtomicBool>,
    ) -> audiolib_core::Result<Vec<AudioFile>> {
        self.snapshot
            .album_members_in_folder(id, folder_id, limit, cancel)
    }
    fn album_members_in_unassigned_directory(
        &self,
        id: i64,
        directory: &str,
        limit: usize,
        cancel: Arc<AtomicBool>,
    ) -> audiolib_core::Result<Vec<AudioFile>> {
        self.snapshot
            .album_members_in_unassigned_directory(id, directory, limit, cancel)
    }
    fn folders(&self, parent_id: Option<i64>) -> audiolib_core::Result<Vec<Folder>> {
        self.snapshot.folders(parent_id)
    }
    fn update_metadata(&self, id: i64, patch: &MetadataPatch) -> audiolib_core::Result<AudioFile> {
        let mut file = self.settle(self.writer.update_metadata(id, patch))?;
        // Legacy servers omit shared entities. Return their authoritative annotations
        // together with the local projection, rather than erasing the UI's binding.
        if file.album_id.is_none()
            && let Ok(local) = self.snapshot.get_audio(id)
        {
            file.album_id = local.album_id;
            file.album_entity = local.album_entity;
        }
        Ok(file)
    }
    fn stats(&self) -> audiolib_core::Result<LibraryStats> {
        self.snapshot.stats()
    }
    fn library_info(&self) -> audiolib_core::Result<LibraryInfo> {
        self.snapshot.library_info()
    }
    fn check_library(&self) -> audiolib_core::Result<IntegrityReport> {
        self.snapshot.check_library()
    }
    fn backup_library(&self, destination: &Path) -> audiolib_core::Result<BackupReport> {
        self.snapshot.backup_library(destination)
    }
    fn restore_library(&self, _source: &Path) -> audiolib_core::Result<RestoreReport> {
        Err(self.refused())
    }
    fn maintain_library(
        &self,
        _action: MaintenanceAction,
    ) -> audiolib_core::Result<MaintenanceReport> {
        Err(self.refused())
    }
    fn update_metadata_batch(
        &self,
        request: &BatchMetadataRequest,
    ) -> audiolib_core::Result<BatchMetadataReport> {
        self.settle(self.writer.update_metadata_batch(request))
            .map(|(report, _)| report)
    }
    fn find_audio_by_path(&self, path: &Path) -> audiolib_core::Result<Option<AudioFile>> {
        self.snapshot.find_audio_by_path(path)
    }
    fn find_folder_by_path(&self, path: &Path) -> audiolib_core::Result<Option<Folder>> {
        self.snapshot.find_folder_by_path(path)
    }
    fn relink_audio(&self, _id: i64, _path: &Path) -> audiolib_core::Result<AudioFile> {
        Err(self.refused())
    }
    fn remove_audio(&self, ids: &[i64]) -> audiolib_core::Result<u64> {
        self.settle_manage(self.writer.remove_audio(ids))
            .map(|report| report.removed)
    }
    fn remove_folder(&self, id: i64) -> audiolib_core::Result<u64> {
        self.settle_manage(self.writer.remove_folder(id))
            .map(|report| report.removed)
    }
    fn scan_history(&self, limit: usize, offset: usize) -> audiolib_core::Result<Vec<ScanSession>> {
        self.snapshot.scan_history(limit, offset)
    }
    fn export_library(
        &self,
        destination: &Path,
        format: LibraryExportFormat,
    ) -> audiolib_core::Result<TransferReport> {
        self.snapshot.export_library(destination, format)
    }
    fn import_annotations(&self, _source: &Path) -> audiolib_core::Result<TransferReport> {
        Err(self.refused())
    }
    // List Waveforms never come from the snapshot (its `waveform_cache` rows may be left
    // over from local mode, §9.3): only from the server, and only when it offers them.
    fn list_waveform_source(&self) -> ListWaveformSource {
        self.list_waveforms
            .as_ref()
            .map_or(ListWaveformSource::Unavailable, |service| service.source())
    }
    fn list_waveforms(
        &self,
        ids: &[i64],
        cancel: Arc<AtomicBool>,
    ) -> audiolib_core::Result<ListWaveformBatch> {
        let Some(service) = &self.list_waveforms else {
            return Err(Error::Validation(
                "This server offers no list Waveforms".into(),
            ));
        };
        service.lookup(ids, &cancel).map_err(|error| {
            if matches!(error, RemoteError::LibraryChanged(_)) {
                self.emit(WriteEvent::LibraryChanged {
                    reason: error.localized(self.chinese),
                });
            }
            core_error(error, self.chinese)
        })
    }
    // List Waveform cache management (§9.5) goes to the server through the writer's
    // client (identity-bound mutations; the snapshot is not involved).
    fn waveform_cache_capabilities(&self) -> WaveformCacheResult<WaveformCacheCapabilities> {
        let capabilities = self.waveform_cache(|client| client.capabilities())?;
        Ok(WaveformCacheCapabilities {
            jobs: capabilities
                .list_waveform_jobs
                .clone()
                .filter(|jobs| jobs.version >= 1)
                .filter(|_| capabilities.feature_enabled("list_waveform_jobs")),
            can_manage: capabilities.may_manage(),
        })
    }
    fn waveform_cache_status(&self) -> WaveformCacheResult<WaveformCacheStatus> {
        self.waveform_cache(|client| client.list_waveform_status())
    }
    fn waveform_cache_settings(&self) -> WaveformCacheResult<WaveformCacheSettings> {
        self.waveform_cache(|client| client.server_settings())
    }
    fn update_waveform_cache_settings(
        &self,
        patch: &WaveformCacheSettingsPatch,
    ) -> WaveformCacheResult<WaveformCacheSettings> {
        self.waveform_cache(|client| client.update_list_waveform_settings(patch))
    }
    fn start_waveform_cache_job(
        &self,
        mode: WaveformJobMode,
        confirm_rebuild: bool,
    ) -> WaveformCacheResult<WaveformCacheStatus> {
        self.waveform_cache(|client| client.start_list_waveform_job(mode, confirm_rebuild))
    }
    fn control_waveform_cache_job(
        &self,
        control: WaveformJobControl,
    ) -> WaveformCacheResult<WaveformCacheStatus> {
        self.waveform_cache(|client| client.control_list_waveform_job(control))
    }
    fn control_waveform_pregeneration(
        &self,
        control: WaveformJobControl,
    ) -> WaveformCacheResult<()> {
        self.waveform_cache(|client| client.control_list_waveform_pregeneration(control))
    }
}

impl RemoteProvider {
    fn waveform_cache<T>(
        &self,
        call: impl FnOnce(&RemoteClient) -> Result<T>,
    ) -> WaveformCacheResult<T> {
        call(self.writer.client())
            .map_err(|error| crate::waveform_cache::waveform_cache_error(&error, self.chinese))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::CacheState;
    use crate::test_server::{TestResponse, TestServer};
    use audiolib_core::{SearchRequest, TagEditMode};
    use std::sync::Mutex as StdMutex;

    const LIBRARY: &str = "lib-w";

    #[test]
    fn indexed_folder_lookup_is_available_offline_and_preserves_source_case() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cached_library(directory.path(), 41);
        let state = cache.state(LIBRARY).unwrap();
        let database = cache.database_path(&state);
        let conn = rusqlite::Connection::open(&database).unwrap();
        audiolib_db::register_library_functions(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO folders(id,folder_path,folder_name) VALUES
             (1,'/offline/Sounds/Wind','Wind'),(2,'/offline/Sounds/wind','wind');",
        )
        .unwrap();
        drop(conn);
        let before = std::fs::read(&database).unwrap();
        let writer = RemoteWriter::new(
            RemoteClient::new("http://127.0.0.1:1", "").unwrap(),
            cache.clone(),
            LIBRARY,
        );
        let provider = RemoteProvider::new(Arc::new(snapshot(&cache)), writer, false, None);
        for (path, id) in [("/offline/Sounds/Wind", 1), ("/offline/Sounds/wind", 2)] {
            assert_eq!(
                provider
                    .find_folder_by_path(Path::new(path))
                    .unwrap()
                    .unwrap()
                    .id,
                id
            );
        }
        assert!(
            provider
                .find_folder_by_path(Path::new("/offline/sounds/Wind"))
                .unwrap()
                .is_none()
        );
        assert_eq!(std::fs::read(&database).unwrap(), before);
        assert_eq!(cache.state(LIBRARY).unwrap(), state);
    }

    #[test]
    fn personal_search_workspace_is_available_offline_without_snapshot_changes() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cached_library(directory.path(), 41);
        let state = cache.state(LIBRARY).unwrap();
        let before = std::fs::read(cache.database_path(&state)).unwrap();
        let writer = RemoteWriter::new(
            RemoteClient::new("http://127.0.0.1:1", "").unwrap(),
            cache.clone(),
            LIBRARY,
        );
        let provider = RemoteProvider::new(Arc::new(snapshot(&cache)), writer, false, None);
        let target = audiolib_core::SearchWorkspaceTarget::new(
            directory.path(),
            format!("server-{LIBRARY}"),
        );
        let mut workspace = provider.load_search_workspace(&target).unwrap();
        workspace.session.tasks.push(audiolib_core::SearchTask {
            id: "task".into(),
            title: "Wind".into(),
            definition: audiolib_core::SearchTaskDefinition {
                query: "wind".into(),
                ..Default::default()
            },
            ..Default::default()
        });
        provider.save_search_workspace(&target, &workspace).unwrap();
        assert_eq!(provider.load_search_workspace(&target).unwrap(), workspace);
        assert_eq!(cache.state(LIBRARY).unwrap(), state);
        assert_eq!(std::fs::read(cache.database_path(&state)).unwrap(), before);
    }

    #[test]
    fn server_identity_conflicts_emit_invalidation_and_leave_the_cache_unchanged() {
        for code in ["LIBRARY_CHANGED", "LIBRARY_ID_REQUIRED"] {
            let directory = tempfile::tempdir().unwrap();
            let cache = cached_library(directory.path(), 41);
            let cached = cache.state(LIBRARY).unwrap();
            let before = std::fs::read(cache.database_path(&cached)).unwrap();
            let server = TestServer::start(move |request| {
                if let Some(response) = crate::test_server::marker_handshake(&request.path) {
                    return response;
                }
                if request.path == "/api/v1/library" {
                    return TestResponse::json(
                        200,
                        &format!(
                            r#"{{"library_id":"{LIBRARY}","identity_bound_writes":true,"schema_version":5,"revision":41}}"#,
                        ),
                    );
                }
                assert_eq!(request.header(crate::LIBRARY_ID_HEADER), Some(LIBRARY));
                TestResponse::json(
                    409,
                    &format!(r#"{{"error_code":"{code}","message":"Library identity mismatch"}}"#,),
                )
            });
            let events = Arc::new(StdMutex::new(Vec::new()));
            let listener: WriteListener = {
                let events = events.clone();
                Arc::new(move |event| events.lock().unwrap().push(event))
            };
            let writer = RemoteWriter::new(
                RemoteClient::new(&server.url, "").unwrap(),
                cache.clone(),
                LIBRARY,
            );
            let provider =
                RemoteProvider::new(Arc::new(snapshot(&cache)), writer, true, Some(listener));
            let error = provider.remove_audio(&[3]).unwrap_err();
            assert!(error.to_string().contains("身份未确认"));
            assert!(matches!(
                events.lock().unwrap().as_slice(),
                [WriteEvent::LibraryChanged { .. }]
            ));
            assert_eq!(cache.active().unwrap(), cached);
            assert_eq!(std::fs::read(cache.database_path(&cached)).unwrap(), before);
        }
    }

    /// A cached snapshot with records 3 (`door.wav`) and 7 (`雷声 近.wav`) at `revision`.
    fn cached_library(directory: &Path, revision: u64) -> RemoteCache {
        let cache = RemoteCache::new(directory);
        let library_dir = cache.library_dir(LIBRARY);
        std::fs::create_dir_all(&library_dir).unwrap();
        let database = library_dir.join("library-r1-1.db");
        drop(SqliteProvider::open(&database).unwrap());
        let conn = rusqlite::Connection::open(&database).unwrap();
        audiolib_db::register_library_functions(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO audio_files(id,file_path,path_key,filename)
             VALUES(3,'/v/door.wav','/v/door.wav','door.wav'),
                   (7,'/v/thunder.wav','/v/thunder.wav','雷声 近.wav');",
        )
        .unwrap();
        conn.query_row("PRAGMA journal_mode=DELETE", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap();
        drop(conn);
        cache
            .save_state(&CacheState {
                library_id: LIBRARY.into(),
                revision,
                schema_version: audiolib_db::SCHEMA_VERSION,
                database_file: "library-r1-1.db".into(),
                ..CacheState::default()
            })
            .unwrap();
        cache
    }

    /// `legacy_rating` is still emitted: servers that have not dropped ratings send it and
    /// the client must tolerate (ignore) it.
    fn file_json(id: i64, tags: &str, legacy_rating: u8, color: &str, plays: u64) -> String {
        let color = if color.is_empty() {
            "null".to_string()
        } else {
            format!("\"{color}\"")
        };
        format!(
            r#"{{"id":{id},"file_path":"/v/{id}.wav","filename":"{id}.wav","folder_id":null,"file_size":52,"sample_rate":48000,"bit_depth":16,"channels":1,"duration":0.1,"tags":"{tags}","rating":{legacy_rating},"color":{color},"notes":"","favorite":false,"bext_description":"","play_count":{plays},"attributes":{{"modified_at":"2026-10-24 00:00:00.123"}},"missing":false}}"#
        )
    }

    struct Seen {
        method: String,
        path: String,
        body: String,
    }

    fn server(
        seen: Arc<StdMutex<Vec<Seen>>>,
        reply: impl Fn(&str, &str) -> TestResponse + Send + Sync + 'static,
    ) -> TestServer {
        server_with_capability_probe(seen, false, reply)
    }

    fn server_with_capability_probe(
        seen: Arc<StdMutex<Vec<Seen>>>,
        probe_capabilities: bool,
        reply: impl Fn(&str, &str) -> TestResponse + Send + Sync + 'static,
    ) -> TestServer {
        TestServer::start(move |request| {
            if probe_capabilities && request.path == "/api/v1/health" {
                return crate::test_server::marker_health();
            }
            if (request.path != "/api/v1/capabilities" || !probe_capabilities)
                && let Some(response) = crate::test_server::marker_handshake(&request.path)
            {
                return response;
            }
            if request.method == "GET" && request.path == "/api/v1/library" {
                return TestResponse::json(
                    200,
                    &format!(
                        r#"{{"library_id":"{LIBRARY}","identity_bound_writes":true,"schema_version":5,"revision":1}}"#,
                    ),
                );
            }
            if request.method != "GET" {
                assert_eq!(request.header(crate::LIBRARY_ID_HEADER), Some(LIBRARY));
            }
            seen.lock().unwrap().push(Seen {
                method: request.method.clone(),
                path: request.path.clone(),
                body: String::from_utf8_lossy(&request.body).into_owned(),
            });
            reply(&request.method, &request.path)
        })
    }

    #[test]
    fn provider_routes_waveform_cache_management_through_data_provider() {
        use audiolib_core::waveform_cache::{WaveformCacheErrorKind, WaveformRunState};
        let directory = tempfile::tempdir().unwrap();
        let cache = cached_library(directory.path(), 1);
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let status = r#"{"state":"paused","total":4,"cached":1,"pending":3,"run_state":"paused","current_rate":0,"eta_secs":null,"job":{"id":"wfj-2","mode":"missing","started_at":1,"done":1,"failed":0,"total":4,"state":"paused"}}"#;
        let server = server_with_capability_probe(seen.clone(), true, move |method, path| {
            match (method, path) {
                ("GET", "/api/v1/capabilities") => crate::test_server::marker_capabilities(serde_json::from_str(r#"{"manage_mode":"lan","can_manage":false,"list_waveform_jobs":{"version":1,"modes":["missing","rebuild","failed"],"rate_setting":true}}"#).unwrap()),
                ("GET", "/api/v1/waveforms/status") => TestResponse::json(200, status),
                ("GET", "/api/v1/settings") => TestResponse::json(
                    200,
                    r#"{"pregenerate_list_waveforms":true,"list_waveform_rate":10,"list_waveform_rate_source":"default","can_change":false}"#,
                ),
                ("POST", "/api/v1/waveforms/jobs") => TestResponse::json(
                    409,
                    r#"{"error_code":"BUSY","message":"A list waveform job is already in progress","job_id":"wfj-2"}"#,
                ),
                ("POST", "/api/v1/waveforms/jobs/current/resume") => TestResponse::json(
                    403,
                    r#"{"error_code":"MANAGE_FORBIDDEN","message":"Management is limited to the LAN"}"#,
                ),
                (method, path) => panic!("unexpected {method} {path}"),
            }
        });
        let writer = RemoteWriter::new(
            RemoteClient::new(&server.url, "").unwrap(),
            cache.clone(),
            LIBRARY,
        );
        let provider: Arc<dyn DataProvider> = Arc::new(RemoteProvider::new(
            Arc::new(snapshot(&cache)),
            writer,
            true,
            None,
        ));
        let capabilities = provider.waveform_cache_capabilities().unwrap();
        assert!(!capabilities.can_manage);
        assert!(capabilities.jobs.unwrap().rate_setting);
        let current = provider.waveform_cache_status().unwrap();
        assert_eq!(current.effective_run_state(), WaveformRunState::Paused);
        assert_eq!(current.eta_secs, None);
        assert_eq!(
            provider
                .waveform_cache_settings()
                .unwrap()
                .list_waveform_rate,
            Some(10)
        );
        let busy = provider
            .start_waveform_cache_job(WaveformJobMode::Missing, false)
            .unwrap_err();
        assert_eq!(busy.kind, WaveformCacheErrorKind::Busy);
        assert!(busy.message.contains("409"), "{}", busy.message);
        let forbidden = provider
            .control_waveform_cache_job(WaveformJobControl::Resume)
            .unwrap_err();
        assert_eq!(forbidden.kind, WaveformCacheErrorKind::Forbidden);
        // A local provider offers none of it.
        assert_eq!(
            snapshot(&cache).waveform_cache_status().unwrap_err().kind,
            WaveformCacheErrorKind::Unsupported
        );
    }

    fn snapshot(cache: &RemoteCache) -> SqliteProvider {
        let state = cache.state(LIBRARY).unwrap();
        SqliteProvider::open_read_only(&cache.database_path(&state)).unwrap()
    }

    #[test]
    fn list_waveforms_come_from_the_server_never_from_snapshot_rows() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cached_library(directory.path(), 41);
        // A list row left over from local mode in the snapshot.
        let state = cache.state(LIBRARY).unwrap();
        rusqlite::Connection::open(cache.database_path(&state))
            .unwrap()
            .execute(
                "INSERT INTO waveform_cache(audio_file_id,zoom_level,samples_per_pixel,
                 min_values,max_values) VALUES(3,'list128-v4-l1',1,x'9cff',x'6400')",
                [],
            )
            .unwrap();
        let reader = Arc::new(snapshot(&cache));
        let posts = Arc::new(StdMutex::new(0));
        let server = {
            let posts = posts.clone();
            TestServer::start(move |request| match request.path.as_str() {
                "/api/v1/capabilities" => {
                    crate::test_server::marker_capabilities(serde_json::json!({
                        "list_waveforms": {
                            "version": 1,
                            "buckets": [128],
                            "max_ids": 200,
                            "encoding": "i16le-minmax-base64",
                        }
                    }))
                }
                "/api/v1/waveforms" => {
                    *posts.lock().unwrap() += 1;
                    TestResponse::json(
                        200,
                        r#"{"library_id":"lib-w","buckets":128,"retry_after":2,"items":[{"id":3,"status":"pending"}]}"#,
                    )
                }
                other => panic!("unexpected {other}"),
            })
        };
        let writer = || {
            RemoteWriter::new(
                RemoteClient::new(&server.url, "").unwrap(),
                cache.clone(),
                LIBRARY,
            )
        };
        let plain = RemoteProvider::new(reader.clone(), writer(), false, None);
        assert_eq!(
            plain.list_waveform_source(),
            ListWaveformSource::Unavailable
        );
        assert!(
            plain
                .list_waveforms(&[3], Arc::new(AtomicBool::new(false)))
                .is_err()
        );
        let service = Arc::new(crate::list_waveforms::RemoteListWaveforms::new(
            RemoteClient::new(&server.url, "").unwrap(),
            LIBRARY,
            crate::list_waveforms::ListWaveformStore::new(
                directory.path().join("list"),
                crate::list_waveforms::DEFAULT_LIST_WAVEFORM_CACHE_BYTES,
            ),
        ));
        let provider =
            RemoteProvider::new(reader, writer(), false, None).with_list_waveforms(service);
        assert_eq!(provider.list_waveform_source(), ListWaveformSource::Server);
        let batch = provider
            .list_waveforms(&[3], Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert_eq!(
            batch.items[0].state,
            audiolib_core::ListWaveformState::Pending { previous: None },
            "the snapshot's waveform_cache row is not used"
        );
        assert_eq!(batch.retry_after, Some(Duration::from_secs(2)));
        assert_eq!(*posts.lock().unwrap(), 1);
        assert!(provider.list_waveform_backlog(0, 10).unwrap().is_empty());
        assert!(
            provider
                .compute_list_waveform(3, Arc::new(AtomicBool::new(false)))
                .is_err(),
            "the client never decodes in server mode"
        );
    }

    #[test]
    fn patch_applies_on_matching_revision_and_asks_for_sync_otherwise() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cached_library(directory.path(), 41);
        let reader = snapshot(&cache);
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let revision = Arc::new(StdMutex::new((41u64, 42u64)));
        let server = {
            let revision = revision.clone();
            server(seen.clone(), move |_, _| {
                let (before, after) = *revision.lock().unwrap();
                TestResponse::json(
                    200,
                    &format!(
                        r#"{{"file":{},"revision_before":{before},"revision":{after}}}"#,
                        file_json(3, "loud door", 4, "#FF8800", 0)
                    ),
                )
            })
        };
        let writer = RemoteWriter::new(
            RemoteClient::new(&server.url, "").unwrap(),
            cache.clone(),
            LIBRARY,
        );
        let patch = MetadataPatch {
            tags: Some("loud door".into()),
            color: Some("#FF8800".into()),
            ..MetadataPatch::default()
        };
        let outcome = writer.update_metadata(3, &patch).unwrap();
        assert_eq!(outcome.cache, CacheUpdate::Applied { revision: 42 });
        assert_eq!((outcome.revision_before, outcome.revision), (41, 42));
        assert_eq!(cache.state(LIBRARY).unwrap().revision, 42);
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen[0].method, "PATCH");
            assert_eq!(seen[0].path, "/api/v1/audio/3/metadata");
            let body: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
            assert_eq!(
                body,
                serde_json::json!({"tags":"loud door","color":"#FF8800"}),
                "omitted fields must not be sent as null"
            );
        }
        // The provider that already had the snapshot open sees the applied values,
        // including the FTS columns.
        let record = reader.get_audio(3).unwrap();
        assert_eq!(record.tags.as_str(), "loud door");
        assert_eq!(record.color.as_deref(), Some("#FF8800"));
        let hits = reader
            .search(&SearchRequest {
                query: "loud".into(),
                limit: 10,
                ..SearchRequest::default()
            })
            .unwrap();
        assert_eq!(hits.results.iter().map(|f| f.id).collect::<Vec<_>>(), [3]);

        // Another client wrote in between: the reply's revision_before is not ours.
        *revision.lock().unwrap() = (50, 51);
        let outcome = writer
            .update_metadata(
                3,
                &MetadataPatch {
                    favorite: Some(true),
                    ..MetadataPatch::default()
                },
            )
            .unwrap();
        assert!(matches!(outcome.cache, CacheUpdate::Resync { .. }));
        assert_eq!(
            cache.state(LIBRARY).unwrap().revision,
            42,
            "left for the sync"
        );

        // Clearing the color is sent as "" and applied as no color.
        *revision.lock().unwrap() = (42, 43);
        let cleared = apply_server_write(
            &cache,
            LIBRARY,
            &[serde_json::from_str(&file_json(3, "loud door", 4, "", 0)).unwrap()],
            42,
            43,
        );
        assert_eq!(cleared, CacheUpdate::Applied { revision: 43 });
        assert_eq!(reader.get_audio(3).unwrap().color, None);
        let body = crate::client::patch_json(&MetadataPatch {
            color: Some(String::new()),
            ..MetadataPatch::default()
        });
        assert_eq!(body, serde_json::json!({"color":""}));

        // An id the snapshot does not have means it diverged: sync, revision unchanged.
        let unknown = apply_server_write(
            &cache,
            LIBRARY,
            &[serde_json::from_str(&file_json(99, "", 0, "", 0)).unwrap()],
            43,
            44,
        );
        assert!(matches!(unknown, CacheUpdate::Resync { .. }));
        assert_eq!(cache.state(LIBRARY).unwrap().revision, 43);
        assert!(matches!(
            apply_server_write(&cache, "other-library", &[], 43, 44),
            CacheUpdate::Resync { .. }
        ));
    }

    #[test]
    fn batch_keeps_server_order_without_duplicates_and_applies_all_records() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cached_library(directory.path(), 7);
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let server = server(seen.clone(), |_, _| {
            TestResponse::json(
                200,
                &format!(
                    r#"{{"report":{{"updated":2}},"files":[{},{}],"revision_before":7,"revision":8}}"#,
                    file_json(7, "rain wood", 0, "", 2),
                    file_json(3, "door wood", 0, "", 0)
                ),
            )
        });
        let writer = RemoteWriter::new(
            RemoteClient::new(&server.url, "").unwrap(),
            cache.clone(),
            LIBRARY,
        );
        let outcome = writer
            .update_metadata_batch(&BatchMetadataRequest {
                ids: vec![7, 3, 7],
                patch: MetadataPatch {
                    tags: Some("wood".into()),
                    ..MetadataPatch::default()
                },
                tag_mode: TagEditMode::Append,
            })
            .unwrap();
        let (report, files) = outcome.value;
        assert_eq!(report.updated, 2);
        assert_eq!(files.iter().map(|f| f.id).collect::<Vec<_>>(), [7, 3]);
        assert_eq!(outcome.cache, CacheUpdate::Applied { revision: 8 });
        let seen = seen.lock().unwrap();
        assert_eq!(
            (seen[0].method.as_str(), seen[0].path.as_str()),
            ("POST", "/api/v1/audio/metadata/batch")
        );
        let body: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"ids":[7,3,7],"patch":{"tags":"wood"},"tag_mode":"append"})
        );
        let reader = snapshot(&cache);
        assert_eq!(reader.get_audio(7).unwrap().tags, "rain wood");
        // Old JSON may contain retired fields; the owned DTO ignores them and
        // serializing a new response must not reintroduce those fields.
        assert!(
            serde_json::to_value(&files[0])
                .unwrap()
                .get("play_count")
                .is_none()
        );
        assert!(
            serde_json::to_value(reader.get_audio(7).unwrap())
                .unwrap()
                .get("play_count")
                .is_none()
        );
        assert_eq!(reader.get_audio(3).unwrap().tags, "door wood");
    }

    #[test]
    fn write_errors_are_classified_and_uncertain_ones_request_a_sync() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cached_library(directory.path(), 1);
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let server = server(seen.clone(), |_, path| match path {
            "/api/v1/audio/1/metadata" => TestResponse::json(
                400,
                r#"{"error_code":"VALIDATION","message":"color must be empty or #RRGGBB"}"#,
            ),
            "/api/v1/audio/2/metadata" => {
                TestResponse::json(404, r#"{"error_code":"NOT_FOUND","message":"no audio 2"}"#)
            }
            "/api/v1/audio/3/metadata" => {
                TestResponse::json(409, r#"{"error_code":"BUSY","message":"scan running"}"#)
            }
            "/api/v1/audio/4/metadata" => TestResponse::json(
                413,
                r#"{"error_code":"PAYLOAD_TOO_LARGE","message":"body over 8 MiB"}"#,
            ),
            "/api/v1/audio/5/metadata" => TestResponse::json(
                408,
                r#"{"error_code":"TIMEOUT","message":"request took longer than 30 s"}"#,
            ),
            "/api/v1/audio/6/metadata" => TestResponse::json(200, r#"{"file":{"id":6"#),
            _ => TestResponse::json(500, r#"{"error_code":"INTERNAL","message":"?"}"#),
        });
        let writer = RemoteWriter::new(
            RemoteClient::new(&server.url, "").unwrap(),
            cache.clone(),
            LIBRARY,
        );
        let patch = MetadataPatch {
            notes: Some("x".into()),
            ..MetadataPatch::default()
        };
        let error = |id| writer.update_metadata(id, &patch).unwrap_err();
        let validation = error(1);
        assert!(matches!(validation, RemoteError::Validation(ref m) if m.contains("color")));
        assert!(matches!(error(2), RemoteError::NotFound(_)));
        assert!(matches!(error(3), RemoteError::Busy { .. }));
        let too_large = error(4);
        assert!(matches!(too_large, RemoteError::PayloadTooLarge(_)));
        assert!(too_large.localized(true).contains("413"));
        let timeout = error(5);
        assert!(matches!(timeout, RemoteError::Timeout(_)));
        assert!(timeout.write_may_have_committed());
        assert!(error(6).write_may_have_committed(), "unreadable reply");
        assert!(!validation.write_may_have_committed());
        assert!(!too_large.write_may_have_committed());
        // Local validation fails before any request.
        let before = seen.lock().unwrap().len();
        let invalid = writer
            .update_metadata(
                1,
                &MetadataPatch {
                    color: Some("red".into()),
                    ..MetadataPatch::default()
                },
            )
            .unwrap_err();
        assert!(matches!(invalid, RemoteError::Validation(_)));
        let empty = writer
            .update_metadata_batch(&BatchMetadataRequest {
                ids: vec![1],
                patch: MetadataPatch::default(),
                tag_mode: TagEditMode::Replace,
            })
            .unwrap_err();
        assert!(matches!(empty, RemoteError::Validation(_)));
        assert_eq!(seen.lock().unwrap().len(), before, "rejected locally");

        // Through the DataProvider: a 408 asks the GUI for a sync, a 404 does not.
        let events = Arc::new(StdMutex::new(Vec::new()));
        let listener: WriteListener = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let provider = RemoteProvider::new(
            Arc::new(snapshot(&cache)),
            writer.clone(),
            true,
            Some(listener),
        );
        let missing = provider.update_metadata(2, &patch).unwrap_err();
        assert!(matches!(missing, Error::NotFound(ref m) if m.contains("找不到")));
        assert!(events.lock().unwrap().is_empty());
        assert!(provider.update_metadata(5, &patch).is_err());
        assert!(matches!(
            events.lock().unwrap().as_slice(),
            [WriteEvent::Resync { reason }] if reason.contains("uncertain")
        ));
        assert!(provider.remove_audio(&[1]).is_err());
        assert!(provider.relink_audio(1, Path::new("x.wav")).is_err());
        assert_eq!(cache.state(LIBRARY).unwrap().revision, 1);
    }

    #[test]
    fn removals_go_to_the_server_and_resync_when_the_revision_moved() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cached_library(directory.path(), 10);
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let server = server(seen.clone(), |method, path| match (method, path) {
            ("POST", "/api/v1/audio/remove") => TestResponse::json(
                200,
                r#"{"removed":2,"not_found":[],"revision_before":10,"revision":11}"#,
            ),
            ("DELETE", "/api/v1/folders/8") => TestResponse::json(
                200,
                r#"{"removed":0,"folders_removed":0,"revision_before":10,"revision":10}"#,
            ),
            ("DELETE", "/api/v1/folders/5") => {
                TestResponse::json(409, r#"{"error_code":"BUSY","message":"scan running"}"#)
            }
            ("DELETE", "/api/v1/folders/6") => TestResponse::json(
                403,
                r#"{"error_code":"MANAGE_FORBIDDEN","message":"Library management (scans, removing records or folders) is disabled on this server (manage_mode = off)"}"#,
            ),
            _ => TestResponse::json(
                404,
                r#"{"error_code":"NOT_FOUND","message":"No such endpoint"}"#,
            ),
        });
        let events = Arc::new(StdMutex::new(Vec::new()));
        let listener: WriteListener = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let writer = RemoteWriter::new(
            RemoteClient::new(&server.url, "").unwrap(),
            cache.clone(),
            LIBRARY,
        );
        assert!(writer.client().api_url().ends_with("/api/v1"));
        let provider =
            RemoteProvider::new(Arc::new(snapshot(&cache)), writer, true, Some(listener));
        assert_eq!(provider.remove_audio(&[3, 7]).unwrap(), 2);
        assert!(matches!(
            events.lock().unwrap().as_slice(),
            [WriteEvent::Resync { reason }] if reason.contains("removed")
        ));
        // The snapshot itself is untouched until the sync replaces it.
        assert_eq!(provider.get_audio(3).unwrap().id, 3);
        assert_eq!(cache.state(LIBRARY).unwrap().revision, 10);
        let busy = provider.remove_folder(5).unwrap_err();
        assert!(
            matches!(busy, Error::Busy(ref m) if m.contains("409")),
            "{busy:?}"
        );
        let forbidden = provider.remove_folder(6).unwrap_err();
        assert!(
            matches!(forbidden, Error::Validation(ref m) if m.contains("已关闭远程管理")),
            "{forbidden:?}"
        );
        let old = provider.remove_folder(7).unwrap_err();
        assert!(
            matches!(old, Error::Validation(ref m) if m.contains("不支持")),
            "{old:?}"
        );
        assert_eq!(events.lock().unwrap().len(), 1, "refusals change nothing");
        // Nothing removed and the snapshot at that revision: no sync.
        assert_eq!(provider.remove_folder(8).unwrap(), 0);
        assert!(matches!(
            events.lock().unwrap().last(),
            Some(WriteEvent::Applied { revision: 10 })
        ));
        assert_eq!(seen.lock().unwrap()[1].method, "DELETE");
    }

    #[test]
    fn concurrent_metadata_writers_publish_each_reply_before_sending_the_next_write() {
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let cache = cached_library(directory.path(), 43);
        let calls = Arc::new(AtomicUsize::new(0));
        let second_cached_revision = Arc::new(AtomicU64::new(0));
        let (first_seen_tx, first_seen_rx) = std::sync::mpsc::sync_channel(1);
        let (first_release_tx, first_release_rx) = std::sync::mpsc::sync_channel(1);
        let first_release_rx = StdMutex::new(first_release_rx);
        let server = {
            let cache = cache.clone();
            let calls = calls.clone();
            let second_cached_revision = second_cached_revision.clone();
            server(Arc::new(StdMutex::new(Vec::new())), move |method, path| {
                assert_eq!((method, path), ("PATCH", "/api/v1/audio/3/metadata"));
                let call = calls.fetch_add(1, Ordering::AcqRel);
                let (before, after) = if call < 2 {
                    (43 + call as u64, 44 + call as u64)
                } else {
                    // Another client changed the server: serialization must not hide it.
                    (48, 49)
                };
                if call == 1 {
                    second_cached_revision
                        .store(cache.state(LIBRARY).unwrap().revision, Ordering::Release);
                }
                let reply = TestResponse::json(
                    200,
                    &format!(
                        r#"{{"file":{},"revision_before":{before},"revision":{after}}}"#,
                        file_json(3, &format!("edit{call}"), 0, "", 0)
                    ),
                );
                if call == 0 {
                    first_seen_tx.send(()).unwrap();
                    first_release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                    // The previous implementation sends its second request during this
                    // delay, applies revision 45 against cache 43 and requests a resync.
                    reply.delayed(Duration::from_millis(400))
                } else {
                    reply
                }
            })
        };
        let writer = || {
            RemoteWriter::new(
                RemoteClient::new(&server.url, "").unwrap(),
                cache.clone(),
                LIBRARY,
            )
        };
        let first = writer();
        let first = std::thread::spawn(move || {
            first.update_metadata(3, &MetadataPatch::default()).unwrap()
        });
        first_seen_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Snapshot reads remain local even while Metadata owns the network-write gate.
        let (read_tx, read_rx) = std::sync::mpsc::sync_channel(1);
        let reading_cache = cache.clone();
        let read = std::thread::spawn(move || {
            read_tx.send(snapshot(&reading_cache).get_audio(3)).unwrap()
        });
        assert_eq!(
            read_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap()
                .id,
            3
        );
        read.join().unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 1);
        // Independent writer instances must share ordering through cache publication.
        let second = writer();
        let second = std::thread::spawn(move || {
            second
                .update_metadata(3, &MetadataPatch::default())
                .unwrap()
        });
        first_release_tx.send(()).unwrap();
        assert_eq!(
            first.join().unwrap().cache,
            CacheUpdate::Applied { revision: 44 }
        );
        assert_eq!(
            second.join().unwrap().cache,
            CacheUpdate::Applied { revision: 45 }
        );
        assert_eq!(second_cached_revision.load(Ordering::Acquire), 44);
        assert_eq!(cache.state(LIBRARY).unwrap().revision, 45);
        assert_eq!(snapshot(&cache).get_audio(3).unwrap().tags, "edit1");
        assert!(matches!(
            writer()
                .update_metadata(3, &MetadataPatch::default())
                .unwrap()
                .cache,
            CacheUpdate::Resync { .. }
        ));
        assert_eq!(cache.state(LIBRARY).unwrap().revision, 45);
    }
}
