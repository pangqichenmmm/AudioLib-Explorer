// crates/audiolib-db/src/lib.rs
//! SQLite library data, lossless legacy import, and bounded background scanning.
#[cfg(test)]
mod folder_rollup_tests;
#[cfg(test)]
mod full_library_validation;
mod id_membership;
mod import_lease;
mod list_waveforms;
pub mod local_play_history;
mod management;
pub mod migration;
mod owned_cancellation;
mod path_identity;
#[cfg(feature = "scanner")]
pub mod scanner;
mod schema;
mod search_bm25;
mod search_cache;
mod search_candidates;
mod search_exclude;
mod search_facets;
mod search_include;
mod search_rank_parallel;
mod search_sorted;
mod search_storage;
mod search_text;
mod search_workspace;
mod space;
pub mod sqlite_engine;
mod sqlite_mmap;
mod wal;
pub use wal::CheckpointStage;
mod waveforms_db;

#[cfg(feature = "scanner")]
use audiolib_core::AudioMetadata;
use audiolib_core::{
    AudioFile, BackupReport, BatchMetadataReport, BatchMetadataRequest, DataProvider, Error,
    FileVisibility, Folder, IntegrityReport, LibraryExportFormat, LibraryInfo, LibraryStats,
    MaintenanceAction, MaintenanceReport, MetadataPatch, RestoreReport, Result, ScanSession,
    SearchPage, SearchRequest, SortOrder, TransferReport,
};
use jieba_rs::Jieba;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, params, params_from_iter, types::Value,
};
#[cfg(feature = "scanner")]
pub use scanner::{ScanCancellation, ScanOptions, ScanService};
/// Current library schema (what `open` migrates a writable library to).
pub use schema::{MIN_SNAPSHOT_SCHEMA_VERSION, SCHEMA_VERSION};
/// Oldest schema of a server snapshot this client opens read-only (single definition lives
/// in `schema.rs`).
pub const MIN_SNAPSHOT_SCHEMA: i64 = schema::MIN_SNAPSHOT_SCHEMA_VERSION;
/// Newest schema of a snapshot this client opens read-only. Older supported schemas
/// use an owned copy when their derived search state needs upgrading; the source is preserved.
pub const MAX_SNAPSHOT_SCHEMA: i64 = schema::SCHEMA_VERSION;
const _: () = assert!(MIN_SNAPSHOT_SCHEMA <= MAX_SNAPSHOT_SCHEMA);

/// Register the exact shared scalar functions required by library indexes/triggers.
///
/// Owned import/validation tools must configure each raw SQLite connection before
/// executing statements that invoke these functions. This does not acquire a
/// library lease or authorize writes; application business writes use DataProvider.
/// Registration itself changes neither database contents nor schema.
pub fn register_library_functions(conn: &Connection) -> Result<()> {
    schema::register_tokenizer(conn, load_tokenizer())
}

pub use search_rank_parallel::{
    ParallelRankingDiagnostics, SQLITE_AUXILIARY_READER_CACHE_KB, auxiliary_search_reader_count,
    parallel_ranking_diagnostics,
};
pub use sqlite_mmap::{SQLITE_READER_MMAP_BYTES, sqlite_reader_mmap_bytes};

/// Maximum auxiliary readers used by one exact ranking job. The provider admits
/// one such job at a time; the original reader forms the eighth partition.
pub const SQLITE_AUXILIARY_SEARCH_READERS: usize = 7;

/// Upgrade work. Live-library row counts are committed resume points; owned temporary
/// snapshot upgrades retain their atomic transaction and discard-on-cancel contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum SnapshotUpgradeProgress {
    /// Validate the implementation revisions and prepare required schema indexes.
    Preparing,
    /// Save and validate the original library before changing its derived search data.
    BackingUp,
    /// Business rows durably copied into the owned candidate.
    Copying {
        /// Completed audio rows in the candidate.
        completed: u64,
        /// Total source audio rows.
        total: u64,
    },
    /// Merge candidate FTS segments without changing business identities.
    Compacting,
    /// Atomically publish the fully validated candidate.
    Publishing,
    /// Recompute derived search text for this many of the snapshot's audio records.
    Tokenizing {
        /// Audio records whose derived text has been recomputed.
        completed: u64,
        /// Total audio records in the transaction's pinned snapshot.
        total: u64,
    },
    /// Build the FTS5 postings; SQLite does not expose a reliable row percentage.
    BuildingFts,
    /// Committed FTS5 insertion progress in a resumable live-library upgrade.
    Indexing {
        /// Indexed records committed so far.
        completed: u64,
        /// Total records to index.
        total: u64,
    },
    /// Check the completed search index before publishing it.
    Validating,
    /// Move committed WAL pages into the library and truncate the WAL.
    Checkpoint,
    /// Complete any remaining resumable extracted-tag backfill.
    SourceTags,
}

/// Whether a snapshot of this `user_version` can be opened read-only by this client.
pub fn snapshot_schema_supported(version: i64) -> bool {
    (MIN_SNAPSHOT_SCHEMA..=MAX_SNAPSHOT_SCHEMA).contains(&version)
}
use serde::{Deserialize, Serialize};
pub use space::ensure_free_space;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
pub use waveforms_db::{
    LIST_WAVEFORMS_COLUMNS, META_COLUMNS, WAVEFORMS_DB_FILE, WaveformsDb, WaveformsDbRow,
    validate_bundle,
};

pub(crate) fn db_error(error: rusqlite::Error) -> Error {
    if matches!(&error, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::OperationInterrupted)
    {
        Error::Cancelled
    } else {
        Error::Database(error.to_string())
    }
}
fn unsigned(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(row.get::<_, i64>(index)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

pub(crate) fn path_key(value: &str) -> String {
    let value = value
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .unwrap_or_else(|| value.strip_prefix(r"\\?\").unwrap_or(value).to_string());
    value
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_lowercase()
}

/// Compare stored path identities without accessing the source filesystem.
/// Windows drive, UNC and separator syntax is normalized; directory-entry case
/// and literal POSIX backslashes remain significant on every reader platform.
/// Resolving an old alias against library rows uses DataProvider path lookup instead.
pub fn indexed_paths_equal(left: &str, right: &str) -> bool {
    path_identity::comparison_key(left) == path_identity::comparison_key(right)
}

/// The text the library stores in `audio_files.file_path` for `path` (as given, normally
/// canonical): the Windows verbatim prefix is dropped (`\\?\C:\x` -> `C:\x`,
/// `\\?\UNC\host\share` -> `\\host\share`); separators are kept as they are.
pub fn display_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    value
        .strip_prefix(r"\\?\UNC\")
        .map(|rest| format!(r"\\{rest}"))
        .unwrap_or_else(|| value.strip_prefix(r"\\?\").unwrap_or(&value).to_string())
}

/// The text the library stores in `folders.folder_path` for the directory `path`:
/// [`display_path`] with every `\\` turned into `/` (so on Windows `C:/Sounds/SFX`).
pub fn folder_path_text(path: &Path) -> String {
    let text = display_path(path);
    if cfg!(windows) || path_identity::windows_path(&text) {
        text.replace('\\', "/")
    } else {
        text
    }
}

pub(crate) fn tokenize(jieba: &Jieba, value: &str) -> String {
    search_text::index_text(jieba, value)
}

// A successful WAV header probe is authoritative for these supported fields.
// Arbitrary plugin/library attributes remain independent of source chunk presence.
#[cfg(feature = "scanner")]
const HEADER_ATTRIBUTES: &[&str] = &[
    "wav_metadata",
    "bwf_description",
    "bwf_originator",
    "bwf_originator_reference",
    "bwf_origination_date",
    "bwf_origination_time",
    "bwf_time_reference",
    "bwf_version",
    "bwf_umid",
    "bwf_coding_history",
    "ixml_scene",
    "ixml_take",
    "ixml_tape",
    "ixml_circled",
    "ixml_wild_track",
    "ixml_no_good",
    "ixml_raw",
    "channel_mask",
    "format_tag",
    "valid_bits_per_sample",
];

/// One writer and independent read snapshots keep owned results independent of sessions.
pub struct SqliteProvider {
    path: PathBuf,
    read_only: bool,
    writer: Mutex<Connection>,
    read_pool: Mutex<Vec<Connection>>,
    revision_reader: Mutex<Option<Connection>>,
    ranking_cache: Mutex<search_cache::RankingCache>,
    document_lengths: Mutex<Option<(u64, Arc<search_bm25::Lengths>)>>,
    parallel_rank_reserved: AtomicBool,
    tokenizer: Arc<Jieba>,
    read_gate: RwLock<()>,
    /// Whether extracted-tag filters can use the fully populated derived column.
    derived_tags: AtomicBool,
    /// Session state of local list Waveforms (computed file versions, decode failures).
    list_waveform_memory: list_waveforms::ListWaveformMemory,
    // Declared last so every SQLite connection closes before the owned directory is removed.
    _owned_snapshot_directory: Option<tempfile::TempDir>,
}

/// Keep cross-process write permission alive until the SQLite transaction/guard ends.
struct WriteLease<'a> {
    conn: MutexGuard<'a, Connection>,
    _isolation: management::AdministrationLease,
}
impl std::ops::Deref for WriteLease<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn
    }
}
impl std::ops::DerefMut for WriteLease<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }
}

struct ReadLease<'a> {
    conn: Option<Connection>,
    pool: &'a Mutex<Vec<Connection>>,
    _gate: RwLockReadGuard<'a, ()>,
    _upgrade: Option<management::SearchUpgradeLease>,
}
impl std::ops::Deref for ReadLease<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn.as_ref().expect("Live read connection")
    }
}
impl std::ops::DerefMut for ReadLease<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.conn.as_mut().expect("Live read connection")
    }
}
impl Drop for ReadLease<'_> {
    fn drop(&mut self) {
        // A cancelled search must never poison a later user of this pooled reader.
        if let Some(conn) = self.conn.as_ref()
            && conn.progress_handler(0, None::<fn() -> bool>).is_err()
        {
            self.conn.take();
        }
        if let (Some(conn), Ok(mut pool)) = (self.conn.take(), self.pool.lock())
            && pool.len() < 4
        {
            pool.push(conn);
        }
    }
}

fn load_tokenizer() -> Arc<Jieba> {
    let mut jieba = Jieba::new();
    for line in include_str!("../../../resources/audio_terms.txt")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let parts: Vec<_> = line.split_whitespace().collect();
        if let Some(index) = parts.iter().rposition(|part| part.parse::<usize>().is_ok()) {
            jieba.add_word(
                &parts[..index].join(" "),
                parts[index].parse().ok(),
                parts.get(index + 1).copied(),
            );
        } else {
            jieba.add_word(line, None, None);
        }
    }
    Arc::new(jieba)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationReport {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub schema_version: i64,
    pub audio_files: u64,
    pub source_preserved: bool,
}

/// Snapshot a read-only legacy source through SQLite's backup API, including committed WAL.
/// Existing destinations are never replaced. Original library tables and IDs remain intact.
pub fn import_legacy(source: &Path, destination: &Path) -> Result<MigrationReport> {
    import_legacy_with_history(source, destination, None)
}

/// Client-local import of a read-only old library. A reviewed old-format copy is
/// published first, then its durable final-path job backs up/imports local history
/// before retirement. A later failure leaves that owned target/job recoverable;
/// another source cannot replace it. The original source always remains intact.
pub fn import_legacy_local(
    source: &Path,
    destination: &Path,
    data_root: &Path,
    history_limit: u32,
) -> Result<MigrationReport> {
    import_legacy_with_history(source, destination, Some((data_root, history_limit)))
}

fn import_legacy_with_history(
    source: &Path,
    destination: &Path,
    history: Option<(&Path, u32)>,
) -> Result<MigrationReport> {
    if !source.is_file() {
        return Err(Error::NotFound(source.display().to_string()));
    }
    if destination.exists() {
        return Err(Error::Validation(
            "Import destination already exists".into(),
        ));
    }
    if let Some(parent) = destination.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let destination_lease = import_lease::ImportDestinationLease::acquire(destination)?;
    let destination = destination_lease.destination();
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let owned = tempfile::Builder::new()
        .prefix("audiolib-import-owned-")
        .tempdir_in(parent)?;
    let staging = tempfile::Builder::new()
        .prefix("audiolib-import-")
        .suffix(".db")
        .tempfile_in(owned.path())?;
    let _stage_sidecar = management::OwnedAdministrationSidecar::new(staging.path())?;
    let src =
        Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(db_error)?;
    schema::validate_library_identity(&src)?;
    src.busy_timeout(Duration::from_secs(5)).map_err(db_error)?;
    {
        let mut dst = Connection::open(staging.path()).map_err(db_error)?;
        let backup = rusqlite::backup::Backup::new(&src, &mut dst).map_err(db_error)?;
        backup
            .run_to_completion(256, Duration::from_millis(10), None)
            .map_err(db_error)?;
        drop(backup);
        let version: i64 = dst
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(db_error)?;
        if version < schema::MIN_SNAPSHOT_SCHEMA_VERSION {
            schema::register_tokenizer(&dst, load_tokenizer())?;
            schema::migrate(&mut dst)?;
            schema::backfill_source_tags(&mut dst)?;
        }
    }
    if let Some((root, limit)) = history {
        let copy = Connection::open_with_flags(staging.path(), OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(db_error)?;
        let mode: String = copy
            .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
            .map_err(db_error)?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err(Error::Validation(
                "Owned import copy must be standalone before publication".into(),
            ));
        }
        drop(copy);
        let cancel = AtomicBool::new(false);
        let diagnosis = migration::diagnose(staging.path(), &cancel)?;
        if diagnosis.decision.disposition
            != Some(audiolib_core::compatibility::MigrationDisposition::Compatible)
            || diagnosis.decision.execution_state
                != audiolib_core::compatibility::ExecutionState::Ready
            || !diagnosis.findings.is_empty()
        {
            // This read-only preflight rejects directly known dependency failures
            // before either the final path or client history is touched. Legacy
            // normalization stays in the shared durable engine, including its
            // business-preservation checks and any deferred dependency diagnosis.
            migration::plan(staging.path(), &cancel)?;
        }
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        destination_lease.write_local_intent(&root, limit)?;
        staging.as_file().sync_all()?;
        staging
            .persist_noclobber(destination)
            .map_err(|error| Error::Io(error.error))?;
        SqliteProvider::validate_existing_for_open(destination)?;
        // The explicit owner is checked against this real final file. Public
        // opens cannot bypass the shared lease while this import still owns it.
        let provider = SqliteProvider::open_with_history_owner_cancellable(
            destination,
            Some((&root, limit)),
            &cancel,
            |_| {},
            Some(&destination_lease),
        )?;
        provider.check_integrity()?;
        let integrity = provider.check_library()?;
        if !integrity.is_ok() {
            return Err(Error::Database(integrity.errors.join("; ")));
        }
        let report = MigrationReport {
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            schema_version: schema::SCHEMA_VERSION,
            audio_files: provider
                .reader()?
                .query_row("SELECT COUNT(*) FROM audio_files", [], |row| {
                    unsigned(row, 0)
                })
                .map_err(db_error)?,
            source_preserved: true,
        };
        provider.checkpoint_wal()?;
        return Ok(report);
    }
    let provider = SqliteProvider::open(staging.path())?;
    provider.check_integrity()?;
    let integrity = provider.check_library()?;
    if !integrity.is_ok() {
        return Err(Error::Database(integrity.errors.join("; ")));
    }
    let report = MigrationReport {
        source: source.to_path_buf(),
        destination: destination.to_path_buf(),
        schema_version: schema::SCHEMA_VERSION,
        audio_files: provider
            .reader()?
            .query_row("SELECT COUNT(*) FROM audio_files", [], |row| {
                unsigned(row, 0)
            })
            .map_err(db_error)?,
        source_preserved: true,
    };
    provider
        .writer()?
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .map_err(db_error)?;
    drop(provider);
    staging.as_file().sync_all()?;
    staging
        .persist_noclobber(destination)
        .map_err(|error| Error::Io(error.error))?;
    Ok(report)
}

impl SqliteProvider {
    /// `DataProvider::library_info` with the given statistics instead of computing them
    /// (a server caches the expensive full-table aggregates, see `audiolib-server`).
    pub fn library_info_with_stats(
        &self,
        stats: audiolib_core::LibraryStats,
    ) -> Result<audiolib_core::LibraryInfo> {
        self.management_info_with(stats)
    }

    /// Open a Rust library database and apply transactional legacy-compatible migrations.
    /// Call import_legacy for the original Python database to preserve that source unchanged.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_progress(path, |_| {})
    }

    /// Open a writable library, synchronously reporting migration phases and rows.
    /// Existing libraries commit resumable batches; a failed upgrade remains unavailable
    /// until a later open resumes it. Its rollback backup is retained beside the library.
    pub fn open_with_progress(
        path: &Path,
        progress: impl FnMut(SnapshotUpgradeProgress),
    ) -> Result<Self> {
        Self::open_with_progress_cancellable(path, &AtomicBool::new(false), progress)
    }

    /// Like `open_with_progress`, stopping at a durable batch boundary when cancelled.
    /// A later writable open resumes the persisted upgrade; source records are retained.
    pub fn open_with_progress_cancellable(
        path: &Path,
        cancel: &AtomicBool,
        progress: impl FnMut(SnapshotUpgradeProgress),
    ) -> Result<Self> {
        Self::open_with_history_cancellable(path, None, cancel, progress)
    }

    /// Explicit client-local create/open. Legacy history is imported from the
    /// verified rollback backup before the shared migration changes the library.
    /// Server authorities and remote snapshots must use their existing entry points.
    pub fn open_local_with_progress_cancellable(
        path: &Path,
        data_root: &Path,
        history_limit: u32,
        cancel: &AtomicBool,
        progress: impl FnMut(SnapshotUpgradeProgress),
    ) -> Result<Self> {
        Self::open_with_history_cancellable(
            path,
            Some((data_root, history_limit)),
            cancel,
            progress,
        )
    }

    fn open_with_history_cancellable(
        path: &Path,
        history: Option<(&Path, u32)>,
        cancel: &AtomicBool,
        progress: impl FnMut(SnapshotUpgradeProgress),
    ) -> Result<Self> {
        Self::open_with_history_owner_cancellable(path, history, cancel, progress, None)
    }

    fn open_with_history_owner_cancellable(
        path: &Path,
        history: Option<(&Path, u32)>,
        cancel: &AtomicBool,
        mut progress: impl FnMut(SnapshotUpgradeProgress),
        import_owner: Option<&import_lease::ImportDestinationLease>,
    ) -> Result<Self> {
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        if path.as_os_str().is_empty() || path == Path::new(":memory:") {
            return Err(Error::Validation(
                "Provider requires a persistent database path".into(),
            ));
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let (open_path, creation_lease, existing_import_lease) = if let Some(owner) = import_owner {
            let canonical = path.canonicalize()?;
            if canonical != owner.destination() {
                return Err(Error::Validation(
                    "Import lease belongs to a different library destination".into(),
                ));
            }
            (canonical, None, None)
        } else if path.exists() {
            let lease = import_lease::ExistingImportOpenLease::acquire(path)?;
            (lease.destination().to_path_buf(), None, Some(lease))
        } else {
            // The exclusive lease rechecks the path after acquisition. It may
            // already exist if another creator finished after our first probe.
            let lease = import_lease::ImportDestinationLease::acquire_for_create(path)?;
            (lease.destination().to_path_buf(), Some(lease), None)
        };
        let path = open_path.as_path();
        // The same persistent inode carries a local owner receipt written before
        // publication. A process crash before job registration cannot erase it.
        let import_intent = if let Some(owner) = import_owner {
            owner.authorize_history(history)?
        } else if let Some(lease) = &creation_lease {
            lease.authorize_history(history)?
        } else if let Some(lease) = &existing_import_lease {
            lease.authorize_history(history)?
        } else {
            unreachable!("Every writable open must own an import lease")
        };
        let history = history.map(|(root, limit)| {
            (
                root,
                import_intent
                    .as_ref()
                    .map_or(limit, |intent| intent.limit()),
            )
        });
        // Establish a new empty file only where the caller explicitly permits create.
        // Existing-library CLI paths validate existence before reaching this method.
        let flags = if creation_lease.is_some() {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        drop(Connection::open_with_flags(path, flags).map_err(db_error)?);
        progress(SnapshotUpgradeProgress::Preparing);
        match history {
            Some((root, limit)) => {
                migration::prepare_local_writable(path, root, limit, cancel, &mut progress)?
            }
            None => migration::prepare_writable(path, cancel, &mut progress)?,
        }
        let _isolation = management::AdministrationLease::shared(path)?;
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(db_error)?;
        // An administrator may replace a file between preparation and lease acquisition.
        if !schema::current_format_ready(&conn)? {
            return Err(Error::Busy(
                "Library changed during startup; retry the shared migration check".into(),
            ));
        }
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
            PRAGMA foreign_keys=ON; PRAGMA cache_size=-65536;
            PRAGMA temp_store=MEMORY;",
        )
        .map_err(db_error)?;
        conn.pragma_update(None, "mmap_size", 0).map_err(db_error)?;
        let tokenizer = load_tokenizer();
        schema::register_tokenizer(&conn, tokenizer.clone())?;
        let derived = schema::source_tags_ready(&conn)?;
        let provider = Self {
            path: path.to_path_buf(),
            read_only: false,
            writer: Mutex::new(conn),
            read_pool: Mutex::new(Vec::new()),
            revision_reader: Mutex::new(None),
            ranking_cache: Mutex::new(Default::default()),
            document_lengths: Mutex::new(None),
            parallel_rank_reserved: AtomicBool::new(false),
            tokenizer,
            read_gate: RwLock::new(()),
            derived_tags: AtomicBool::new(derived),
            list_waveform_memory: Default::default(),
            _owned_snapshot_directory: None,
        };
        // Only a completed local/p1 provider can retire the durable import owner.
        // Keep its lock inode; contention during shared-to-exclusive cleanup
        // leaves the receipt available for the next matching local open.
        if let Some(intent) = &import_intent {
            if let Some(owner) = import_owner {
                owner.clear_completed_intent(intent)?;
            } else if let Some(lease) = &creation_lease {
                lease.clear_completed_intent(intent)?;
            } else if let Some(lease) = &existing_import_lease {
                lease.try_clear_completed_intent(intent)?;
            }
        }
        Ok(provider)
    }

    /// Upgrade older source-keyword text through checked maintenance. Idempotent;
    /// scans or another administrator can return Busy without discarding the library.
    pub fn ensure_source_search_current(&self) -> Result<bool> {
        if self.read_only {
            return Ok(false);
        }
        {
            let writer = self.writer_unlocked()?;
            if schema::search_text_is_current(&writer)? {
                return Ok(false);
            }
        }
        self.management_maintain(MaintenanceAction::RebuildSearch)?;
        self.derived_tags.store(true, Ordering::Release);
        Ok(true)
    }

    /// Resume a partial extracted-tag backfill under the ordinary administration
    /// lease, excluding restore/maintenance while preserving batched progress.
    pub fn ensure_source_tags_current(&self) -> Result<u64> {
        if self.read_only {
            return Ok(0);
        }
        let mut writer = self.writer()?;
        let filled = schema::backfill_source_tags(&mut writer)?;
        let ready = schema::source_tags_ready(&writer)?;
        self.derived_tags.store(ready, Ordering::Release);
        Ok(filled)
    }

    /// File backing this provider; older read-only inputs may use an owned upgraded copy.
    pub fn database_path(&self) -> &Path {
        &self.path
    }

    /// Read the derived search-text revision without writing or creating a database.
    /// Schema-v3 snapshots remain readable even when this returns `false`.
    pub fn has_current_search_text(path: &Path) -> Result<bool> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(db_error)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        schema::validate_library_identity(&conn)?;
        schema::search_text_is_current(&conn)
    }

    /// Probe the derived keyword, directory and visibility-search state without modifying the
    /// source. Snapshot owners must upgrade it before opening a live read-only view.
    pub fn has_current_search_state(path: &Path) -> Result<bool> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(db_error)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        schema::validate_library_identity(&conn)?;
        schema::search_state_is_current(&conn)
    }

    /// Upgrade derived implementation state in a snapshot the caller owns exclusively.
    /// The caller must validate its original identity/integrity before calling this;
    /// live libraries use `open_existing` and its cross-instance administration lease.
    /// This method never creates a missing file and keeps rollback-journal mode, so a
    /// downloaded staging file can be atomically published without WAL/SHM dependencies.
    pub fn upgrade_owned_snapshot(path: &Path) -> Result<bool> {
        Self::upgrade_owned_snapshot_cancellable(path, &AtomicBool::new(false))
    }

    /// Upgrade only an exclusively owned snapshot, interrupting SQLite work on cancellation.
    /// The caller removes the cancelled staging copy; live libraries must use their
    /// administration workflow and must never be passed to this operation.
    pub fn upgrade_owned_snapshot_cancellable(path: &Path, cancel: &AtomicBool) -> Result<bool> {
        Self::upgrade_owned_snapshot_with_progress_cancellable(path, cancel, |_| {})
    }

    /// Upgrade an exclusively owned snapshot with synchronous phase and row progress.
    /// Uses the shared durable migration engine inside the caller's owned directory.
    /// Callers may discard that entire directory on cancellation or retain it to resume.
    /// Callbacks must return promptly and must not access the upgrading database.
    pub fn upgrade_owned_snapshot_with_progress_cancellable(
        path: &Path,
        cancel: &AtomicBool,
        mut progress: impl FnMut(SnapshotUpgradeProgress),
    ) -> Result<bool> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(db_error)?;
        schema::validate_library_identity(&conn)?;
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(db_error)?;
        if version > MAX_SNAPSHOT_SCHEMA {
            return Err(Error::NewerDatabase {
                what: "schema".into(),
                found: version,
                supported: MAX_SNAPSHOT_SCHEMA,
            });
        }
        if !snapshot_schema_supported(version) {
            return Err(Error::Database(format!(
                "Snapshot requires schema {MIN_SNAPSHOT_SCHEMA}..={MAX_SNAPSHOT_SCHEMA}, found {version}"
            )));
        }
        if version == schema::SCHEMA_VERSION && schema::current_format_ready(&conn)? {
            return Ok(false);
        }
        drop(conn);
        migration::prepare_writable(path, cancel, &mut progress)?;
        Ok(true)
    }

    /// `PRAGMA data_version` of this provider's writer connection. SQLite changes it only
    /// when another connection commits, never for this provider's own writes, so a
    /// caller can tell whether anyone else (normally another process) wrote meanwhile.
    pub fn foreign_data_version(&self) -> Result<i64> {
        self.writer_unlocked()?
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .map_err(db_error)
    }

    /// Fails with `Busy` while any process holds this library's scan lease (a running or
    /// paused scan, or a restore, maintenance, relink, batch edit or record removal in
    /// progress). The lease is only probed and released at once, never kept, so callers
    /// can refuse an operation early with the same error the operation itself would give.
    pub fn ensure_library_idle(&self) -> Result<()> {
        management::LibraryLease::acquire(&self.path).map(drop)
    }

    /// Open a known Rust library; original Python or arbitrary SQLite files must be imported.
    pub fn open_existing(path: &Path) -> Result<Self> {
        Self::open_existing_with_progress(path, |_| {})
    }

    /// Validate an existing Rust library before opening and reporting its upgrade.
    /// Missing or unrelated databases are never initialized by this entry point.
    pub fn open_existing_with_progress(
        path: &Path,
        progress: impl FnMut(SnapshotUpgradeProgress),
    ) -> Result<Self> {
        Self::open_existing_with_progress_cancellable(path, &AtomicBool::new(false), progress)
    }

    /// Validate an existing library and resume its upgrade with batch-boundary cancellation.
    pub fn open_existing_with_progress_cancellable(
        path: &Path,
        cancel: &AtomicBool,
        progress: impl FnMut(SnapshotUpgradeProgress),
    ) -> Result<Self> {
        Self::validate_existing_for_open(path)?;
        Self::open_with_progress_cancellable(path, cancel, progress)
    }

    /// Open an existing local client library, importing legacy local history
    /// before retirement. Missing/unrelated sources are rejected without creating files.
    pub fn open_existing_local_with_progress_cancellable(
        path: &Path,
        data_root: &Path,
        history_limit: u32,
        cancel: &AtomicBool,
        progress: impl FnMut(SnapshotUpgradeProgress),
    ) -> Result<Self> {
        Self::validate_existing_for_open(path)?;
        Self::open_local_with_progress_cancellable(path, data_root, history_limit, cancel, progress)
    }

    fn validate_existing_for_open(path: &Path) -> Result<()> {
        if !path.is_file() {
            return Err(Error::NotFound(path.display().to_string()));
        }
        let probe = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(db_error)?;
        let version: i64 = probe
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(db_error)?;
        schema::validate_library_identity(&probe)?;
        if version > schema::SCHEMA_VERSION {
            return Err(Error::NewerDatabase {
                what: "schema".into(),
                found: version,
                supported: schema::SCHEMA_VERSION,
            });
        }
        if !(1..=schema::SCHEMA_VERSION).contains(&version) {
            return Err(Error::Validation(format!(
                "Use Import to create a copy first; supported Rust schema is 1..={}, found {version}",
                schema::SCHEMA_VERSION
            )));
        }
        Ok(())
    }

    /// Open a schema-current library without writing its source file. Older derived
    /// search text or a missing required search index is upgraded in an owned temporary
    /// snapshot retained by the provider.
    /// `database_path` then names that copy; reopen to observe later source commits.
    /// Current search revisions keep their original path and ordinary reader behavior.
    pub fn open_read_only(path: &Path) -> Result<Self> {
        let source = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(db_error)?;
        schema::ensure_ready(&source)?;
        source
            .busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        let version: i64 = source
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(db_error)?;
        if version > MAX_SNAPSHOT_SCHEMA {
            return Err(Error::NewerDatabase {
                what: "schema".into(),
                found: version,
                supported: MAX_SNAPSHOT_SCHEMA,
            });
        }
        if !snapshot_schema_supported(version) {
            return Err(Error::Database(format!(
                "Read-only database requires schema {MIN_SNAPSHOT_SCHEMA}..={MAX_SNAPSHOT_SCHEMA}, found {version}"
            )));
        }
        let (conn, actual_path, owned_directory) = if schema::search_state_is_current(&source)? {
            (source, path.to_path_buf(), None)
        } else {
            schema::validate_library_identity(&source)?;
            let directory = tempfile::Builder::new()
                .prefix("audiolib-readonly-")
                .tempdir()?;
            let copy_path = directory.path().join("library.db");
            {
                let mut copy = Connection::open(&copy_path).map_err(db_error)?;
                // Pin one consistent snapshot before copying, including committed WAL data.
                // Large backup steps need no inter-step sleep while this snapshot is pinned.
                source
                    .execute_batch("BEGIN; SELECT COUNT(*) FROM sqlite_schema;")
                    .map_err(db_error)?;
                rusqlite::backup::Backup::new(&source, &mut copy)
                    .map_err(db_error)?
                    .run_to_completion(4096, Duration::ZERO, None)
                    .map_err(db_error)?;
            }
            drop(source);
            let report = Self::check_owned_file(&copy_path)?;
            if !report.is_ok() {
                return Err(Error::Database(format!(
                    "Read-only source integrity check failed: {}",
                    serde_json::to_string(&report).unwrap_or_default()
                )));
            }
            Self::upgrade_owned_snapshot(&copy_path)?;
            let report = Self::check_owned_file(&copy_path)?;
            if !report.is_ok() {
                return Err(Error::Database(
                    "Upgraded read-only snapshot failed its integrity check".into(),
                ));
            }
            let conn = Connection::open_with_flags(&copy_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(db_error)?;
            (conn, copy_path, Some(directory))
        };
        let derived = schema::source_tags_ready(&conn)?;
        Ok(Self {
            path: actual_path,
            read_only: true,
            writer: Mutex::new(conn),
            read_pool: Mutex::new(Vec::new()),
            revision_reader: Mutex::new(None),
            ranking_cache: Mutex::new(Default::default()),
            document_lengths: Mutex::new(None),
            parallel_rank_reserved: AtomicBool::new(false),
            tokenizer: load_tokenizer(),
            read_gate: RwLock::new(()),
            derived_tags: AtomicBool::new(derived),
            list_waveform_memory: Default::default(),
            _owned_snapshot_directory: owned_directory,
        })
    }

    /// Server mode: write the annotation values of records returned by the server (after a
    /// metadata write) into a downloaded snapshot, so the read-only
    /// provider that has it open sees them without a new download. Keeps the snapshot's
    /// rollback-journal mode (no WAL, no migrations) and refreshes the tokenized FTS
    /// columns exactly like `update_metadata`. One transaction: an id missing from the
    /// snapshot rolls everything back and returns `NotFound` (the snapshot diverged).
    /// Missing or unsupported authoritative `attributes.modified_at` returns
    /// `Validation`, requiring a full snapshot instead of advancing stale sort state.
    pub fn apply_snapshot_records(path: &Path, files: &[AudioFile]) -> Result<()> {
        static TOKENIZER: std::sync::OnceLock<Arc<Jieba>> = std::sync::OnceLock::new();
        let tokenizer = TOKENIZER.get_or_init(load_tokenizer);
        let _administration = management::AdministrationLease::shared(path)?;
        let _upgrade = management::SearchUpgradeLease::acquire(path, false)?;
        let mut conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(db_error)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(db_error)?;
        if version > MAX_SNAPSHOT_SCHEMA {
            return Err(Error::NewerDatabase {
                what: "schema".into(),
                found: version,
                supported: MAX_SNAPSHOT_SCHEMA,
            });
        }
        if !snapshot_schema_supported(version) {
            return Err(Error::Database(format!(
                "Snapshot requires schema {MIN_SNAPSHOT_SCHEMA}..={MAX_SNAPSHOT_SCHEMA}, found {version}"
            )));
        }
        // The probe rejects all known newer feature revisions before any write.
        // An older search revision remains compatible with annotation patching.
        let _current_search_text = schema::search_text_is_current(&conn)?;
        schema::ensure_ready(&conn)?;
        schema::validate_library_identity(&conn)?;
        schema::register_tokenizer(&conn, tokenizer.clone())?;
        if version == schema::SCHEMA_VERSION && !schema::readable_format_ready(&conn)? {
            return Err(Error::Validation(
                "Snapshot format is not ready for incremental Metadata".into(),
            ));
        }
        // A reply without its authoritative sort timestamp cannot safely advance
        // the cache revision. Older/incomplete DTOs take the caller's full-sync path.
        // Check library compatibility first so future formats retain their diagnosis.
        let modified = files
            .iter()
            .map(snapshot_modified_at)
            .collect::<Result<Vec<_>>>()?;
        let tx = conn.transaction().map_err(db_error)?;
        {
            let mut update = tx
                .prepare(
                    if version==schema::SCHEMA_VERSION {
                        "UPDATE audio_files SET user_tags=?1,user_notes=?2,favorite=?3,user_color=?4,modified_at=?5 WHERE id=?6"
                    } else {"UPDATE audio_files SET user_tags=?1,user_tags_tokenized=?2,user_notes=?3,
                    user_notes_tokenized=?4,favorite=?5,user_color=?6,modified_at=?7
                    WHERE id=?8"},
                )
                .map_err(db_error)?;
            for (file, modified_at) in files.iter().zip(modified) {
                let changed = if version == schema::SCHEMA_VERSION {
                    update.execute(params![
                        file.tags,
                        file.notes,
                        i64::from(file.favorite),
                        file.color.as_deref().filter(|color| !color.is_empty()),
                        modified_at,
                        file.id
                    ])
                } else {
                    update.execute(params![
                        file.tags,
                        tokenize(tokenizer, &file.tags),
                        file.notes,
                        tokenize(tokenizer, &file.notes),
                        i64::from(file.favorite),
                        file.color.as_deref().filter(|color| !color.is_empty()),
                        modified_at,
                        file.id,
                    ])
                }
                .map_err(db_error)?;
                if changed == 0 {
                    return Err(Error::NotFound(format!("Audio {}", file.id)));
                }
            }
        }
        tx.commit().map_err(db_error)
    }

    fn writer(&self) -> Result<WriteLease<'_>> {
        if self.read_only {
            return Err(Error::Validation("Library is read-only".into()));
        }
        let isolation = management::AdministrationLease::shared(&self.path)?;
        let conn = self.writer_unlocked()?;
        schema::ensure_ready(&conn)?;
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(db_error)?;
        if version != schema::SCHEMA_VERSION {
            return Err(Error::Busy(
                "Library format changed; reopen before writing".into(),
            ));
        }
        Ok(WriteLease {
            conn,
            _isolation: isolation,
        })
    }

    // Only administration already holding the exclusive OS lease may bypass writer().
    // require_writable uses this solely to inspect the connection's open flags.
    fn writer_unlocked(&self) -> Result<MutexGuard<'_, Connection>> {
        self.writer
            .lock()
            .map_err(|_| Error::Database("Database writer lock was poisoned".into()))
    }

    fn reader(&self) -> Result<ReadLease<'_>> {
        let upgrade = if self.read_only {
            None
        } else {
            Some(management::SearchUpgradeLease::acquire(&self.path, false)?)
        };
        let gate = self
            .read_gate
            .read()
            .map_err(|_| Error::Database("Library reader gate was poisoned".into()))?;
        if let Some(conn) = self
            .read_pool
            .lock()
            .map_err(|_| Error::Database("Reader pool lock was poisoned".into()))?
            .pop()
        {
            schema::ensure_ready(&conn)?;
            return Ok(ReadLease {
                conn: Some(conn),
                pool: &self.read_pool,
                _gate: gate,
                _upgrade: upgrade,
            });
        }
        // A ReadLease exclusively owns this !Sync connection until it returns to
        // the mutex-protected pool. Match rusqlite's default NO_MUTEX mode: SQLite
        // otherwise adds a redundant recursive mutex to every step/column access.
        let conn = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(db_error)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        conn.execute_batch(
            "PRAGMA query_only=ON; PRAGMA cache_size=-65536; PRAGMA temp_store=MEMORY;",
        )
        .map_err(db_error)?;
        conn.pragma_update(None, "mmap_size", sqlite_reader_mmap_bytes(&self.path))
            .map_err(db_error)?;
        schema::register_casefold(&conn)?;
        schema::ensure_ready(&conn)?;
        Ok(ReadLease {
            conn: Some(conn),
            pool: &self.read_pool,
            _gate: gate,
            _upgrade: upgrade,
        })
    }

    fn search_reader(&self, cancel: Option<Arc<AtomicBool>>) -> Result<ReadLease<'_>> {
        if cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Err(Error::Cancelled);
        }
        let conn = self.reader()?;
        if cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Err(Error::Cancelled);
        }
        if let Some(cancel) = cancel {
            conn.progress_handler(2000, Some(move || cancel.load(Ordering::Acquire)))
                .map_err(db_error)?;
        }
        Ok(conn)
    }

    pub fn check_integrity(&self) -> Result<()> {
        // Integrity checks must not reuse an FTS cursor's cached segment view after writes.
        let conn = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(db_error)?;
        let check: String = conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(db_error)?;
        if check != "ok" {
            return Err(Error::Database(check));
        }
        let violations: u64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                unsigned(row, 0)
            })
            .map_err(db_error)?;
        if violations > 0 {
            return Err(Error::Database(format!(
                "{violations} foreign key violations retained; repair the imported copy"
            )));
        }
        Ok(())
    }

    /// Delete a library record and related records, never the source audio file.
    pub fn delete_audio(&self, id: i64) -> Result<()> {
        self.remove_audio(&[id]).map(|_| ())
    }

    #[cfg(feature = "scanner")]
    pub(crate) fn start_scan(&self, root: &Path, incremental: bool) -> Result<i64> {
        let mut conn = self.writer()?;
        let tx = conn.transaction().map_err(db_error)?;
        // The exclusive scan lease proves any prior Running session was abandoned.
        tx.execute_batch("UPDATE scan_sessions SET status='failed',end_time=strftime('%Y-%m-%d %H:%M:%f','now'),
            error_details='[{\"path\":\"\",\"message\":\"Previous scanning process exited before finalization\"}]' WHERE status='running';")
            .map_err(db_error)?;
        let root_id = ensure_folder(&tx, root, root)?;
        repair_folder_subtree(&tx, root_id)?;
        tx.execute(
            "INSERT INTO scan_sessions(start_time,scan_path,scan_type,status)
            VALUES(strftime('%Y-%m-%d %H:%M:%f','now'),?1,?2,'running')",
            params![
                display_path(root),
                if incremental { "incremental" } else { "full" }
            ],
        )
        .map_err(db_error)?;
        let id = tx.last_insert_rowid();
        tx.commit().map_err(db_error)?;
        Ok(id)
    }

    /// Create (or re-attach) the folder row of `folder` and every ancestor up to `root`.
    /// Remote scans of a subdirectory call it first so the scanned subtree hangs below
    /// its configured root instead of becoming a separate top-level folder.
    #[cfg(feature = "scanner")]
    pub fn ensure_scan_folder(&self, folder: &Path, root: &Path) -> Result<()> {
        let mut conn = self.writer()?;
        let tx = conn.transaction().map_err(db_error)?;
        ensure_folder(&tx, folder, root)?;
        tx.commit().map_err(db_error)
    }

    #[cfg(feature = "scanner")]
    pub(crate) fn write_scan_batch(
        &self,
        items: &[ScannedAudio],
        session_id: i64,
    ) -> Result<(u64, u64)> {
        let mut conn = self.writer()?;
        let tx = conn.transaction().map_err(db_error)?;
        let result = self.apply_scan_items(&tx, items, Some(session_id))?;
        tx.commit().map_err(db_error)?;
        Ok(result)
    }

    #[cfg(feature = "scanner")]
    fn apply_scan_items(
        &self,
        tx: &Connection,
        items: &[ScannedAudio],
        session_id: Option<i64>,
    ) -> Result<(u64, u64)> {
        let mut added = 0;
        let mut updated = 0;
        let mut folders = std::collections::HashMap::new();
        let mut find_existing = tx.prepare_cached(
            "SELECT file_path,id,metadata_json,COALESCE(file_size,0),source_mtime_ns FROM audio_files WHERE path_key=?1 ORDER BY id",
        ).map_err(db_error)?;
        let mut header_columns = tx.prepare_cached(
            "UPDATE audio_files SET bwf_originator=?2,bwf_originator_reference=?3,
            bwf_origination_date=?4,bwf_origination_time=?5,bwf_time_reference=?6,bwf_version=?7,
            bwf_umid=?8,ixml_tape=?9,ixml_circled=?10,ixml_wild_track=?11,ixml_no_good=?12 WHERE id=?1",
        ).map_err(db_error)?;
        for item in items {
            let file_size = i64::try_from(item.size)
                .map_err(|_| Error::Validation("Audio file exceeds SQLite size range".into()))?;
            let text = display_path(&item.path);
            let key = path_key(&text);
            let candidates = find_existing
                .query_map([&key], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        (
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, Option<i64>>(4)?,
                        ),
                    ))
                })
                .map_err(db_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db_error)?;
            let prior = path_identity::choose(&text, candidates, true)?;
            let existing = prior.as_ref().map(|prior| prior.0);
            let folder = item
                .path
                .parent()
                .ok_or_else(|| Error::Validation("Audio file has no parent".into()))?;
            let folder_id = match folders.entry((folder, item.root.as_path())) {
                std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    *entry.insert(ensure_folder(tx, folder, &item.root)?)
                }
            };
            let filename = item
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let mut attributes = item.metadata.attributes.clone();
            if let Some((id, json, old_size, old_mtime)) = prior {
                if old_size != file_size || old_mtime != Some(item.mtime_ns) {
                    tx.prepare_cached("DELETE FROM waveform_cache WHERE audio_file_id=?1")
                        .map_err(db_error)?
                        .execute([id])
                        .map_err(db_error)?;
                }
                let mut previous = serde_json::from_str::<serde_json::Value>(&json)
                    .unwrap_or_else(|_| serde_json::json!({}));
                if let (Some(previous), Some(current)) =
                    (previous.as_object_mut(), attributes.as_object())
                {
                    for field in HEADER_ATTRIBUTES {
                        previous.remove(*field);
                    }
                    previous.extend(
                        current
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone())),
                    );
                }
                if previous.is_object() {
                    attributes = previous;
                }
            }
            let source_tags_text =
                schema::fold_case(&audiolib_core::source_tags(&attributes).join(", "));
            let ix_scene = attributes
                .get("ixml_scene")
                .and_then(|value| value.as_str());
            let ix_take = attributes.get("ixml_take").and_then(|value| value.as_str());
            let description = item.metadata.bext_description.as_str();
            if let Some(id) = existing {
                tx.prepare_cached("UPDATE audio_files SET filename=?2,file_size=?3,sample_rate=?4,bit_depth=?5,channels=?6,
                    duration=?7,codec=?8,bwf_description=?9,folder_id=?10,source_mtime_ns=?11,metadata_json=?12,
                    ixml_scene=?13,ixml_take=?14,seen_scan_id=?15,source_tags_text=?16,missing=0,
                    modified_at=strftime('%Y-%m-%d %H:%M:%f','now') WHERE id=?1").map_err(db_error)?.execute(params![id,
                    filename,file_size,item.metadata.sample_rate,item.metadata.bit_depth,item.metadata.channels,
                    item.metadata.duration,item.metadata.encoding,description,folder_id,item.mtime_ns,
                    attributes.to_string(),ix_scene,ix_take,session_id,source_tags_text]).map_err(db_error)?;
                updated += 1;
            } else {
                let id = schema::allocate_audio_id(tx)?;
                tx.prepare_cached("INSERT INTO audio_files(file_path,path_key,filename,file_size,sample_rate,bit_depth,channels,
                    duration,codec,bwf_description,folder_id,source_mtime_ns,metadata_json,
                    ixml_scene,ixml_take,seen_scan_id,
                    id,missing,created_at,modified_at,source_tags_text) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,0,
                    strftime('%Y-%m-%d %H:%M:%f','now'),strftime('%Y-%m-%d %H:%M:%f','now'),?18)").map_err(db_error)?.execute(params![
                    display_path(&item.path),key,filename,file_size,item.metadata.sample_rate,item.metadata.bit_depth,item.metadata.channels,
                    item.metadata.duration,item.metadata.encoding,description,folder_id,item.mtime_ns,
                    attributes.to_string(),ix_scene,ix_take,session_id,id,source_tags_text]).map_err(db_error)?;
                added += 1;
            }
            // Missing supported header fields clear legacy columns after successful probing.
            let id = existing.unwrap_or_else(|| tx.last_insert_rowid());
            let mut header_values = Vec::with_capacity(12);
            header_values.push(Value::Integer(id));
            for column in [
                "bwf_originator",
                "bwf_originator_reference",
                "bwf_origination_date",
                "bwf_origination_time",
                "bwf_time_reference",
                "bwf_version",
                "bwf_umid",
                "ixml_tape",
                "ixml_circled",
                "ixml_wild_track",
                "ixml_no_good",
            ] {
                let value = match attributes.get(column) {
                    Some(value) => match value {
                        serde_json::Value::String(v) => Value::Text(v.clone()),
                        serde_json::Value::Bool(v) => Value::Integer(i64::from(*v)),
                        serde_json::Value::Number(v) => {
                            Value::Integer(v.as_i64().unwrap_or_default())
                        }
                        _ => Value::Null,
                    },
                    None => Value::Null,
                };
                header_values.push(value);
            }
            header_columns
                .execute(params_from_iter(header_values))
                .map_err(db_error)?;
        }
        Ok((added, updated))
    }

    #[cfg(feature = "scanner")]
    pub(crate) fn mark_seen(&self, ids: &[i64], session_id: i64) -> Result<()> {
        let mut conn = self.writer()?;
        let tx = conn.transaction().map_err(db_error)?;
        {
            // Unchanged files are only recorded in a per-connection TEMP table: rewriting
            // the row (and its partial indexes) for every unchanged file cost ~0.3 ms each
            // (~5 min per 900k files). Only a non-present file is touched; imported
            // NULL/nonbinary flags follow the same exact-zero visibility contract.
            ensure_scan_seen_table(&tx)?;
            let mut remember = tx
                .prepare_cached(
                    "INSERT OR IGNORE INTO temp.scan_seen(session_id,audio_id) VALUES(?1,?2)",
                )
                .map_err(db_error)?;
            let mut reappear = tx
                .prepare_cached("UPDATE audio_files SET missing=0 WHERE id=?1 AND missing IS NOT 0")
                .map_err(db_error)?;
            for id in ids {
                remember
                    .execute(params![session_id, id])
                    .map_err(db_error)?;
                reappear.execute([id]).map_err(db_error)?;
            }
        }
        tx.commit().map_err(db_error)
    }

    #[cfg(feature = "scanner")]
    pub(crate) fn finish_scan(
        &self,
        root: &Path,
        progress: &mut audiolib_core::ScanProgress,
        cleanup: bool,
        recursive: bool,
        interrupt: &Arc<AtomicBool>,
    ) -> Result<()> {
        let mut conn = self.writer()?;
        let result = run_interruptible(&mut conn, interrupt, |conn| {
            let tx = conn.transaction().map_err(db_error)?;
            finish_scan_transaction(&tx, root, progress, cleanup, recursive)?;
            tx.commit().map_err(db_error)
        });
        if result.is_err()
            && (interrupt.load(Ordering::Acquire) || matches!(result, Err(Error::Cancelled)))
        {
            // The heavy transaction rolled back. Leave a terminal session row so
            // history does not stay "running" after quit aborted the rollup.
            mark_scan_session_cancelled(&conn, progress)?;
            return Err(Error::Cancelled);
        }
        result
    }
}

/// Stop a statement on this connection when `interrupt` becomes set.
/// The progress handler is removed and the watcher joined before return, so the
/// next statement is not cancelled. A successful `work` result is preserved when
/// the flag races in after commit.
#[cfg(feature = "scanner")]
fn run_interruptible<T>(
    conn: &mut Connection,
    interrupt: &Arc<AtomicBool>,
    work: impl FnOnce(&mut Connection) -> Result<T>,
) -> Result<T> {
    if interrupt.load(Ordering::Acquire) {
        return Err(Error::Cancelled);
    }
    let flag = Arc::clone(interrupt);
    conn.progress_handler(2000, Some(move || flag.load(Ordering::Acquire)))
        .map_err(db_error)?;
    let handle = conn.get_interrupt_handle();
    let flag = Arc::clone(interrupt);
    let result = std::thread::scope(|scope| {
        let (done, waiting) = std::sync::mpsc::sync_channel::<()>(1);
        scope.spawn(move || {
            loop {
                if flag.load(Ordering::Acquire) {
                    handle.interrupt();
                }
                match waiting.recv_timeout(Duration::from_millis(20)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
        });
        let result = work(conn);
        drop(done);
        result
    });
    if conn.progress_handler(0, None::<fn() -> bool>).is_err() {
        // A handler that still returns true would cancel the next writer user.
        let _ = conn.progress_handler(2000, Some(|| false));
    }
    result
}

#[cfg(feature = "scanner")]
fn finish_scan_transaction(
    tx: &rusqlite::Transaction<'_>,
    root: &Path,
    progress: &mut audiolib_core::ScanProgress,
    cleanup: bool,
    recursive: bool,
) -> Result<()> {
    if cleanup {
        let folder = ensure_folder(tx, root, root)?;
        let stored_root: String = tx
            .query_row(
                "SELECT folder_path FROM folders WHERE id=?1",
                [folder],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        // Missing files retain annotations, playlists, and IDs and can reconnect later.
        let (scope, scope_value) = if recursive {
            (
                // Hierarchy membership survives native case-only aliases and
                // volume roots. Rows without a folder retain the old path scope.
                "(folder_id IN (SELECT descendant_id FROM folder_closure WHERE ancestor_id=?1) OR (folder_id IS NULL AND ((?3='/' AND substr(file_path,1,1)='/') OR audiolib_path_identity(file_path)=?3 COLLATE BINARY OR substr(audiolib_path_identity(file_path),1,length(?3)+1)=?3||'/' COLLATE BINARY)))",
                Value::Integer(folder),
            )
        } else {
            ("folder_id=?1 AND ?3 IS NOT NULL", Value::Integer(folder))
        };
        ensure_scan_seen_table(tx)?;
        progress.removed = tx.execute(
                &format!(
                    "UPDATE audio_files SET missing=1 WHERE {scope} AND missing=0 AND lower(filename) LIKE '%.wav'
                AND (seen_scan_id IS NULL OR seen_scan_id<>?2)
                AND NOT EXISTS(SELECT 1 FROM temp.scan_seen s WHERE s.session_id=?2 AND s.audio_id=audio_files.id)"
                ),
                params![scope_value, progress.session_id, path_identity::comparison_key(&stored_root)],
            )
            .map_err(db_error)? as u64;
    }
    ensure_scan_seen_table(tx)?;
    tx.execute(
        "DELETE FROM temp.scan_seen WHERE session_id<=?1",
        [progress.session_id],
    )
    .map_err(db_error)?;
    refresh_folder_statistics(tx)?;
    let status = match progress.state {
        audiolib_core::ScanState::Running => "running",
        audiolib_core::ScanState::Completed => "completed",
        audiolib_core::ScanState::Cancelled => "cancelled",
        audiolib_core::ScanState::Failed => "failed",
    };
    tx.execute("UPDATE scan_sessions SET end_time=strftime('%Y-%m-%d %H:%M:%f','now'),status=?2,
            files_scanned=?3,files_added=?4,files_updated=?5,files_skipped=?6,errors=?7,error_details=?8,
            files_removed=?9,errors_truncated=?10 WHERE id=?1",
            params![progress.session_id,status,progress.processed as i64,progress.added as i64,progress.updated as i64,progress.unchanged as i64,
                progress.errors as i64,serde_json::to_string(&progress.error_details).map_err(|e|Error::Validation(e.to_string()))?,
                progress.removed as i64,progress.errors_truncated as i64]).map_err(db_error)?;
    Ok(())
}

#[cfg(feature = "scanner")]
fn mark_scan_session_cancelled(
    conn: &Connection,
    progress: &audiolib_core::ScanProgress,
) -> Result<()> {
    let details = serde_json::to_string(&progress.error_details)
        .map_err(|error| Error::Validation(error.to_string()))?;
    conn.execute(
        "UPDATE scan_sessions SET end_time=strftime('%Y-%m-%d %H:%M:%f','now'),status='cancelled',
            files_scanned=?2,files_added=?3,files_updated=?4,files_skipped=?5,errors=?6,error_details=?7,
            files_removed=?8,errors_truncated=?9 WHERE id=?1 AND status='running'",
        params![
            progress.session_id,
            progress.processed as i64,
            progress.added as i64,
            progress.updated as i64,
            progress.unchanged as i64,
            progress.errors as i64,
            details,
            progress.removed as i64,
            progress.errors_truncated as i64
        ],
    )
    .map_err(db_error)?;
    Ok(())
}

#[cfg(feature = "scanner")]
pub(crate) struct ScannedAudio {
    pub path: PathBuf,
    pub root: PathBuf,
    pub size: u64,
    pub mtime_ns: i64,
    pub metadata: AudioMetadata,
}

struct RankingSnapshot<'a> {
    revision: u64,
    cache_eligible: bool,
    start: Instant,
    cancel: Option<&'a Arc<AtomicBool>>,
}

impl SqliteProvider {
    fn observe_search_revision(&self) -> Result<u64> {
        let mut observer = self.revision_reader.lock().unwrap();
        if observer.is_none() {
            // Every observer access holds revision_reader; no connection or
            // statement escapes this mutex guard.
            let conn = Connection::open_with_flags(
                &self.path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(db_error)?;
            conn.busy_timeout(Duration::from_secs(5))
                .map_err(db_error)?;
            *observer = Some(conn);
        }
        observer
            .as_ref()
            .unwrap()
            .query_row("PRAGMA data_version", [], |row| unsigned(row, 0))
            .map_err(db_error)
    }

    fn rank_document_lengths(
        &self,
        tx: &Connection,
        stable_revision: Option<u64>,
        cancel: Option<&AtomicBool>,
    ) -> Result<Option<Arc<search_bm25::Lengths>>> {
        let Some(revision) = stable_revision else {
            return search_bm25::load_lengths(tx, cancel);
        };
        let mut cached = self
            .document_lengths
            .lock()
            .map_err(|_| Error::Database("Document-length cache lock was poisoned".into()))?;
        if let Some((cached_revision, lengths)) = cached.as_ref()
            && *cached_revision == revision
        {
            return Ok(Some(lengths.clone()));
        }
        // The caller pinned this transaction between matching observer values.
        // Decode each document length once rather than issuing a docsize lookup
        // for every scored row. A concurrent commit only forbids publication;
        // the owned values remain valid for this request's pinned snapshot.
        let lengths = search_bm25::load_lengths(tx, cancel)?;
        if self.observe_search_revision()? == revision {
            *cached = lengths.as_ref().map(|lengths| (revision, lengths.clone()));
        }
        Ok(lengths)
    }

    fn ranked_search(
        &self,
        tx: &Connection,
        request: &SearchRequest,
        snapshot: RankingSnapshot<'_>,
    ) -> Result<Option<SearchPage>> {
        let RankingSnapshot {
            revision,
            cache_eligible,
            start,
            cancel,
        } = snapshot;
        let check_cancel = || {
            if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        };
        check_cancel()?;
        let query = compile_query(&self.tokenizer, &request.query)?;
        let cached = cache_eligible
            .then(|| self.ranking_cache.lock().unwrap().get(revision, &query))
            .flatten();
        let cache_hit = cached.is_some();
        let ids = if let Some(ids) = cached {
            ids
        } else {
            let Some(mut ranked) = search_rank_parallel::collect(
                self,
                tx,
                query.clone(),
                search_rank_parallel::Snapshot {
                    revision,
                    cache_eligible,
                    cancel,
                },
            )?
            else {
                return Ok(None);
            };
            check_cancel()?;
            // Every input range is in ascending rowid order and ranges are
            // concatenated in that order. Stable score sorting therefore keeps
            // the exact ID tie-break without comparing it for every equal score.
            ranked.sort_by(|a, b| a.1.total_cmp(&b.1));
            check_cancel()?;
            let mut ids = ranked.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
            // Collect may reuse the wider (id,score) allocation; retained cache entries
            // account for compact i64 identities rather than spare tuple capacity.
            ids.shrink_to_fit();
            let ids = Arc::new(ids);
            // Comparing the same observer before/after the pinned reader snapshot
            // prevents publishing a ranking under a revision from another commit.
            if cache_eligible && self.observe_search_revision()? == revision {
                self.ranking_cache
                    .lock()
                    .unwrap()
                    .insert(revision, query, ids.clone());
            }
            ids
        };
        let visibility_only = request.folder_id.is_none()
            && request.filters
                == audiolib_core::SearchFilters {
                    visibility: request.filters.visibility,
                    ..Default::default()
                };
        let (membership, exclude) = if visibility_only
            && request.filters.visibility == FileVisibility::All
        {
            (None, false)
        } else {
            let mut filter_request = request.clone();
            filter_request.query.clear();
            // Preserve the visibility predicate: the visible folder/format/filter
            // indexes cover IDs only while their missing=0 condition is present.
            // Removing it forces duration and other fields back onto wide rows.
            let SearchSql {
                from,
                conditions,
                values,
            } = search_sql(
                &self.tokenizer,
                &filter_request,
                true,
                self.derived_tags.load(Ordering::Acquire),
            )?;
            // A library-wide exclude-only filter collects the IDs it drops (files not
            // present and excluded tagged files), not the ID of every passing file.
            let exclusion = if request.folder_id.is_none() {
                search_exclude::Exclusion::plan(
                    tx,
                    &filter_request,
                    self.derived_tags.load(Ordering::Acquire),
                )?
            } else {
                None
            };
            let exclude = exclusion.is_some()
                || visibility_only && request.filters.visibility == FileVisibility::Present;
            let filter_from = search_candidates::covering_numeric_filter_from(tx, request)?
                .unwrap_or(from.as_str());
            let (sql, values) = if let Some(exclusion) = &exclusion {
                exclusion.dropped_ids()
            } else if exclude {
                ("SELECT id FROM audio_files INDEXED BY idx_audio_files_not_present WHERE missing IS NOT 0".to_string(), values)
            } else if visibility_only && request.filters.visibility == FileVisibility::Missing {
                (
                    "SELECT id FROM audio_files WHERE missing=1".to_string(),
                    values,
                )
            } else {
                (
                    format!(
                        "SELECT af.id FROM {filter_from} WHERE {}",
                        conditions.join(" AND ")
                    ),
                    values,
                )
            };
            if ids.is_empty() {
                return Ok(Some(SearchPage {
                    query: request.query.clone(),
                    execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
                    cache_hit,
                    ..Default::default()
                }));
            }
            let mut statement = tx.prepare(&sql).map_err(db_error)?;
            let mut rows = statement
                .query(params_from_iter(&values))
                .map_err(db_error)?;
            let mut membership = if visibility_only {
                // Sparse missing IDs need no extra pass over a broad GUI ranking.
                id_membership::IdMembership::sparse(0)
            } else {
                id_membership::IdMembership::for_ranked_ids(&ids, &check_cancel)?
            };
            let mut membership_rows = 0usize;
            while let Some(row) = rows.next().map_err(db_error)? {
                if membership_rows.is_multiple_of(1024) {
                    check_cancel()?;
                }
                if membership_rows == search_cache::MAX_RANKED_IDS {
                    return Ok(None);
                }
                membership_rows += 1;
                membership.insert(row.get(0).map_err(db_error)?);
            }
            (Some(membership), exclude)
        };
        let mut total_count = 0u64;
        let mut page_ids = Vec::with_capacity(request.limit);
        for (index, id) in ids.iter().copied().enumerate() {
            if index.is_multiple_of(1024) {
                check_cancel()?;
            }
            if membership
                .as_ref()
                .is_some_and(|set| set.contains(&id) == exclude)
            {
                continue;
            }
            if total_count >= request.offset as u64 && page_ids.len() < request.limit {
                page_ids.push(id);
            }
            total_count += 1;
        }
        check_cancel()?;
        let json = serde_json::to_string(&page_ids).map_err(|e| Error::Database(e.to_string()))?;
        let projection = audio_projection(tx)?;
        let mut statement = tx.prepare(&format!("SELECT {projection} FROM audio_files af WHERE af.id IN (SELECT value FROM json_each(?1))"))
            .map_err(db_error)?;
        let mut files = statement
            .query_map([json], audio_from_row)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?
            .into_iter()
            .map(|file| (file.id, file))
            .collect::<std::collections::HashMap<_, _>>();
        let results = page_ids
            .into_iter()
            .map(|id| {
                files.remove(&id).ok_or_else(|| {
                    Error::Database("Ranking and Metadata snapshots disagree".into())
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Some(SearchPage {
            returned_count: results.len(),
            results,
            total_count,
            query: request.query.clone(),
            execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
            cache_hit,
        }))
    }

    fn search_impl(
        &self,
        request: &SearchRequest,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<SearchPage> {
        request.validate()?;
        let start = Instant::now();
        let mut conn = self.search_reader(cancel.clone())?;
        let revision_before = self.observe_search_revision()?;
        let tx = conn.transaction().map_err(db_error)?;
        // Pin the snapshot before deciding whether identities from a cached ranking
        // may be reused. data_version itself is read on the dedicated observer.
        tx.query_row("SELECT id FROM audio_files LIMIT 1", [], |row| {
            row.get::<_, i64>(0)
        })
        .optional()
        .map_err(db_error)?;
        let revision = self.observe_search_revision()?;
        if !request.query.trim().is_empty()
            && request.order_by == SortOrder::Relevance
            && let Some(page) = self.ranked_search(
                &tx,
                request,
                RankingSnapshot {
                    revision,
                    cache_eligible: revision_before == revision,
                    start,
                    cancel: cancel.as_ref(),
                },
            )?
        {
            return Ok(page);
        }
        let SearchSql {
            from,
            conditions,
            mut values,
        } = search_sql(
            &self.tokenizer,
            request,
            true,
            self.derived_tags.load(Ordering::Acquire),
        )?;
        let where_sql = conditions.join(" AND ");
        let has_query = !request.query.trim().is_empty();
        if let Some(Value::Text(query)) = values.first()
            && let Some(page) = search_sorted::filtered_filename_page(
                &tx,
                &self.tokenizer,
                request,
                query,
                start,
                || {
                    if cancel
                        .as_ref()
                        .is_some_and(|flag| flag.load(Ordering::Acquire))
                    {
                        Err(Error::Cancelled)
                    } else {
                        Ok(())
                    }
                },
            )?
        {
            return Ok(page);
        }
        let unfiltered_fts =
            has_query && values.len() == 1 && conditions.len() == 2 && request.folder_id.is_none();
        let not_present_probe = if unfiltered_fts
            && request.filters.visibility == FileVisibility::Present
        {
            // Stop after 129 index entries: sparse non-present records make subtraction
            // cheap, while a dense set must retain the ordinary joined count. Legacy
            // NULL/nonbinary flags must agree with Present's exact missing=0 predicate.
            tx.query_row(
                    "SELECT COUNT(*) FROM (SELECT 1 FROM audio_files INDEXED BY idx_audio_files_not_present WHERE missing IS NOT 0 LIMIT 129)",
                    [],
                    |row| unsigned(row, 0),
                )
                .map_err(db_error)?
        } else {
            0
        };
        // An include filter tests its terms on the sparse tag index; only matches read
        // their wide row. Sort index walks would test every row they pass.
        let inclusion = search_include::Inclusion::plan(
            &tx,
            request,
            self.derived_tags.load(Ordering::Acquire),
        )?;
        let no_missing = unfiltered_fts
            && (request.filters.visibility == FileVisibility::All
                || request.filters.visibility == FileVisibility::Present && not_present_probe == 0);
        let count_sql = if no_missing {
            "SELECT COUNT(*) FROM audio_search WHERE audio_search MATCH ?1".to_string()
        } else if unfiltered_fts
            && request.filters.visibility == FileVisibility::Present
            && not_present_probe <= 128
        {
            // Count postings once, then subtract only non-present matches through the
            // partial visibility index and FTS5's rowid/MATCH lookup. One unavailable file
            // must not force a primary-key probe for every broad matching posting.
            // Both subqueries use this transaction's pinned snapshot and bound query.
            "SELECT (SELECT COUNT(*) FROM audio_search WHERE audio_search MATCH ?1)
             -(SELECT COUNT(*) FROM audio_files af INDEXED BY idx_audio_files_not_present WHERE af.missing IS NOT 0 AND EXISTS(
                SELECT 1 FROM audio_search WHERE rowid=af.id AND audio_search MATCH ?1))"
                .to_string()
        } else {
            let from = inclusion
                .as_ref()
                .map_or(from.as_str(), |plan| plan.count_from());
            format!("SELECT COUNT(*) FROM {from} WHERE {where_sql}")
        };
        // An exclude-only filter counts present files minus excluded tagged files from
        // narrow indexes rather than testing every wide row. Its page keeps the shared
        // statement; dense matches read a proven sort index and stop after the page.
        let exclusion = search_exclude::Exclusion::plan(
            &tx,
            request,
            self.derived_tags.load(Ordering::Acquire),
        )?;
        let total: u64 = if let Some(exclusion) = &exclusion {
            exclusion.total(&tx, request)?
        } else {
            tx.query_row(&count_sql, params_from_iter(&values), |row| {
                unsigned(row, 0)
            })
            .map_err(db_error)?
        };
        if request.offset as u64 >= total {
            return Ok(SearchPage {
                returned_count: 0,
                results: Vec::new(),
                total_count: total,
                query: request.query.clone(),
                execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
                cache_hit: false,
            });
        }
        let sorted_page_sql =
            if request.order_by == SortOrder::Tags && !self.derived_tags.load(Ordering::Acquire) {
                None
            } else {
                search_sorted::page_sql(&tx, request, no_missing, total)?
            };
        let order = match request.order_by {
            SortOrder::Relevance if has_query => "bm25(audio_search)",
            SortOrder::Relevance | SortOrder::Filename => "af.filename COLLATE NOCASE",
            SortOrder::Duration => "af.duration",
            SortOrder::SampleRate => "af.sample_rate",
            SortOrder::Modified => "af.modified_at",
            SortOrder::Channels => "af.channels",
            // Library and source tags share Unicode trimming and ICU folding. Blank
            // annotations use the precomputed source key without parsing source JSON.
            SortOrder::Tags if self.derived_tags.load(Ordering::Acquire) => {
                "COALESCE(audiolib_user_tags_key(af.user_tags),af.source_tags_text) COLLATE NOCASE"
            }
            SortOrder::Tags => {
                "COALESCE(audiolib_user_tags_key(af.user_tags),audiolib_source_tags_key(af.metadata_json)) COLLATE NOCASE"
            }
        };
        let descending = request.descending && request.order_by != SortOrder::Relevance;
        values.push(Value::Integer(request.limit as i64));
        values
            .push(Value::Integer(i64::try_from(request.offset).map_err(
                |_| Error::Validation("Offset is too large".into()),
            )?));
        // SQLite's bounded top-N sorter is faster than FTS5's internal rank sorter for
        // very broad matches. Hydrate only the ranked page after confirming visibility
        // in this transaction; the scores are exactly FTS5's default BM25 scores.
        let projection = audio_projection(&tx)?;
        let page_sql = if let Some(sql) = sorted_page_sql {
            sql
        } else if no_missing && request.order_by == SortOrder::Relevance {
            format!(
                "WITH ranked AS MATERIALIZED (
                    SELECT rowid,bm25(audio_search) AS score FROM audio_search
                    WHERE audio_search MATCH ? ORDER BY score,rowid ASC LIMIT ? OFFSET ?
                ) SELECT {projection} FROM ranked
                INNER JOIN audio_files af ON af.id=ranked.rowid
                ORDER BY ranked.score ASC,af.id ASC"
            )
        } else {
            // Sort only IDs and the selected key. Fetch the wide Metadata DTOs after
            // pagination for filtered FTS, folder browsing, and alternative sort orders.
            let direction = if descending { "DESC" } else { "ASC" };
            let from = match (&exclusion, &inclusion) {
                (Some(exclusion), _) => exclusion.page_from(&tx, request, total)?.unwrap_or(&from),
                (None, Some(inclusion)) => {
                    inclusion.page_from(&tx, request, total)?.unwrap_or(&from)
                }
                (None, None) => &from,
            };
            format!(
                "WITH ranked AS MATERIALIZED (
                    SELECT af.id,{order} AS sort_value FROM {from} WHERE {where_sql}
                    ORDER BY sort_value {direction},af.id ASC LIMIT ? OFFSET ?
                ) SELECT {projection} FROM ranked JOIN audio_files af ON af.id=ranked.id
                ORDER BY ranked.sort_value {direction},af.id ASC"
            )
        };
        let mut statement = tx.prepare(&page_sql).map_err(db_error)?;
        let results = statement
            .query_map(params_from_iter(&values), audio_from_row)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        Ok(SearchPage {
            returned_count: results.len(),
            results,
            total_count: total,
            query: request.query.clone(),
            execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
            cache_hit: false,
        })
    }

    fn search_folders_impl(
        &self,
        request: &SearchRequest,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<Vec<Folder>> {
        request.validate()?;
        let has_query = !request.query.trim().is_empty();
        let visibility_only = request.filters
            == (audiolib_core::SearchFilters {
                visibility: request.filters.visibility,
                ..Default::default()
            });
        let check_cancel = || {
            if cancel
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        };
        let SearchSql {
            conditions, values, ..
        } = search_sql(
            &self.tokenizer,
            request,
            false,
            self.derived_tags.load(Ordering::Acquire),
        )?;
        // FTS emits arbitrary row IDs. Use a narrow covering lookup rather than
        // fetching each matching file's wide Metadata row just for its folder.
        // Exclude terms read only the covered tag columns, so they keep this scan as long
        // as another tag filter limits the rows to tagged files.
        let tagged_folder_scan = !has_query
            && self.derived_tags.load(Ordering::Acquire)
            && request.filters.visibility == FileVisibility::Present
            && (request.filters.tagged_only
                || request.filters.tag_contains.is_some()
                || !request.filters.include_tags.is_empty())
            && (audiolib_core::SearchFilters {
                tagged_only: false,
                tag_contains: None,
                include_tags: Vec::new(),
                exclude_tags: Vec::new(),
                ..request.filters.clone()
            }) == audiolib_core::SearchFilters::default();
        let from = folder_search_from(has_query, visibility_only, tagged_folder_scan);
        let mut conn = self.search_reader(cancel.clone())?;
        let revision_before = has_query
            .then(|| self.observe_search_revision())
            .transpose()?;
        let tx = conn.transaction().map_err(db_error)?;
        let (ranked_ids, pinned_revision) = if let Some(revision_before) = revision_before {
            // The complete ranking is query-only, even when its page was scoped
            // or filtered. Reuse it only when it belongs to this pinned snapshot.
            tx.query_row("SELECT id FROM audio_files LIMIT 1", [], |row| {
                row.get::<_, i64>(0)
            })
            .optional()
            .map_err(db_error)?;
            let revision = self.observe_search_revision()?;
            if revision_before == revision {
                let query = compile_query(&self.tokenizer, &request.query)?;
                (
                    self.ranking_cache.lock().unwrap().get(revision, &query),
                    Some(revision),
                )
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };
        check_cancel()?;
        // A concurrent first page may not have published its BM25 ranking yet.
        // Collect query-only membership locally in this same pinned transaction;
        // these unscored IDs must never enter the ranking cache.
        let query_ids = match (values.first(), ranked_ids.as_ref()) {
            (Some(Value::Text(query)), None) if has_query => {
                search_candidates::query_ids(&tx, query, &check_cancel, || {
                    let Some(revision) = pinned_revision else {
                        return Ok(None);
                    };
                    match self.ranking_cache.try_lock() {
                        Ok(cache) => Ok(cache.peek(revision, query)),
                        Err(std::sync::TryLockError::WouldBlock) => Ok(None),
                        Err(std::sync::TryLockError::Poisoned(_)) => {
                            Err(Error::Database("Ranking cache lock was poisoned".into()))
                        }
                    }
                })?
            }
            _ => None,
        };
        // Project only filter IDs first. This lets existing partial/format
        // indexes cover common filters, before any folder lookup or FTS work.
        // Retained rankings stay in their original BM25 order; only temporary
        // intersection IDs may be reordered for narrow directory lookups.
        let retained_ids = ranked_ids
            .as_deref()
            .map(Vec::as_slice)
            .or_else(|| query_ids.as_deref().map(Vec::as_slice));
        // Exclude-only facets subtract excluded tagged files (per folder without a
        // query, from the complete query IDs with one); collecting every matching
        // ID would read each present file's wide row. Facets ignore folder scope.
        let exclusion = {
            let mut filter_request = request.clone();
            filter_request.query.clear();
            search_exclude::Exclusion::plan(
                &tx,
                &filter_request,
                self.derived_tags.load(Ordering::Acquire),
            )?
        };
        let use_filter_ids = exclusion.is_none()
            && !visibility_only
            && ((!has_query && !tagged_folder_scan) || retained_ids.is_some());
        let candidate_ids = if retained_ids.is_some_and(<[i64]>::is_empty) {
            // Completeness is already known, so no filter scan can add a match.
            Some(Vec::new())
        } else if let (Some(exclusion), Some(ids)) = (&exclusion, retained_ids) {
            Some(exclusion.without_excluded(&tx, ids, &check_cancel)?)
        } else if use_filter_ids {
            let mut filter_request = request.clone();
            filter_request.query.clear();
            // Facets count all matching folders, independent of the selected
            // folder. Keep the covering-index proof aligned with that SQL scope.
            filter_request.folder_id = None;
            let filter_sql = search_sql(
                &self.tokenizer,
                &filter_request,
                false,
                self.derived_tags.load(Ordering::Acquire),
            )?;
            let cold_basic_filter = query_ids.is_some()
                && request.filters.visibility == FileVisibility::Present
                && (audiolib_core::SearchFilters {
                    sample_rate: None,
                    duration_min: None,
                    duration_max: None,
                    ..request.filters.clone()
                }) == audiolib_core::SearchFilters::default();
            let filter_from = if cold_basic_filter {
                search_candidates::visible_filter_from(&tx)?
            } else {
                // A cached BM25 ranking changes only the source of query IDs.
                // Its numeric membership scan still needs the same covering
                // index; otherwise idx_duration fetches wide Metadata rows.
                search_candidates::covering_numeric_filter_from(&tx, &filter_request)?
                    .unwrap_or(filter_sql.from.as_str())
            };
            let filter_query = format!(
                "SELECT af.id FROM {filter_from} WHERE {}",
                filter_sql.conditions.join(" AND ")
            );
            if cold_basic_filter {
                search_candidates::filtered_ids_full_scan(
                    &tx,
                    &filter_query,
                    &filter_sql.values,
                    query_ids.as_deref().expect("cold query membership"),
                    &check_cancel,
                )?
            } else {
                search_candidates::filtered_ids(
                    &tx,
                    &filter_query,
                    &filter_sql.values,
                    retained_ids,
                    &check_cancel,
                )?
            }
        } else {
            None
        };
        let complete_ids = candidate_ids
            .as_deref()
            .or_else(|| visibility_only.then_some(retained_ids).flatten());
        // FTS emits row IDs, not folder order. SQL GROUP BY would sort one row per
        // matching file. Stream those IDs into folder-sized counters instead, with
        // a run accumulator for the usual contiguous files from the same directory.
        // Counts and closure rollup share one snapshot, including during a scan.
        let (direct, values) = if let Some(exclusion) = exclusion.as_ref().filter(|_| !has_query) {
            exclusion.folder_counts()
        } else if has_query || complete_ids.is_some() {
            let mut counter = FolderCounter::default();
            if let Some(ids) = complete_ids {
                if let Some(counts) = search_facets::ranked_counts(
                    &tx,
                    ids,
                    request.filters.visibility,
                    &check_cancel,
                )? {
                    counter = counts;
                } else {
                    counter = search_facets::chunked_counts(
                        &tx,
                        ids,
                        request.filters.visibility,
                        &check_cancel,
                    )?;
                }
            } else {
                let mut statement = tx
                    .prepare(&format!(
                        "SELECT af.folder_id FROM {from} WHERE {}",
                        conditions.join(" AND ")
                    ))
                    .map_err(db_error)?;
                let mut rows = statement
                    .query(params_from_iter(&values))
                    .map_err(db_error)?;
                while let Some(row) = rows.next().map_err(db_error)? {
                    counter.push(row.get(0).map_err(db_error)?);
                }
            }
            check_cancel()?;
            let counts = counter.into_counts();
            let json =
                serde_json::to_string(&counts).map_err(|e| Error::Database(e.to_string()))?;
            ("SELECT json_extract(value,'$[0]') AS folder_id,json_extract(value,'$[1]') AS matches FROM json_each(?)".to_string(), vec![Value::Text(json)])
        } else {
            (
                format!(
                    "SELECT af.folder_id,COUNT(*) AS matches FROM {from} WHERE {} GROUP BY af.folder_id",
                    conditions.join(" AND ")
                ),
                values,
            )
        };
        // Roll up only folder-sized rows, independent of the requested page/scope.
        // Scans can leave ANALYZE statistics describing a formerly tiny library.
        // Keep direct counts as the outer loop and use the descendant index; a
        // reversed full closure/direct join otherwise grows quadratically. The
        // final loop likewise hydrates each ancestor by its primary key.
        let sql = folder_rollup_sql(&direct);
        tx.prepare(&sql)
            .map_err(db_error)?
            .query_map(params_from_iter(&values), |row| {
                Ok(Folder {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    name: row.get(2)?,
                    parent_id: row.get(3)?,
                    file_count: unsigned(row, 4)?,
                })
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db_error)
    }
}

impl DataProvider for SqliteProvider {
    fn load_search_workspace(
        &self,
        target: &audiolib_core::SearchWorkspaceTarget,
    ) -> Result<audiolib_core::SearchWorkspace> {
        search_workspace::load(target)
    }

    fn save_search_workspace(
        &self,
        target: &audiolib_core::SearchWorkspaceTarget,
        workspace: &audiolib_core::SearchWorkspace,
    ) -> Result<()> {
        search_workspace::save(target, workspace)
    }

    fn search_revision(&self) -> Result<Option<u64>> {
        let _gate = self.read_gate.read().unwrap();
        // data_version is comparable only on the SAME connection. A dedicated observer
        // sees commits from our writer as well as other processes and snapshot overlays.
        self.observe_search_revision().map(Some)
    }

    fn search(&self, request: &SearchRequest) -> Result<SearchPage> {
        self.search_impl(request, None)
    }
    fn search_cancellable(
        &self,
        request: &SearchRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<SearchPage> {
        let result = self.search_impl(request, Some(cancel.clone()));
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        result
    }
    fn search_folders(&self, request: &SearchRequest) -> Result<Vec<Folder>> {
        self.search_folders_impl(request, None)
    }
    fn search_folders_cancellable(
        &self,
        request: &SearchRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<Vec<Folder>> {
        let result = self.search_folders_impl(request, Some(cancel.clone()));
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        result
    }

    fn get_audio(&self, id: i64) -> Result<AudioFile> {
        let reader = self.reader()?;
        let projection = audio_projection(&reader)?;
        reader
            .query_row(
                &format!("SELECT {projection} FROM audio_files af WHERE af.id=?1"),
                [id],
                audio_from_row,
            )
            .optional()
            .map_err(db_error)?
            .ok_or_else(|| Error::NotFound(format!("Audio {id}")))
    }

    fn folders(&self, parent_id: Option<i64>) -> Result<Vec<Folder>> {
        let conn = self.reader()?;
        let mut statement = conn
            .prepare(
                "SELECT id,folder_path,folder_name,parent_id,total_file_count FROM folders
            WHERE parent_id IS ?1 ORDER BY folder_name COLLATE NOCASE,id",
            )
            .map_err(db_error)?;
        statement
            .query_map([parent_id], |row| {
                Ok(Folder {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    name: row.get(2)?,
                    parent_id: row.get(3)?,
                    file_count: unsigned(row, 4)?,
                })
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db_error)
    }
    fn get_folder(&self, id: i64) -> Result<Folder> {
        self.reader()?.query_row(
            "SELECT id,folder_path,folder_name,parent_id,total_file_count FROM folders WHERE id=?1",
            [id], |row| Ok(Folder { id:row.get(0)?, path:row.get(1)?, name:row.get(2)?, parent_id:row.get(3)?, file_count:unsigned(row,4)? }),
        ).optional().map_err(db_error)?.ok_or_else(|| Error::NotFound(format!("Folder {id}")))
    }

    fn update_metadata(&self, id: i64, patch: &MetadataPatch) -> Result<AudioFile> {
        patch.validate()?;
        let conn = self.writer()?;
        let mut assignments: Vec<String> =
            vec!["modified_at=strftime('%Y-%m-%d %H:%M:%f','now')".into()];
        let mut values = Vec::<Value>::new();
        if let Some(tags) = &patch.tags {
            assignments.push("user_tags=?".into());
            values.push(Value::Text(tags.clone()));
        }
        if let Some(notes) = &patch.notes {
            assignments.push("user_notes=?".into());
            values.push(Value::Text(notes.clone()));
        }
        if let Some(value) = patch.favorite {
            assignments.push("favorite=?".into());
            values.push(Value::Integer(i64::from(value)));
        }
        if let Some(value) = &patch.color {
            assignments.push("user_color=?".into());
            values.push(if value.is_empty() {
                Value::Null
            } else {
                Value::Text(value.clone())
            });
        }
        values.push(Value::Integer(id));
        if conn
            .execute(
                &format!(
                    "UPDATE audio_files SET {} WHERE id=?",
                    assignments.join(",")
                ),
                params_from_iter(&values),
            )
            .map_err(db_error)?
            == 0
        {
            return Err(Error::NotFound(format!("Audio {id}")));
        }
        let projection = audio_projection(&conn)?;
        conn.query_row(
            &format!("SELECT {projection} FROM audio_files af WHERE af.id=?1"),
            [id],
            audio_from_row,
        )
        .map_err(db_error)
    }

    fn stats(&self) -> Result<LibraryStats> {
        self.reader()?
            .query_row(
                "SELECT COUNT(*),COALESCE(SUM(duration),0),COALESCE(SUM(file_size),0),
            (SELECT COUNT(*) FROM folders),(SELECT COUNT(*) FROM audio_files WHERE missing=1)
            FROM audio_files WHERE missing=0",
                [],
                |row| {
                    Ok(LibraryStats {
                        audio_files: unsigned(row, 0)?,
                        total_duration: row.get(1)?,
                        total_bytes: unsigned(row, 2)?,
                        folders: unsigned(row, 3)?,
                        missing_files: unsigned(row, 4)?,
                    })
                },
            )
            .map_err(db_error)
    }

    fn library_info(&self) -> Result<LibraryInfo> {
        self.management_info()
    }
    fn check_library(&self) -> Result<IntegrityReport> {
        self.management_check()
    }
    fn backup_library(&self, destination: &Path) -> Result<BackupReport> {
        self.management_backup(destination)
    }
    fn restore_library(&self, source: &Path) -> Result<RestoreReport> {
        self.management_restore(source)
    }
    fn maintain_library(&self, action: MaintenanceAction) -> Result<MaintenanceReport> {
        self.management_maintain(action)
    }
    fn update_metadata_batch(&self, request: &BatchMetadataRequest) -> Result<BatchMetadataReport> {
        self.management_batch(request)
    }
    fn find_audio_by_path(&self, path: &Path) -> Result<Option<AudioFile>> {
        self.management_lookup(path)
    }
    fn find_folder_by_path(&self, path: &Path) -> Result<Option<Folder>> {
        let resolved = if self.read_only {
            path.to_path_buf()
        } else {
            path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
        };
        let reader = self.reader()?;
        // Pin candidate selection and row projection to one SQLite snapshot so
        // a concurrent scan cannot reuse the chosen numeric ID between reads.
        let transaction = reader.unchecked_transaction().map_err(db_error)?;
        let Some(id) = path_identity::folder_id_with_alias(
            &transaction,
            &display_path(&resolved),
            !self.read_only,
        )?
        else {
            return Ok(None);
        };
        let folder = transaction.query_row(
            "SELECT id,folder_path,folder_name,parent_id,total_file_count FROM folders WHERE id=?1",
            [id], |row| Ok(Folder { id:row.get(0)?, path:row.get(1)?, name:row.get(2)?, parent_id:row.get(3)?, file_count:unsigned(row,4)? }),
        ).optional().map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
        Ok(folder)
    }
    fn relink_audio(&self, id: i64, path: &Path) -> Result<AudioFile> {
        self.management_relink(id, path)
    }
    fn relink_folder(&self, source: &Path, target: &Path) -> Result<u64> {
        self.management_relink_folder(source, target, &AtomicBool::new(false))
    }
    fn relink_folder_cancellable(
        &self,
        source: &Path,
        target: &Path,
        cancel: Arc<AtomicBool>,
    ) -> Result<u64> {
        self.management_relink_folder(source, target, &cancel)
    }
    fn remove_audio(&self, ids: &[i64]) -> Result<u64> {
        self.management_remove(ids)
    }
    fn remove_folder(&self, id: i64) -> Result<u64> {
        self.management_remove_folder(id)
    }
    fn scan_history(&self, limit: usize, offset: usize) -> Result<Vec<ScanSession>> {
        self.management_scans(limit, offset)
    }
    fn export_library(
        &self,
        destination: &Path,
        format: LibraryExportFormat,
    ) -> Result<TransferReport> {
        self.management_export(destination, format)
    }
    fn import_annotations(&self, source: &Path) -> Result<TransferReport> {
        self.management_import(source)
    }

    fn list_waveform_source(&self) -> audiolib_core::ListWaveformSource {
        // A read-only snapshot (server mode) never uses its `waveform_cache` rows (§9.3).
        if self.read_only || !cfg!(feature = "scanner") {
            audiolib_core::ListWaveformSource::Unavailable
        } else {
            audiolib_core::ListWaveformSource::Local
        }
    }
    fn list_waveforms(
        &self,
        ids: &[i64],
        cancel: Arc<AtomicBool>,
    ) -> Result<audiolib_core::ListWaveformBatch> {
        self.list_waveforms_impl(ids, &cancel)
    }
    #[cfg(feature = "scanner")]
    fn compute_list_waveform(
        &self,
        id: i64,
        cancel: Arc<AtomicBool>,
    ) -> Result<audiolib_core::ListWaveformState> {
        self.compute_list_waveform_impl(id, &cancel)
    }
    fn list_waveform_backlog(&self, after: i64, limit: usize) -> Result<Vec<i64>> {
        self.list_waveform_backlog_impl(after, limit)
    }
}

#[derive(Default)]
struct FolderCounter {
    counts: std::collections::BTreeMap<i64, u64>,
    last: Option<i64>,
    run: u64,
}

impl FolderCounter {
    fn push(&mut self, folder: Option<i64>) {
        if folder != self.last {
            if let Some(last) = self.last {
                *self.counts.entry(last).or_default() += self.run;
            }
            self.last = folder;
            self.run = 0;
        }
        self.run += 1;
    }

    fn into_counts(mut self) -> Vec<(i64, u64)> {
        if let Some(last) = self.last {
            *self.counts.entry(last).or_default() += self.run;
        }
        self.counts.into_iter().collect()
    }
}

// Count, page and facet fallbacks must keep MATCH as the outer loop. Otherwise
// filter selectivity or stale statistics can make every eligible audio_files ID
// reopen the complete positional query. Facets can retain their covering lookup.
fn fts_search_from(covering_facets: bool) -> &'static str {
    if covering_facets {
        "audio_search CROSS JOIN audio_files af INDEXED BY idx_audio_search_facets
         ON af.id=audio_search.rowid"
    } else {
        "audio_search CROSS JOIN audio_files af ON af.id=audio_search.rowid"
    }
}

fn folder_search_from(
    has_query: bool,
    visibility_only: bool,
    tagged_folder_scan: bool,
) -> &'static str {
    if has_query {
        fts_search_from(visibility_only)
    } else if tagged_folder_scan {
        // The shared query builder supplies the same partial tag predicate.
        "audio_files af INDEXED BY idx_audio_visible_tagged_folders"
    } else {
        "audio_files af"
    }
}

fn folder_rollup_sql(direct: &str) -> String {
    format!(
        "WITH direct AS MATERIALIZED ({direct}), totals AS MATERIALIZED (
            SELECT fc.ancestor_id,SUM(d.matches) AS matches
            FROM direct d CROSS JOIN folder_closure fc INDEXED BY idx_closure_descendant
            ON fc.descendant_id=d.folder_id
            GROUP BY fc.ancestor_id
        ), hydrated AS MATERIALIZED (
          SELECT t.ancestor_id AS id,
            (SELECT folder_path FROM folders WHERE id=t.ancestor_id) AS folder_path,
            (SELECT folder_name FROM folders WHERE id=t.ancestor_id) AS folder_name,
            (SELECT parent_id FROM folders WHERE id=t.ancestor_id) AS parent_id,t.matches
          FROM totals t WHERE t.matches>0
        ) SELECT id,folder_path,folder_name,parent_id,matches FROM hydrated
          WHERE folder_path IS NOT NULL ORDER BY folder_name COLLATE NOCASE,id"
    )
}

struct SearchSql {
    from: String,
    conditions: Vec<String>,
    values: Vec<Value>,
}

// Results and directory facets must compile exactly the same query and filters.
fn search_sql(
    tokenizer: &Jieba,
    request: &SearchRequest,
    include_folder: bool,
    derived_tags: bool,
) -> Result<SearchSql> {
    let mut conditions = vec![
        match request.filters.visibility {
            FileVisibility::Present => "af.missing=0",
            FileVisibility::Missing => "af.missing=1",
            FileVisibility::All => "1=1",
        }
        .to_string(),
    ];
    let mut values = Vec::<Value>::new();
    let mut from = "audio_files af".to_string();
    if !request.query.trim().is_empty() {
        from = fts_search_from(false).into();
        conditions.push("audio_search MATCH ?".into());
        values.push(Value::Text(compile_query(tokenizer, &request.query)?));
    }
    if let Some(folder) = request.folder_id.filter(|_| include_folder) {
        if request.include_subfolders {
            conditions.push(
                "af.folder_id IN (SELECT descendant_id FROM folder_closure WHERE ancestor_id=?)"
                    .into(),
            );
        } else {
            conditions.push("af.folder_id=?".into());
        }
        values.push(Value::Integer(folder));
    }
    let filters = &request.filters;
    for (column, value) in [
        ("sample_rate", filters.sample_rate.map(i64::from)),
        ("bit_depth", filters.bit_depth.map(i64::from)),
        ("channels", filters.channels.map(i64::from)),
    ] {
        if let Some(value) = value {
            conditions.push(format!("af.{column}=?"));
            values.push(Value::Integer(value));
        }
    }
    if let Some(value) = filters.duration_min {
        conditions.push("af.duration>=?".into());
        values.push(Value::Real(value));
    }
    if let Some(value) = filters.duration_max {
        conditions.push("af.duration<=?".into());
        values.push(Value::Real(value));
    }
    if filters.favorites_only {
        conditions.push("af.favorite=1".into());
    }
    // Both filters are written so the planner can use idx_audio_visible_tagged_any: its
    // partial WHERE is exactly the "has any tag" term below, so the visible filename order
    // is kept (no sorter) and only rows that have tags are visited. `derived_tags == false`
    // (v3/v4 snapshots, unfinished backfill) falls back to parsing metadata_json per row.
    const ANY_TAG: &str = "(COALESCE(trim(af.user_tags),'')<>'' OR af.source_tags_text<>'')";
    if filters.tagged_only {
        conditions.push(if derived_tags {
            ANY_TAG.into()
        } else {
            "(COALESCE(trim(af.user_tags),'')<>'' OR audiolib_has_source_tags(af.metadata_json))"
                .into()
        });
    }
    let has_include = filters.tag_contains.is_some() || !filters.include_tags.is_empty();
    if derived_tags && has_include {
        conditions.push(ANY_TAG.into());
    }
    if filters.include_tags.is_empty() && filters.exclude_tags.is_empty() {
        // A single include term keeps the statement earlier versions issue.
        if let Some(tag) = &filters.tag_contains {
            conditions.push(if derived_tags {
                "(instr(audiolib_casefold(COALESCE(af.user_tags,'')),?)>0 OR instr(af.source_tags_text,?)>0)"
                    .into()
            } else {
                "(instr(audiolib_casefold(COALESCE(af.user_tags,'')),?)>0 OR instr(audiolib_casefold(audiolib_source_tags(af.metadata_json)),?)>0)"
                    .into()
            });
            let folded = schema::fold_case(tag);
            values.push(Value::Text(folded.clone()));
            values.push(Value::Text(folded));
        }
    } else {
        // Several terms fold each row's tags once in audiolib_tag_terms instead of once per
        // term. Exclude-only filters are not index-limited, so files without any tags,
        // which no exclude term can match, skip the call.
        let call = if derived_tags {
            "audiolib_tag_terms(af.user_tags,af.source_tags_text,?)"
        } else {
            "audiolib_tag_terms(af.user_tags,audiolib_casefold(audiolib_source_tags(af.metadata_json)),?)"
        };
        conditions.push(if derived_tags && !has_include {
            format!("((COALESCE(trim(af.user_tags),'')='' AND af.source_tags_text='') OR {call}=1)")
        } else {
            format!("{call}=1")
        });
        let terms: Vec<(bool, String)> = filters
            .tag_contains
            .iter()
            .chain(&filters.include_tags)
            .map(|tag| (false, schema::fold_case(tag)))
            .chain(
                filters
                    .exclude_tags
                    .iter()
                    .map(|tag| (true, schema::fold_case(tag))),
            )
            .collect();
        values.push(Value::Text(
            serde_json::to_string(&terms).map_err(|error| Error::Database(error.to_string()))?,
        ));
    }
    if let Some(color) = &filters.color {
        conditions.push("af.user_color COLLATE NOCASE=?".into());
        values.push(Value::Text(color.clone()));
    }
    Ok(SearchSql {
        from,
        conditions,
        values,
    })
}

// Typed, owned result columns: preserve unknown attributes and all legacy BWF/iXML fields.
// Legacy NULL/nonbinary visibility stays unchanged in storage; only exact zero is present.
const AUDIO_SELECT: &str = "af.id,af.file_path,af.filename,af.folder_id,COALESCE(af.file_size,0),
 COALESCE(af.sample_rate,0),COALESCE(af.bit_depth,0),COALESCE(af.channels,0),COALESCE(af.duration,0),
 COALESCE(af.user_tags,''),af.user_color,COALESCE(af.user_notes,''),af.favorite,
 COALESCE(af.bwf_description,''),af.metadata_json,
 af.bwf_originator,af.bwf_originator_reference,af.bwf_origination_date,af.bwf_origination_time,
 af.bwf_time_reference,af.bwf_version,af.bwf_umid,af.ixml_scene,af.ixml_take,af.ixml_tape,
 af.ixml_circled,af.ixml_wild_track,af.ixml_no_good,af.codec,af.created_at,af.modified_at,
 af.peak_amplitude,af.rms_level,af.spectral_centroid,af.zero_crossing_rate,(af.missing IS NOT 0)";

fn audio_projection(_conn: &Connection) -> Result<std::borrow::Cow<'static, str>> {
    // The same owned DTO projection reads legacy v3..v5, p0 and current p1;
    // retired library playback fields are never selected or surfaced.
    Ok(std::borrow::Cow::Borrowed(AUDIO_SELECT))
}

fn snapshot_modified_at(file: &AudioFile) -> Result<&str> {
    let value = file
        .attributes
        .get("modified_at")
        .and_then(|value| value.as_str());
    let valid = value.filter(|value| {
        (19..=29).contains(&value.len())
            && value.as_bytes().get(10) == Some(&b' ')
            && chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f").is_ok()
    });
    valid.ok_or_else(|| {
        Error::Validation(format!(
            "Audio {} has no supported authoritative modified_at; a full snapshot is required",
            file.id
        ))
    })
}

fn audio_from_row(row: &Row<'_>) -> rusqlite::Result<AudioFile> {
    let raw: String = row.get(14)?;
    let mut attributes =
        serde_json::from_str::<serde_json::Value>(&raw).unwrap_or_else(|_| serde_json::json!({}));
    if !attributes.is_object() {
        attributes = serde_json::json!({"legacy_attributes":attributes});
    }
    let object = attributes.as_object_mut().expect("attributes object");
    let description: String = row.get(13)?;
    if !description.is_empty() {
        object.insert("bwf_description".into(), description.into());
    }
    for (index, name) in [
        (15, "bwf_originator"),
        (16, "bwf_originator_reference"),
        (17, "bwf_origination_date"),
        (18, "bwf_origination_time"),
        (21, "bwf_umid"),
        (22, "ixml_scene"),
        (23, "ixml_take"),
        (24, "ixml_tape"),
        (28, "encoding"),
        (29, "created_at"),
        (30, "modified_at"),
    ] {
        if let Some(value) = row.get::<_, Option<String>>(index)? {
            object.insert(name.into(), value.into());
        }
    }
    for (index, name) in [
        (31, "peak_amplitude"),
        (32, "rms_level"),
        (33, "spectral_centroid"),
        (34, "zero_crossing_rate"),
    ] {
        if let Some(value) = row.get::<_, Option<f64>>(index)? {
            object.insert(name.into(), value.into());
        }
    }
    for (index, name) in [
        (19, "bwf_time_reference"),
        (20, "bwf_version"),
        (25, "ixml_circled"),
        (26, "ixml_wild_track"),
        (27, "ixml_no_good"),
    ] {
        if let Some(value) = row.get::<_, Option<i64>>(index)? {
            object.insert(name.into(), value.into());
        }
    }
    Ok(AudioFile {
        id: row.get(0)?,
        file_path: row.get(1)?,
        filename: row.get(2)?,
        folder_id: row.get(3)?,
        file_size: unsigned(row, 4)?,
        sample_rate: row.get(5)?,
        bit_depth: row.get(6)?,
        channels: row.get(7)?,
        duration: row.get(8)?,
        tags: row.get(9)?,
        color: row.get(10)?,
        notes: row.get(11)?,
        favorite: row.get(12)?,
        bext_description: row.get(13)?,
        attributes,
        missing: row.get(35)?,
    })
}

/// Plain-text queries are quoted tokens, so punctuation cannot become SQL or FTS operators.
fn compile_query(jieba: &Jieba, query: &str) -> Result<String> {
    search_text::compile_query(jieba, query)
}

#[cfg(feature = "scanner")]
fn ensure_folder(conn: &Connection, folder: &Path, root: &Path) -> Result<i64> {
    let display = folder_path_text(folder);
    let existing = path_identity::folder_id(conn, &display)?
        .map(|id| {
            conn.query_row(
                "SELECT id,parent_id FROM folders WHERE id=?1",
                [id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .map_err(db_error)
        })
        .transpose()?;
    let parent_id = if folder == root {
        None
    } else {
        folder
            .parent()
            .filter(|p| p.starts_with(root))
            .map(|p| ensure_folder(conn, p, root))
            .transpose()?
    };
    if let Some((id, old_parent)) = existing {
        // A directory scanned as a root can later become a descendant of a wider root.
        // Scanning that child again must not detach an already established parent.
        if let Some(parent) = parent_id
            && old_parent != Some(parent)
        {
            conn.execute(
                "UPDATE folders SET parent_id=?2 WHERE id=?1",
                params![id, parent],
            )
            .map_err(db_error)?;
            repair_folder_subtree(conn, id)?;
        } else {
            conn.execute("INSERT OR IGNORE INTO folder_closure(ancestor_id,descendant_id,depth) VALUES(?1,?1,0)",[id]).map_err(db_error)?;
        }
        return Ok(id);
    }
    let depth: i64 = if let Some(parent) = parent_id {
        conn.query_row("SELECT depth+1 FROM folders WHERE id=?1", [parent], |row| {
            row.get(0)
        })
        .map_err(db_error)?
    } else {
        0
    };
    conn.execute("INSERT INTO folders(folder_path,folder_name,parent_id,depth,file_count,total_file_count,created_at,updated_at) VALUES(?1,?2,?3,?4,0,0,
        strftime('%Y-%m-%d %H:%M:%f','now'),strftime('%Y-%m-%d %H:%M:%f','now'))", params![display,
        folder.file_name().unwrap_or(folder.as_os_str()).to_string_lossy(),parent_id,depth]).map_err(db_error)?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO folder_closure(ancestor_id,descendant_id,depth) VALUES(?1,?1,0)",
        [id],
    )
    .map_err(db_error)?;
    if let Some(parent) = parent_id {
        conn.execute(
            "INSERT OR IGNORE INTO folder_closure(ancestor_id,descendant_id,depth)
            SELECT ancestor_id,?1,depth+1 FROM folder_closure WHERE descendant_id=?2",
            params![id, parent],
        )
        .map_err(db_error)?;
    }
    Ok(id)
}

#[cfg(feature = "scanner")]
fn repair_folder_subtree(conn: &Connection, id: i64) -> Result<()> {
    // UNION and the ancestry trail also keep malformed legacy cycles finite.
    conn.execute("WITH RECURSIVE subtree(id) AS (SELECT ?1 UNION SELECT f.id FROM folders f JOIN subtree s ON f.parent_id=s.id)
        DELETE FROM folder_closure WHERE descendant_id IN (SELECT id FROM subtree)",[id]).map_err(db_error)?;
    conn.execute("WITH RECURSIVE subtree(id) AS (SELECT ?1 UNION SELECT f.id FROM folders f JOIN subtree s ON f.parent_id=s.id),
        ancestry(descendant,ancestor,depth,trail) AS (
            SELECT id,id,0,printf('/%d/',id) FROM subtree UNION ALL
            SELECT a.descendant,f.parent_id,a.depth+1,a.trail||printf('%d/',f.parent_id)
            FROM ancestry a JOIN folders f ON f.id=a.ancestor
            WHERE f.parent_id IS NOT NULL AND instr(a.trail,printf('/%d/',f.parent_id))=0)
        INSERT INTO folder_closure(ancestor_id,descendant_id,depth) SELECT ancestor,descendant,depth FROM ancestry",[id]).map_err(db_error)?;
    conn.execute("WITH RECURSIVE subtree(id) AS (SELECT ?1 UNION SELECT f.id FROM folders f JOIN subtree s ON f.parent_id=s.id)
        UPDATE folders SET depth=COALESCE((SELECT MAX(fc.depth+anchor.depth) FROM folder_closure fc
            JOIN folders anchor ON anchor.id=fc.ancestor_id
            WHERE fc.descendant_id=folders.id AND anchor.parent_id IS NULL),depth)
        WHERE id IN (SELECT id FROM subtree)",[id]).map_err(db_error)?;
    Ok(())
}

/// IDs of the unchanged files seen by the running scan (see `mark_seen`). A TEMP table
/// lives only on the writer connection: no schema change, nothing in snapshots.
#[cfg(feature = "scanner")]
fn ensure_scan_seen_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS scan_seen(session_id INTEGER NOT NULL, audio_id INTEGER NOT NULL,
         PRIMARY KEY(session_id,audio_id)) WITHOUT ROWID;",
    )
    .map_err(db_error)
}

fn refresh_folder_statistics(conn: &Connection) -> Result<()> {
    // Aggregate audio once, then roll up folder-sized rows instead of rescanning
    // every descendant's audio three times for every ancestor in a large library.
    conn.execute_batch("WITH direct AS MATERIALIZED (
        SELECT folder_id,COUNT(*) AS files,COALESCE(SUM(duration),0) AS duration,
            COALESCE(SUM(file_size),0) AS bytes FROM audio_files WHERE missing=0 GROUP BY folder_id
        ), totals AS MATERIALIZED (
        SELECT fc.ancestor_id,SUM(d.files) AS files,SUM(d.duration) AS duration,SUM(d.bytes) AS bytes
        FROM folder_closure fc JOIN direct d ON d.folder_id=fc.descendant_id GROUP BY fc.ancestor_id)
        UPDATE folders SET file_count=COALESCE((SELECT files FROM direct WHERE folder_id=folders.id),0),
        total_file_count=COALESCE((SELECT files FROM totals WHERE ancestor_id=folders.id),0),
        total_duration=COALESCE((SELECT duration FROM totals WHERE ancestor_id=folders.id),0),
        total_size=COALESCE((SELECT bytes FROM totals WHERE ancestor_id=folders.id),0),
        updated_at=strftime('%Y-%m-%d %H:%M:%f','now');").map_err(db_error)
}

#[cfg(test)]
mod search_folder_plan_tests {
    use super::*;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    fn fixture(stale_statistics: bool) -> (tempfile::TempDir, SqliteProvider) {
        let directory = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&directory.path().join("facet-plan.db")).unwrap();
        let conn = Connection::open(provider.database_path()).unwrap();
        schema::register_tokenizer(&conn, load_tokenizer()).unwrap();
        conn.execute_batch(
            "INSERT INTO folders(id,folder_path,folder_name) VALUES(1,'root','root');
             INSERT INTO folder_closure VALUES(1,1,0);
             INSERT INTO audio_files(id,file_path,filename,folder_id,missing,sample_rate,duration)
             VALUES(1,'root/ambient.wav','ambient.wav',1,0,48000,1.0);",
        )
        .unwrap();
        if stale_statistics {
            conn.execute_batch("ANALYZE;").unwrap();
        }
        conn.execute_batch(
            "WITH RECURSIVE ids(id) AS (
               VALUES(2) UNION ALL SELECT id+1 FROM ids WHERE id<256
             ) INSERT INTO audio_files(id,file_path,filename,folder_id,missing,sample_rate,duration)
               SELECT id,'root/'||id||'.wav','wood.wav',1,0,48000,1.0 FROM ids;",
        )
        .unwrap();
        if stale_statistics {
            let old: String = conn
                .query_row(
                    "SELECT stat FROM sqlite_stat1 WHERE idx='idx_audio_visible_filter'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(old.split_whitespace().next(), Some("1"));
        } else {
            let statistics: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE name='sqlite_stat1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(statistics, 0);
        }
        (directory, provider)
    }

    fn filtered_request() -> SearchRequest {
        SearchRequest {
            query: "ambient".into(),
            filters: audiolib_core::SearchFilters {
                sample_rate: Some(48_000),
                duration_min: Some(0.0),
                duration_max: Some(5.0),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn count_page_and_facet_fallback_plans_keep_match_outer_with_any_statistics() {
        for stale_statistics in [false, true] {
            let (_directory, provider) = fixture(stale_statistics);
            let conn = Connection::open(provider.database_path()).unwrap();
            for visibility_only in [false, true] {
                let request = if visibility_only {
                    SearchRequest {
                        query: "ambient".into(),
                        ..Default::default()
                    }
                } else {
                    filtered_request()
                };
                // This is the production query builder and fallback FROM, used
                // both when no ranking exists and when candidate IDs overflow.
                let SearchSql {
                    conditions, values, ..
                } = search_sql(&provider.tokenizer, &request, false, true).unwrap();
                let from = folder_search_from(true, visibility_only, false);
                let plan = conn
                    .prepare(&format!(
                        "EXPLAIN QUERY PLAN SELECT af.folder_id FROM {from} WHERE {}",
                        conditions.join(" AND ")
                    ))
                    .unwrap()
                    .query_map(params_from_iter(&values), |row| row.get::<_, String>(3))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                assert!(
                    plan[0].starts_with("SCAN audio_search VIRTUAL TABLE INDEX"),
                    "stale={stale_statistics}, visibility_only={visibility_only}: {plan:?}"
                );
                assert!(!plan[0].contains("=M"), "{plan:?}");
                if visibility_only {
                    assert!(
                        plan.iter()
                            .any(|step| step
                                .contains("USING COVERING INDEX idx_audio_search_facets")),
                        "{plan:?}"
                    );
                }
            }
            // Non-relevance sorting and an exhausted ranking budget use this
            // same SearchSql for their joined count and bounded page SELECT.
            let SearchSql {
                from,
                conditions,
                values,
            } = search_sql(&provider.tokenizer, &filtered_request(), true, true).unwrap();
            for (columns, suffix) in [
                ("COUNT(*)", ""),
                (
                    "af.id",
                    "ORDER BY af.filename COLLATE NOCASE,af.id LIMIT 7 OFFSET 3",
                ),
                (
                    "af.id",
                    "ORDER BY bm25(audio_search),af.id LIMIT 7 OFFSET 3",
                ),
            ] {
                let plan = conn
                    .prepare(&format!(
                        "EXPLAIN QUERY PLAN SELECT {columns} FROM {from} WHERE {} {suffix}",
                        conditions.join(" AND ")
                    ))
                    .unwrap()
                    .query_map(params_from_iter(&values), |row| row.get::<_, String>(3))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                assert!(
                    plan[0].starts_with("SCAN audio_search VIRTUAL TABLE INDEX"),
                    "stale={stale_statistics}, projection={columns}, suffix={suffix}: {plan:?}"
                );
                assert!(!plan[0].contains("=M"), "{plan:?}");
            }
            // A fresh provider has no ranking to reuse; its actual fallback must
            // still execute the complete query and retain the filtered count.
            let folders = provider.search_folders(&filtered_request()).unwrap();
            assert_eq!(folders.len(), 1);
            assert_eq!((folders[0].id, folders[0].file_count), (1, 1));
        }
    }

    #[test]
    fn small_cached_filtered_facets_do_not_reopen_match() {
        let (_directory, provider) = fixture(false);
        let request = filtered_request();
        assert_eq!(provider.search(&request).unwrap().total_count, 1);
        assert!(provider.search(&request).unwrap().cache_hit);
        let reader = provider.reader().unwrap();
        // Reusing this one-ID ranking needs only the covering filter-ID query
        // and folder lookups. Reject any attempt to prepare the old FTS join.
        reader
            .authorizer(Some(|context: AuthContext<'_>| {
                if matches!(context.action, AuthAction::Read { table_name, .. }
                    if table_name == "audio_search")
                {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }))
            .unwrap();
        drop(reader);
        let folders = provider.search_folders(&request).unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!((folders[0].id, folders[0].file_count), (1, 1));
        let reader = provider.reader().unwrap();
        reader
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
    }
}

#[cfg(test)]
mod scan_statistics_tests {
    use super::*;

    fn verify_statistics(records: i64) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE folders(id INTEGER PRIMARY KEY,file_count INTEGER,total_file_count INTEGER,total_duration REAL,total_size INTEGER,updated_at TEXT);
            CREATE TABLE audio_files(folder_id INTEGER,missing INTEGER,duration REAL,file_size INTEGER);
            CREATE INDEX by_folder ON audio_files(folder_id);
            CREATE TABLE folder_closure(ancestor_id INTEGER,descendant_id INTEGER,PRIMARY KEY(ancestor_id,descendant_id));
            WITH RECURSIVE ids(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM ids WHERE id<256)
            INSERT INTO folders(id) SELECT id FROM ids;
            WITH RECURSIVE ancestry(ancestor,descendant) AS (
                SELECT id,id FROM folders UNION ALL SELECT ancestor/2,descendant FROM ancestry WHERE ancestor>1)
            INSERT INTO folder_closure SELECT ancestor,descendant FROM ancestry;").unwrap();
        conn.execute("WITH RECURSIVE ids(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM ids WHERE id<?1)
            INSERT INTO audio_files SELECT 128+id%128,id%10=0,CASE WHEN id%7=0 THEN NULL ELSE 1.25 END,
            CASE WHEN id%11=0 THEN NULL ELSE 4096 END FROM ids", [records]).unwrap();
        let started = Instant::now();
        refresh_folder_statistics(&conn).unwrap();
        let elapsed = started.elapsed();
        // Independent direct queries include empty folders, missing files, null
        // source fields and a nine-level hierarchy; annotations are not involved.
        let reference_started = Instant::now();
        let mismatches: i64 = conn.query_row("SELECT COUNT(*) FROM folders f WHERE
            file_count<>(SELECT COUNT(*) FROM audio_files WHERE folder_id=f.id AND missing=0)
            OR total_file_count<>(SELECT COUNT(*) FROM audio_files a JOIN folder_closure c ON c.descendant_id=a.folder_id WHERE c.ancestor_id=f.id AND a.missing=0)
            OR total_duration<>COALESCE((SELECT SUM(duration) FROM audio_files a JOIN folder_closure c ON c.descendant_id=a.folder_id WHERE c.ancestor_id=f.id AND a.missing=0),0)
            OR total_size<>COALESCE((SELECT SUM(file_size) FROM audio_files a JOIN folder_closure c ON c.descendant_id=a.folder_id WHERE c.ancestor_id=f.id AND a.missing=0),0)", [], |row| row.get(0)).unwrap();
        assert_eq!(mismatches, 0);
        println!(
            "scan statistics: {records} records, 256 folders, 9 levels; aggregate {elapsed:?}; direct reference {:?}",
            reference_started.elapsed()
        );
    }

    #[test]
    fn folder_statistics_match_direct_queries() {
        verify_statistics(10_000);
    }

    #[test]
    #[ignore = "explicit million-record scan finalization benchmark"]
    fn million_record_scan_statistics() {
        verify_statistics(1_000_000);
    }
}

#[cfg(test)]
mod lease_tests {
    use super::*;

    #[test]
    fn idle_check_reports_a_held_scan_lease_without_keeping_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let provider = SqliteProvider::open(&path).unwrap();
        provider.ensure_library_idle().unwrap();
        // Probing twice works: the probe never keeps the lease.
        provider.ensure_library_idle().unwrap();
        let held = management::LibraryLease::acquire(&path).unwrap();
        assert!(matches!(
            provider.ensure_library_idle(),
            Err(Error::Busy(_))
        ));
        assert!(matches!(provider.remove_audio(&[1]), Err(Error::Busy(_))));
        drop(held);
        provider.ensure_library_idle().unwrap();
    }
}

#[cfg(test)]
mod stored_path_tests {
    use super::{display_path, folder_path_text, path_key};
    use std::path::Path;

    /// Windows-style inputs are plain strings here, so this runs on every platform.
    #[test]
    fn stored_paths_drop_the_verbatim_prefix_and_folders_use_forward_slashes() {
        let disk = Path::new(r"\\?\C:\Users\me\Temp\.tmpAb12\sounds\Doors\Inner");
        assert_eq!(
            display_path(disk),
            r"C:\Users\me\Temp\.tmpAb12\sounds\Doors\Inner"
        );
        assert_eq!(
            folder_path_text(disk),
            "C:/Users/me/Temp/.tmpAb12/sounds/Doors/Inner"
        );
        let plain = Path::new(r"C:\Users\me\sounds\Doors");
        assert_eq!(folder_path_text(plain), "C:/Users/me/sounds/Doors");
        let unc = Path::new(r"\\?\UNC\nas\share\SFX");
        assert_eq!(display_path(unc), r"\\nas\share\SFX");
        assert_eq!(folder_path_text(unc), "//nas/share/SFX");
        let unix = Path::new("/Volumes/SFX/Doors");
        assert_eq!(display_path(unix), "/Volumes/SFX/Doors");
        assert_eq!(folder_path_text(unix), "/Volumes/SFX/Doors");
        // The lookup key agrees for the stored file text and the canonical input.
        assert_eq!(
            path_key(&display_path(disk)),
            path_key(&disk.to_string_lossy())
        );
    }
}

#[cfg(test)]
mod search_count_tests {
    use super::*;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn fts_count_density_guard_avoids_correlated_count_above_128_non_present_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let provider = SqliteProvider::open(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        schema::register_tokenizer(&conn, load_tokenizer()).unwrap();
        conn.execute_batch(
            "WITH RECURSIVE ids(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM ids WHERE id<4096)
             INSERT INTO audio_files(id,file_path,filename,missing)
             SELECT id,printf('root/%d.wav',id),
             CASE WHEN id%2=0 THEN 'metal.wav' ELSE 'wood.wav' END,0 FROM ids;",
        )
        .unwrap();
        for (missing, expected_counts) in [(0, 2), (128, 3), (129, 2), (4096, 2)] {
            conn.execute(
                "UPDATE audio_files SET missing=CASE WHEN id<=?1
                 THEN CASE id%3 WHEN 0 THEN 1 WHEN 1 THEN 2 ELSE -1 END ELSE 0 END",
                [missing],
            )
            .unwrap();
            let count_calls = Arc::new(AtomicUsize::new(0));
            let observed = count_calls.clone();
            let reader = provider.reader().unwrap();
            reader
                .authorizer(Some(move |context: AuthContext<'_>| {
                    if let AuthAction::Function { function_name } = context.action
                        && function_name.eq_ignore_ascii_case("count")
                    {
                        let calls = observed.fetch_add(1, Ordering::AcqRel) + 1;
                        // The bounded probe and joined count need two aggregates.
                        // Preparing a third aggregate would enter the correlated
                        // missing-match count and must fail for a dense library.
                        if missing > 128 && calls > 2 {
                            return Authorization::Deny;
                        }
                    }
                    Authorization::Allow
                }))
                .unwrap();
            drop(reader);
            let page = provider
                .search(&SearchRequest {
                    query: "metal".into(),
                    // Keep exercising the generic count density guard. Filename
                    // pages can now count exact complete membership without COUNT.
                    order_by: SortOrder::Modified,
                    offset: 10_000,
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(page.total_count, 2048 - missing as u64 / 2);
            assert!(page.results.is_empty());
            assert_eq!(count_calls.load(Ordering::Acquire), expected_counts);
            let reader = provider.reader().unwrap();
            reader
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
        }
    }
}

#[cfg(test)]
mod tag_term_tests {
    use super::*;
    use audiolib_core::SearchFilters;
    use std::path::Path;

    fn tag_library(path: &Path) -> SqliteProvider {
        let provider = SqliteProvider::open(path).unwrap();
        let conn = Connection::open(path).unwrap();
        schema::register_tokenizer(&conn, load_tokenizer()).unwrap();
        conn.execute_batch(
            "INSERT INTO folders(id,folder_path,folder_name) VALUES(1,'root','root');
             INSERT INTO folder_closure VALUES(1,1,0);",
        )
        .unwrap();
        for (id, user_tags, keywords, missing) in [
            (1, Some("engine, exterior"), "", 0),
            (2, Some("Engine, interior"), "", 0),
            (3, Some(""), "Engine; Rain", 0),
            (4, Some("door"), "Interior", 0),
            (5, None, "", 0),
            (6, Some("ENGINE, Exterior, rain"), "", 0),
            (7, Some("engine"), "", 1),
        ] {
            let metadata = if keywords.is_empty() {
                "{}".to_owned()
            } else {
                serde_json::json!({"wav_metadata": {"fields": {"keywords": keywords}}}).to_string()
            };
            conn.execute(
                "INSERT INTO audio_files(id,file_path,filename,folder_id,user_tags,metadata_json,
                 missing,source_tags_text)
                 VALUES(?1,?2,'sound.wav',1,?3,?4,?5,COALESCE(audiolib_source_tags_key(?4),''))",
                params![id, format!("root/{id}.wav"), user_tags, metadata, missing],
            )
            .unwrap();
        }
        provider
    }

    /// Matching IDs; the count and the directory facet must agree with the page.
    fn matching(provider: &SqliteProvider, query: &str, filters: &SearchFilters) -> Vec<i64> {
        let request = SearchRequest {
            query: query.into(),
            filters: filters.clone(),
            ..Default::default()
        };
        let page = provider.search(&request).unwrap();
        let mut ids: Vec<i64> = page.results.iter().map(|file| file.id).collect();
        ids.sort_unstable();
        assert_eq!(page.total_count as usize, ids.len(), "{filters:?}");
        let folders = provider.search_folders(&request).unwrap();
        let facet = folders
            .iter()
            .find(|folder| folder.id == 1)
            .map_or(0, |folder| folder.file_count);
        assert_eq!(facet as usize, ids.len(), "facet for {filters:?}");
        ids
    }

    #[test]
    fn include_terms_all_match_and_any_exclude_term_removes_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let provider = tag_library(&dir.path().join("library.db"));
        let filters = |contains: Option<&str>, include: &[&str], exclude: &[&str]| SearchFilters {
            tag_contains: contains.map(Into::into),
            include_tags: include.iter().map(|tag| (*tag).into()).collect(),
            exclude_tags: exclude.iter().map(|tag| (*tag).into()).collect(),
            ..Default::default()
        };
        let cases = [
            // Include only: library and extracted tags, case-insensitive.
            (filters(Some("engine"), &[], &[]), vec![1, 2, 3, 6]),
            (filters(None, &["engine", "EXTERIOR"], &[]), vec![1, 6]),
            (filters(Some("engine"), &["rain"], &[]), vec![3, 6]),
            // Exclude only: untagged files remain, extracted tags exclude too.
            (filters(None, &[], &["interior"]), vec![1, 3, 5, 6]),
            (filters(None, &[], &["Engine", "door"]), vec![5]),
            (filters(None, &[], &["%"]), vec![1, 2, 3, 4, 5, 6]),
            // Both sides.
            (filters(Some("engine"), &[], &["rain"]), vec![1, 2]),
            (filters(Some("engine"), &[], &["rain", "INTERIOR"]), vec![1]),
            // A term on both sides matches nothing, whatever its case.
            (filters(Some("engine"), &[], &["ENGINE"]), vec![]),
            (filters(None, &["Rain"], &["rain"]), vec![]),
            (
                SearchFilters {
                    tagged_only: true,
                    ..filters(None, &[], &["engine"])
                },
                vec![4],
            ),
        ];
        for ready in [false, true] {
            provider.derived_tags.store(ready, Ordering::Release);
            for query in ["", "sound"] {
                for (filters, expected) in &cases {
                    assert_eq!(
                        &matching(&provider, query, filters),
                        expected,
                        "{filters:?} query={query:?} derived={ready}"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod search_cancellation_tests {
    use super::*;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

    #[test]
    fn cached_ranking_cancellation_stops_before_metadata_hydration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let provider = SqliteProvider::open(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        schema::register_tokenizer(&conn, load_tokenizer()).unwrap();
        conn.execute(
            "INSERT INTO audio_files(file_path,filename) VALUES('metal.wav','metal.wav')",
            [],
        )
        .unwrap();
        let request = SearchRequest {
            query: "metal".into(),
            ..Default::default()
        };
        assert!(!provider.search(&request).unwrap().cache_hit);
        let cancel = Arc::new(AtomicBool::new(false));
        let hydrated = Arc::new(AtomicBool::new(false));
        let reader = provider.reader().unwrap();
        let signal = cancel.clone();
        let read_metadata = hydrated.clone();
        // Cancel after cache lookup while preparing the zero-row missing-ID scan.
        // This tiny SQL does not reach the VM progress interval. Rust must observe
        // cancellation before it walks the retained IDs or prepares DTO hydration.
        reader
            .authorizer(Some(move |context: AuthContext<'_>| {
                if let AuthAction::Read {
                    table_name,
                    column_name,
                    ..
                } = context.action
                {
                    if table_name == "audio_files" && column_name == "missing" {
                        signal.store(true, Ordering::Release);
                    }
                    if table_name == "json_each" {
                        read_metadata.store(true, Ordering::Release);
                    }
                }
                Authorization::Allow
            }))
            .unwrap();
        drop(reader);
        assert!(matches!(
            provider.search_cancellable(&request, cancel),
            Err(Error::Cancelled)
        ));
        assert!(!hydrated.load(Ordering::Acquire));
        let reader = provider.reader().unwrap();
        reader
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        drop(reader);
        assert!(provider.search(&request).unwrap().cache_hit);
    }

    #[test]
    fn empty_search_cancellation_after_reader_checkout_is_observed() {
        cancelled_empty_query_after_reader_checkout(false);
    }

    #[test]
    fn empty_facets_cancellation_after_reader_checkout_is_observed() {
        cancelled_empty_query_after_reader_checkout(true);
    }

    fn cancelled_empty_query_after_reader_checkout(facets: bool) {
        let dir = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&dir.path().join("library.db")).unwrap();
        let request = SearchRequest::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let reader = provider.reader().unwrap();
        let cancel_on_begin = cancel.clone();
        // BEGIN is prepared only after the cancellable query has checked out this
        // exact pooled connection and installed its progress handler. Cancel here
        // without scheduler races, sleeps, or taking the reader gate recursively.
        reader
            .authorizer(Some(move |context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Transaction {
                        operation: TransactionOperation::Begin
                    }
                ) {
                    cancel_on_begin.store(true, Ordering::Release);
                }
                Authorization::Allow
            }))
            .unwrap();
        drop(reader);
        let result = if facets {
            provider
                .search_folders_cancellable(&request, cancel.clone())
                .map(|folders| assert!(folders.is_empty()))
        } else {
            provider
                .search_cancellable(&request, cancel.clone())
                .map(|page| assert_eq!(page.total_count, 0))
        };
        assert!(cancel.load(Ordering::Acquire));
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");

        // The flag remains cancelled: a later long query on the same reader would
        // fail if its previous progress handler leaked into the pool.
        let reader = provider.reader().unwrap();
        assert!(
            reader.is_autocommit(),
            "Cancelled query left an open transaction"
        );
        reader
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        let total: i64 = reader
            .query_row(
                "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<10000) SELECT sum(x) FROM n",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(total, 50_005_000);
        drop(reader);
        assert_eq!(provider.search(&request).unwrap().total_count, 0);
        assert!(provider.search_folders(&request).unwrap().is_empty());
        assert_eq!(provider.stats().unwrap().audio_files, 0);
    }

    #[test]
    fn revision_observer_tracks_own_external_and_restored_commits() {
        let dir = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&dir.path().join("library.db")).unwrap();
        let initial = provider.search_revision().unwrap();
        assert!(initial.is_some());
        assert_eq!(initial, provider.search_revision().unwrap());
        provider
            .writer
            .lock()
            .unwrap()
            .execute_batch("CREATE TABLE revision_probe(value INTEGER)")
            .unwrap();
        let own = provider.search_revision().unwrap();
        assert_ne!(initial, own);
        let readonly = SqliteProvider::open_read_only(provider.database_path()).unwrap();
        let readonly_before = readonly.search_revision().unwrap();
        let external = Connection::open(provider.database_path()).unwrap();
        external
            .execute("INSERT INTO revision_probe VALUES (?1)", [1])
            .unwrap();
        let changed = provider.search_revision().unwrap();
        assert_ne!(own, changed);
        assert_ne!(readonly_before, readonly.search_revision().unwrap());
        // Pooled reads cannot switch or reset the observer's connection-local token.
        provider.search(&SearchRequest::default()).unwrap();
        assert_eq!(changed, provider.search_revision().unwrap());
        drop(external);
        drop(readonly);
        let backup = dir.path().join("backup.db");
        provider.backup_library(&backup).unwrap();
        provider
            .maintain_library(audiolib_core::MaintenanceAction::RebuildSearch)
            .unwrap();
        let maintained = provider.search_revision().unwrap();
        assert_ne!(changed, maintained);
        provider.restore_library(&backup).unwrap();
        assert_ne!(maintained, provider.search_revision().unwrap());
        assert_eq!(
            provider
                .search(&SearchRequest::default())
                .unwrap()
                .total_count,
            0
        );
    }

    #[test]
    fn source_tag_sort_keeps_unicode_order_when_derived_keys_become_available() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("library.db");
        let provider = SqliteProvider::open(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        schema::register_tokenizer(&connection, load_tokenizer()).unwrap();
        for (id, tag) in [(1, "Straße"), (2, "strassz")] {
            let metadata =
                serde_json::json!({"wav_metadata": {"fields": {"category": tag}}}).to_string();
            connection.execute(
                "INSERT INTO audio_files(id,file_path,filename,metadata_json,missing,source_tags_text)
                 VALUES(?1,?2,'sound.wav',?3,0,audiolib_source_tags_key(?3))",
                params![id, format!("{id}.wav"), metadata],
            ).unwrap();
        }
        drop(connection);
        for ready in [false, true] {
            provider.derived_tags.store(ready, Ordering::Release);
            for (descending, expected) in [(false, vec![1, 2]), (true, vec![2, 1])] {
                let page = provider
                    .search(&SearchRequest {
                        order_by: SortOrder::Tags,
                        descending,
                        ..Default::default()
                    })
                    .unwrap();
                assert_eq!(
                    page.results.iter().map(|row| row.id).collect::<Vec<_>>(),
                    expected,
                    "Unicode source tag ordering changed with derived readiness {ready}"
                );
            }
        }
    }

    #[test]
    fn tag_sort_shares_unicode_keys_and_blank_fallback_across_stable_fts_pages() {
        const INITIAL_MODIFIED: &str = "2000-01-01 00:00:00.000";
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unicode-tags.db");
        let provider = SqliteProvider::open(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        schema::register_tokenizer(&connection, load_tokenizer()).unwrap();
        for (id, tags, source, missing) in [
            (1, None, "Straße", false),
            (2, Some(""), "strassz", false),
            (3, Some("  STRASSE\u{3000}"), "zulu", false),
            (4, Some("\u{3000}\u{2003}\t\r\n"), "alpha", false),
            (5, Some("\u{a0}\u{2028}"), "", false),
            (6, Some("Zulu"), "aardvark", false),
            (7, None, "aardvark", true),
            (8, Some("\u{3000}Straße\u{3000}"), "zulu", false),
        ] {
            let metadata = serde_json::json!({
                "wav_metadata": {"fields": {"category": source}},
                "private": {"retained": id}
            })
            .to_string();
            connection
                .execute(
                    "INSERT INTO audio_files(id,file_path,filename,user_tags,metadata_json,
                     missing,source_tags_text,modified_at) VALUES(?1,?2,'metal.wav',?3,?4,?5,
                     audiolib_source_tags_key(?4),?6)",
                    params![
                        id,
                        format!("{id}.wav"),
                        tags,
                        metadata,
                        missing,
                        INITIAL_MODIFIED
                    ],
                )
                .unwrap();
        }
        drop(connection);
        let original = provider.get_audio(1).unwrap().attributes;
        // Copying the displayed source tag into the editable annotation must not
        // change its key. Both kinds of blank annotations must select the source.
        for tags in [
            "",
            "Straße",
            "\u{3000}Straße\u{3000}",
            "\u{3000}\u{2003}\t\r\n",
        ] {
            provider
                .update_metadata(
                    1,
                    &MetadataPatch {
                        tags: Some(tags.into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            let file = provider.get_audio(1).unwrap();
            assert_eq!(file.tags, tags);
            let modified_at = file.attributes["modified_at"].as_str().unwrap();
            let modified =
                chrono::NaiveDateTime::parse_from_str(modified_at, "%Y-%m-%d %H:%M:%S%.f").unwrap();
            let initial =
                chrono::NaiveDateTime::parse_from_str(INITIAL_MODIFIED, "%Y-%m-%d %H:%M:%S%.f")
                    .unwrap();
            assert!(modified > initial);
            // The DTO includes the library edit timestamp; every original Metadata
            // value, including unknown extension fields, must otherwise be retained.
            let mut expected_attributes = original.clone();
            expected_attributes["modified_at"] = modified_at.into();
            assert_eq!(file.attributes, expected_attributes);
            assert_eq!(
                audiolib_core::display_tags(&file.tags, &file.attributes),
                "Straße"
            );
            let after_edit = file.attributes.clone();
            for ready in [false, true] {
                provider.derived_tags.store(ready, Ordering::Release);
                for query in ["", "metal"] {
                    for (descending, expected) in [
                        (false, vec![5, 4, 1, 3, 8, 2, 6]),
                        (true, vec![6, 2, 1, 3, 8, 4, 5]),
                    ] {
                        let mut actual = Vec::new();
                        for offset in [0, 2, 4, 6, 8] {
                            let request = SearchRequest {
                                query: query.into(),
                                order_by: SortOrder::Tags,
                                descending,
                                limit: 2,
                                offset,
                                ..Default::default()
                            };
                            let page = provider.search(&request).unwrap();
                            let repeated = provider.search(&request).unwrap();
                            let expected_page = expected
                                .iter()
                                .copied()
                                .skip(offset)
                                .take(2)
                                .collect::<Vec<_>>();
                            let ids = page.results.iter().map(|file| file.id).collect::<Vec<_>>();
                            assert_eq!(page.total_count, 7);
                            assert_eq!(page.returned_count, expected_page.len());
                            assert_eq!(
                                ids, expected_page,
                                "tags={tags:?}, ready={ready}, {request:?}"
                            );
                            assert_eq!(
                                ids,
                                repeated
                                    .results
                                    .iter()
                                    .map(|file| file.id)
                                    .collect::<Vec<_>>()
                            );
                            for file in &page.results {
                                assert_eq!(file.attributes["private"]["retained"], file.id);
                                assert!(!file.missing);
                            }
                            actual.extend(ids);
                        }
                        assert_eq!(actual, expected);
                    }
                }
            }
            let after_queries = provider.get_audio(1).unwrap();
            assert_eq!(after_queries.tags, tags);
            assert_eq!(after_queries.attributes, after_edit);
        }
        assert_eq!(
            provider.get_audio(4).unwrap().tags,
            "\u{3000}\u{2003}\t\r\n"
        );
        assert_eq!(provider.get_audio(5).unwrap().tags, "\u{a0}\u{2028}");
        assert_eq!(
            provider.get_audio(8).unwrap().tags,
            "\u{3000}Straße\u{3000}"
        );
    }

    #[test]
    fn cancelled_searches_do_not_poison_pooled_readers() {
        let dir = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&dir.path().join("library.db")).unwrap();
        let request = SearchRequest::default();
        let cancel = Arc::new(AtomicBool::new(true));
        assert!(matches!(
            provider.search_cancellable(&request, cancel.clone()),
            Err(Error::Cancelled)
        ));
        assert!(matches!(
            provider.search_folders_cancellable(&request, cancel.clone()),
            Err(Error::Cancelled)
        ));
        cancel.store(false, Ordering::Release);
        // Cancel after acquiring the actual search lease: SQLite's VM must interrupt
        // in-flight execution, and Drop must remove the callback before pool reuse.
        let reader = provider.search_reader(Some(cancel.clone())).unwrap();
        cancel.store(true, Ordering::Release);
        let result = reader.query_row("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000) SELECT sum(x) FROM n", [], |row| row.get::<_, i64>(0)).map_err(db_error);
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        drop(reader);
        let reader = provider.reader().unwrap();
        let total: i64 = reader.query_row("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<10000) SELECT sum(x) FROM n", [], |row| row.get(0)).unwrap();
        assert_eq!(total, 50_005_000);
        drop(reader);
        assert_eq!(provider.search(&request).unwrap().total_count, 0);
        assert!(provider.search_folders(&request).unwrap().is_empty());
        assert_eq!(provider.stats().unwrap().audio_files, 0);
    }
}

#[cfg(all(test, feature = "scanner"))]
mod finish_interrupt_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn interrupt_before_finish_marks_the_session_cancelled() {
        let root = tempfile::tempdir().unwrap();
        let sounds = root.path().join("sounds");
        std::fs::create_dir(&sounds).unwrap();
        let provider = SqliteProvider::open(&root.path().join("library.db")).unwrap();
        let session = provider.start_scan(&sounds, true).unwrap();
        let mut progress = audiolib_core::ScanProgress {
            session_id: session,
            state: audiolib_core::ScanState::Completed,
            ..Default::default()
        };
        let interrupt = Arc::new(AtomicBool::new(true));
        let started = Instant::now();
        let error = provider
            .finish_scan(&sounds, &mut progress, true, true, &interrupt)
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(error, Error::Cancelled));
        let status: String = Connection::open(provider.database_path())
            .unwrap()
            .query_row(
                "SELECT status FROM scan_sessions WHERE id=?1",
                [session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "cancelled");
    }

    #[test]
    fn interrupt_during_statement_returns_cancelled_and_the_connection_recovers() {
        let mut conn = Connection::open_in_memory().unwrap();
        let interrupt = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&interrupt);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            flag.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let result = run_interruptible(&mut conn, &interrupt, |conn| {
            conn.query_row(
                "WITH RECURSIVE n(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM n WHERE x<1000000000) SELECT SUM(x) FROM n",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(db_error)
        });
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
