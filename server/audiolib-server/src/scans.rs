// server/audiolib-server/src/scans.rs
//! Remote scans: `POST /scans`, `GET /scans/{id}`, `GET /scans/current` and
//! `POST /scans/{id}/pause|resume|cancel` (doc/api/README.md §6.6).
//!
//! A scan runs the workspace scanner (`audiolib_db::ScanService`, the same code as local
//! mode) on its own OS thread, so it never blocks the async runtime and has no time
//! limit. At most one scan runs per server (the scanner's library lease additionally
//! excludes scans and administration by other processes). The scanner's own rules apply
//! unchanged: an unreachable root fails the scan without touching records, read errors
//! or an empty root disable missing-file reconciliation, and slowness only slows it down.
//!
//! While a scan runs the library revision is held (`RevisionTracker::begin_hold`), so
//! clients are not told to resync after every batch; when it ends the revision is bumped
//! once if the scan changed anything (or another process wrote meanwhile), otherwise not.
use crate::{
    ApiError, AppState,
    auth::SettingsLease,
    roots::{
        RootMonitor, RootState, describe_io_error, is_transient_network_error, probe_root,
        without_verbatim,
    },
    state::INLINE_RECHECK,
};
use audiolib_core::{DataProvider, Error, LibraryStats, ScanProgress, ScanSession, ScanState};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

/// Largest accepted `workers` value (the scanner's own bound).
pub const MAX_SCAN_WORKERS: u32 = 16;
/// Finished sessions kept in memory with their full status (older ones are answered
/// from the library's scan history).
const RECENT_SESSIONS: usize = 32;
/// Longest accepted `subpath`.
const MAX_SUBPATH_BYTES: usize = 4096;
/// Scan sessions searched in the library history when a session is not in memory.
const HISTORY_LOOKUP_LIMIT: usize = 10_000;
/// Metadata transaction size, as in local mode.
const BATCH_SIZE: usize = 128;

/// `mode` of `POST /scans`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    /// Probe only new or changed files (size/mtime); never marks files missing.
    #[default]
    Incremental,
    /// Probe every file; a clean full scan marks files that were not found as missing.
    Full,
}

impl ScanMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Incremental => "incremental",
            Self::Full => "full",
        }
    }
}

/// Body of `POST /scans`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScanRequest {
    /// Configured root name (`GET /roots`).
    pub root: String,
    /// Directory below the root, `/`-separated and relative; empty or omitted = the root.
    #[serde(default)]
    pub subpath: Option<String>,
    /// `incremental` (default) or `full`.
    #[serde(default)]
    pub mode: ScanMode,
    /// Metadata worker threads 1..=16; `null`, omitted or 0 = automatic.
    #[serde(default)]
    pub workers: Option<u32>,
}

/// Reply of `GET /scans/current`: always 200, `scan` is `null` when no scan runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CurrentScanResponse {
    /// The running or paused scan, in the same shape as `GET /scans/{id}`.
    pub scan: Option<ScanResponse>,
}

/// Reply of every other `/scans` endpoint: the core `ScanProgress` (same field names as in
/// local mode) plus what the server knows about the session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanResponse {
    /// The scanner's progress; `session_id` is the library's scan session id.
    #[serde(flatten)]
    pub progress: ScanProgress,
    /// Configured root name, `null` when the session's path is under no configured root.
    pub root: Option<String>,
    /// Scanned directory relative to the root (`""` = the root itself), `null` like `root`.
    pub subpath: Option<String>,
    /// `incremental` or `full`.
    pub mode: String,
    /// Whether this server process is running the session (poll while `true`).
    pub active: bool,
    /// A pause was requested and not yet resumed (`paused` turns `true` once the workers
    /// actually stopped at a safe point).
    pub pause_requested: bool,
    /// A cancel was requested (the scan stops after its current file).
    pub cancel_requested: bool,
    /// Library revision after the scan finished; `null` while it runs (and for sessions
    /// from the history). Compare it with the client's cached revision.
    pub revision: Option<u64>,
}

struct JobStatus {
    progress: ScanProgress,
    active: bool,
    revision: Option<u64>,
    shutdown: bool,
}

struct ScanJob {
    root: String,
    subpath: String,
    mode: ScanMode,
    control: ScanCancellation,
    status: Mutex<JobStatus>,
}

impl ScanJob {
    fn status(&self) -> MutexGuard<'_, JobStatus> {
        self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn session_id(&self) -> i64 {
        self.status().progress.session_id
    }

    fn response(&self) -> ScanResponse {
        let status = self.status();
        ScanResponse {
            progress: status.progress.clone(),
            root: Some(self.root.clone()),
            subpath: Some(self.subpath.clone()),
            mode: self.mode.as_str().into(),
            active: status.active,
            pause_requested: status.active && self.control.is_pause_requested(),
            cancel_requested: status.active && self.control.is_cancelled(),
            revision: status.revision,
        }
    }
}

#[derive(Default)]
struct Jobs {
    active: Option<Arc<ScanJob>>,
    recent: VecDeque<Arc<ScanJob>>,
    shutting_down: bool,
}

/// Scan sessions of this server process: the running one and recently finished ones.
#[derive(Default)]
pub struct ScanManager {
    jobs: Mutex<Jobs>,
    finished: Condvar,
}

/// `POST /scans/{id}/...` actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanAction {
    /// Stop the workers at the next safe point; the session stays open.
    Pause,
    /// Continue a paused scan.
    Resume,
    /// Stop the scan; work already committed stays, no missing-file reconciliation.
    Cancel,
}

impl ScanAction {
    fn verb(self) -> &'static str {
        match self {
            Self::Pause => "paused",
            Self::Resume => "resumed",
            Self::Cancel => "cancelled",
        }
    }
}

/// 409 `SCAN_STATE`: the session cannot take the requested action in its current state.
fn state_conflict(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "SCAN_STATE", message)
}

fn state_name(state: ScanState) -> &'static str {
    match state {
        ScanState::Running => "running",
        ScanState::Completed => "completed",
        ScanState::Cancelled => "cancelled",
        ScanState::Failed => "failed",
    }
}

impl ScanManager {
    fn jobs(&self) -> MutexGuard<'_, Jobs> {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The running scan, if any (`session_id` is 0 while it is still resolving its
    /// directory, before the library session exists).
    pub fn current(&self) -> Option<ScanResponse> {
        self.jobs().active.as_ref().map(|job| job.response())
    }

    /// Whether the running (or paused) scan belongs to root `name`.
    pub fn is_scanning(&self, name: &str) -> bool {
        self.jobs()
            .active
            .as_ref()
            .is_some_and(|job| job.root == name)
    }

    fn find(&self, id: i64) -> Option<Arc<ScanJob>> {
        if id <= 0 {
            return None;
        }
        let jobs = self.jobs();
        jobs.active
            .iter()
            .chain(jobs.recent.iter())
            .find(|job| job.session_id() == id)
            .cloned()
    }

    fn busy(jobs: &Jobs) -> Result<(), ApiError> {
        if jobs.shutting_down {
            return Err(ApiError::unavailable("The server is shutting down"));
        }
        if let Some(job) = &jobs.active {
            let id = job.session_id();
            return Err(Error::Busy(if id > 0 {
                format!("A scan is already running for this library (session {id})")
            } else {
                "A scan is already starting for this library".into()
            })
            .into());
        }
        Ok(())
    }

    fn finish(&self, job: &Arc<ScanJob>, keep: bool) {
        let mut jobs = self.jobs();
        if jobs
            .active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, job))
        {
            jobs.active = None;
        }
        if keep {
            jobs.recent.push_front(job.clone());
            jobs.recent.truncate(RECENT_SESSIONS);
        }
        drop(jobs);
        self.finished.notify_all();
    }

    /// Blocking (at most `wait`). Refuse new scans, cancel the running one and wait
    /// for it to record its session as cancelled. Returns whether no scan is left.
    pub fn shutdown(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        let mut jobs = self.jobs();
        jobs.shutting_down = true;
        if let Some(job) = &jobs.active {
            let mut status = job.status();
            if !status.shutdown {
                status.shutdown = true;
                drop(status);
                job.control.cancel();
                tracing::info!(
                    session_id = job.session_id(),
                    "Cancelling the running scan for shutdown"
                );
            }
        }
        while jobs.active.is_some() {
            let now = Instant::now();
            if now >= deadline {
                if wait.is_zero() {
                    return false;
                }
                tracing::warn!(
                    "The scan did not finish before shutdown; its session stays `running` until the next scan marks it failed"
                );
                return false;
            }
            jobs = self
                .finished
                .wait_timeout(jobs, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }
}

/// Owns a startup reservation until the spawned scan thread takes responsibility.
/// A failed root recheck, spawn error or startup panic releases the slot.
struct ScanReservation<'a> {
    manager: &'a ScanManager,
    job: &'a Arc<ScanJob>,
    handed_off: bool,
}

impl Drop for ScanReservation<'_> {
    fn drop(&mut self) {
        if !self.handed_off {
            self.manager.finish(self.job, false);
        }
    }
}

/// Validate a `subpath` lexically: relative, `/`-separated, no `.`/`..` components.
/// Empty components (doubled or trailing `/`) are dropped. Returns the normalized form.
pub fn normalize_subpath(raw: &str) -> Result<String, ApiError> {
    if raw.len() > MAX_SUBPATH_BYTES || raw.contains('\0') {
        return Err(ApiError::validation(
            "subpath must be at most 4096 bytes without NUL characters",
        ));
    }
    if raw.starts_with('/') || raw.contains('\\') || Path::new(raw).is_absolute() {
        return Err(ApiError::validation(
            "subpath must be relative to the root and use '/' as separator",
        ));
    }
    let parts: Vec<&str> = raw.split('/').filter(|part| !part.is_empty()).collect();
    if parts.iter().any(|part| *part == "." || *part == "..") {
        return Err(ApiError::validation(
            "subpath must not contain '.' or '..' components",
        ));
    }
    Ok(parts.join("/"))
}

/// Everything client-visible that a scan can change besides its own counters.
fn library_shape(state: &AppState) -> Option<LibraryStats> {
    state.provider().stats().ok()
}

fn same_shape(before: &Option<LibraryStats>, after: &Option<LibraryStats>) -> bool {
    match (before, after) {
        (Some(before), Some(after)) => {
            before.audio_files == after.audio_files
                && before.folders == after.folders
                && before.missing_files == after.missing_files
                && before.total_bytes == after.total_bytes
                && before.total_duration.to_bits() == after.total_duration.to_bits()
        }
        _ => false,
    }
}

impl AppState {
    /// Blocking (root recheck at most about [`INLINE_RECHECK`]). `POST /scans`: validate
    /// the request, reserve the library's scan slot and start the scan thread. The
    /// receiver yields the first progress (with the new `session_id`) or the error that
    /// prevented the session from starting.
    pub fn start_scan(
        &self,
        request: ScanRequest,
    ) -> Result<oneshot::Receiver<Result<ScanResponse, ApiError>>, ApiError> {
        let subpath = normalize_subpath(request.subpath.as_deref().unwrap_or(""))?;
        let workers = request.workers.unwrap_or(0);
        if workers > MAX_SCAN_WORKERS {
            return Err(ApiError::validation(format!(
                "workers must be 1..={MAX_SCAN_WORKERS} (or null for automatic)"
            )));
        }
        // Match root edits' lock order: local root-edit lock, OS settings lease,
        // then the scanner's per-database OS lease. Reload before taking the fixed
        // monitor snapshot, including edits committed by another server instance.
        let edit = self.roots_edit_lock();
        let settings_lease = self.security().settings_lease()?;
        self.refresh_roots_locked(&settings_lease)?;
        let monitor = self.root_monitor();
        let index = monitor
            .roots()
            .iter()
            .position(|root| root.name == request.root)
            .ok_or_else(|| {
                ApiError::not_found(format!("No library root named {:?}", request.root))
            })?;
        let manager = self.scans();
        ScanManager::busy(&manager.jobs())?;
        let job = Arc::new(ScanJob {
            root: request.root,
            subpath,
            mode: request.mode,
            control: ScanCancellation::default(),
            status: Mutex::new(JobStatus {
                progress: ScanProgress::default(),
                active: true,
                revision: None,
                shutdown: false,
            }),
        });
        {
            let mut jobs = manager.jobs();
            ScanManager::busy(&jobs)?;
            jobs.active = Some(job.clone());
        }
        let mut reservation = ScanReservation {
            manager,
            job: &job,
            handed_off: false,
        };
        drop(edit);
        // Reserve before checking a sleeping root so concurrent removal cannot
        // invalidate the requested root while this bounded recheck waits.
        let mut status = monitor.status(index);
        if status.state == RootState::Offline {
            status = monitor.check_now(index, INLINE_RECHECK);
        }
        if status.state == RootState::Offline {
            return Err(ApiError::root_offline(format!(
                "Library root {:?} is offline: {}",
                job.root,
                status.message.unwrap_or_default()
            )));
        }
        let (sender, receiver) = oneshot::channel();
        let state = self.clone();
        let thread_job = job.clone();
        let spawned = std::thread::Builder::new()
            .name("audiolib-scan".into())
            .spawn(move || {
                run_scan(
                    &state,
                    &thread_job,
                    monitor,
                    index,
                    workers as usize,
                    sender,
                    Some(settings_lease),
                )
            });
        if let Err(error) = spawned {
            return Err(ApiError::internal(format!(
                "Cannot start the scan thread: {error}"
            )));
        }
        reservation.handed_off = true;
        Ok(receiver)
    }

    /// Blocking. `GET /scans/{id}`: a session of this process, else from the history.
    pub fn scan_status(&self, id: i64) -> Result<ScanResponse, ApiError> {
        if let Some(job) = self.scans().find(id) {
            return Ok(job.response());
        }
        self.history_session(id)?
            .map(|session| self.history_response(session))
            .ok_or_else(|| ApiError::not_found(format!("No scan session {id}")))
    }

    /// Blocking. `POST /scans/{id}/pause|resume|cancel`. Repeating the action that is
    /// already in effect answers 200 (idempotent); impossible transitions are 409
    /// `SCAN_STATE`.
    pub fn control_scan(&self, id: i64, action: ScanAction) -> Result<ScanResponse, ApiError> {
        let Some(job) = self.scans().find(id) else {
            return match self.history_session(id)? {
                Some(session) => Err(state_conflict(format!(
                    "Scan session {id} is not running in this server ({}); it cannot be {}",
                    state_name(session.state),
                    action.verb()
                ))),
                None => Err(ApiError::not_found(format!("No scan session {id}"))),
            };
        };
        let (active, state, finalizing) = {
            let status = job.status();
            (
                status.active,
                status.progress.state,
                status.progress.finalizing,
            )
        };
        if !active {
            return Err(state_conflict(format!(
                "Scan session {id} has already finished ({}); it cannot be {}",
                state_name(state),
                action.verb()
            )));
        }
        let control = &job.control;
        let finishing = finalizing || control.is_finishing();
        match action {
            ScanAction::Cancel => {
                if !control.is_cancelled() {
                    if finishing {
                        return Err(state_conflict(format!(
                            "Scan session {id} is finalizing its results; it can no longer be cancelled"
                        )));
                    }
                    control.cancel();
                    tracing::info!(session_id = id, "Scan cancel requested");
                }
            }
            ScanAction::Pause | ScanAction::Resume if control.is_cancelled() => {
                return Err(state_conflict(format!(
                    "Scan session {id} is being cancelled; it cannot be {}",
                    action.verb()
                )));
            }
            ScanAction::Pause | ScanAction::Resume if finishing => {
                return Err(state_conflict(format!(
                    "Scan session {id} is finalizing its results; it cannot be {}",
                    action.verb()
                )));
            }
            ScanAction::Pause => {
                if !control.is_pause_requested() {
                    if !control.pause() {
                        return Err(state_conflict(format!(
                            "Scan session {id} can no longer be paused"
                        )));
                    }
                    tracing::info!(session_id = id, "Scan pause requested");
                }
            }
            ScanAction::Resume => {
                if control.is_pause_requested() {
                    control.resume();
                    tracing::info!(session_id = id, "Scan resumed");
                }
            }
        }
        Ok(job.response())
    }

    fn history_session(&self, id: i64) -> Result<Option<ScanSession>, ApiError> {
        if id <= 0 {
            return Ok(None);
        }
        Ok(self
            .provider()
            .scan_history(HISTORY_LOOKUP_LIMIT, 0)?
            .into_iter()
            .find(|session| session.id == id))
    }

    /// A history row as a `/scans` reply (counters only; `discovered` = `scanned`).
    fn history_response(&self, session: ScanSession) -> ScanResponse {
        let path = PathBuf::from(&session.scan_path);
        let monitor = self.root_monitor();
        let located = monitor.root_index_for(&path).and_then(|index| {
            let root = &monitor.roots()[index];
            let relative = monitor
                .status(index)
                .canonical
                .and_then(|canonical| {
                    // History paths are stored without the Windows verbatim prefix.
                    path.strip_prefix(&canonical)
                        .or_else(|_| path.strip_prefix(without_verbatim(&canonical)))
                        .ok()
                        .map(Path::to_path_buf)
                })
                .or_else(|| {
                    path.strip_prefix(&root.server_path)
                        .ok()
                        .map(Path::to_path_buf)
                })?;
            Some((
                root.name.clone(),
                relative.to_string_lossy().replace('\\', "/"),
            ))
        });
        let (root, subpath) = located.unzip();
        ScanResponse {
            progress: ScanProgress {
                session_id: session.id,
                state: session.state,
                paused: false,
                finalizing: false,
                discovered: session.scanned,
                processed: session.scanned,
                added: session.added,
                updated: session.updated,
                unchanged: session.unchanged,
                // Older persisted ScanSession rows have no table-level counters.
                vendor_metadata: Default::default(),
                removed: session.removed,
                errors: session.errors,
                error_details: session.error_details,
                errors_truncated: session.errors_truncated,
                current_path: session.scan_path,
                message: if session.state == ScanState::Running {
                    "Not running in this server process (interrupted, or run by another process)"
                        .into()
                } else {
                    String::new()
                },
            },
            root,
            subpath,
            mode: session.scan_type,
            active: false,
            pause_requested: false,
            cancel_requested: false,
            revision: None,
        }
    }
}

/// Why the scan thread could not create a session (sent to the waiting `POST /scans`).
fn resolve_failure(root_name: &str, subpath: &str, error: &std::io::Error) -> ApiError {
    if is_transient_network_error(error) {
        ApiError::root_offline(format!(
            "Library root {root_name:?} is offline: Root is not answering reliably: {error}"
        ))
    } else if matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    ) {
        ApiError::not_found(format!(
            "No directory {subpath:?} in library root {root_name:?}"
        ))
    } else {
        Error::Io(std::io::Error::new(error.kind(), error.to_string())).into()
    }
}

/// Resolve the root and the scanned directory on the scan thread (a sleeping disk may
/// take long): `(canonical root, canonical target)`.
fn resolve_directories(
    monitor: &RootMonitor,
    index: usize,
    job: &ScanJob,
) -> Result<(PathBuf, PathBuf), ApiError> {
    let root_config = &monitor.roots()[index];
    let probe = probe_root(&root_config.server_path);
    monitor.record(index, probe.clone());
    let root = probe.map_err(|reason| {
        ApiError::root_offline(format!("Library root {:?} is offline: {reason}", job.root))
    })?;
    if job.subpath.is_empty() {
        return Ok((root.clone(), root));
    }
    let target = fs::canonicalize(root.join(&job.subpath))
        .map_err(|error| resolve_failure(&job.root, &job.subpath, &error))?;
    if !target.starts_with(&root) {
        return Err(ApiError::validation(format!(
            "subpath {:?} resolves outside library root {:?} (symbolic link?)",
            job.subpath, job.root
        )));
    }
    match fs::metadata(&target) {
        Ok(metadata) if metadata.is_dir() => Ok((root, target)),
        Ok(_) => Err(ApiError::not_found(format!(
            "{:?} in library root {:?} is not a directory",
            job.subpath, job.root
        ))),
        Err(error) => Err(resolve_failure(&job.root, &job.subpath, &error)),
    }
}

/// Map a scanner error that happened before the session existed.
fn start_failure(error: Error, root_name: &str) -> ApiError {
    match error {
        Error::Io(io) if is_transient_network_error(&io) => ApiError::root_offline(format!(
            "Library root {root_name:?} is offline: Root is not answering reliably: {io}"
        )),
        Error::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            ApiError::root_offline(format!(
                "Library root {root_name:?} is offline: {}",
                describe_io_error(&io)
            ))
        }
        other => other.into(),
    }
}

/// Body of the scan thread.
fn run_scan(
    state: &AppState,
    job: &Arc<ScanJob>,
    monitor: Arc<RootMonitor>,
    index: usize,
    workers: usize,
    sender: oneshot::Sender<Result<ScanResponse, ApiError>>,
    settings_lease: Option<SettingsLease>,
) {
    let mut sender = Some(sender);
    let mut cleanup = ScanCleanup {
        state,
        job,
        holding_revision: false,
        settings_lease,
    };
    // Includes root resolution and completion bookkeeping, not only ScanService.
    // The cleanup guard also runs if panic recovery itself unwinds.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_scan_inner(
            state,
            job,
            &monitor,
            index,
            workers,
            &mut sender,
            &mut cleanup,
        )
    }));
    let result = outcome.unwrap_or_else(|_| {
        let mut status = job.status();
        status.progress.state = ScanState::Failed;
        status.progress.message = "The scan thread panicked".into();
        drop(status);
        Err(ApiError::internal("The scan thread panicked"))
    });
    // An unsuccessful start becomes observable only after all its reservations
    // are released. Otherwise an immediate retry can hit the old settings lease
    // or scan slot even though the preceding request already reported failure.
    drop(cleanup);
    if let Some(sender) = sender.take() {
        let error = result
            .err()
            .unwrap_or_else(|| ApiError::internal("The scan ended without reporting a session"));
        let _ = sender.send(Err(error));
    }
}

/// Releases settings, scan-slot and revision reservations on every exit, including
/// a panic before session creation.
struct ScanCleanup<'a> {
    state: &'a AppState,
    job: &'a Arc<ScanJob>,
    holding_revision: bool,
    // Keep cross-instance root edits excluded until ScanService's first callback
    // confirms its database scan lease. Errors and panics release it as well.
    settings_lease: Option<SettingsLease>,
}

impl Drop for ScanCleanup<'_> {
    fn drop(&mut self) {
        drop(self.settings_lease.take());
        if self.holding_revision {
            let _ = self.state.tracker().end_hold(true);
        }
        {
            let mut status = self.job.status();
            status.active = false;
            status.progress.paused = false;
        }
        self.state
            .scans()
            .finish(self.job, self.job.session_id() > 0);
    }
}

fn run_scan_inner(
    state: &AppState,
    job: &Arc<ScanJob>,
    monitor: &RootMonitor,
    index: usize,
    workers: usize,
    sender: &mut Option<oneshot::Sender<Result<ScanResponse, ApiError>>>,
    cleanup: &mut ScanCleanup<'_>,
) -> Result<(), ApiError> {
    let (root, target) = resolve_directories(monitor, index, job)?;
    let started = Instant::now();
    let provider = state.provider().clone();
    state.tracker().begin_hold()?;
    cleanup.holding_revision = true;
    let shape_before = library_shape(state);
    // New and changed records are queued for list waveform pre-generation afterwards.
    let scan_started_at = state.list_waveforms().database_now();
    let foreign_before = provider.foreign_data_version().ok();
    let options = ScanOptions {
        recursive: true,
        incremental: job.mode == ScanMode::Incremental,
        batch_size: BATCH_SIZE,
        workers,
        allowed_roots: vec![root.clone()],
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // A subdirectory scan hangs its subtree below the configured root (and keeps an
        // existing parent); the scanner itself treats the scanned directory as its root.
        if target != root {
            provider.ensure_scan_folder(&target, &root)?;
        }
        ScanService::new(provider.clone()).scan(&target, &options, &job.control, |progress| {
            // ScanService calls back only after acquiring its OS scan lease and
            // creating the session. Hand off exclusion without a root-edit gap.
            drop(cleanup.settings_lease.take());
            let first = {
                let mut status = job.status();
                let first = status.progress.session_id == 0;
                status.progress = progress;
                first
            };
            if first && let Some(sender) = sender.take() {
                let response = job.response();
                tracing::info!(
                    session_id = response.progress.session_id,
                    root = %job.root,
                    subpath = %job.subpath,
                    mode = job.mode.as_str(),
                    "Scan started"
                );
                let _ = sender.send(Ok(response));
            }
        })
    }));
    let result =
        outcome.unwrap_or_else(|_| Err(Error::Database("The scan thread panicked".into())));
    // A failed start has no first callback to release this reservation. The
    // scanner has returned, so release it before revision/Waveform bookkeeping
    // can wait on other workers. The error still stays private until ScanCleanup
    // releases the job slot below.
    drop(cleanup.settings_lease.take());
    let session_started = job.session_id() > 0;
    // Conservative: count the scan as a change unless it provably changed nothing
    // client-visible and no other connection committed meanwhile.
    let unchanged = result.as_ref().is_ok_and(|progress| {
        progress.added == 0
            && progress.updated == 0
            && progress.removed == 0
            && progress.vendor_metadata.updated == 0
    }) || (!session_started && result.is_err());
    let unchanged = unchanged
        && same_shape(&shape_before, &library_shape(state))
        && foreign_before.is_some()
        && provider.foreign_data_version().ok() == foreign_before;
    cleanup.holding_revision = false;
    let revision = match state.tracker().end_hold(!unchanged) {
        Ok(info) => Some(info.revision),
        Err(error) => {
            tracing::warn!(%error, "Cannot record the revision after a scan");
            None
        }
    };
    {
        let mut status = job.status();
        match &result {
            Ok(progress) => status.progress = progress.clone(),
            Err(error) if session_started => {
                // The scanner already reported the failed final progress; keep it.
                if status.progress.state == ScanState::Running {
                    status.progress.state = ScanState::Failed;
                    status.progress.message = error.to_string();
                }
            }
            Err(_) => {}
        }
        if status.shutdown && status.progress.state == ScanState::Cancelled {
            status.progress.message = "Cancelled because the server is shutting down".into();
        }
        status.active = false;
        status.revision = revision;
        status.progress.paused = false;
        let progress = &status.progress;
        tracing::info!(
            session_id = progress.session_id,
            state = state_name(progress.state),
            discovered = progress.discovered,
            added = progress.added,
            updated = progress.updated,
            unchanged = progress.unchanged,
            removed = progress.removed,
            errors = progress.errors,
            vendor_tables = progress.vendor_metadata.tables,
            vendor_matched = progress.vendor_metadata.matched,
            vendor_updated = progress.vendor_metadata.updated,
            vendor_ambiguous = progress.vendor_metadata.ambiguous,
            vendor_unmatched = progress.vendor_metadata.unmatched,
            revision = revision.unwrap_or_default(),
            bumped = !unchanged,
            secs = started.elapsed().as_secs_f64(),
            "Scan finished"
        );
    }
    // Includes the part committed before a cancellation.
    state.list_waveforms().scan_finished(
        state.security().pregenerate_list_waveforms(),
        job.session_id(),
        scan_started_at.as_deref(),
    );
    if sender.is_some() {
        let error = match result {
            Err(error) => start_failure(error, &job.root),
            Ok(_) => ApiError::internal("The scan ended without reporting a session"),
        };
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_errors_wait_until_reservations_are_released() {
        for (subpath, index, expected, database_busy) in [
            ("absent", 0, "NOT_FOUND", false),
            ("notes.txt", 0, "NOT_FOUND", false),
            ("", 0, "BUSY", true),
            ("", usize::MAX, "INTERNAL", false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let database = dir.path().join("library.db");
            drop(audiolib_db::SqliteProvider::open(&database).unwrap());
            fs::write(dir.path().join("notes.txt"), b"not a directory").unwrap();
            let config = crate::ServerConfig::from_toml_str(
                &format!(
                    "library_db='library.db'\n[[roots]]\nname='SFX'\nserver_path='{}'\n",
                    dir.path().display()
                ),
                dir.path(),
            )
            .unwrap();
            let state = AppState::open(&config, None).unwrap();
            let second = AppState::open(&config, None).unwrap();
            let database_lease = database_busy.then(|| {
                let file = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(dir.path().join("library.db.scan.lock"))
                    .unwrap();
                file.try_lock().unwrap();
                file
            });
            let before = state.revision().unwrap().revision;
            let job = Arc::new(ScanJob {
                root: "SFX".into(),
                subpath: subpath.into(),
                mode: ScanMode::Incremental,
                control: ScanCancellation::default(),
                status: Mutex::new(JobStatus {
                    progress: ScanProgress::default(),
                    active: true,
                    revision: None,
                    shutdown: false,
                }),
            });
            let lease = state.security().settings_lease().unwrap();
            assert!(matches!(
                second.security().settings_lease(),
                Err(Error::Busy(_))
            ));
            let mut jobs = state.scans().jobs();
            jobs.active = Some(job.clone());
            let (sender, mut receiver) = oneshot::channel();
            let thread_state = state.clone();
            let thread_job = job.clone();
            let thread = std::thread::spawn(move || {
                run_scan(
                    &thread_state,
                    &thread_job,
                    thread_state.root_monitor(),
                    index,
                    1,
                    sender,
                    Some(lease),
                )
            });
            // The jobs mutex holds cleanup at its final step. Reacquiring the
            // settings lease proves cleanup has started, without depending on
            // whether the receiver or the scan thread wins a scheduling race.
            // Busy or transient errors are retried until the deadline; only the
            // deadline fails the wait.
            let waited = Instant::now();
            let deadline = waited + Duration::from_secs(30);
            let mut last_error = None;
            let settings_released = loop {
                match second.security().settings_lease() {
                    Ok(lease) => {
                        drop(lease);
                        break true;
                    }
                    Err(error) => {
                        last_error = Some(error.to_string());
                        if Instant::now() >= deadline {
                            break false;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            };
            let response_pending = matches!(
                receiver.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            );
            // Unblock and join before asserting, including on regression failure.
            drop(jobs);
            thread.join().unwrap();
            assert!(
                settings_released,
                "{subpath:?}: settings lease not released after {:?} (last error: {last_error:?})",
                waited.elapsed()
            );
            assert!(
                response_pending,
                "{subpath:?}: error published before cleanup"
            );
            assert_eq!(
                receiver.blocking_recv().unwrap().unwrap_err().code(),
                expected,
                "{subpath:?}"
            );
            assert!(state.scans().current().is_none());
            assert!(!job.response().active);
            assert!(second.security().settings_lease().is_ok());
            assert_eq!(state.revision().unwrap().revision, before);
            assert!(state.provider().scan_history(10, 0).unwrap().is_empty());
            drop(database_lease);
            assert!(second.provider().ensure_library_idle().is_ok());
        }
    }

    #[test]
    fn scan_admission_hands_settings_exclusion_to_the_database_lease() {
        let dir = tempfile::tempdir().unwrap();
        drop(audiolib_db::SqliteProvider::open(&dir.path().join("library.db")).unwrap());
        let config = crate::ServerConfig::from_toml_str(
            &format!(
                "library_db='library.db'\n[[roots]]\nname='SFX'\nserver_path='{}'\n",
                dir.path().display()
            ),
            dir.path(),
        )
        .unwrap();
        let state = AppState::open(&config, None).unwrap();
        let second = AppState::open(&config, None).unwrap();
        let edit = state.roots_edit_lock();
        let lease = state.security().settings_lease().unwrap();
        state.refresh_roots_locked(&lease).unwrap();
        let monitor = state.root_monitor();
        let error = second
            .start_scan(ScanRequest {
                root: "SFX".into(),
                ..Default::default()
            })
            .unwrap_err();
        assert_eq!(error.code(), "BUSY");
        assert!(second.scans().current().is_none());
        let control = ScanCancellation::default();
        assert!(control.pause());
        let job = Arc::new(ScanJob {
            root: "SFX".into(),
            subpath: String::new(),
            mode: ScanMode::Incremental,
            control,
            status: Mutex::new(JobStatus {
                progress: ScanProgress::default(),
                active: true,
                revision: None,
                shutdown: false,
            }),
        });
        state.scans().jobs().active = Some(job.clone());
        drop(edit);
        let (sender, receiver) = oneshot::channel();
        let thread_state = state.clone();
        let thread_job = job.clone();
        let thread = std::thread::spawn(move || {
            run_scan(
                &thread_state,
                &thread_job,
                monitor,
                0,
                1,
                sender,
                Some(lease),
            )
        });
        let started = receiver.blocking_recv().unwrap();
        let settings_released = second.security().settings_lease().is_ok();
        let database_reserved =
            matches!(second.provider().ensure_library_idle(), Err(Error::Busy(_)));
        // Always wake and join the paused scanner before assertions can unwind.
        job.control.cancel();
        thread.join().unwrap();
        assert!(started.is_ok());
        assert!(settings_released);
        assert!(database_reserved);
        assert!(state.scans().current().is_none());
        assert!(second.provider().ensure_library_idle().is_ok());
    }

    #[test]
    fn a_resolution_panic_releases_the_scan_slot_and_answers_the_request() {
        let dir = tempfile::tempdir().unwrap();
        drop(audiolib_db::SqliteProvider::open(&dir.path().join("library.db")).unwrap());
        let config = crate::ServerConfig::from_toml_str(
            &format!(
                "library_db='library.db'\n[[roots]]\nname='SFX'\nserver_path='{}'\n",
                dir.path().display()
            ),
            dir.path(),
        )
        .unwrap();
        let state = AppState::open(&config, None).unwrap();
        let job = Arc::new(ScanJob {
            root: "SFX".into(),
            subpath: String::new(),
            mode: ScanMode::Incremental,
            control: ScanCancellation::default(),
            status: Mutex::new(JobStatus {
                progress: ScanProgress::default(),
                active: true,
                revision: None,
                shutdown: false,
            }),
        });
        state.scans().jobs().active = Some(job.clone());
        let (sender, receiver) = oneshot::channel();
        // Simulate an unexpected failure before ScanService creates a session.
        run_scan(
            &state,
            &job,
            state.root_monitor(),
            usize::MAX,
            1,
            sender,
            Some(state.security().settings_lease().unwrap()),
        );
        assert_eq!(
            receiver.blocking_recv().unwrap().unwrap_err().code(),
            "INTERNAL"
        );
        assert!(state.scans().current().is_none());
        assert!(!job.response().active);
        assert!(state.security().settings_lease().is_ok());
        // The following reservation is usable instead of becoming permanently BUSY.
        let started = state
            .start_scan(ScanRequest {
                root: "SFX".into(),
                ..Default::default()
            })
            .unwrap();
        assert!(started.blocking_recv().unwrap().is_ok());
        assert!(state.scans().shutdown(Duration::from_secs(10)));
    }

    #[test]
    fn scan_cleanup_releases_a_revision_hold_when_completion_unwinds() {
        let dir = tempfile::tempdir().unwrap();
        drop(audiolib_db::SqliteProvider::open(&dir.path().join("library.db")).unwrap());
        let config = crate::ServerConfig::from_toml_str(
            &format!(
                "library_db='library.db'\n[[roots]]\nname='SFX'\nserver_path='{}'\n",
                dir.path().display()
            ),
            dir.path(),
        )
        .unwrap();
        let state = AppState::open(&config, None).unwrap();
        let job = Arc::new(ScanJob {
            root: "SFX".into(),
            subpath: String::new(),
            mode: ScanMode::Incremental,
            control: ScanCancellation::default(),
            status: Mutex::new(JobStatus {
                progress: ScanProgress::default(),
                active: true,
                revision: None,
                shutdown: false,
            }),
        });
        state.scans().jobs().active = Some(job.clone());
        let before = state.revision().unwrap().revision;
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            state.tracker().begin_hold().unwrap();
            let _cleanup = ScanCleanup {
                state: &state,
                job: &job,
                holding_revision: true,
                settings_lease: Some(state.security().settings_lease().unwrap()),
            };
            panic!("completion failure after beginning the hold");
        }));
        assert!(panic.is_err());
        assert!(state.scans().current().is_none());
        assert!(!job.response().active);
        assert!(state.revision().unwrap().revision > before);
        assert!(state.security().settings_lease().is_ok());
    }

    #[test]
    fn subpaths_are_relative_and_cannot_climb() {
        assert_eq!(normalize_subpath("").unwrap(), "");
        assert_eq!(normalize_subpath("a/b/").unwrap(), "a/b");
        assert_eq!(normalize_subpath("a//b").unwrap(), "a/b");
        assert_eq!(
            normalize_subpath("Foley 01/Doors").unwrap(),
            "Foley 01/Doors"
        );
        for bad in [
            "/abs",
            "..",
            "a/../b",
            "./a",
            "a/.",
            "a\\b",
            "\\\\nas\\x",
            "a\0b",
        ] {
            let error = normalize_subpath(bad).unwrap_err();
            assert_eq!(error.code(), "VALIDATION", "{bad:?}");
        }
        assert!(normalize_subpath(&"x".repeat(4097)).is_err());
    }

    #[test]
    fn shape_comparison_needs_both_sides() {
        let stats = LibraryStats {
            audio_files: 3,
            ..Default::default()
        };
        assert!(same_shape(&Some(stats.clone()), &Some(stats.clone())));
        assert!(!same_shape(&None, &Some(stats.clone())));
        let mut more = stats.clone();
        more.missing_files = 1;
        assert!(!same_shape(&Some(stats), &Some(more)));
    }
}
