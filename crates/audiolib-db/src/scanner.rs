// crates/audiolib-db/src/scanner.rs
//! Bounded enumeration and parallel Metadata probing feed one cancellable batch writer.
use crate::management::LibraryLease;
use crate::{ScannedAudio, SqliteProvider, db_error, display_path, path_key};
use audiolib_core::vendor_import::{VendorScanSummary, is_skipped_name};
use audiolib_core::{Error, Result, ScanIssue, ScanProgress, ScanState, validate_scan_root};
use rusqlite::{Connection, OpenFlags};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub recursive: bool,
    pub incremental: bool,
    pub batch_size: usize,
    /// Metadata workers, 0 selects up to eight workers from available CPU parallelism.
    pub workers: usize,
    pub allowed_roots: Vec<PathBuf>,
}
impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            recursive: true,
            incremental: true,
            batch_size: 128,
            workers: 0,
            allowed_roots: vec![],
        }
    }
}

impl ScanOptions {
    /// Resolve automatic parallelism, leaving two logical CPUs for playback and the UI.
    pub fn worker_count(&self) -> usize {
        if self.workers == 0 {
            std::thread::available_parallelism()
                .map_or(1, |count| count.get().saturating_sub(2).clamp(1, 8))
        } else {
            self.workers
        }
    }
}

/// Shared nonblocking scan controls. Use a fresh token for every scan session.
#[derive(Clone, Default)]
pub struct ScanCancellation(Arc<ScanControl>);
#[derive(Default)]
struct ScanControl {
    cancelled: AtomicBool,
    paused: AtomicBool,
    finishing: AtomicBool,
    /// Aborts the final folder-statistics statement. Independent of `cancelled` so a
    /// close can release that statement without changing enumeration-cancel timing.
    interrupt: Arc<AtomicBool>,
    gate: Mutex<()>,
    wake: Condvar,
}
impl ScanCancellation {
    /// Cancel queued work and wake paused workers. A final transaction is allowed to drain.
    pub fn cancel(&self) {
        let _guard = self.0.gate.lock().unwrap();
        if !self.is_finishing() {
            self.0.cancelled.store(true, Ordering::Release);
            self.0.paused.store(false, Ordering::Release);
            self.0.wake.notify_all();
        }
    }
    /// Stop enumeration and abort the final folder-statistics statement.
    /// Unlike [`Self::cancel`], this still applies while that statement is draining.
    pub fn interrupt(&self) {
        let _guard = self.0.gate.lock().unwrap();
        self.0.interrupt.store(true, Ordering::Release);
        self.0.cancelled.store(true, Ordering::Release);
        self.0.paused.store(false, Ordering::Release);
        self.0.wake.notify_all();
    }
    /// Abort only the final folder-statistics statement. Enumeration keeps running
    /// until [`Self::cancel`] or [`Self::interrupt`].
    ///
    /// Close sets this before Metadata drain so that statement can release the
    /// writer. A failed close must call [`Self::clear_finish_interrupt`]; leaving
    /// the flag set aborts a later rollup and keeps folder statistics stale.
    pub fn interrupt_finish(&self) {
        self.0.interrupt.store(true, Ordering::Release);
    }
    /// Drop a finish-only interrupt after close is abandoned.
    /// [`Self::interrupt`] also marks the scan cancelled and is left intact, so
    /// quit can still abort the folder-statistics statement.
    pub fn clear_finish_interrupt(&self) {
        let _guard = self.0.gate.lock().unwrap();
        if !self.is_cancelled() {
            self.0.interrupt.store(false, Ordering::Release);
        }
    }
    /// Whether [`Self::interrupt`] or [`Self::interrupt_finish`] requested the final statement to stop.
    pub fn is_interrupted(&self) -> bool {
        self.0.interrupt.load(Ordering::Acquire)
    }
    pub(crate) fn interrupt_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0.interrupt)
    }
    /// Whether cancellation was accepted before final reconciliation.
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
    /// Request a pause without waiting for filesystem or database work on the caller.
    pub fn pause(&self) -> bool {
        let _guard = self.0.gate.lock().unwrap();
        if self.is_cancelled() || self.is_finishing() {
            return false;
        }
        self.0.paused.store(true, Ordering::Release);
        true
    }
    /// Continue the same in-memory scan, retaining its queue, counters and session.
    pub fn resume(&self) {
        let _guard = self.0.gate.lock().unwrap();
        self.0.paused.store(false, Ordering::Release);
        self.0.wake.notify_all();
    }
    /// A requested pause; ScanProgress::paused acknowledges the consumer boundary.
    pub fn is_pause_requested(&self) -> bool {
        self.0.paused.load(Ordering::Acquire)
    }
    /// Whether the final atomic database transaction is draining.
    pub fn is_finishing(&self) -> bool {
        self.0.finishing.load(Ordering::Acquire)
    }
    fn wait(&self) {
        let guard = self.0.gate.lock().unwrap();
        if self.is_pause_requested() && !self.is_cancelled() {
            // The timeout also lets the producer observe its independent stop flag on errors.
            drop(
                self.0
                    .wake
                    .wait_timeout(guard, Duration::from_millis(100))
                    .unwrap(),
            );
        }
    }
    fn checkpoint(
        &self,
        progress: &mut ScanProgress,
        callback: &mut impl FnMut(ScanProgress),
    ) -> bool {
        while self.is_pause_requested() && !self.is_cancelled() {
            progress.paused = true;
            callback(progress.clone());
            self.wait();
        }
        if progress.paused {
            progress.paused = false;
            callback(progress.clone());
        }
        !self.is_cancelled()
    }
    fn begin_finish(&self, progress: &mut ScanProgress, callback: &mut impl FnMut(ScanProgress)) {
        loop {
            self.checkpoint(progress, callback);
            let _guard = self.0.gate.lock().unwrap();
            if !self.is_pause_requested() {
                self.0.finishing.store(true, Ordering::Release);
                progress.finalizing = true;
                break;
            }
        }
        callback(progress.clone());
    }
}

pub struct ScanService {
    provider: Arc<SqliteProvider>,
}

const MAX_SCAN_ISSUES: usize = 100;
const VENDOR_SPOOL_PAGES: u32 = 128 * 1024 * 1024 / 4096;

/// Derived path queue only: source tables stay untouched and memory does not
/// grow with the directory tree. Connection fields close before TempDir cleanup.
struct VendorTableSpool {
    connection: Connection,
    directory: tempfile::TempDir,
}

impl VendorTableSpool {
    fn new(scan_root: &Path) -> Result<Self> {
        Self::with_page_limit(scan_root, VENDOR_SPOOL_PAGES)
    }

    fn with_page_limit(scan_root: &Path, pages: u32) -> Result<Self> {
        if !(2..=VENDOR_SPOOL_PAGES).contains(&pages) {
            return Err(Error::Validation(
                "Invalid automatic table spool budget".into(),
            ));
        }
        let parent = std::env::temp_dir().canonicalize()?;
        if parent.starts_with(scan_root) {
            return Err(Error::Validation(
                "Automatic table staging requires a temporary directory outside the scan root"
                    .into(),
            ));
        }
        #[cfg(windows)]
        if matches!(parent.components().next(), Some(std::path::Component::Prefix(prefix))
            if matches!(prefix.kind(), std::path::Prefix::UNC(..) | std::path::Prefix::VerbatimUNC(..)))
        {
            return Err(Error::Validation(
                "Automatic table staging requires a local temporary directory".into(),
            ));
        }
        let directory = tempfile::Builder::new()
            .prefix("audiolib-scan-tables-")
            .tempdir_in(parent)?;
        let connection = Connection::open(directory.path().join("paths.db")).map_err(db_error)?;
        // The spool is disposable, never a business/recovery database. Each
        // autocommitted path has a bounded memory journal and a 128 MiB file cap.
        connection
            .execute_batch(&format!(
                "PRAGMA page_size=4096; PRAGMA max_page_count={pages};
             PRAGMA journal_mode=MEMORY; PRAGMA synchronous=OFF; PRAGMA cache_size=-512;
             CREATE TABLE paths(path TEXT PRIMARY KEY NOT NULL) WITHOUT ROWID;"
            ))
            .map_err(db_error)?;
        Ok(Self {
            connection,
            directory,
        })
    }

    fn push(&self, path: &Path) -> Result<()> {
        let path = path.to_str().ok_or_else(|| {
            Error::Validation("Automatic table source path is not valid Unicode".into())
        })?;
        self.connection
            .prepare_cached("INSERT OR IGNORE INTO paths(path) VALUES(?1)")
            .map_err(db_error)?
            .execute([path])
            .map_err(db_error)?;
        Ok(())
    }

    fn reader(&self) -> Result<Connection> {
        Connection::open_with_flags(
            self.directory.path().join("paths.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(db_error)
    }
}

fn vendor_total(base: VendorScanSummary, table: VendorScanSummary) -> VendorScanSummary {
    VendorScanSummary {
        tables: base.tables.saturating_add(table.tables),
        rows: base.rows.saturating_add(table.rows),
        matched: base.matched.saturating_add(table.matched),
        updated: base.updated.saturating_add(table.updated),
        unchanged: base.unchanged.saturating_add(table.unchanged),
        ambiguous: base.ambiguous.saturating_add(table.ambiguous),
        unmatched: base.unmatched.saturating_add(table.unmatched),
    }
}

fn record_issue(progress: &mut ScanProgress, path: String, message: String) {
    progress.errors += 1;
    progress.current_path = path.clone();
    progress.message = message.clone();
    if progress.error_details.len() < MAX_SCAN_ISSUES {
        progress.error_details.push(ScanIssue { path, message });
    } else {
        progress.errors_truncated += 1;
    }
}
impl ScanService {
    pub fn new(provider: Arc<SqliteProvider>) -> Self {
        Self { provider }
    }

    /// Run on a background thread. Partial, cancelled, failed and empty scans never mark
    /// files missing (an unreachable or slow root only delays or fails the scan).
    pub fn scan(
        &self,
        root: &Path,
        options: &ScanOptions,
        cancellation: &ScanCancellation,
        mut progress_callback: impl FnMut(ScanProgress),
    ) -> Result<ScanProgress> {
        if options.batch_size == 0 || options.batch_size > 10_000 {
            return Err(Error::Validation(
                "Scan batch_size must be 1..=10000".into(),
            ));
        }
        if options.workers > 16 {
            return Err(Error::Validation("Scan workers must be 0..=16".into()));
        }
        let root = validate_scan_root(root, &options.allowed_roots)?;
        let _lease = LibraryLease::acquire(self.provider.database_path())?;
        let session_id = self.provider.start_scan(&root, options.incremental)?;
        let mut progress = ScanProgress {
            session_id,
            current_path: display_path(&root),
            ..Default::default()
        };
        progress_callback(progress.clone());
        if cancellation.is_cancelled() {
            cancellation.begin_finish(&mut progress, &mut progress_callback);
            progress.state = ScanState::Cancelled;
            self.finish_session(&root, &mut progress, false, options.recursive, cancellation)?;
            progress_callback(progress.clone());
            return Ok(progress);
        }
        let result: Result<()> = (|| {
            let mut vendor_tables = None;
            self.consume(
                &root,
                options,
                cancellation,
                &mut progress,
                &mut progress_callback,
                &mut vendor_tables,
            )?;
            // consume has flushed WAV batches and joined every probe reader.
            // Keep the outer scan lease, but no main-database guard/transaction,
            // while the independent importer takes its own short writer batches.
            if let Some(tables) = &vendor_tables {
                self.import_vendor_tables(
                    tables,
                    &root,
                    cancellation,
                    &mut progress,
                    &mut progress_callback,
                )?;
            }
            Ok(())
        })();
        if matches!(result, Err(Error::Cancelled)) {
            cancellation.cancel();
        } else if let Err(error) = result {
            // Errors must also wake the producer and must never wait for an external resume.
            cancellation.cancel();
            cancellation.begin_finish(&mut progress, &mut progress_callback);
            progress.state = ScanState::Failed;
            let failed_path = progress.current_path.clone();
            record_issue(&mut progress, failed_path, error.to_string());
            self.finish_session(&root, &mut progress, false, options.recursive, cancellation)?;
            progress_callback(progress);
            return Err(error);
        }
        cancellation.begin_finish(&mut progress, &mut progress_callback);
        progress.state = if cancellation.is_cancelled() {
            ScanState::Cancelled
        } else {
            ScanState::Completed
        };
        // A full scan of a root directory that is completely empty (hidden files aside)
        // never marks anything missing: that is far more likely an unmounted network
        // volume (an empty mount-point directory) than a library whose every file and
        // folder was deleted.
        let cleanup = !options.incremental
            && progress.errors == 0
            && progress.state == ScanState::Completed
            && (progress.discovered > 0 || root_has_entries(&root));
        self.finish_session(
            &root,
            &mut progress,
            cleanup,
            options.recursive,
            cancellation,
        )?;
        progress_callback(progress.clone());
        Ok(progress)
    }

    /// Run final reconciliation. A finish-only interrupt waits, then runs again
    /// when that interrupt is cleared (failed close). [`ScanCancellation::interrupt`]
    /// stays terminal so quit does not sit on the folder-statistics statement.
    fn finish_session(
        &self,
        root: &Path,
        progress: &mut ScanProgress,
        cleanup: bool,
        recursive: bool,
        cancellation: &ScanCancellation,
    ) -> Result<()> {
        loop {
            match self.provider.finish_scan(
                root,
                progress,
                cleanup,
                recursive,
                &cancellation.interrupt_flag(),
            ) {
                Err(Error::Cancelled) if !cancellation.is_cancelled() => {
                    while cancellation.is_interrupted() && !cancellation.is_cancelled() {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    if cancellation.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                }
                other => return other,
            }
        }
    }

    fn consume(
        &self,
        root: &Path,
        options: &ScanOptions,
        cancellation: &ScanCancellation,
        progress: &mut ScanProgress,
        callback: &mut impl FnMut(ScanProgress),
        vendor_tables: &mut Option<VendorTableSpool>,
    ) -> Result<()> {
        let mut batch = Vec::with_capacity(options.batch_size);
        let mut seen = Vec::with_capacity(options.batch_size);
        let mut last_folder: Option<PathBuf> = None;
        let mut last_progress = Instant::now();
        let producer_stop = AtomicBool::new(false);
        // Exclude only this provider's verified owned store, never a directory
        // name pattern that could also identify the user's audio library.
        let vendor_owned_root = self.provider.vendor_import_owned_root()?;
        std::thread::scope(|scope| -> Result<()> {
            let workers = options.worker_count();
            // Bound decoded Metadata independently of the transaction size. Each worker
            // holds at most one probe; neither a slow disk nor a slow writer grows a backlog.
            let (sender, jobs) = mpsc::sync_channel(workers * 2);
            let jobs = Arc::new(Mutex::new(jobs));
            let (results, receiver) = mpsc::sync_channel(workers * 2);
            let stop = &producer_stop;
            for _ in 0..workers {
                let jobs = jobs.clone();
                let results = results.clone();
                scope.spawn(move || {
                    let run = || -> Result<()> {
                        let reader = self.provider.reader()?;
                        let mut fingerprint = reader.prepare(FINGERPRINT_SQL).map_err(db_error)?;
                        while producer_ready(cancellation, stop) {
                            // The mutex protects only dequeueing, never file I/O or parsing.
                            let entry = jobs
                                .lock()
                                .unwrap()
                                .recv_timeout(Duration::from_millis(100));
                            let entry = match entry {
                                Ok(entry) => entry,
                                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            };
                            if !producer_ready(cancellation, stop) {
                                break;
                            }
                            let result = match entry {
                                Entry::Failure(path, message) => Probed::Failure(path, message),
                                Entry::Vendor(path) => Probed::Vendor(path),
                                Entry::Audio(path) => {
                                    let result = probe_entry(
                                        &path,
                                        root,
                                        options.incremental,
                                        &mut fingerprint,
                                    );
                                    Probed::Audio(path, result)
                                }
                            };
                            if !producer_ready(cancellation, stop) || results.send(result).is_err()
                            {
                                break;
                            }
                        }
                        Ok(())
                    };
                    if let Err(error) = run() {
                        let _ = results.send(Probed::Fatal(error));
                    }
                });
            }
            // Only workers retain job receivers/result senders, so disconnect propagates
            // through both queues even on error, cancellation, or a paused scan.
            drop(jobs);
            drop(results);
            scope.spawn(move || {
                let mut visited = HashSet::new();
                walk_directory(
                    root,
                    ScanTree {
                        root,
                        excluded: vendor_owned_root.as_deref(),
                    },
                    options.recursive,
                    cancellation,
                    stop,
                    &sender,
                    &mut visited,
                );
            });
            let result = (|| -> Result<()> {
                loop {
                    if !cancellation.checkpoint(progress, callback) {
                        break;
                    }
                    let entry = match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(entry) => entry,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            callback(progress.clone());
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    match entry {
                        Probed::Fatal(error) => return Err(error),
                        Probed::Failure(path, message) => {
                            record_issue(progress, display_path(&path), message);
                            callback(progress.clone());
                        }
                        Probed::Vendor(path) => {
                            if vendor_tables.is_none() {
                                *vendor_tables = Some(VendorTableSpool::new(root)?);
                            }
                            vendor_tables.as_ref().unwrap().push(&path)?;
                        }
                        Probed::Audio(path, result) => {
                            progress.discovered += 1;
                            progress.current_path = display_path(&path);
                            let processed = (|| -> Result<()> {
                                if let Some(folder) = path.parent()
                                    && last_folder.as_deref() != Some(folder)
                                {
                                    self.provider.ensure_scan_folder(folder, root)?;
                                    last_folder = Some(folder.to_path_buf());
                                }
                                match result? {
                                    ProbeOutcome::Unchanged(key) => {
                                        seen.push(key);
                                        progress.unchanged += 1;
                                    }
                                    ProbeOutcome::Changed(item) => batch.push(*item),
                                }
                                Ok(())
                            })();
                            progress.processed += 1;
                            if let Err(error) = processed {
                                record_issue(
                                    progress,
                                    progress.current_path.clone(),
                                    error.to_string(),
                                );
                                callback(progress.clone());
                            }
                            // Cancellation during header probing must not commit the pending batch.
                            if !cancellation.checkpoint(progress, callback) {
                                break;
                            }
                            if batch.len() + seen.len() >= options.batch_size {
                                self.flush(&mut batch, &mut seen, progress)?;
                                callback(progress.clone());
                            }
                            if last_progress.elapsed() >= Duration::from_millis(100) {
                                callback(progress.clone());
                                last_progress = Instant::now();
                            }
                        }
                    }
                }
                if cancellation.checkpoint(progress, callback) {
                    self.flush(&mut batch, &mut seen, progress)?;
                }
                Ok(())
            })();
            // Always release a paused or backpressured producer before the scoped join.
            producer_stop.store(true, Ordering::Release);
            cancellation.0.wake.notify_all();
            drop(receiver);
            result
        })
    }

    fn import_vendor_tables(
        &self,
        tables: &VendorTableSpool,
        root: &Path,
        cancellation: &ScanCancellation,
        progress: &mut ScanProgress,
        callback: &mut impl FnMut(ScanProgress),
    ) -> Result<()> {
        // This cursor belongs only to the local disposable queue. It holds no
        // main-library reader or writer and materializes one source path at a time.
        let reader = tables.reader()?;
        let mut statement = reader
            .prepare("SELECT path FROM paths ORDER BY path")
            .map_err(db_error)?;
        let mut paths = statement.query([]).map_err(db_error)?;
        let mut last_progress = Instant::now();
        while let Some(row) = paths.next().map_err(db_error)? {
            if !cancellation.checkpoint(progress, callback) {
                return Err(Error::Cancelled);
            }
            let path = PathBuf::from(row.get::<_, String>(0).map_err(db_error)?);
            progress.current_path = display_path(&path);
            callback(progress.clone());
            if !cancellation.checkpoint(progress, callback) {
                return Err(Error::Cancelled);
            }
            let base = progress.vendor_metadata;
            let session_id = progress.session_id;
            let mut observed = base;
            let mut checkpoint = |table: &VendorScanSummary| {
                // Importer callbacks are cumulative for this table, including
                // the latest durable batch on cancellation; never sum them twice.
                progress.vendor_metadata = vendor_total(base, *table);
                let durable_change = observed.updated != progress.vendor_metadata.updated
                    || observed.unchanged != progress.vendor_metadata.unchanged;
                let first_table = observed.tables != progress.vendor_metadata.tables;
                observed = progress.vendor_metadata;
                let keep_going = cancellation.checkpoint(progress, callback);
                if first_table
                    || durable_change
                    || last_progress.elapsed() >= Duration::from_millis(100)
                {
                    callback(progress.clone());
                    last_progress = Instant::now();
                }
                // The callback itself may request pause/cancel. Importer invokes
                // us without a main-library guard, so acknowledge it now rather
                // than admitting one more metadata batch.
                keep_going && cancellation.checkpoint(progress, callback)
            };
            let result = self
                .provider
                .scan_vendor_table(&path, root, session_id, &mut checkpoint);
            match result {
                Ok(summary) => progress.vendor_metadata = vendor_total(base, summary),
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(error) => record_issue(progress, display_path(&path), error.to_string()),
            }
            callback(progress.clone());
        }
        Ok(())
    }

    fn flush(
        &self,
        batch: &mut Vec<ScannedAudio>,
        seen: &mut Vec<i64>,
        progress: &mut ScanProgress,
    ) -> Result<()> {
        if !batch.is_empty() {
            let (added, updated) = self.provider.write_scan_batch(batch, progress.session_id)?;
            progress.added += added;
            progress.updated += updated;
            batch.clear();
        }
        if !seen.is_empty() {
            self.provider.mark_seen(seen, progress.session_id)?;
            seen.clear();
        }
        Ok(())
    }
}

enum Entry {
    Audio(PathBuf),
    Vendor(PathBuf),
    Failure(PathBuf, String),
}

enum Probed {
    Audio(PathBuf, Result<ProbeOutcome>),
    Vendor(PathBuf),
    Failure(PathBuf, String),
    Fatal(Error),
}

enum ProbeOutcome {
    Unchanged(i64),
    Changed(Box<ScannedAudio>),
}

const FINGERPRINT_SQL: &str = "SELECT file_path,id,COALESCE(file_size,0),source_mtime_ns,\
    json_extract(CASE WHEN json_valid(audiolib_metadata_json(metadata_json)) THEN audiolib_metadata_json(metadata_json) ELSE '{}' END,\
    '$.container','$.wav_metadata.version','$.wav_metadata.vorbis_version',\
    '$.wav_metadata.mp3_version','$.wav_metadata.aiff_version','$.wav_metadata.container_version')\
    FROM audio_files WHERE path_key=?1";

fn extractor_is_current(path: &Path, row: &rusqlite::Row<'_>) -> rusqlite::Result<bool> {
    use audiolib_core::SourceAudioFormat;
    // SQLite returns only these six fingerprint values. Decode that small array
    // once rather than allocating or decoding the source's complete Metadata.
    let Some(fields) = row
        .get_ref(4)?
        .as_str()
        .ok()
        .filter(|json| json.len() <= 512)
        .and_then(|json| serde_json::from_str::<[serde_json::Value; 6]>(json).ok())
    else {
        return Ok(false);
    };
    let container = fields[0]
        .as_str()
        .map(str::to_ascii_uppercase)
        .unwrap_or_default();
    // Only older RF64/BW64 records need the corrected magic identity. Ordinary
    // RIFF keeps its existing extractor revision and unchanged fast path.
    if matches!(container.as_str(), "RF64" | "BW64") && fields[5].as_i64() != Some(1) {
        return Ok(false);
    }
    let wav = fields[1].as_i64();
    let vorbis = fields[2].as_i64();
    let mp3 = fields[3].as_i64();
    let aiff = fields[4].as_i64();
    let family = match container.as_str() {
        "MP3" => Some(SourceAudioFormat::Mp3),
        "AIFF" | "AIFC" => Some(SourceAudioFormat::Aiff),
        "FLAC" => Some(SourceAudioFormat::Flac),
        "OGG" | "OGG VORBIS" => Some(SourceAudioFormat::Ogg),
        "WAV" | "RIFF" | "RF64" | "BW64" => Some(SourceAudioFormat::Wav),
        _ if mp3.is_some() => Some(SourceAudioFormat::Mp3),
        _ if aiff.is_some() => Some(SourceAudioFormat::Aiff),
        _ if vorbis.is_some() => Some(SourceAudioFormat::Ogg),
        _ => audiolib_core::source_audio_format(path),
    };
    Ok(match family {
        Some(SourceAudioFormat::Wav) => wav == Some(audiolib_audio::WAV_METADATA_VERSION),
        Some(SourceAudioFormat::Flac | SourceAudioFormat::Ogg) => {
            vorbis == Some(audiolib_audio::VORBIS_METADATA_VERSION)
        }
        Some(SourceAudioFormat::Mp3) => mp3 == Some(audiolib_audio::MP3_METADATA_VERSION),
        Some(SourceAudioFormat::Aiff) => aiff == Some(audiolib_audio::AIFF_METADATA_VERSION),
        None => false,
    })
}

fn probe_entry(
    path: &Path,
    root: &Path,
    incremental: bool,
    fingerprint: &mut rusqlite::Statement<'_>,
) -> Result<ProbeOutcome> {
    let info = fs::metadata(path)?;
    let size = info.len();
    let mtime_ns = i64::try_from(
        info.modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Validation("File timestamp precedes Unix epoch".into()))?
            .as_nanos(),
    )
    .map_err(|_| Error::Validation("File timestamp is out of range".into()))?;
    if incremental {
        let text = display_path(path);
        let key = path_key(&text);
        let candidates = fingerprint
            .query_map([&key], |row| {
                // Imported extension Metadata may use another marker type. Refresh
                // those records instead of turning a stale marker into a probe error.
                Ok((
                    row.get::<_, String>(0)?,
                    (
                        row.get::<_, i64>(1)?,
                        crate::unsigned(row, 2)?,
                        row.get::<_, Option<i64>>(3)?,
                        extractor_is_current(path, row)?,
                    ),
                ))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        if let Some((id, prior_size, prior_mtime, current_extractor)) =
            crate::path_identity::choose(&text, candidates, true)?
            && (prior_size, prior_mtime, current_extractor) == (size, Some(mtime_ns), true)
        {
            return Ok(ProbeOutcome::Unchanged(id));
        }
    }
    let metadata = audiolib_audio::probe(path)?;
    Ok(ProbeOutcome::Changed(Box::new(ScannedAudio {
        path: path.to_path_buf(),
        root: root.to_path_buf(),
        size,
        mtime_ns,
        metadata,
    })))
}

#[derive(Clone, Copy)]
struct ScanTree<'a> {
    root: &'a Path,
    excluded: Option<&'a Path>,
}

fn walk_directory(
    directory: &Path,
    tree: ScanTree<'_>,
    recursive: bool,
    cancellation: &ScanCancellation,
    stop: &AtomicBool,
    sender: &SyncSender<Entry>,
    visited: &mut HashSet<PathBuf>,
) {
    if !producer_ready(cancellation, stop) {
        return;
    }
    let canonical = match directory.canonicalize() {
        Ok(path) if path.starts_with(tree.root) => path,
        Ok(_) => {
            let _ = sender.send(Entry::Failure(
                directory.into(),
                "Directory resolves outside scan root".into(),
            ));
            return;
        }
        Err(error) => {
            let _ = sender.send(Entry::Failure(directory.into(), error.to_string()));
            return;
        }
    };
    if tree.excluded == Some(canonical.as_path()) {
        return;
    }
    if !visited.insert(canonical.clone()) {
        let _ = sender.send(Entry::Failure(
            directory.into(),
            "Directory cycle was not traversed".into(),
        ));
        return;
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            let _ = sender.send(Entry::Failure(directory.into(), error.to_string()));
            visited.remove(&canonical);
            return;
        }
    };
    for entry in entries {
        if !producer_ready(cancellation, stop) {
            return;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                if sender
                    .send(Entry::Failure(directory.into(), error.to_string()))
                    .is_err()
                {
                    return;
                }
                continue;
            }
        };
        let path = entry.path();
        match entry.file_type() {
            Ok(kind) if kind.is_symlink() => {
                let sent = sender.send(Entry::Failure(
                    path,
                    "Symbolic link was not traversed; missing-file reconciliation is disabled"
                        .into(),
                ));
                if sent.is_err() {
                    return;
                }
            }
            Ok(kind) if kind.is_dir() && recursive => {
                walk_directory(&path, tree, true, cancellation, stop, sender, visited)
            }
            Ok(kind) if kind.is_file() && audiolib_audio::is_supported_audio_extension(&path) => {
                if sender.send(Entry::Audio(path)).is_err() {
                    return;
                }
            }
            Ok(kind) if kind.is_file() && is_vendor_table(&path) => {
                if sender.send(Entry::Vendor(path)).is_err() {
                    return;
                }
            }
            Err(error) => {
                let sent = sender.send(Entry::Failure(path, error.to_string()));
                if sent.is_err() {
                    return;
                }
            }
            _ => {}
        }
    }
    visited.remove(&canonical);
}

fn is_vendor_table(path: &Path) -> bool {
    !path
        .file_name()
        .is_some_and(|name| is_skipped_name(&name.to_string_lossy()))
        && path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| {
                ["csv", "tsv", "xls", "xlsx"]
                    .iter()
                    .any(|known| extension.eq_ignore_ascii_case(known))
            })
}

/// Whether `root` lists at least one visible entry; unreadable counts as empty (no
/// cleanup). Hidden entries (`.DS_Store`, `._*`, `.localized`, ...) are ignored: macOS
/// Finder leaves them in an unmounted mount-point directory too. So are NAS and OS
/// housekeeping entries (Synology `@eaDir`, `#recycle`, `#snapshot`, Windows
/// `$RECYCLE.BIN`, `System Volume Information`, `Thumbs.db`, `desktop.ini`), which a share
/// or mount point can contain without holding any library content.
fn root_has_entries(root: &Path) -> bool {
    fs::read_dir(root).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| !is_housekeeping_entry(&entry.file_name().to_string_lossy()))
    })
}

fn is_housekeeping_entry(name: &str) -> bool {
    const NAMES: [&str; 7] = [
        "@eaDir",
        "#recycle",
        "#snapshot",
        "$RECYCLE.BIN",
        "System Volume Information",
        "Thumbs.db",
        "desktop.ini",
    ];
    name.starts_with('.') || NAMES.iter().any(|known| name.eq_ignore_ascii_case(known))
}

fn producer_ready(cancellation: &ScanCancellation, stop: &AtomicBool) -> bool {
    while cancellation.is_pause_requested()
        && !cancellation.is_cancelled()
        && !stop.load(Ordering::Acquire)
    {
        cancellation.wait();
    }
    !cancellation.is_cancelled() && !stop.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_path_spool_is_unique_bounded_and_failed_insert_preserves_queued_paths() {
        let source = tempfile::tempdir().unwrap();
        let spool = VendorTableSpool::with_page_limit(source.path(), 2).unwrap();
        let path = source.path().join("catalog.csv");
        spool.push(&path).unwrap();
        spool.push(&path).unwrap();
        assert!(
            spool
                .push(&source.path().join("x".repeat(32 * 1024)))
                .is_err()
        );
        let reader = spool.reader().unwrap();
        let queued: String = reader
            .query_row("SELECT path FROM paths", [], |row| row.get(0))
            .unwrap();
        assert_eq!(queued, path.to_str().unwrap());
        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM paths", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(
            reader.execute("DELETE FROM paths", []).is_err(),
            "iteration is read-only"
        );
    }

    #[test]
    fn cancel_during_finish_does_not_interrupt_and_interrupt_does() {
        let cancellation = ScanCancellation::default();
        let mut progress = ScanProgress::default();
        cancellation.begin_finish(&mut progress, &mut |_| {});
        assert!(cancellation.is_finishing());
        cancellation.cancel();
        assert!(!cancellation.is_cancelled());
        assert!(!cancellation.is_interrupted());
        cancellation.interrupt_finish();
        assert!(cancellation.is_interrupted());
        assert!(!cancellation.is_cancelled());
        cancellation.clear_finish_interrupt();
        assert!(
            !cancellation.is_interrupted(),
            "a finish-only interrupt must be retractable"
        );
        cancellation.interrupt_finish();
        cancellation.interrupt();
        assert!(cancellation.is_cancelled());
        assert!(cancellation.is_interrupted());
        cancellation.clear_finish_interrupt();
        assert!(
            cancellation.is_interrupted() && cancellation.is_cancelled(),
            "quit's full interrupt must survive a later retract"
        );
    }
}
