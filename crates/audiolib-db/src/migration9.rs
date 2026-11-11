// crates/audiolib-db/src/migration9.rs
//! Online, resumable schema 8 → 9 directory-segment migration.
//!
//! The main FTS is never rebuilt. Only directory segments are normalized. A stable
//! OS lock serializes runners; SQLite transactions bind each batch to its checkpoint.
//! Administration leases exclude replacement, while ordinary annotations remain usable.
#[cfg(feature = "test-hooks")]
mod hooks;
mod report;
mod storage;
#[cfg(test)]
mod tests;
use crate::{dir_search, management::AdministrationLease};
use audiolib_core::{DataProvider, Error, Result, SearchRequest, SearchScope};
pub use report::{Batch, Failure, Progress, Report, State, Switch};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// Operations supported by both command and HTTP adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Startup,
    Start,
    Pause,
    Resume,
    Cancel,
    Status,
    Rollback,
}
impl std::str::FromStr for Action {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "startup" => Ok(Self::Startup),
            "start" => Ok(Self::Start),
            "pause" => Ok(Self::Pause),
            "resume" => Ok(Self::Resume),
            "cancel" => Ok(Self::Cancel),
            "status" => Ok(Self::Status),
            "rollback" => Ok(Self::Rollback),
            _ => Err(Error::Validation("Unknown migration action".into())),
        }
    }
}
/// Execution controls. Production cancellation uses an owned atomic flag or control action.
#[derive(Default)]
pub struct Options {
    #[cfg(feature = "test-hooks")]
    pub sql_trace: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    protocol: u32,
    library_id: String,
    backup: Option<String>,
    report: Report,
}

// Keep a separate stable inode: locking the JSON file itself would lose
// exclusion when its atomic publication replaces that inode. All current
// load/status readers share this lease with save. Legacy/read-only sources
// without a report lock remain readable without creating coordination files.
struct ReportLease(fs::File);
impl ReportLease {
    fn reader(path: &Path) -> Result<Option<Self>> {
        let file = match fs::OpenOptions::new()
            .read(true)
            .open(sidecar(path, ".migration9-report.lock"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("report_reader_open", error)),
        };
        file.lock_shared()
            .map_err(|error| io_error("report_reader_lock", error))?;
        Ok(Some(Self(file)))
    }

    fn writer(path: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(sidecar(path, ".migration9-report.lock"))
            .map_err(|error| io_error("report_writer_open", error))?;
        file.lock()
            .map_err(|error| io_error("report_writer_lock", error))?;
        Ok(Self(file))
    }
}
impl Drop for ReportLease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}
fn invalid(message: &str) -> Error {
    Error::Validation(message.into())
}
fn io_error(stage: &'static str, error: std::io::Error) -> Error {
    tracing::warn!(stage, os_code = error.raw_os_error(), kind = ?error.kind(), "Schema-9 filesystem operation failed");
    #[cfg(feature = "test-hooks")]
    eprintln!(
        "SMV2_IO_FAILURE stage={stage} os_code={:?} kind={:?}",
        error.raw_os_error(),
        error.kind()
    );
    Error::Io(error)
}
// Keep diagnostics structural: SQL text, bound Metadata and filesystem paths are
// deliberately excluded. Test-hook runs can retain the native SQLite cause.
fn db_error(error: rusqlite::Error) -> Error {
    if let rusqlite::Error::SqliteFailure(code, _) = &error {
        tracing::warn!(
            sqlite_extended_code = code.extended_code,
            "Schema-9 SQLite operation failed"
        );
        #[cfg(feature = "test-hooks")]
        eprintln!("SMV2_SQLITE_FAILURE extended_code={}", code.extended_code);
        if matches!(
            code.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        ) {
            return Error::Busy("Schema-9 SQLite writer is busy; retry the migration".into());
        }
    }
    crate::db_error(error)
}

// SQLite's default busy handler backs off for increasingly long intervals. A
// stream of short annotation transactions can repeatedly win those intervals.
// Retry only writer admission, without an open transaction or a held write lock.
// Admission has a finite ten-second deadline and cancellation is inspected
// between attempts. This wait owns no writer lock; batch timings measure the
// admitted transaction separately. Restore the ordinary two-second busy timeout
// before executing any SQL inside an admitted transaction.
fn write_admission<T>(
    conn: &Connection,
    cancel: &AtomicBool,
    mut attempt: impl FnMut() -> rusqlite::Result<T>,
) -> Result<T> {
    if !conn.is_autocommit() {
        return Err(invalid(
            "Migration writer admission requires an idle connection",
        ));
    }
    conn.busy_timeout(Duration::ZERO).map_err(db_error)?;
    let started = Instant::now();
    let deadline = Duration::from_secs(10);
    let mut retries = 0u64;
    let result = loop {
        if cancel.load(Ordering::Acquire) {
            break Err(Error::Cancelled);
        }
        match attempt() {
            Ok(value) => break Ok(value),
            Err(error) => {
                let retryable = matches!(&error, rusqlite::Error::SqliteFailure(code, _)
                    if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked));
                let remaining = deadline.saturating_sub(started.elapsed());
                if !retryable || !conn.is_autocommit() || remaining.is_zero() {
                    break Err(db_error(error));
                }
                // Vary the short delay to avoid remaining in phase with a
                // competing writer's repeated commit/reacquire cycle.
                std::thread::sleep(Duration::from_millis(1 + retries % 8).min(remaining));
                retries += 1;
            }
        }
    };
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(db_error)?;
    if retries != 0 {
        tracing::debug!(
            retries,
            wait_ms = started.elapsed().as_millis(),
            "Schema-9 writer admission completed"
        );
    }
    result
}

fn begin_write<'a>(conn: &'a Connection, cancel: &AtomicBool) -> Result<rusqlite::Transaction<'a>> {
    // new_unchecked accepts a shared Connection reference; write_admission
    // requires autocommit and never attempts a nested transaction.
    write_admission(conn, cancel, || {
        rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
    })
}
fn open(path: &Path, write: bool) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        if write {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        },
    )
    .map_err(db_error)?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(db_error)?;
    Ok(conn)
}
fn version(conn: &Connection) -> Result<i64> {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(db_error)
}
fn count(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    value
        .try_into()
        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}
fn identity(conn: &Connection) -> Result<String> {
    crate::albums::namespace(conn)
}
fn exists(conn: &Connection, table: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )
    .map_err(db_error)
}
fn load(path: &Path, conn: &Connection) -> Result<Saved> {
    let id = identity(conn)?;
    let _report = ReportLease::reader(path)?;
    match fs::read(sidecar(path, ".migration9.json")) {
        Ok(bytes) => {
            let saved: Saved = serde_json::from_slice(&bytes)
                .map_err(|_| invalid("Invalid schema-9 migration record"))?;
            if saved.protocol != 1 || saved.library_id != id {
                return Err(invalid("Migration record belongs to another library"));
            }
            if ["backup", "path_build", "optimize", "switch"]
                .iter()
                .any(|key| !saved.report.phases.contains_key(*key))
                || saved
                    .report
                    .phases
                    .values()
                    .any(|ms| !ms.is_finite() || *ms < 0.0)
                || saved
                    .report
                    .batches
                    .iter()
                    .any(|batch| !batch.ms.is_finite() || batch.ms < 0.0)
                || saved
                    .report
                    .eta_s
                    .is_some_and(|eta| !eta.is_finite() || eta < 0.0)
            {
                return Err(invalid("Invalid migration report metrics"));
            }
            Ok(saved)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Saved {
            protocol: 1,
            library_id: id,
            backup: None,
            report: Report::default(),
        }),
        Err(e) => Err(e.into()),
    }
}
fn save(path: &Path, saved: &Saved) -> Result<()> {
    let _report = ReportLease::writer(path)?;
    let mut tmp = tempfile::NamedTempFile::new_in(
        path.parent()
            .ok_or_else(|| invalid("Database parent missing"))?,
    )
    .map_err(|error| io_error("save_create", error))?;
    serde_json::to_writer(tmp.as_file_mut(), saved).map_err(|e| invalid(&e.to_string()))?;
    tmp.as_file_mut()
        .sync_all()
        .map_err(|error| io_error("save_sync", error))?;
    tmp.persist(sidecar(path, ".migration9.json"))
        .map_err(|e| io_error("save_persist", e.error))?;
    Ok(())
}
fn markers(conn: &Connection) -> Result<Switch> {
    let user_version = version(conn)?;
    let source_search_rev = conn
        .query_row(
            "SELECT value FROM library_meta WHERE key='source_search_rev'",
            [],
            |r| r.get::<_, String>(0),
        )
        .map_err(db_error)?
        .parse()
        .map_err(|_| invalid("Invalid search revision"))?;
    if ![2, 3].contains(&source_search_rev) {
        return Err(invalid("Unsupported source search revision"));
    }
    Ok(Switch {
        source_search_rev,
        user_version,
        features: {
            let mut statement = conn.prepare(
                "SELECT feature FROM audiolib_schema_features WHERE (feature='audio_sources' OR (?1=9 AND feature IN ('search_path','vorbis_tags','vendor_metadata'))) AND version=1 ORDER BY feature",
            ).map_err(db_error)?;
            statement
                .query_map([user_version], |row| row.get(0))
                .map_err(db_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db_error)?
        },
    })
}

struct Owner(fs::File);
impl Owner {
    fn acquire(path: &Path) -> Result<Self> {
        // Do not reuse .upgrade.lock: old search readers take that lock. Never
        // unlink this stable inode: releasing a file lock cannot race replacement.
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(sidecar(path, ".migration9.lock"))?;
        file.try_lock().map_err(|e| match e {
            fs::TryLockError::WouldBlock => Error::Busy("Schema-9 runner already active".into()),
            fs::TryLockError::Error(e) => e.into(),
        })?;
        file.set_len(0)?;
        write!(file, "{}", std::process::id())?;
        Ok(Self(file))
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Read durable state without creating a database, sidecar, or migration job.
pub fn status(path: &Path) -> Result<Report> {
    let path = path.canonicalize()?;
    let conn = open(&path, false)?;
    conn.execute_batch("BEGIN").map_err(db_error)?;
    let mut saved = load(&path, &conn)?;
    saved.report.switch = Some(markers(&conn)?);
    if exists(&conn, "v9_checkpoint")? {
        let (done, total): (u64, u64) = conn
            .query_row(
                "SELECT done,total FROM v9_checkpoint WHERE singleton=1",
                [],
                |r| Ok((count(r, 0)?, count(r, 1)?)),
            )
            .map_err(db_error)?;
        saved.report.progress = Progress { done, total };
    }
    if version(&conn)? == 9 {
        saved.report.state = State::Done;
        saved.report.error_code = None;
    } else if matches!(
        saved.report.state,
        State::Running | State::Switching | State::BackingUp | State::Preflight
    ) && !owner_active(&path)?
    {
        saved.report.state = State::Paused;
    }
    Ok(saved.report)
}

fn owner_active(path: &Path) -> Result<bool> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(sidecar(path, ".migration9.lock"))
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    match file.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

fn failed(mut report: Report, error: Failure) -> Report {
    report.state = if matches!(error, Failure::DiskFull | Failure::Locked) {
        State::Paused
    } else {
        State::Failed
    };
    report.error_code = Some(error);
    report
}
fn classify(error: &Error) -> Failure {
    match error {
        Error::Busy(_) => Failure::Locked,
        Error::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Failure::ReadOnly,
        Error::Io(e) if e.kind() == std::io::ErrorKind::StorageFull => Failure::DiskFull,
        _ if error.to_string().contains("database or disk is full") => Failure::DiskFull,
        _ if error.to_string().contains("integrity") => Failure::IntegrityFailed,
        _ => Failure::IoError,
    }
}

/// Execute one operation. `Start`/`Resume` block until a durable stop or completion;
/// HTTP schedules them on a worker. Every outcome uses the same report type.
pub fn execute(path: &Path, action: Action, options: &Options, cancel: &AtomicBool) -> Report {
    match execute_inner(path, action, options, cancel) {
        Ok(report) => report,
        Err(e) => {
            tracing::warn!(error = %e, "Schema-9 migration operation failed");
            failed(Report::default(), classify(&e))
        }
    }
}
fn execute_inner(
    path: &Path,
    action: Action,
    options: &Options,
    cancel: &AtomicBool,
) -> Result<Report> {
    let path = path.canonicalize()?;
    if action == Action::Status {
        return status(&path);
    }
    let conn = open(&path, false)?;
    let mut saved = load(&path, &conn)?;
    saved.report = status(&path)?;
    let _owner = match Owner::acquire(&path) {
        Ok(owner) => owner,
        Err(Error::Busy(_)) => {
            if matches!(action, Action::Pause | Action::Cancel) {
                storage::request_control(&path, action)?;
                return status(&path);
            }
            saved.report.error_code = Some(
                if matches!(saved.report.state, State::Running | State::Switching) {
                    Failure::AlreadyRunning
                } else {
                    Failure::Locked
                },
            );
            return Ok(saved.report);
        }
        Err(e) => return Err(e),
    };
    if action == Action::Startup
        && std::env::var("AUDIOLIB_DEFER_SCHEMA9").as_deref() == Ok("1")
        && version(&conn)? == 8
    {
        saved.report.state = State::Deferred;
        saved.report.error_code = None;
        save(&path, &saved)?;
        return Ok(saved.report);
    }
    if action == Action::Rollback {
        return storage::rollback(&path, saved);
    }
    if version(&conn)? == 9 {
        validate_ready(&conn)?;
        saved.report.state = State::Done;
        saved.report.error_code = None;
        saved.report.batches.clear();
        return Ok(saved.report);
    }
    if version(&conn)? != 8 || !crate::schema::current_format_ready(&conn)? {
        return Err(invalid(
            "Schema-9 migration requires a validated schema-8 library",
        ));
    }
    if action == Action::Pause {
        if exists(&conn, "v9_checkpoint")? {
            saved.report.state = State::Paused;
            save(&path, &saved)?;
        }
        return Ok(saved.report);
    }
    if action == Action::Cancel {
        let _isolation = AdministrationLease::shared(&path)?;
        storage::discard(&path)?;
        saved.report.state = State::Cancelled;
        saved.report.error_code = None;
        save(&path, &saved)?;
        return Ok(saved.report);
    }
    let result = run(&path, &mut saved, options, cancel);
    if let Err(e) = result {
        if matches!(e, Error::Cancelled) {
            saved.report.state = State::Paused;
            saved.report.error_code = None;
        } else {
            saved.report = failed(saved.report, classify(&e));
        }
        let _ = save(&path, &saved);
    }
    Ok(saved.report)
}

fn run(path: &Path, saved: &mut Saved, options: &Options, cancel: &AtomicBool) -> Result<()> {
    let isolation = AdministrationLease::shared(path)?;
    saved.report.error_code = None;
    if fs::metadata(path)?.permissions().readonly()
        || fs::metadata(path.parent().unwrap())?
            .permissions()
            .readonly()
    {
        saved.report = failed(saved.report.clone(), Failure::ReadOnly);
        save(path, saved)?;
        return Ok(());
    }
    // Actual write-access probe catches directory ACLs before touching the source.
    drop(tempfile::NamedTempFile::new_in(path.parent().unwrap())?);
    let mut conn = open(path, true)?;
    if identity(&conn)? != saved.library_id
        || version(&conn)? != 8
        || !crate::schema::current_format_ready(&conn)?
    {
        return Err(invalid("Source changed before schema-9 preflight"));
    }
    #[cfg(feature = "test-hooks")]
    if let Some(trace) = &options.sql_trace {
        hooks::trace(&mut conn, trace)?;
    }
    let _ = options;
    drop(begin_write(&conn, cancel)?);
    let resume = exists(&conn, "v9_checkpoint")?;
    if !resume {
        // A new attempt gets a fresh recovery point, including writes made after
        // an earlier cancellation/rollback. Older backups remain on disk.
        saved.backup = None;
        saved.report = Report {
            state: State::Preflight,
            switch: Some(markers(&conn)?),
            ..Default::default()
        };
        let free = crate::space::free_space(path.parent().unwrap());
        #[cfg(feature = "test-hooks")]
        let free = hooks::number("AUDIOLIB_MIGRATION_FAKE_FREE_BYTES").or(free);
        let required = fs::metadata(path)?.len().saturating_mul(13).div_ceil(10);
        if free.is_none_or(|free| free < required) {
            saved.report = failed(saved.report.clone(), Failure::InsufficientSpace);
            save(path, saved)?;
            return Ok(());
        }
        save(path, saved)?;
        saved.report.state = State::BackingUp;
        save(path, saved)?;
        let start = Instant::now();
        if let Err(error) = storage::backup(path, &conn, saved, cancel) {
            if matches!(error, Error::Cancelled) {
                saved.report.state = State::Paused;
            } else {
                saved.report = failed(saved.report.clone(), Failure::BackupFailed);
            }
            save(path, saved)?;
            return Ok(());
        }
        saved
            .report
            .phases
            .insert("backup".into(), start.elapsed().as_secs_f64() * 1000.0);
        if cancel.load(Ordering::Acquire) {
            saved.report.state = State::Paused;
            save(path, saved)?;
            return Ok(());
        }
        storage::prepare(&mut conn, cancel)?;
    } else {
        storage::validate_backup(path, saved)?;
    }
    crate::register_library_functions(&conn)?;
    storage::clear_control(&conn, cancel)?;
    saved.report.progress = conn
        .query_row(
            "SELECT done,total FROM v9_checkpoint WHERE singleton=1",
            [],
            |r| {
                Ok(Progress {
                    done: count(r, 0)?,
                    total: count(r, 1)?,
                })
            },
        )
        .map_err(db_error)?;
    // The first committed sample completes write-latency calibration. Preparation
    // retains the preflight state until that checkpoint exists durably.
    saved.report.state = if saved.report.progress.done == 0 {
        State::Preflight
    } else {
        State::Running
    };
    save(path, saved)?;
    let mut batch_rows = 5000usize;
    #[cfg(feature = "test-hooks")]
    {
        batch_rows = hooks::number("AUDIOLIB_MIGRATION_BATCH_ROWS")
            .map_or(batch_rows, |n| n.clamp(1, 50_000) as usize);
    }
    // Sample several transactions even on small directory trees so latency feedback
    // can reduce the next write lock before the entire backfill has completed.
    let remaining: u64 = conn
        .query_row(
            "SELECT max(total-done,0) FROM v9_checkpoint WHERE singleton=1",
            [],
            |r| count(r, 0),
        )
        .map_err(db_error)?;
    batch_rows = batch_rows.min(remaining.div_ceil(4).max(1) as usize);
    #[cfg(feature = "test-hooks")]
    let mut batch = 0;
    loop {
        if stop(path, &conn, saved, cancel)? {
            return Ok(());
        }
        let admission_started = Instant::now();
        let tx = begin_write(&conn, cancel)?;
        let start = Instant::now();
        let last: Option<i64> = tx
            .query_row(
                "SELECT last_id FROM v9_checkpoint WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        let ids = tx
            .prepare("SELECT id FROM folders WHERE (?1 IS NULL OR id>?1) ORDER BY id LIMIT ?2")
            .map_err(db_error)?
            .query_map(params![last, batch_rows as i64], |r| r.get::<_, i64>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        if ids.is_empty() {
            break;
        }
        #[cfg(feature = "test-hooks")]
        {
            batch += 1;
        }
        for id in &ids {
            dir_search::reindex_segment(&tx, *id)?;
        }
        #[cfg(feature = "test-hooks")]
        {
            hooks::slow(batch);
            hooks::abort(&format!("abort:mid_batch:{batch}"));
            if hooks::fault(&format!("sqlite_full:batch:{batch}")) {
                return Err(Error::Io(std::io::Error::from(
                    std::io::ErrorKind::StorageFull,
                )));
            }
        }
        tx.execute("UPDATE v9_checkpoint SET last_id=?1,done=done+?2,total=(SELECT count(*) FROM folders) WHERE singleton=1", params![ids.last(), ids.len() as i64]).map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        saved.report.state = State::Running;
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        saved.report.batches.push(Batch {
            rows: ids.len() as u64,
            ms,
        });
        *saved.report.phases.get_mut("path_build").unwrap() +=
            admission_started.elapsed().as_secs_f64() * 1000.0;
        saved.report.progress = conn
            .query_row(
                "SELECT done,total FROM v9_checkpoint WHERE singleton=1",
                [],
                |r| {
                    Ok(Progress {
                        done: count(r, 0)?,
                        total: count(r, 1)?,
                    })
                },
            )
            .map_err(db_error)?;
        saved.report.eta_s = Some(
            ms / ids.len() as f64
                * saved
                    .report
                    .progress
                    .total
                    .saturating_sub(saved.report.progress.done) as f64
                / 1000.0,
        );
        save(path, saved)?;
        #[cfg(feature = "test-hooks")]
        {
            hooks::abort(&format!("abort:batch_boundary:{batch}"));
            if hooks::number("AUDIOLIB_MIGRATION_PAUSE_AFTER_BATCHES")
                .is_some_and(|n| batch as u64 >= n)
            {
                saved.report.state = State::Paused;
                save(path, saved)?;
                return Ok(());
            }
        }
        if ms > 1000.0 {
            batch_rows = (batch_rows / 2).max(1);
        }
        // Give ordinary writers a scheduling opportunity between transactions.
        std::thread::sleep(Duration::from_millis(2));
    }
    if stop(path, &conn, saved, cancel)? {
        return Ok(());
    }
    let optimize = Instant::now();
    #[cfg(feature = "test-hooks")]
    hooks::abort("abort:optimize");
    let optimize_tx = begin_write(&conn, cancel)?;
    optimize_tx
        .execute(
            "INSERT INTO audio_dir_search(audio_dir_search) VALUES('optimize')",
            [],
        )
        .map_err(db_error)?;
    optimize_tx.commit().map_err(db_error)?;
    saved
        .report
        .phases
        .insert("optimize".into(), optimize.elapsed().as_secs_f64() * 1000.0);
    drop(isolation);
    storage::publish(path, &mut conn, saved, cancel)?;
    Ok(())
}

fn stop(path: &Path, conn: &Connection, saved: &mut Saved, cancel: &AtomicBool) -> Result<bool> {
    // A control writer waiting for the preceding batch must commit before the
    // next batch can overtake it. Never wait here with a SQLite writer held.
    let _control = storage::ControlLease::acquire(path)?;
    stop_requested(path, conn, saved, cancel)
}

fn stop_requested(
    path: &Path,
    conn: &Connection,
    saved: &mut Saved,
    cancel: &AtomicBool,
) -> Result<bool> {
    let cmd: Option<String> = conn
        .query_row("SELECT cmd FROM v9_control WHERE singleton=1", [], |r| {
            r.get(0)
        })
        .optional()
        .map_err(db_error)?;
    if cmd.as_deref() == Some("cancel") {
        storage::discard(path)?;
        saved.report.state = State::Cancelled;
    } else if cancel.load(Ordering::Acquire) || cmd.as_deref() == Some("pause") {
        saved.report.state = State::Paused;
    } else {
        return Ok(false);
    }
    save(path, saved)?;
    Ok(true)
}

/// Validate the complete s9 directory contract before opening a server writer.
pub fn validate_ready(conn: &Connection) -> Result<()> {
    storage::validate_ready(conn)
}

/// Search adapter used by the migration CLI; the provider owns all business reads.
pub fn search(
    path: &Path,
    query: String,
    scope: SearchScope,
    limit: usize,
    cursor: Option<String>,
) -> Result<Report> {
    let provider =
        crate::SqliteProvider::open_server_existing(path, &AtomicBool::new(false), |_| {})?;
    let page = provider.search(&SearchRequest {
        query,
        scope,
        limit,
        cursor,
        ..Default::default()
    })?;
    let mut report = status(path)?;
    report.ids = Some(page.results.into_iter().map(|f| f.id).collect());
    report.next_cursor = Some(page.next_cursor);
    Ok(report)
}

impl crate::SqliteProvider {
    /// Open an existing server library. Schema 9 is explicitly admitted only here;
    /// desktop/ordinary local writer opens retain their schema-8 refusal contract.
    pub fn open_server_existing(
        path: &Path,
        cancel: &AtomicBool,
        progress: impl FnMut(crate::SnapshotUpgradeProgress),
    ) -> Result<Self> {
        use std::sync::{Mutex, RwLock};
        if cancel.load(Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        let path = path.canonicalize()?;
        let conn = open(&path, false)?;
        if version(&conn)? != 9 {
            drop(conn);
            let mut provider =
                Self::open_existing_with_progress_cancellable(&path, cancel, progress)?;
            provider.server_schema9 = true;
            return Ok(provider);
        }
        validate_ready(&conn)?;
        drop(conn);
        let _lease = AdministrationLease::shared(&path)?;
        let conn = open(&path, true)?;
        validate_ready(&conn)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY;").map_err(db_error)?;
        let tokenizer = crate::load_tokenizer();
        crate::schema::register_tokenizer(&conn, tokenizer.clone())?;
        Ok(Self {
            album_namespace: RwLock::new(crate::albums::namespace(&conn)?),
            legacy_album_scope: RwLock::new(crate::albums::legacy_scope(&conn)?),
            path,
            read_only: false,
            server_schema9: true,
            writer: Mutex::new(conn),
            read_pool: Mutex::new(Vec::new()),
            revision_reader: Mutex::new(None),
            cursor_session: uuid::Uuid::new_v4(),
            ranking_cache: Mutex::new(Default::default()),
            first_page_cache: Mutex::new(Default::default()),
            search_upgrade_pool: Default::default(),
            document_lengths: Mutex::new(Default::default()),
            search_cache_observation: Mutex::new(None),
            parallel_rank_reserved: AtomicBool::new(false),
            tokenizer,
            read_gate: RwLock::new(()),
            derived_tags: AtomicBool::new(true),
            list_waveform_memory: Default::default(),
            dir_index: Default::default(),
            _owned_snapshot_directory: None,
        })
    }
}
