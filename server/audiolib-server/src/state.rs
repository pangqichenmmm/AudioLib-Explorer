// server/audiolib-server/src/state.rs
//! Shared server state: the library `DataProvider`, its revision tracker and configured roots.
//! Every method that touches SQLite or the filesystem is blocking; async handlers call them
//! through `tokio::task::spawn_blocking`.
use crate::list_waveform::{ListOptions, ListWaveforms};
use crate::preview::TranscodeRegistry;
use crate::revision::{RevisionInfo, RevisionTracker, WriteOutcome};
use crate::roots::{
    RootMonitor, RootPolicy, RootState, RootStatus, is_transient_network_error, probe_root,
};
use crate::scans::ScanManager;
use crate::snapshot_registry::{SnapshotLease, SnapshotRegistry, SnapshotRepresentation};
use crate::waveform::{self, WaveformCache, WaveformKey, WaveformParams};
use crate::{
    auth::Security,
    config::{BearerToken, ServerConfig},
};
use audiolib_core::{
    AudioFile, BatchMetadataReport, BatchMetadataRequest, DataProvider, Error, LibraryInfo,
    LibraryStats, MetadataPatch, Result,
};
use audiolib_db::{SnapshotUpgradeProgress, SqliteProvider};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, PoisonError, RwLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

/// Concurrent `/library/snapshot` preparations; further requests get 409 `BUSY`.
const SNAPSHOT_SLOTS: usize = 2;
/// Upper bound for the folder walk that resolves a root's file count.
const MAX_FOLDER_DEPTH: usize = 256;
/// Name prefix of snapshot temporary directories (see [`AppState::prepare_snapshot`]).
const SNAPSHOT_PREFIX: &str = "audiolib-snapshot-";
/// Budget of the inline root recheck done by a stream on a root cached as offline and by
/// `POST /roots/{name}/check`.
pub const INLINE_RECHECK: Duration = Duration::from_secs(2);
/// Unfinished stream opens allowed per root, per `max_streams` + `max_previews` slot
/// (stream and preview opens share this bound). Slots are released
/// when a waiting client disconnects, but a blocked `open()` (sleeping disk, pending
/// macOS permission prompt) keeps its thread; beyond this many such opens the root's
/// streams are refused with `ROOT_OFFLINE` until they finish.
const UNFINISHED_OPENS_PER_SLOT: usize = 4;
/// A stream file open slower than this is logged as a warning.
pub const SLOW_OPEN_WARNING: Duration = Duration::from_secs(10);
/// zstd level for compressed snapshots (multithreaded; see doc/api/README.md §6.1).
const SNAPSHOT_ZSTD_LEVEL: i32 = 3;

/// Reply of `PATCH /audio/{id}/metadata` and `POST /audio/{id}/plays`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioWriteResponse {
    /// The record after the write.
    pub file: AudioFile,
    /// Library revision covering every commit before this write.
    pub revision_before: u64,
    /// Library revision after this write (`revision_before + 1`).
    pub revision: u64,
}

/// Reply of `POST /audio/metadata/batch`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BatchWriteResponse {
    /// `DataProvider::update_metadata_batch` report.
    pub report: BatchMetadataReport,
    /// Every changed record after the write, in request order without duplicates.
    pub files: Vec<AudioFile>,
    /// Library revision covering every commit before this write.
    pub revision_before: u64,
    /// Library revision after this write (`revision_before + 1`).
    pub revision: u64,
}

/// `GET /library` reply: the core `LibraryInfo` fields plus identity and revision.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryResponse {
    /// Completed persistent format positively identified from the actual library.
    pub snapshot_format: audiolib_core::compatibility::SnapshotFormat,
    /// Stable library identifier (see [`crate::revision`]).
    pub library_id: String,
    /// Monotonic change revision.
    pub revision: u64,
    /// This server rejects mutations unless their snapshot library identity matches.
    /// Clients must keep older servers (field absent/false) read-only.
    #[serde(default)]
    pub identity_bound_writes: bool,
    /// `DataProvider::library_info`, flattened; includes `schema_version` (SQLite user_version).
    /// `database_bytes` is main file + `-wal` file; `wal_bytes` is the WAL part.
    #[serde(flatten)]
    pub info: LibraryInfo,
}

/// One `GET /roots` entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootResponse {
    /// Configured root name.
    pub name: String,
    /// Configured server directory.
    pub server_path: String,
    /// Configured WebDAV URL, if any.
    pub webdav_url: Option<String>,
    /// Present (non-missing) files below the root when the root is a library folder.
    pub file_count: Option<u64>,
    /// `false` only when the root is `offline`; from the cached background check.
    #[serde(default = "default_true")]
    pub online: bool,
    /// `checking`, `online`, `waking` or `offline` (see [`RootState`]).
    #[serde(default = "default_status")]
    pub status: RootState,
    /// Reason when `offline`, otherwise `null`.
    #[serde(default)]
    pub status_message: Option<String>,
    /// Unix seconds of the last completed root check or stream open, `null` before any.
    #[serde(default)]
    pub checked_at: Option<u64>,
    /// Library records below the root marked missing (`missing = 1`); library only.
    #[serde(default)]
    pub missing_count: u64,
    /// Latest scan session of the root or a directory below it, `null` when none.
    #[serde(default)]
    pub last_scan: Option<RootLastScan>,
    /// A scan of this root (or a directory below it) is running or paused in this server.
    #[serde(default)]
    pub scanning: bool,
}

/// `last_scan` of a `GET /roots` entry, from the library's scan history.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootLastScan {
    /// Scan session id (`GET /scans/{id}`, `GET /history/scans`).
    pub id: i64,
    /// Scanned directory relative to the root (`""` = the whole root).
    pub subpath: String,
    /// `incremental` or `full`.
    pub mode: String,
    /// `running`, `completed`, `cancelled` or `failed` as recorded in the library.
    pub state: String,
    /// Unix seconds of the session start.
    pub started_at: Option<i64>,
    /// Unix seconds of the session end, `null` while it runs (or if it was interrupted).
    pub finished_at: Option<i64>,
    /// Files processed.
    pub scanned: u64,
    /// New records.
    pub added: u64,
    /// Changed (re-read) records.
    pub updated: u64,
    /// Records marked missing by this session.
    pub removed: u64,
    /// Files skipped as unchanged (incremental).
    pub unchanged: u64,
    /// Files that could not be read.
    pub errors: u64,
}

fn default_true() -> bool {
    true
}
fn default_status() -> RootState {
    RootState::Online
}

/// Why `/audio/{id}/stream` could not open a file.
#[derive(Debug)]
pub enum OpenError {
    /// Library error: unknown id (404), missing file (404), I/O (500), ...
    Library(Error),
    /// The file's root is offline or not answering (answered as 503 `ROOT_OFFLINE`).
    RootOffline {
        /// Configured root name.
        root: String,
        /// Path-free reason.
        reason: String,
    },
    /// Every cover decode slot stayed in use for [`crate::artwork::ARTWORK_SLOT_WAIT`]
    /// (answered as 503 `BUSY` with `Retry-After`).
    ArtworkBusy,
}

impl From<Error> for OpenError {
    fn from(error: Error) -> Self {
        Self::Library(error)
    }
}

/// Why [`AppState::stat_audio`] found no usable file.
#[derive(Debug)]
pub enum StatFailure {
    /// Unknown, outside every root, or not a regular file.
    Missing,
    /// The file's root is offline or not answering.
    RootOffline {
        /// Configured root name.
        root: String,
    },
    /// Any other error (permissions, ...).
    Other(Error),
}

/// Weak [`AppState`] handle.
pub(crate) struct WeakAppState(Weak<Inner>);

impl WeakAppState {
    pub(crate) fn upgrade(&self) -> Option<AppState> {
        self.0.upgrade().map(|inner| AppState { inner })
    }
}

/// An opened audio file whose canonical path lies under a configured root.
#[derive(Debug)]
pub struct ServedAudio {
    /// Open handle; metadata below was read from this handle.
    pub file: fs::File,
    /// Canonical path that passed the root check.
    pub path: PathBuf,
    /// File length in bytes.
    pub len: u64,
    /// Strong entity tag of the opened file (see [`crate::stream::file_etag`]).
    pub etag: String,
    /// Original file name from the library record (download `Content-Disposition`).
    pub filename: String,
    /// The library record (format metadata for `/audio/{id}/preview?codec=flac`).
    pub record: AudioFile,
}

/// One snapshot file in an owned temporary directory, removed when dropped.
pub struct SnapshotFile {
    // Release the owner lock before TempDir removes the owned payload.
    _owner: fs::File,
    /// Owning temporary directory; dropping it deletes the file.
    pub directory: tempfile::TempDir,
    /// File path inside `directory`.
    pub path: PathBuf,
    /// File size in bytes.
    pub bytes: u64,
    /// Process-local identity of these exact immutable bytes, distinct per encoding.
    pub id: String,
}

impl SnapshotFile {
    /// Strong entity tag of this immutable representation; a new file gets a new id.
    pub fn etag(&self) -> String {
        format!("\"snapshot-{}\"", self.id)
    }

    #[cfg(test)]
    pub(crate) fn test_file(bytes: u64) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let owner = snapshot_owner(directory.path()).unwrap();
        let path = directory.path().join("payload");
        fs::File::create(&path).unwrap().set_len(bytes).unwrap();
        Self {
            _owner: owner,
            directory,
            path,
            bytes,
            id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

/// Snapshot body encoding chosen from `Accept-Encoding`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotEncoding {
    /// The SQLite file as is.
    Identity,
    /// The SQLite file compressed with zstd (`Content-Encoding: zstd`).
    Zstd,
}

/// A consistent library copy built for exactly one `(library_id, revision)`. The server
/// keeps the newest one and serves it again while the revision is unchanged; it is
/// never served for another revision. After replacement, transmitted representations
/// remain only in the bounded retention registry or their active response leases.
pub struct PreparedSnapshot {
    /// Format inspected from the completed backup, not inferred from the server version.
    pub snapshot_format: audiolib_core::compatibility::SnapshotFormat,
    /// The SQLite copy.
    pub plain: Arc<SnapshotFile>,
    /// zstd-compressed copy, built on the first request that accepts it.
    zstd: Mutex<Option<Arc<SnapshotFile>>>,
    /// Library identity at preparation time.
    pub library_id: String,
    /// Revision observed immediately before the copy; the copy is at least this new.
    pub revision: u64,
    /// SQLite user_version of the snapshot itself.
    pub schema_version: i64,
}

impl PreparedSnapshot {
    /// Blocking. The zstd copy, compressing the plain copy on first use.
    pub fn zstd_file(&self) -> Result<Arc<SnapshotFile>> {
        self.zstd_file_cancellable(&AtomicBool::new(false))
    }

    fn zstd_file_cancellable(&self, cancel: &AtomicBool) -> Result<Arc<SnapshotFile>> {
        use std::io::{Read, Write};
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let mut slot = self
            .zstd
            .lock()
            .map_err(|_| Error::Database("Snapshot cache lock was poisoned".into()))?;
        if let Some(file) = slot.as_ref().filter(|file| file.path.is_file()) {
            return Ok(file.clone());
        }
        let parent = self
            .plain
            .directory
            .path()
            .parent()
            .unwrap_or(Path::new("."))
            .to_path_buf();
        // zstd level 3 gave 6-7 % on real libraries; ask for twice that before starting.
        audiolib_db::ensure_free_space(&parent, self.plain.bytes / 8)?;
        let directory = tempfile::Builder::new()
            .prefix(SNAPSHOT_PREFIX)
            .tempdir_in(parent)?;
        let owner = snapshot_owner(directory.path())?;
        let path = directory.path().join("library-snapshot.db.zst");
        let started = Instant::now();
        {
            let mut input = fs::File::open(&self.plain.path)?;
            let output = fs::File::create(&path)?;
            let mut encoder = zstd::stream::Encoder::new(output, SNAPSHOT_ZSTD_LEVEL)?;
            let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
            encoder.multithread(u32::try_from(threads.min(8)).unwrap_or(1))?;
            encoder.include_contentsize(true)?;
            encoder.set_pledged_src_size(Some(self.plain.bytes))?;
            let mut buffer = vec![0u8; 1024 * 1024];
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Err(Error::Cancelled);
                }
                let read = input.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                encoder.write_all(&buffer[..read])?;
            }
            encoder.finish()?.sync_all()?;
        }
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let bytes = fs::metadata(&path)?.len();
        tracing::info!(
            revision = self.revision,
            plain = self.plain.bytes,
            compressed = bytes,
            millis = started.elapsed().as_millis() as u64,
            "Compressed library snapshot"
        );
        let file = Arc::new(SnapshotFile {
            _owner: owner,
            directory,
            path,
            bytes,
            id: uuid::Uuid::new_v4().to_string(),
        });
        *slot = Some(file.clone());
        Ok(file)
    }
}

/// What `stream_target` reports to the async side while an open runs (for the
/// slow-open warning).
#[derive(Clone, Debug, Default)]
pub struct OpenProgress {
    target: Arc<Mutex<Option<(String, String)>>>,
}

impl OpenProgress {
    /// `(root name, library file path)` once the record has been read.
    pub fn target(&self) -> Option<(String, String)> {
        self.target.lock().ok().and_then(|target| target.clone())
    }
    fn set(&self, root: String, path: String) {
        if let Ok(mut target) = self.target.lock() {
            *target = Some((root, path));
        }
    }
    /// The open finished; later work (e.g. decoding) is not an open.
    pub(crate) fn clear(&self) {
        if let Ok(mut target) = self.target.lock() {
            *target = None;
        }
    }
}

/// Blocking. Cache a waveform body; a failed write only costs a recomputation later.
fn store(cache: &WaveformCache, key: &WaveformKey, zstd: bool, bytes: &[u8]) {
    if let Err(error) = cache.put(key, zstd, bytes) {
        tracing::warn!(%error, "Cannot write the waveform cache");
    }
}

/// Outcome of [`AppState::waveform`].
#[derive(Debug)]
pub enum WaveformReply {
    /// The client's `If-None-Match` already names this waveform (304).
    NotModified {
        /// Entity tag of the waveform in the requested encoding.
        etag: String,
    },
    /// The waveform body.
    Body {
        /// Entity tag of the waveform in this encoding.
        etag: String,
        /// JSON ([`crate::waveform::WaveformResponse`]), zstd-compressed when `zstd`.
        bytes: Vec<u8>,
        /// Whether `bytes` is zstd-compressed.
        zstd: bool,
    },
}

/// Result of one snapshot preparation: the plain snapshot and, for zstd, its compressed copy.
pub type SnapshotOutput = (Arc<PreparedSnapshot>, Option<Arc<SnapshotFile>>);

/// A snapshot preparation running on a blocking thread. It is not tied to the request
/// that started it: a client that gives up (or gets 503 `BUSY`) leaves it running, and
/// the next request joins it instead of starting another copy.
///
/// The state only keeps a `Weak` pointer: the finished result (and with it the snapshot
/// files) is owned by the requests that wait for it. Only the selected representation
/// is then handed to the bounded retention registry, independently of this job.
struct SnapshotJobState {
    result: Mutex<Option<Arc<Result<SnapshotOutput>>>>,
    done: tokio::sync::Notify,
}

impl SnapshotJobState {
    fn finished(&self) -> Option<Arc<Result<SnapshotOutput>>> {
        self.result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

type SnapshotJob = std::sync::Weak<SnapshotJobState>;

/// Copy of an error (`Error` holds an `io::Error`, which is not `Clone`).
pub fn clone_error(error: &Error) -> Error {
    match error {
        Error::Validation(m) => Error::Validation(m.clone()),
        Error::Database(m) => Error::Database(m.clone()),
        Error::Audio(m) => Error::Audio(m.clone()),
        Error::Io(io) => Error::Io(std::io::Error::new(io.kind(), io.to_string())),
        Error::Cancelled => Error::Cancelled,
        Error::Busy(m) => Error::Busy(m.clone()),
        Error::NotFound(m) => Error::NotFound(m.clone()),
        Error::NewerDatabase {
            what,
            found,
            supported,
        } => Error::NewerDatabase {
            what: what.clone(),
            found: *found,
            supported: *supported,
        },
    }
}

/// Statistics of `GET /library` with the library and revision they were computed for.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedStats {
    library_id: String,
    revision: u64,
    stats: LibraryStats,
    /// How long the computation took (small libraries are recomputed inline, see
    /// [`INLINE_STATS_MILLIS`]).
    #[serde(default)]
    millis: u64,
}

/// In-memory copy of the statistics (plus a sidecar `<database>.stats.json` so a restart
/// serves them at once).
struct StatsCache {
    value: Mutex<Option<CachedStats>>,
    /// Held while the statistics are being computed (single flight).
    compute: Mutex<()>,
    refreshing: AtomicBool,
    sidecar: Option<PathBuf>,
}

impl StatsCache {
    fn load(database: PathBuf) -> Self {
        let mut name = database.into_os_string();
        name.push(".stats.json");
        let sidecar = PathBuf::from(name);
        let value = fs::read(&sidecar)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CachedStats>(&bytes).ok());
        Self {
            value: Mutex::new(value),
            compute: Mutex::new(()),
            refreshing: AtomicBool::new(false),
            sidecar: Some(sidecar),
        }
    }

    fn get(&self) -> Option<CachedStats> {
        self.value
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Store `entry`, unless the cache already holds a newer, still valid entry of the
    /// same library (revision in `entry.revision+1..=current`): a slow background
    /// computation may finish after a stats-neutral write carried the cache forward. An
    /// entry newer than `current` (sidecar from elsewhere) is always replaced.
    fn put(&self, entry: CachedStats, current: u64) {
        let mut value = self.value.lock().unwrap_or_else(PoisonError::into_inner);
        if value.as_ref().is_some_and(|cached| {
            cached.library_id == entry.library_id
                && cached.revision > entry.revision
                && cached.revision <= current
        }) {
            return;
        }
        if let Some(sidecar) = &self.sidecar
            && let Ok(bytes) = serde_json::to_vec(&entry)
        {
            // Best effort: a missing or unwritable sidecar only costs a recompute.
            let temporary = sidecar.with_extension("json.tmp");
            if fs::write(&temporary, bytes).is_ok() {
                let _ = fs::rename(&temporary, sidecar);
            }
        }
        *value = Some(entry);
    }

    /// Relabel an exact cache for `before` as valid for `after` (a write that cannot
    /// change any aggregate, such as a play or a metadata edit).
    fn carry_forward(&self, library_id: &str, before: u64, after: u64) {
        let Some(mut entry) = self.get() else {
            return;
        };
        if entry.library_id == library_id && entry.revision == before && after > before {
            entry.revision = after;
            self.put(entry, after);
        }
    }
}

/// A statistics computation faster than this is simply redone inline when the revision
/// changed (the reply is then always exact); slower ones are served stale meanwhile.
const INLINE_STATS_MILLIS: u64 = 250;

struct Inner {
    stopping: AtomicBool,
    provider: Arc<SqliteProvider>,
    tracker: RevisionTracker,
    /// Immutable identity of this provider; never takes the revision/write mutex.
    library_id: String,
    /// Current root monitor; replaced as a whole when roots change on the admin page.
    roots: RwLock<Arc<RootMonitor>>,
    root_policy: RootPolicy,
    /// `GET /library` statistics cache (see [`StatsCache`]).
    stats: StatsCache,
    /// Serializes root additions and removals from the admin page.
    roots_edit: Mutex<()>,
    stream_open_delay_ms: AtomicU64,
    snapshot_delay_ms: AtomicU64,
    stat_delay_ms: AtomicU64,
    security: Arc<Security>,
    snapshot_dir: Option<PathBuf>,
    snapshot_slots: Arc<Semaphore>,
    snapshot_cache: Mutex<Option<Arc<PreparedSnapshot>>>,
    snapshot_registry: SnapshotRegistry,
    /// The snapshot preparation in flight (or last finished), per [`SnapshotEncoding`].
    snapshot_jobs: Mutex<[Option<SnapshotJob>; 2]>,
    /// How long `GET /library/snapshot` waits for a preparation (`snapshot_wait_secs`).
    snapshot_wait: Duration,
    stream_slots: Arc<Semaphore>,
    preview_slots: Arc<Semaphore>,
    /// `max_streams + max_previews`: the slots whose opens may be stuck on one root.
    open_slots: usize,
    waveforms: WaveformCache,
    waveform_slots: Arc<Semaphore>,
    /// `max_streams` / `max_previews`: a slot in use means audio is being served.
    max_streams: usize,
    max_previews: usize,
    /// `max_transcodes` / `max_waveforms` (admin runtime view only).
    max_transcodes: usize,
    max_waveforms: usize,
    /// List thumbnail waveforms (`POST /waveforms`, pre-generation; §9.1–§9.2).
    list: Arc<ListWaveforms>,
    ffmpeg_path: PathBuf,
    transcode_slots: Arc<Semaphore>,
    /// Cover decode slots ([`crate::artwork::ARTWORK_SLOTS`]); cache hits and 304s skip them.
    artwork_slots: Arc<Semaphore>,
    transcodes: TranscodeRegistry,
    scans: ScanManager,
    /// Read-only connection for the `/roots` aggregates (missing counts, last scans).
    roots_db: Mutex<Connection>,
}

/// Active slots of one limit.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct SlotCount {
    /// Slots in use now.
    pub active: usize,
    /// Configured limit (`config.toml`, needs a restart).
    pub max: usize,
}

/// [`AppState::slot_usage`].
#[derive(Clone, Copy, Debug, Serialize)]
pub struct SlotUsage {
    /// `/audio/{id}/stream` (`max_streams`).
    pub streams: SlotCount,
    /// `/audio/{id}/preview` (`max_previews`).
    pub previews: SlotCount,
    /// ffmpeg transcodes (`max_transcodes`).
    pub transcodes: SlotCount,
    /// Waveform decodes (`max_waveforms`).
    pub waveforms: SlotCount,
}

/// Cached snapshot copy of [`SnapshotRuntime`].
#[derive(Clone, Debug, Serialize)]
pub struct CachedSnapshot {
    /// Revision the copy was built for.
    pub revision: u64,
    /// SQLite user_version of the copy.
    pub schema_version: i64,
    /// Size of the plain copy.
    pub bytes: u64,
    /// Size of the zstd copy, once built.
    pub zstd_bytes: Option<u64>,
}

/// [`AppState::snapshot_runtime`].
#[derive(Clone, Debug, Serialize)]
pub struct SnapshotRuntime {
    /// A plain snapshot is being prepared.
    pub preparing: bool,
    /// A zstd snapshot is being prepared.
    pub preparing_zstd: bool,
    /// The newest prepared copy kept for reuse.
    pub cached: Option<CachedSnapshot>,
}

/// Cheaply cloneable handle shared by all request handlers.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

impl AppState {
    /// Blocking. Open the configured existing library (never creating one) and its tracker.
    /// `token` is the environment token: required with `config.auth` (then enforced on
    /// every endpoint); otherwise accepted from external clients when given (see
    /// `crate::auth`). The admin files beside the library are loaded.
    pub fn open(config: &ServerConfig, token: impl Into<Option<BearerToken>>) -> Result<Self> {
        Self::open_with_security(config, Security::from_config(config, token.into()))
    }

    /// Like [`AppState::open`] with already loaded security (the server shares one
    /// [`Security`] between the starting service and the opened library).
    pub fn open_with_security(config: &ServerConfig, security: Arc<Security>) -> Result<Self> {
        Self::open_with_progress(config, security, |_| {})
    }

    /// Open with committed migration progress suitable for a lightweight health response.
    pub fn open_with_progress(
        config: &ServerConfig,
        security: Arc<Security>,
        progress: impl FnMut(SnapshotUpgradeProgress),
    ) -> Result<Self> {
        Self::open_with_progress_cancellable(config, security, &AtomicBool::new(false), progress)
    }

    /// Open with cancellation at committed upgrade boundaries.
    pub fn open_with_progress_cancellable(
        config: &ServerConfig,
        security: Arc<Security>,
        cancel: &AtomicBool,
        progress: impl FnMut(SnapshotUpgradeProgress),
    ) -> Result<Self> {
        // The root checkers start only once the library is open, so failed (retried)
        // opens never leave checker threads behind on a hung volume.
        // Roots changed on the admin page replace the `config.toml` list.
        let root_security = security.clone();
        Self::open_inner(config, security, cancel, progress, || {
            let roots = root_security
                .roots_override()
                .unwrap_or_else(|| config.roots.clone());
            RootMonitor::start(&roots, config.root_policy())
        })
    }

    /// Like [`AppState::open`] with a caller-built root monitor (tests use it to simulate
    /// slow or failing roots). The monitor's roots should match `config.roots`.
    pub fn open_with_monitor(
        config: &ServerConfig,
        token: impl Into<Option<BearerToken>>,
        monitor: RootMonitor,
    ) -> Result<Self> {
        Self::open_inner(
            config,
            Security::from_config(config, token.into()),
            &AtomicBool::new(false),
            |_| {},
            || monitor,
        )
    }

    fn open_inner(
        config: &ServerConfig,
        security: Arc<Security>,
        cancel: &AtomicBool,
        mut report: impl FnMut(SnapshotUpgradeProgress),
        monitor: impl FnOnce() -> RootMonitor,
    ) -> Result<Self> {
        if config.auth && security.env_token().is_none() {
            return Err(Error::Validation(
                "auth = true requires a bearer token".into(),
            ));
        }
        // Large snapshots belong on the library's storage, not an OS temp directory
        // that may be an in-memory tmpfs on Linux.
        let snapshot_dir = config.snapshot_dir.clone().unwrap_or_else(|| {
            config
                .library_db
                .parent()
                .unwrap_or(Path::new("."))
                .join("snapshots")
        });
        // Allow another instance to finish creating and locking a new owner marker.
        remove_stale_snapshots(&snapshot_dir, Some(Duration::from_secs(30)));
        let opened = Instant::now();
        let provider = Arc::new(SqliteProvider::open_existing_with_progress_cancellable(
            &config.library_db,
            cancel,
            |progress| {
                report(progress);
                let elapsed_seconds = opened.elapsed().as_secs_f64();
                match progress {
                    SnapshotUpgradeProgress::Preparing => tracing::info!(
                        phase = "preparing",
                        elapsed_seconds,
                        "Library startup: checking schema and search indexes"
                    ),
                    SnapshotUpgradeProgress::Tokenizing { completed, total } => tracing::info!(
                        phase = "tokenizing",
                        completed,
                        total,
                        elapsed_seconds,
                        "Library startup: rebuilding search text (committed rows)"
                    ),
                    SnapshotUpgradeProgress::BuildingFts => tracing::info!(
                        phase = "building_fts",
                        elapsed_seconds,
                        "Library startup: building FTS5 postings"
                    ),
                    SnapshotUpgradeProgress::SourceTags => tracing::info!(
                        phase = "source_tags",
                        elapsed_seconds,
                        "Library startup: checking extracted source tags"
                    ),
                    SnapshotUpgradeProgress::Copying { completed, total } => tracing::info!(
                        phase = "copying",
                        completed,
                        total,
                        elapsed_seconds,
                        "Library startup: copying retained records (committed rows)"
                    ),
                    SnapshotUpgradeProgress::Indexing { completed, total } => tracing::info!(
                        phase = "indexing",
                        completed,
                        total,
                        elapsed_seconds,
                        "Library startup: building FTS5 (committed rows)"
                    ),
                    stage => {
                        tracing::info!(?stage, elapsed_seconds, "Library startup: maintenance")
                    }
                }
            },
        )?);
        tracing::info!(
            elapsed_seconds = opened.elapsed().as_secs_f64(),
            "Library startup: database opened"
        );
        crate::manage::recover_root_relink(&provider, &security)?;
        let tracker = RevisionTracker::open(provider.database_path())?;
        let library_id = tracker.current()?.library_id;
        let provider_path = provider.database_path().to_path_buf();
        let roots_db = Connection::open_with_flags(
            provider.database_path(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .and_then(|conn| {
            conn.busy_timeout(Duration::from_secs(5))?;
            conn.execute_batch("PRAGMA query_only=ON;")?;
            Ok(conn)
        })
        .map_err(|error| Error::Database(error.to_string()))?;
        if !crate::preview::ffmpeg_available(&config.ffmpeg_path) {
            tracing::warn!(
                path = %config.ffmpeg_path.display(),
                "ffmpeg is not available: /audio/{{id}}/preview?codec=flac answers 503 TRANSCODER_UNAVAILABLE (codec=pcm is unaffected)"
            );
        }
        let list = Arc::new(ListWaveforms::new(
            ListOptions {
                cache_root: config.waveform_cache_path(),
                cache_mb: config.list_waveform_cache_mb,
                workers: config.effective_list_waveform_workers(),
                rate: config.list_waveform_rate,
                pause_stats_on_playback: config.list_waveform_pause_stats_on_playback,
            },
            &library_id,
            provider.database_path(),
        ));
        // Root directories are deliberately not touched here (a sleeping NAS disk would
        // delay startup); the root monitor checks them in the background.
        let state = Self {
            inner: Arc::new(Inner {
                stopping: AtomicBool::new(false),
                provider,
                tracker,
                library_id,
                roots: RwLock::new(Arc::new(monitor())),
                root_policy: config.root_policy(),
                roots_edit: Mutex::new(()),
                stats: StatsCache::load(provider_path),
                stream_open_delay_ms: AtomicU64::new(0),
                snapshot_delay_ms: AtomicU64::new(0),
                stat_delay_ms: AtomicU64::new(0),
                security,
                snapshot_dir: Some(snapshot_dir),
                snapshot_slots: Arc::new(Semaphore::new(SNAPSHOT_SLOTS)),
                snapshot_cache: Mutex::new(None),
                snapshot_registry: SnapshotRegistry::default(),
                snapshot_jobs: Mutex::new([None, None]),
                snapshot_wait: Duration::from_secs(config.snapshot_wait_secs),
                stream_slots: Arc::new(Semaphore::new(config.max_streams)),
                preview_slots: Arc::new(Semaphore::new(config.max_previews)),
                open_slots: config.max_streams + config.max_previews,
                waveforms: WaveformCache::new(
                    config.waveform_cache_path(),
                    config.waveform_cache_mb,
                ),
                waveform_slots: Arc::new(Semaphore::new(config.max_waveforms)),
                max_streams: config.max_streams,
                max_previews: config.max_previews,
                max_transcodes: config.max_transcodes,
                max_waveforms: config.max_waveforms,
                list,
                ffmpeg_path: config.ffmpeg_path.clone(),
                transcode_slots: Arc::new(Semaphore::new(config.max_transcodes)),
                artwork_slots: Arc::new(Semaphore::new(crate::artwork::ARTWORK_SLOTS)),
                transcodes: TranscodeRegistry::default(),
                scans: ScanManager::default(),
                roots_db: Mutex::new(roots_db),
            }),
        };
        crate::list_waveform::spawn_workers(&state);
        Ok(state)
    }

    /// List thumbnail waveforms of this library (§9.1–§9.2).
    pub fn list_waveforms(&self) -> &Arc<ListWaveforms> {
        &self.inner.list
    }

    /// Handle that does not keep the library open (background workers).
    pub(crate) fn downgrade(&self) -> WeakAppState {
        WeakAppState(Arc::downgrade(&self.inner))
    }

    /// Whether any `/audio/{id}/stream` or `/audio/{id}/preview` response is in progress
    /// (one of their slots is taken).
    pub fn playback_active(&self) -> bool {
        self.inner.stream_slots.available_permits() < self.inner.max_streams
            || self.inner.preview_slots.available_permits() < self.inner.max_previews
    }

    /// A stream or preview response was admitted (list waveform yield rules, §9.2).
    pub(crate) fn note_playback_start(&self) {
        self.inner.list.note_playback_start();
    }

    /// `max_streams + max_previews`.
    pub(crate) fn open_slot_count(&self) -> usize {
        self.inner.open_slots
    }

    /// Blocking. Read the metadata of audio `id`'s file at `file_path` without opening
    /// it: the same canonical root containment as [`AppState::open_audio`], registered
    /// with the root monitor while it runs. Returns the canonical path and the file's
    /// stream entity tag.
    pub(crate) fn stat_audio(
        &self,
        id: i64,
        file_path: &str,
        index: Option<usize>,
    ) -> std::result::Result<(PathBuf, String), StatFailure> {
        let monitor = self.root_monitor();
        let _registration = index.map(|index| monitor.begin_open(index));
        let classify = |error: OpenError| match error {
            OpenError::RootOffline { root, .. } => StatFailure::RootOffline { root },
            OpenError::Library(Error::NotFound(_)) => StatFailure::Missing,
            OpenError::Library(error) => StatFailure::Other(error),
            // Only cover decodes report this; a metadata read never waits for one.
            OpenError::ArtworkBusy => {
                StatFailure::Other(Error::Busy("Artwork slots are busy".into()))
            }
        };
        let unavailable = || Error::NotFound(format!("Audio {id} file is missing on the server"));
        let delay = self.inner.stat_delay_ms.load(Ordering::Relaxed);
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
        let path = fs::canonicalize(file_path)
            .map_err(|error| classify(self.access_failure(index, error, unavailable())))?;
        let candidates: Vec<usize> = match index {
            Some(index) => vec![index],
            None => (0..monitor.roots().len()).collect(),
        };
        let mut matched = None;
        for candidate in candidates {
            match fs::canonicalize(&monitor.roots()[candidate].server_path) {
                Ok(root) if path.starts_with(&root) => {
                    matched = Some((candidate, root));
                    break;
                }
                Ok(_) => {}
                Err(error) if index == Some(candidate) => {
                    return Err(classify(self.access_failure(index, error, unavailable())));
                }
                Err(_) => {}
            }
        }
        let Some((root_index, root)) = matched else {
            return Err(StatFailure::Missing);
        };
        let metadata = match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return Err(StatFailure::Missing),
            Err(error) => {
                return Err(classify(self.access_failure(
                    Some(root_index),
                    error,
                    unavailable(),
                )));
            }
        };
        monitor.record(root_index, Ok(root));
        Ok((path, crate::stream::file_etag(&metadata)))
    }

    /// Stop new snapshot work, drain admitted preparations and scans, release cached
    /// files, then checkpoint and persist the revision baseline. A timeout stays visible.
    pub fn shutdown(&self, timeout: Duration) -> Result<()> {
        self.inner.stopping.store(true, Ordering::Release);
        self.inner.snapshot_registry.close();
        self.inner.list.shutdown();
        if let Some(wfdb) = self.inner.list.wfdb() {
            wfdb.close_bundles();
        }
        let started = Instant::now();
        if !self.inner.scans.shutdown(timeout) {
            return Err(Error::Busy("A scan is still finishing".into()));
        }
        let jobs: Vec<_> = self
            .inner
            .snapshot_jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .flatten()
            .filter_map(std::sync::Weak::upgrade)
            .collect();
        while jobs.iter().any(|job| job.finished().is_none())
            || self.inner.stats.refreshing.load(Ordering::Acquire)
        {
            if started.elapsed() >= timeout {
                return Err(Error::Busy(
                    "Snapshot or statistics preparation is still finishing".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(jobs);
        self.invalidate_snapshot();
        self.inner.tracker.checkpoint(&self.inner.provider)
    }

    /// The library provider; future endpoints use it for all business data access.
    pub fn provider(&self) -> &Arc<SqliteProvider> {
        &self.inner.provider
    }

    /// Immutable library identity pinned by this AppState and its DataProvider.
    /// Reading it never waits for database writes or revision polling.
    pub fn library_id(&self) -> &str {
        &self.inner.library_id
    }

    /// Remote scans (`POST /scans`, see [`crate::scans`]).
    pub fn scans(&self) -> &ScanManager {
        &self.inner.scans
    }

    /// Configured roots and their cached status.
    pub(crate) fn root_monitor(&self) -> Arc<RootMonitor> {
        self.inner
            .roots
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replace the configured roots without a restart: a new monitor (fresh checks)
    /// takes over; requests already running keep the old one until they finish.
    pub(crate) fn roots_edit_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.inner
            .roots_edit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn replace_roots(&self, roots: &[crate::config::RootConfig]) {
        let monitor = Arc::new(RootMonitor::start(roots, self.inner.root_policy));
        *self
            .inner
            .roots
            .write()
            .unwrap_or_else(PoisonError::into_inner) = monitor;
    }

    /// The revision tracker (scans hold it while they run).
    pub(crate) fn tracker(&self) -> &RevisionTracker {
        &self.inner.tracker
    }

    /// The environment bearer token, if configured (see [`Security::env_token`]).
    pub fn token(&self) -> Option<&BearerToken> {
        self.inner.security.env_token()
    }

    /// Authentication and admin settings.
    pub fn security(&self) -> &Arc<Security> {
        &self.inner.security
    }

    pub(crate) fn snapshot_slots(&self) -> Arc<Semaphore> {
        self.inner.snapshot_slots.clone()
    }

    /// Retain only the selected representation, independently of its prepared source.
    pub(crate) fn retain_snapshot(
        &self,
        prepared: Arc<PreparedSnapshot>,
        file: Option<Arc<SnapshotFile>>,
        encoding: SnapshotEncoding,
    ) -> Result<SnapshotLease> {
        self.inner.snapshot_registry.admit(SnapshotRepresentation {
            file: file.unwrap_or_else(|| prepared.plain.clone()),
            library_id: prepared.library_id.clone(),
            revision: prepared.revision,
            snapshot_format: prepared.snapshot_format.clone(),
            encoding,
            uncompressed_bytes: prepared.plain.bytes,
        })
    }

    /// Run a snapshot preparation step; when it fails for lack of disk space, release
    /// the least recently used idle retained snapshot of an older revision and retry,
    /// until it succeeds or an eviction frees no space (then the `StorageFull` error,
    /// 507, is returned; issue #35). Entries of `revision`, the revision being prepared,
    /// are never released: they cannot make room and may be its resumable copy.
    fn with_snapshot_space<T>(
        &self,
        revision: Option<u64>,
        step: impl FnMut() -> Result<T>,
    ) -> Result<T> {
        retry_on_storage_full(step, || {
            self.inner
                .snapshot_registry
                .evict_oldest_idle(self.library_id(), revision)
                > 0
        })
    }

    /// Find an exact process-local representation belonging to this library only.
    pub(crate) fn retained_snapshot(&self, id: &str) -> Option<SnapshotLease> {
        self.inner.snapshot_registry.get(id, self.library_id())
    }

    pub(crate) fn stream_slots(&self) -> Arc<Semaphore> {
        self.inner.stream_slots.clone()
    }

    /// Slots of `/audio/{id}/preview` (`max_previews`), separate from `/stream`.
    pub(crate) fn preview_slots(&self) -> Arc<Semaphore> {
        self.inner.preview_slots.clone()
    }

    pub(crate) fn waveform_slots(&self) -> Arc<Semaphore> {
        self.inner.waveform_slots.clone()
    }

    pub(crate) fn transcode_slots(&self) -> Arc<Semaphore> {
        self.inner.transcode_slots.clone()
    }

    /// Cover decode slots. Public only so tests can hold every slot.
    #[doc(hidden)]
    pub fn artwork_slots(&self) -> Arc<Semaphore> {
        self.inner.artwork_slots.clone()
    }

    pub(crate) fn transcodes(&self) -> &TranscodeRegistry {
        &self.inner.transcodes
    }

    /// Configured ffmpeg executable (`ffmpeg_path`).
    pub fn ffmpeg_path(&self) -> &Path {
        &self.inner.ffmpeg_path
    }

    /// Process ids of running ffmpeg previews (diagnostics and tests).
    pub fn transcode_pids(&self) -> Vec<u32> {
        self.inner.transcodes.pids()
    }

    /// Directory of the waveform cache.
    pub fn waveform_cache_dir(&self) -> &Path {
        self.inner.waveforms.dir()
    }

    /// Blocking. `GET /audio/{id}/waveform`: open the file like a stream (same root
    /// checks, inline recheck and `RootOffline` answers), then serve the waveform from
    /// the cache or compute it. A cache hit reads only the file's metadata, never its
    /// audio. `cancel` stops a running decode (the client disconnected).
    pub fn waveform(
        &self,
        id: i64,
        params: &WaveformParams,
        zstd: bool,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
        progress: &OpenProgress,
    ) -> std::result::Result<WaveformReply, OpenError> {
        let ServedAudio {
            file, path, etag, ..
        } = self.open_audio_reporting(id, progress)?;
        drop(file);
        progress.clear();
        let key = WaveformKey::new(self.library_id(), id, &path, &etag, params);
        let etag = key.etag(zstd);
        if if_none_match.is_some_and(|value| crate::stream::none_match(value, &etag)) {
            return Ok(WaveformReply::NotModified { etag });
        }
        let cache = &self.inner.waveforms;
        if let Some(bytes) = cache.get(&key, zstd) {
            return Ok(WaveformReply::Body { etag, bytes, zstd });
        }
        let json = match zstd.then(|| cache.get(&key, false)).flatten() {
            Some(json) => json,
            None => {
                let started = Instant::now();
                let json = waveform::compute(&path, params, cancel)
                    .map_err(|error| self.decode_failure(&path, error))?;
                tracing::info!(
                    id,
                    buckets = params.buckets,
                    bytes = json.len(),
                    ms = started.elapsed().as_millis() as u64,
                    "Computed waveform"
                );
                store(cache, &key, false, &json);
                json
            }
        };
        let bytes = if zstd {
            let packed = waveform::compress(&json)?;
            store(cache, &key, true, &packed);
            packed
        } else {
            json
        };
        Ok(WaveformReply::Body { etag, bytes, zstd })
    }

    /// A decode error on a network root that stopped answering is `RootOffline`.
    pub(crate) fn decode_failure(&self, path: &Path, error: Error) -> OpenError {
        if let Error::Io(io) = &error
            && is_transient_network_error(io)
        {
            let monitor = self.root_monitor();
            let root = monitor
                .root_index_for(path)
                .map_or_else(String::new, |index| monitor.roots()[index].name.clone());
            return OpenError::RootOffline {
                root,
                reason: format!("Root is not answering reliably: {io}"),
            };
        }
        OpenError::Library(error)
    }

    /// Last known identity and revision without polling the database.
    pub fn revision(&self) -> Result<RevisionInfo> {
        self.inner.tracker.current()
    }

    /// Start a background statistics refresh when the cache is older than `revision`.
    fn refresh_stats_if_stale(&self, revision: &crate::revision::RevisionInfo) {
        let stale = self.inner.stats.get().is_none_or(|entry| {
            entry.library_id != revision.library_id || entry.revision != revision.revision
        });
        if stale {
            self.refresh_stats_in_background();
        }
    }

    /// Blocking. Detect committed changes and return the (possibly bumped) revision.
    pub fn poll_revision(&self) -> Result<RevisionInfo> {
        self.inner.tracker.poll()
    }

    /// Blocking. Bump the revision after a server-side write through the provider.
    pub fn bump_revision(&self) -> Result<RevisionInfo> {
        self.inner.tracker.bump()
    }

    /// Blocking. Serialize a provider write and advance its revision once. Concurrent
    /// local commits invalidate incremental replies through the writer connection's
    /// change observation (see [`RevisionTracker::record_own_write`]).
    pub fn write<T>(
        &self,
        operation: impl FnOnce(&SqliteProvider) -> Result<T>,
    ) -> Result<WriteOutcome<T>> {
        self.inner.tracker.record_own_write(
            || operation(self.inner.provider.as_ref()),
            || self.inner.provider.foreign_data_version(),
        )
    }

    /// Blocking. Like [`AppState::write`], but only `changed` operations advance their
    /// own revision; concurrent foreign commits are still counted (see
    /// [`RevisionTracker::record_own_write_if`]).
    pub fn write_if<T>(
        &self,
        operation: impl FnOnce(&SqliteProvider) -> Result<T>,
        changed: impl FnOnce(&T) -> bool,
    ) -> Result<WriteOutcome<T>> {
        self.inner.tracker.record_own_write_if(
            || operation(self.inner.provider.as_ref()),
            changed,
            || self.inner.provider.foreign_data_version(),
        )
    }

    /// Blocking. Run a provider write that changes no snapshot content clients must
    /// re-download for (play bookkeeping); isolated writes retain the revision (see
    /// [`RevisionTracker::record_own_write_quiet`]).
    pub fn write_quiet<T>(
        &self,
        operation: impl FnOnce(&SqliteProvider) -> Result<T>,
    ) -> Result<WriteOutcome<T>> {
        self.inner.tracker.record_own_write_quiet(
            || operation(self.inner.provider.as_ref()),
            || self.inner.provider.foreign_data_version(),
        )
    }

    /// Blocking. Run `query` on the read-only connection that also serves the `/roots`
    /// aggregates (never the writer, so it does not wait for writes).
    pub(crate) fn read_query<T>(
        &self,
        query: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> Result<T> {
        let conn = self
            .inner
            .roots_db
            .lock()
            .map_err(|_| Error::Database("Roots connection lock was poisoned".into()))?;
        query(&conn).map_err(|error| Error::Database(error.to_string()))
    }

    /// Forget the cached snapshot after a write that bumped the revision. It would never
    /// be served for the new revision anyway; dropping it frees its files as soon as no
    /// response streams them. Must not be called while holding the revision tracker.
    pub(crate) fn invalidate_snapshot(&self) {
        if let Ok(mut cache) = self.inner.snapshot_cache.lock() {
            *cache = None;
        }
    }

    /// After a write that cannot change the `GET /library` aggregates (plays, metadata
    /// edits: file count, folders, duration, bytes and missing count stay the same), keep
    /// the statistics cache exact instead of rescanning the whole table (~30 s cold on a
    /// 909k-row library) once per play.
    fn carry_stats_forward<T>(&self, outcome: &WriteOutcome<T>) {
        if let Ok(current) = self.inner.tracker.current() {
            self.inner.stats.carry_forward(
                &current.library_id,
                outcome.revision_before,
                outcome.revision,
            );
        }
    }

    /// Blocking. `DataProvider::update_metadata` with revision bookkeeping.
    pub fn update_metadata(&self, id: i64, patch: &MetadataPatch) -> Result<AudioWriteResponse> {
        patch.validate()?;
        let outcome = self.write(|provider| provider.update_metadata(id, patch))?;
        self.carry_stats_forward(&outcome);
        Ok(AudioWriteResponse {
            file: outcome.value,
            revision_before: outcome.revision_before,
            revision: outcome.revision,
        })
    }

    /// Compatibility endpoint: validate/read the record without recording playback.
    pub fn record_play(&self, id: i64) -> Result<AudioWriteResponse> {
        let outcome = self.write_if(|provider| provider.get_audio(id), |_| false)?;
        Ok(AudioWriteResponse {
            file: outcome.value,
            revision_before: outcome.revision_before,
            revision: outcome.revision,
        })
    }

    /// Blocking. `DataProvider::update_metadata_batch`, returning the changed records.
    pub fn update_metadata_batch(
        &self,
        request: &BatchMetadataRequest,
    ) -> Result<BatchWriteResponse> {
        request.validate()?;
        let mut seen = std::collections::HashSet::new();
        let ids: Vec<i64> = request
            .ids
            .iter()
            .copied()
            .filter(|id| seen.insert(*id))
            .collect();
        let outcome = self.write(|provider| {
            let report = provider.update_metadata_batch(request)?;
            let files = ids
                .iter()
                .map(|id| provider.get_audio(*id))
                .collect::<Result<Vec<_>>>()?;
            Ok((report, files))
        })?;
        self.carry_stats_forward(&outcome);
        let (report, files) = outcome.value;
        Ok(BatchWriteResponse {
            report,
            files,
            revision_before: outcome.revision_before,
            revision: outcome.revision,
        })
    }

    /// Blocking. Build the `GET /library` reply. The revision is read before the info,
    /// so the reported data is never older than the reported revision.
    pub fn library_response(&self) -> Result<LibraryResponse> {
        let revision = self.poll_revision()?;
        let stats = self.library_stats(&revision)?;
        let mut info = self.inner.provider.library_info_with_stats(stats)?;
        info.database_bytes = info.database_bytes.saturating_add(info.wal_bytes);
        // Remote callers only get the file name; the full path is not exposed over the API.
        info.database = info
            .database
            .file_name()
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        Ok(LibraryResponse {
            snapshot_format: self.snapshot_format()?,
            library_id: revision.library_id,
            revision: revision.revision,
            identity_bound_writes: true,
            info,
        })
    }

    /// Inspect the actual ready library without copying it or scanning business rows.
    pub fn snapshot_format(&self) -> Result<audiolib_core::compatibility::SnapshotFormat> {
        inspected_snapshot_format(self.inner.provider.database_path(), &self.inner.stopping)
    }

    /// Library statistics for `GET /library`. The aggregates scan the whole `audio_files`
    /// table (seconds, and ~30 s on a cold 10 GB file), so they are cached with the
    /// revision they were computed for: an exact hit is returned at once; a cache from an
    /// older revision is returned as is while one background thread recomputes it
    /// (stale-while-revalidate, the counts only lag the very last writes); only the very
    /// first request without any cache (no sidecar, no warm-up finished yet) waits for the
    /// computation, and concurrent callers share it.
    fn library_stats(&self, revision: &crate::revision::RevisionInfo) -> Result<LibraryStats> {
        let cache = &self.inner.stats;
        let cached = cache.get();
        match cached {
            Some(entry) if entry.library_id == revision.library_id => {
                if entry.revision == revision.revision {
                    return Ok(entry.stats);
                }
                if entry.millis > INLINE_STATS_MILLIS {
                    self.refresh_stats_in_background();
                    return Ok(entry.stats);
                }
            }
            _ => {}
        }
        let _compute = cache.compute.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = cache.get()
            && entry.library_id == revision.library_id
            && entry.revision == revision.revision
        {
            return Ok(entry.stats);
        }
        self.compute_stats(revision)
    }

    /// Blocking. Compute and store the statistics for `revision` (read before computing,
    /// so the cache is never labelled newer than its data).
    fn compute_stats(&self, revision: &crate::revision::RevisionInfo) -> Result<LibraryStats> {
        let started = Instant::now();
        let stats = self.inner.provider.stats()?;
        let current = self
            .inner
            .tracker
            .current()
            .map_or(revision.revision, |now| now.revision);
        self.inner.stats.put(
            CachedStats {
                library_id: revision.library_id.clone(),
                revision: revision.revision,
                stats: stats.clone(),
                millis: started.elapsed().as_millis() as u64,
            },
            current,
        );
        tracing::info!(
            revision = revision.revision,
            audio_files = stats.audio_files,
            millis = started.elapsed().as_millis() as u64,
            "Computed library statistics"
        );
        Ok(stats)
    }

    /// Recompute the statistics on a plain thread unless one is already running. Also
    /// used at startup and when the revision poller sees a new revision, so `/library`
    /// rarely finds a stale cache.
    pub fn refresh_stats_in_background(&self) {
        if self.inner.stopping.load(Ordering::Acquire) {
            return;
        }
        let cache = &self.inner.stats;
        if cache.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.inner.stopping.load(Ordering::Acquire) {
            cache.refreshing.store(false, Ordering::Release);
            return;
        }
        let state = self.clone();
        let spawned = std::thread::Builder::new()
            .name("audiolib-stats".into())
            .spawn(move || {
                let outcome = state.revision().and_then(|revision| {
                    let _compute = state
                        .inner
                        .stats
                        .compute
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    // Startup warm-up with a current sidecar (or a stats-neutral write
                    // that already carried the cache forward): nothing to recompute.
                    if let Some(entry) = state.inner.stats.get()
                        && entry.library_id == revision.library_id
                        && entry.revision == revision.revision
                    {
                        return Ok(entry.stats);
                    }
                    state.compute_stats(&revision)
                });
                if let Err(error) = outcome {
                    tracing::warn!(%error, "Library statistics refresh failed");
                }
                state.inner.stats.refreshing.store(false, Ordering::Release);
            });
        if spawned.is_err() {
            cache.refreshing.store(false, Ordering::Release);
        }
    }

    /// Blocking. Build the `GET /roots` reply from configuration and library folders.
    /// Never touches root directories: online status comes from the root monitor's
    /// cache and `file_count` from library folders.
    pub fn roots_response(&self) -> Result<Vec<RootResponse>> {
        let monitor = self.root_monitor();
        (0..monitor.roots().len())
            .map(|index| self.root_response(&monitor, index, monitor.status(index)))
            .collect()
    }

    pub(crate) fn root_response(
        &self,
        monitor: &RootMonitor,
        index: usize,
        status: RootStatus,
    ) -> Result<RootResponse> {
        let root = &monitor.roots()[index];
        let key = status.canonical.as_deref().unwrap_or(&root.server_path);
        let (missing_count, last_scan) = self.root_aggregates(key)?;
        Ok(RootResponse {
            missing_count,
            last_scan,
            scanning: self.inner.scans.is_scanning(&root.name),
            name: root.name.clone(),
            server_path: root.server_path.display().to_string(),
            webdav_url: root.webdav_url.clone(),
            file_count: root_file_count(self.inner.provider.as_ref(), key)?,
            online: status.online(),
            status: status.state,
            status_message: status.message,
            checked_at: status.checked_at,
        })
    }

    /// Blocking. Missing records below `root` (through the partial `missing = 1` index,
    /// never a walk over every file) and the newest scan session of `root` or below it.
    fn root_aggregates(&self, root: &Path) -> Result<(u64, Option<RootLastScan>)> {
        let key = folder_key(&root.to_string_lossy());
        if key.is_empty() {
            return Ok((0, None));
        }
        let conn = self
            .inner
            .roots_db
            .lock()
            .map_err(|_| Error::Database("Roots connection lock was poisoned".into()))?;
        let database = |error: rusqlite::Error| Error::Database(error.to_string());
        // `+path_key` keeps the planner on idx_audio_files_missing (few rows) instead of
        // a range over every record of the root.
        let missing: i64 = conn
            .prepare_cached(
                "SELECT COUNT(*) FROM audio_files WHERE missing=1 \
                 AND +path_key >= ?1 || '/' AND +path_key < ?1 || '0'",
            )
            .and_then(|mut statement| statement.query_row([&key], |row| row.get(0)))
            .map_err(database)?;
        let last = conn
            .prepare_cached(
                "SELECT id, COALESCE(scan_path,''), COALESCE(scan_type,''), COALESCE(status,''), \
                 CAST(strftime('%s', start_time) AS INTEGER), CAST(strftime('%s', end_time) AS INTEGER), \
                 COALESCE(files_scanned,0), COALESCE(files_added,0), COALESCE(files_updated,0), \
                 COALESCE(files_removed,0), COALESCE(files_skipped,0), COALESCE(errors,0) \
                 FROM scan_sessions WHERE lower(replace(scan_path,'\\','/')) = ?1 \
                 OR substr(lower(replace(scan_path,'\\','/')), 1, length(?1) + 1) = ?1 || '/' \
                 ORDER BY id DESC LIMIT 1",
            )
            .and_then(|mut statement| {
                statement
                    .query_row([&key], |row| {
                        let count = |index: usize| -> rusqlite::Result<u64> {
                            Ok(u64::try_from(row.get::<_, i64>(index)?).unwrap_or_default())
                        };
                        let path: String = row.get(1)?;
                        let subpath = folder_key(&path)
                            .get(key.len()..)
                            .map(|rest| rest.trim_start_matches('/').to_string())
                            .unwrap_or_default();
                        // Keep the original case of the directory names.
                        let subpath = path
                            .replace('\\', "/")
                            .get(path.len().saturating_sub(subpath.len())..)
                            .filter(|original| original.eq_ignore_ascii_case(&subpath))
                            .map_or(subpath, str::to_string);
                        Ok(RootLastScan {
                            id: row.get(0)?,
                            subpath,
                            mode: row.get(2)?,
                            state: row.get(3)?,
                            started_at: row.get(4)?,
                            finished_at: row.get(5)?,
                            scanned: count(6)?,
                            added: count(7)?,
                            updated: count(8)?,
                            removed: count(9)?,
                            unchanged: count(10)?,
                            errors: count(11)?,
                        })
                    })
                    .optional()
            })
            .map_err(database)?;
        Ok((u64::try_from(missing).unwrap_or_default(), last))
    }

    /// Blocking (at most about [`INLINE_RECHECK`] plus a library query). `POST
    /// /roots/{name}/check`: recheck root `name` now and return its `/roots` entry. A
    /// check still unresolved after the budget keeps running in the background and the
    /// entry shows the state at that moment.
    pub fn check_root(&self, name: &str) -> Result<RootResponse> {
        let monitor = self.root_monitor();
        let index = monitor
            .roots()
            .iter()
            .position(|root| root.name == name)
            .ok_or_else(|| Error::NotFound(format!("No library root named {name:?}")))?;
        let status = monitor.check_now(index, INLINE_RECHECK);
        self.root_response(&monitor, index, status)
    }

    /// Blocking. `DataProvider::get_audio`; `NotFound` when no such row exists.
    pub fn audio(&self, id: i64) -> Result<AudioFile> {
        self.inner.provider.get_audio(id)
    }

    /// Blocking. Open the file of audio `id` for streaming. The library path is
    /// canonicalized (resolving symlinks and `..`) and must lie under a canonicalized
    /// configured root. Files outside every root, missing files and non-regular files
    /// all get the same `NotFound` message, so the reply never reveals whether an
    /// out-of-root path exists. On Unix the opened handle must also be the same file
    /// (device + inode) as the checked in-root path.
    ///
    /// This is the only request path that touches root directories. There is no time
    /// limit: opening a file on a sleeping NAS disk may take 20-30 s or more. A root
    /// cached as offline gets a bounded inline recheck ([`INLINE_RECHECK`]) and is refused
    /// with [`OpenError::RootOffline`] unless that brings it back. The open registers with
    /// the root monitor while it runs (a hung open turns the root `waking`, then
    /// `offline`); a root already holding 4 x (`max_streams` + `max_previews`) unfinished
    /// opens (opens whose clients gave up but whose blocked system calls have not
    /// returned) is refused too, so stuck threads stay bounded. A failed open whose root turns out to be unreachable
    /// marks the root offline, and a successful open marks it online.
    pub fn open_audio(&self, id: i64) -> std::result::Result<ServedAudio, OpenError> {
        self.open_audio_reporting(id, &OpenProgress::default())
    }

    /// [`AppState::open_audio`], publishing the root and path to `progress` as soon as
    /// the record is read (the stream handler logs them when the open is slow).
    pub fn open_audio_reporting(
        &self,
        id: i64,
        progress: &OpenProgress,
    ) -> std::result::Result<ServedAudio, OpenError> {
        let audio = self.audio(id)?;
        let monitor = self.root_monitor();
        let index = monitor.root_index_for(Path::new(&audio.file_path));
        let root_name = index.map_or_else(String::new, |index| monitor.roots()[index].name.clone());
        progress.set(root_name.clone(), audio.file_path.clone());
        if let Some(index) = index {
            let mut status = monitor.status(index);
            if status.state == RootState::Offline {
                status = monitor.check_now(index, INLINE_RECHECK);
            }
            if status.state == RootState::Offline {
                return Err(OpenError::RootOffline {
                    root: root_name,
                    reason: status.message.unwrap_or_default(),
                });
            }
            let unfinished = monitor.opens_in_progress(index);
            if unfinished
                >= self
                    .inner
                    .open_slots
                    .saturating_mul(UNFINISHED_OPENS_PER_SLOT)
            {
                return Err(OpenError::RootOffline {
                    root: root_name,
                    reason: format!("{unfinished} earlier file opens have not finished yet"),
                });
            }
        }
        let _registration = index.map(|index| monitor.begin_open(index));
        let started = Instant::now();
        let result = self.open_in_root(id, audio, index);
        let elapsed = started.elapsed();
        if elapsed >= SLOW_OPEN_WARNING {
            let (root, path) = progress.target().unwrap_or_default();
            tracing::warn!(
                id,
                root = %root,
                path = %path,
                secs = elapsed.as_secs_f64(),
                ok = result.is_ok(),
                "Stream file open was slow"
            );
        }
        result
    }

    fn open_in_root(
        &self,
        id: i64,
        audio: AudioFile,
        index: Option<usize>,
    ) -> std::result::Result<ServedAudio, OpenError> {
        let monitor = self.root_monitor();
        let unavailable = || Error::NotFound(format!("Audio {id} file is missing on the server"));
        let delay = self.inner.stream_open_delay_ms.load(Ordering::Relaxed);
        if delay > 0 {
            std::thread::sleep(Duration::from_millis(delay));
        }
        let path = match fs::canonicalize(&audio.file_path) {
            Ok(path) => path,
            Err(error) => return Err(self.access_failure(index, error, unavailable())),
        };
        // Candidate roots: the lexically matching one, or every root as a fallback.
        let candidates: Vec<usize> = match index {
            Some(index) => vec![index],
            None => (0..monitor.roots().len()).collect(),
        };
        let mut matched = None;
        for candidate in candidates {
            match fs::canonicalize(&monitor.roots()[candidate].server_path) {
                Ok(root) if path.starts_with(&root) => {
                    matched = Some((candidate, root));
                    break;
                }
                Ok(_) => {}
                Err(error) if index == Some(candidate) => {
                    return Err(self.access_failure(index, error, unavailable()));
                }
                Err(_) => {}
            }
        }
        let Some((root_index, root)) = matched else {
            tracing::warn!(
                id,
                path = %path.display(),
                "Refusing to stream a file outside the configured roots"
            );
            return Err(unavailable().into());
        };
        let checked = match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return Err(unavailable().into()),
            Err(error) => return Err(self.access_failure(Some(root_index), error, unavailable())),
        };
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) => return Err(self.access_failure(Some(root_index), error, unavailable())),
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(error) => return Err(self.access_failure(Some(root_index), error, unavailable())),
        };
        if !metadata.is_file() || !same_file(&checked, &metadata) {
            tracing::warn!(
                id,
                path = %path.display(),
                "Opened file differs from the checked in-root path; refusing to stream"
            );
            return Err(unavailable().into());
        }
        monitor.record(root_index, Ok(root));
        let filename = if audio.filename.is_empty() {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            audio.filename.clone()
        };
        Ok(ServedAudio {
            file,
            etag: crate::stream::file_etag(&metadata),
            path,
            len: metadata.len(),
            filename,
            record: audio,
        })
    }

    /// Classify a failed file access on the stream path: when the root itself is
    /// unreachable the root is marked offline ([`OpenError::RootOffline`]); otherwise a
    /// missing file is `missing` and anything else an I/O error.
    fn access_failure(
        &self,
        index: Option<usize>,
        error: std::io::Error,
        missing: Error,
    ) -> OpenError {
        if let Some(index) = index {
            let monitor = self.root_monitor();
            let probe = probe_root(&monitor.roots()[index].server_path);
            let offline = probe.as_ref().err().cloned();
            monitor.record(index, probe);
            if let Some(reason) = offline {
                return OpenError::RootOffline {
                    root: monitor.roots()[index].name.clone(),
                    reason,
                };
            }
            // The root answers but the file access hit a dropped connection or timeout:
            // retryable, and not a reason to call the file missing or the server broken.
            if is_transient_network_error(&error) {
                return OpenError::RootOffline {
                    root: monitor.roots()[index].name.clone(),
                    reason: format!("Root is not answering reliably: {error}"),
                };
            }
        }
        if is_missing(&error) {
            missing.into()
        } else {
            Error::from(error).into()
        }
    }

    /// Test hook: every snapshot preparation starts after this delay (a huge library).
    #[doc(hidden)]
    pub fn set_snapshot_prepare_delay(&self, delay: Duration) {
        self.inner
            .snapshot_delay_ms
            .store(delay.as_millis() as u64, Ordering::Relaxed);
    }

    /// Test hook: sleep this long inside every list waveform metadata read
    /// ([`AppState::stat_audio`]; simulates a slow network volume).
    #[doc(hidden)]
    pub fn set_stat_delay(&self, delay: Duration) {
        self.inner
            .stat_delay_ms
            .store(delay.as_millis() as u64, Ordering::Relaxed);
    }

    /// Test hook: sleep this long inside every stream open (simulates a sleeping disk).
    #[doc(hidden)]
    pub fn set_stream_open_delay(&self, delay: Duration) {
        self.inner
            .stream_open_delay_ms
            .store(delay.as_millis() as u64, Ordering::Relaxed);
    }

    /// Slot usage of streams, previews, transcodes and waveforms against their limits
    /// (semaphore permits only; for `GET /admin/api/runtime`).
    pub fn slot_usage(&self) -> SlotUsage {
        let used = |slots: &Semaphore, max: usize| SlotCount {
            active: max.saturating_sub(slots.available_permits()),
            max,
        };
        let inner = &self.inner;
        SlotUsage {
            streams: used(&inner.stream_slots, inner.max_streams),
            previews: used(&inner.preview_slots, inner.max_previews),
            transcodes: used(&inner.transcode_slots, inner.max_transcodes),
            waveforms: used(&inner.waveform_slots, inner.max_waveforms),
        }
    }

    /// `(estimated bytes, limit bytes)` of the full waveform cache; the estimate is
    /// `None` until the first periodic trim measured the directory (no filesystem access).
    pub fn waveform_cache_usage(&self) -> (Option<u64>, u64) {
        (
            self.inner.waveforms.estimated_bytes(),
            self.inner.waveforms.limit_bytes(),
        )
    }

    /// In-memory snapshot state: preparations in flight and the cached copy, if any.
    pub fn snapshot_runtime(&self) -> SnapshotRuntime {
        let preparing = {
            let jobs = self
                .inner
                .snapshot_jobs
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let busy = |slot: &Option<SnapshotJob>| {
                slot.as_ref()
                    .and_then(std::sync::Weak::upgrade)
                    .is_some_and(|job| job.finished().is_none())
            };
            [busy(&jobs[0]), busy(&jobs[1])]
        };
        let cached = self
            .inner
            .snapshot_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        SnapshotRuntime {
            preparing: preparing[0],
            preparing_zstd: preparing[1],
            cached: cached.map(|snapshot| CachedSnapshot {
                revision: snapshot.revision,
                schema_version: snapshot.schema_version,
                bytes: snapshot.plain.bytes,
                zstd_bytes: snapshot
                    .zstd
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .map(|file| file.bytes),
            }),
        }
    }

    /// Cached status of every configured root (never touches the filesystem).
    pub fn root_statuses(&self) -> Vec<RootStatus> {
        let monitor = self.root_monitor();
        (0..monitor.roots().len())
            .map(|index| monitor.status(index))
            .collect()
    }

    /// Wait up to `snapshot_wait_secs` for the snapshot (and its zstd copy when `zstd`).
    /// `None` when the preparation takes longer; it then continues in the background and
    /// a later call joins it (single flight per encoding). Must run inside the runtime.
    pub async fn snapshot_within_budget(&self, zstd: bool) -> Option<Result<SnapshotOutput>> {
        let job = self.snapshot_job(zstd);
        let wait = async {
            loop {
                let notified = job.done.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if let Some(result) = job.finished() {
                    return result;
                }
                notified.await;
            }
        };
        let result = tokio::time::timeout(self.inner.snapshot_wait, wait)
            .await
            .ok()?;
        Some(match &*result {
            Ok(output) => Ok(output.clone()),
            Err(error) => Err(clone_error(error)),
        })
    }

    fn snapshot_job(&self, zstd: bool) -> Arc<SnapshotJobState> {
        let mut jobs = self
            .inner
            .snapshot_jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let slot = &mut jobs[usize::from(zstd)];
        if let Some(job) = slot
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .filter(|job| job.finished().is_none())
        {
            return job;
        }
        let job = Arc::new(SnapshotJobState {
            result: Mutex::new(None),
            done: tokio::sync::Notify::new(),
        });
        if self.inner.stopping.load(Ordering::Acquire) {
            *job.result.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(Arc::new(Err(Error::Cancelled)));
            return job;
        }
        let running = job.clone();
        let state = self.clone();
        tokio::task::spawn_blocking(move || {
            let delay = state.inner.snapshot_delay_ms.load(Ordering::Relaxed);
            if delay > 0 {
                std::thread::sleep(Duration::from_millis(delay));
            }
            let result = state
                .with_snapshot_space(state.revision().ok().map(|info| info.revision), || {
                    state.prepare_snapshot()
                })
                .and_then(|prepared| {
                    let file = if zstd {
                        Some(state.with_snapshot_space(Some(prepared.revision), || {
                            prepared.zstd_file_cancellable(&state.inner.stopping)
                        })?)
                    } else {
                        None
                    };
                    Ok((prepared, file))
                });
            if let Err(error) = &result {
                tracing::warn!(%error, "Library snapshot preparation failed");
            }
            *running
                .result
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(result));
            running.done.notify_waiters();
        });
        *slot = Some(Arc::downgrade(&job));
        job
    }

    /// Blocking. The snapshot for the current revision: the cached one when it was built
    /// for exactly this `(library_id, revision)` and its file still exists, otherwise a
    /// fresh copy made with `DataProvider::backup_library` (which then replaces the cache).
    /// Preparation is single-flight: concurrent requests wait for one copy.
    pub fn prepare_snapshot(&self) -> Result<Arc<PreparedSnapshot>> {
        if self.inner.stopping.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let revision = self.poll_revision()?;
        let mut cache = self
            .inner
            .snapshot_cache
            .lock()
            .map_err(|_| Error::Database("Snapshot cache lock was poisoned".into()))?;
        if self.inner.stopping.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        if let Some(entry) = cache.as_ref().filter(|entry| {
            entry.library_id == revision.library_id
                && entry.revision == revision.revision
                && entry.plain.path.is_file()
        }) {
            return Ok(entry.clone());
        }
        // Drop the outdated entry first: its files go once no response streams them.
        *cache = None;
        let started = Instant::now();
        let mut builder = tempfile::Builder::new();
        builder.prefix(SNAPSHOT_PREFIX);
        let directory = match &self.inner.snapshot_dir {
            Some(dir) => {
                fs::create_dir_all(dir)?;
                builder.tempdir_in(dir)?
            }
            None => builder.tempdir()?,
        };
        let owner = snapshot_owner(directory.path())?;
        let path = directory.path().join("library-snapshot.db");
        let report = self
            .inner
            .provider
            .backup_library_cancellable(&path, &self.inner.stopping)?;
        if self.inner.stopping.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| Error::Database(error.to_string()))?;
        let schema_version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|error| Error::Database(error.to_string()))?;
        drop(conn);
        tracing::info!(
            revision = revision.revision,
            bytes = report.bytes,
            millis = started.elapsed().as_millis() as u64,
            "Prepared library snapshot"
        );
        let entry = Arc::new(PreparedSnapshot {
            snapshot_format: inspected_snapshot_format(&path, &self.inner.stopping)?,
            plain: Arc::new(SnapshotFile {
                _owner: owner,
                directory,
                path,
                bytes: report.bytes,
                id: uuid::Uuid::new_v4().to_string(),
            }),
            zstd: Mutex::new(None),
            library_id: revision.library_id,
            revision: revision.revision,
            schema_version,
        });
        *cache = Some(entry.clone());
        Ok(entry)
    }
}

fn inspected_snapshot_format(
    path: &Path,
    cancel: &AtomicBool,
) -> Result<audiolib_core::compatibility::SnapshotFormat> {
    use audiolib_core::compatibility::{LibraryAccess, SnapshotFormat};
    let inspection = audiolib_db::migration::inspect(path, LibraryAccess::Read, false, cancel)?;
    if !inspection.evidence.search_ready {
        return Err(Error::Busy(
            "Library format is not ready for snapshot publication".into(),
        ));
    }
    let id = inspection.format.snapshot_format_id().ok_or_else(|| {
        Error::Validation("Library has no supported completed snapshot format".into())
    })?;
    Ok(SnapshotFormat {
        id,
        format: inspection.format,
    })
}

fn snapshot_owner(directory: &Path) -> Result<fs::File> {
    let owner = fs::File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(directory.join(".owner.lock"))?;
    owner
        .try_lock()
        .map_err(|error| Error::Busy(format!("Snapshot ownership unavailable: {error}")))?;
    Ok(owner)
}

/// Remove leftover `audiolib-snapshot-*` directories (from a crash or kill) in `dir`;
/// with `min_age`, only those whose mtime is at least that old. Only real directories with
/// our prefix are touched (symlinks and files are left alone); failures are logged.
pub fn remove_stale_snapshots(dir: &Path, min_age: Option<Duration>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(SNAPSHOT_PREFIX) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        // A prefix and age are not ownership proof. Legacy unmarked directories are
        // retained; a live instance or response retains its lock even across shutdown.
        let owner_path = path.join(".owner.lock");
        if !fs::symlink_metadata(&owner_path).is_ok_and(|meta| meta.is_file()) {
            continue;
        }
        let Ok(owner) = fs::File::options().read(true).write(true).open(owner_path) else {
            continue;
        };
        if owner.try_lock().is_err() {
            continue;
        }
        if let Some(min_age) = min_age {
            let old_enough = metadata
                .modified()
                .ok()
                .and_then(|time| time.elapsed().ok())
                .is_some_and(|age| age >= min_age);
            if !old_enough {
                continue;
            }
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => tracing::info!(path = %path.display(), "Removed stale snapshot directory"),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "Cannot remove stale snapshot directory")
            }
        }
    }
}

/// Retry `step` while it fails with `StorageFull` and `release` reports freed space;
/// stop with that error as soon as a release frees nothing.
fn retry_on_storage_full<T>(
    mut step: impl FnMut() -> Result<T>,
    mut release: impl FnMut() -> bool,
) -> Result<T> {
    loop {
        match step() {
            Err(Error::Io(error))
                if error.kind() == std::io::ErrorKind::StorageFull && release() =>
            {
                tracing::info!(%error, "Released an idle retained snapshot to free disk space");
            }
            result => return result,
        }
    }
}

/// Spawn the background task that polls for library changes every `every`.
pub fn spawn_revision_poller(state: AppState, every: Duration) -> tokio::task::JoinHandle<()> {
    // Warm the `GET /library` statistics cache in the background right after startup.
    state.refresh_stats_in_background();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            interval.tick().await;
            if state.inner.stopping.load(Ordering::Acquire) {
                return;
            }
            let poll_state = state.clone();
            match tokio::task::spawn_blocking(move || {
                poll_state.inner.snapshot_registry.prune();
                poll_state.poll_revision()
            })
            .await
            {
                Ok(Ok(revision)) => state.refresh_stats_if_stale(&revision),
                Ok(Err(error)) => tracing::warn!(%error, "Library change check failed"),
                Err(error) => tracing::warn!(%error, "Library change check task failed"),
            }
        }
    })
}

/// Whether two metadata records describe the same file: device + inode on Unix. Other
/// platforms have no stable equivalent in std, so only the path check applies there.
#[cfg(unix)]
fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    true
}

fn is_missing(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Normalize like the library's path keys: forward slashes, no trailing slash, lowercase.
pub(crate) fn folder_key(path: &str) -> String {
    let path = path
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .unwrap_or_else(|| path.strip_prefix(r"\\?\").unwrap_or(path).to_string());
    path.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

pub(crate) fn is_ancestor(ancestor: &str, path: &str) -> bool {
    !ancestor.is_empty()
        && path.len() > ancestor.len()
        && path.starts_with(ancestor)
        && path.as_bytes()[ancestor.len()] == b'/'
}

/// Walk `DataProvider::folders` from the top level toward `root` (compared lexically,
/// without filesystem access); `None` when the root is not (yet) a library folder.
fn root_file_count(provider: &dyn DataProvider, root: &Path) -> Result<Option<u64>> {
    let target = folder_key(&root.to_string_lossy());
    let mut parent = None;
    for _ in 0..MAX_FOLDER_DEPTH {
        let mut next = None;
        for folder in provider.folders(parent)? {
            let key = folder_key(&folder.path);
            if key == target {
                return Ok(Some(folder.file_count));
            }
            if is_ancestor(&key, &target) {
                next = Some(folder.id);
                break;
            }
        }
        match next {
            Some(id) => parent = Some(id),
            None => return Ok(None),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_full_releases_idle_snapshots_then_reports_507() {
        let full = || Error::Io(std::io::Error::new(std::io::ErrorKind::StorageFull, "full"));
        let mut idle = 2_u32;
        let mut calls = 0;
        let result: Result<()> = retry_on_storage_full(
            || {
                calls += 1;
                Err(full())
            },
            || {
                let released = idle > 0;
                idle = idle.saturating_sub(1);
                released
            },
        );
        assert!(matches!(result, Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::StorageFull));
        assert_eq!(calls, 3);
        let mut calls = 0;
        let ok = retry_on_storage_full(
            || {
                calls += 1;
                if calls == 1 { Err(full()) } else { Ok(7) }
            },
            || true,
        );
        assert_eq!(ok.unwrap(), 7);
        let other: Result<()> = retry_on_storage_full(
            || Err(Error::Cancelled),
            || panic!("only StorageFull releases snapshots"),
        );
        assert!(matches!(other, Err(Error::Cancelled)));
    }

    #[test]
    fn default_snapshot_storage_is_beside_the_library() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("library.db");
        drop(SqliteProvider::open(&database).unwrap());
        let config =
            ServerConfig::from_toml_str("library_db='library.db'", directory.path()).unwrap();
        assert!(config.snapshot_dir.is_none());
        let state = AppState::open(&config, None).unwrap();
        let snapshot = state.prepare_snapshot().unwrap();
        assert!(
            snapshot
                .plain
                .path
                .starts_with(directory.path().join("snapshots"))
        );
        assert!(snapshot.plain.path.is_file());
        drop(snapshot);
        state.shutdown(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn folder_keys_match_library_normalization() {
        assert_eq!(folder_key(r"\\?\C:\Sounds\SFX\"), "c:/sounds/sfx");
        assert_eq!(folder_key(r"\\?\UNC\nas\share"), "//nas/share");
        assert_eq!(folder_key("/Volumes/Data/SFX/"), "/volumes/data/sfx");
        assert!(is_ancestor("/volumes/data", "/volumes/data/sfx"));
        assert!(!is_ancestor("/volumes/da", "/volumes/data/sfx"));
        assert!(!is_ancestor("", "/volumes"));
    }

    #[cfg(unix)]
    #[test]
    fn same_file_compares_device_and_inode() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"a").unwrap();
        let opened = fs::File::open(&a).unwrap().metadata().unwrap();
        assert!(same_file(&fs::metadata(&a).unwrap(), &opened));
        assert!(!same_file(&fs::metadata(&b).unwrap(), &opened));
        // A symlink resolves to its target, which is the same file.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        assert!(same_file(&fs::metadata(&link).unwrap(), &opened));
    }
}
