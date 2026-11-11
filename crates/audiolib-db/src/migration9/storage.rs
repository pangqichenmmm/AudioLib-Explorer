// crates/audiolib-db/src/migration9/storage.rs
//! SQLite backup, checkpoint, directory change capture and atomic publication.
use super::*;
use crate::management::LibraryLease;

fn completed_format(conn: &Connection, schema: i64) -> Result<&'static str> {
    use audiolib_core::compatibility::{
        AUDIO_SOURCES_FEATURE, PLAN_B_AUDIO_FORMAT, PLAN_B_FORMAT, PLAN_B_V9_AUDIO_FORMAT,
        PLAN_B_V9_FORMAT,
    };
    let extended = match crate::schema::feature_version(conn, AUDIO_SOURCES_FEATURE)? {
        None => false,
        Some(1) => true,
        Some(_) => return Err(invalid("Unsupported extended audio source revision")),
    };
    match (schema, extended) {
        (8, false) => Ok(PLAN_B_FORMAT),
        (8, true) => Ok(PLAN_B_AUDIO_FORMAT),
        (9, false) => Ok(PLAN_B_V9_FORMAT),
        (9, true) => Ok(PLAN_B_V9_AUDIO_FORMAT),
        _ => Err(invalid("Unsupported source-audio schema")),
    }
}

/// Serialize control admission with batch boundaries and publication. Control
/// writers acquire this before their administration lease. The runner waits only
/// outside SQLite write transactions, then observes every admitted command.
/// Keep the sidecar in place to preserve its cross-process lock identity.
pub(super) struct ControlLease(fs::File);
impl ControlLease {
    pub(super) fn acquire(path: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(sidecar(path, ".migration9-control.lock"))
            .map_err(|error| io_error("control_open", error))?;
        file.lock()
            .map_err(|error| io_error("control_lock", error))?;
        Ok(Self(file))
    }
}
impl Drop for ControlLease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

const TRACK: &str = "
CREATE TABLE v9_checkpoint(singleton INTEGER PRIMARY KEY CHECK(singleton=1),phase TEXT NOT NULL,last_id INTEGER,done INTEGER NOT NULL,total INTEGER NOT NULL);
INSERT INTO v9_checkpoint SELECT 1,'path_build',NULL,0,count(*) FROM folders;
CREATE TABLE IF NOT EXISTS v9_control(singleton INTEGER PRIMARY KEY CHECK(singleton=1),cmd TEXT NOT NULL,requested_at TEXT NOT NULL);
DELETE FROM v9_control;
CREATE TABLE v9_dirty_dirs(id INTEGER PRIMARY KEY);
CREATE TRIGGER v9_dir_insert AFTER INSERT ON folders BEGIN INSERT OR IGNORE INTO v9_dirty_dirs VALUES(new.id); END;
CREATE TRIGGER v9_dir_update AFTER UPDATE OF folder_name,parent_id ON folders BEGIN INSERT OR IGNORE INTO v9_dirty_dirs VALUES(new.id); END;
CREATE TRIGGER v9_dir_delete AFTER DELETE ON folders BEGIN INSERT OR IGNORE INTO v9_dirty_dirs VALUES(old.id); END;
";
const DROP_TRACK: &str = "DROP TRIGGER IF EXISTS v9_dir_insert; DROP TRIGGER IF EXISTS v9_dir_update; DROP TRIGGER IF EXISTS v9_dir_delete; DROP TABLE IF EXISTS v9_dirty_dirs; DROP TABLE IF EXISTS v9_checkpoint;";

pub(super) fn prepare(conn: &mut Connection, cancel: &AtomicBool) -> Result<()> {
    write_admission(conn, cancel, || {
        conn.execute_batch("PRAGMA journal_mode=WAL;")
    })?;
    let tx = begin_write(conn, cancel)?;
    // A prior explicit s8 experimental path rebuild is disposable derived state.
    tx.execute_batch("DROP TABLE IF EXISTS audio_dir_search; DROP TABLE IF EXISTS audio_dir_runs;")
        .map_err(db_error)?;
    tx.execute_batch(dir_search::CREATE_DIR_FTS)
        .map_err(db_error)?;
    tx.execute_batch(dir_search::CREATE_RUNS)
        .map_err(db_error)?;
    dir_search::install_tracking(&tx)?;
    tx.execute("DELETE FROM audiolib_dir_dirty", [])
        .map_err(db_error)?;
    tx.execute_batch(TRACK).map_err(db_error)?;
    tx.execute(
        "DELETE FROM library_meta WHERE key IN ('search_path_rev','snapshot_format')",
        [],
    )
    .map_err(db_error)?;
    tx.commit().map_err(db_error)
}

fn backup_path(path: &Path, saved: &Saved) -> Result<PathBuf> {
    let name = saved
        .backup
        .as_deref()
        .ok_or_else(|| invalid("Missing migration rollback backup"))?;
    if !name.starts_with("audiolib-rollback-mig9-")
        || !name.ends_with(".db")
        || Path::new(name).components().count() != 1
    {
        return Err(invalid("Invalid migration backup name"));
    }
    Ok(path.parent().unwrap().join(name))
}
pub(super) fn validate_backup(path: &Path, saved: &Saved) -> Result<()> {
    let conn = open(&backup_path(path, saved)?, false)?;
    if version(&conn)? != 8
        || identity(&conn)? != saved.library_id
        || !crate::schema::current_format_ready(&conn)?
    {
        return Err(invalid("Rollback backup identity or format mismatch"));
    }
    check(&conn)
}
fn check(conn: &Connection) -> Result<()> {
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(db_error)?;
    if check != "ok"
        || conn
            .prepare("PRAGMA foreign_key_check")
            .map_err(db_error)?
            .exists([])
            .map_err(db_error)?
    {
        return Err(invalid("Migration integrity check failed"));
    }
    Ok(())
}
pub(super) fn backup(
    path: &Path,
    source: &Connection,
    saved: &mut Saved,
    cancel: &AtomicBool,
) -> Result<()> {
    #[cfg(feature = "test-hooks")]
    if hooks::fault("backup_fail") {
        return Err(invalid("Injected backup failure"));
    }
    if saved.backup.is_some() {
        return validate_backup(path, saved);
    }
    let file = tempfile::Builder::new()
        .prefix(".mig9-backup-")
        .tempfile_in(path.parent().unwrap())?;
    let mut destination = Connection::open(file.path()).map_err(db_error)?;
    {
        let backup = rusqlite::backup::Backup::new(source, &mut destination).map_err(db_error)?;
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            match backup.step(4096).map_err(db_error)? {
                rusqlite::backup::StepResult::Done => break,
                rusqlite::backup::StepResult::Busy | rusqlite::backup::StepResult::Locked => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => (),
            }
        }
    }
    check(&destination)?;
    if identity(&destination)? != saved.library_id || version(&destination)? != 8 {
        return Err(invalid("Backup source changed"));
    }
    drop(destination);
    file.as_file().sync_all()?;
    let name = format!("audiolib-rollback-mig9-{}.db", uuid::Uuid::new_v4());
    file.persist_noclobber(path.parent().unwrap().join(&name))
        .map_err(|e| Error::Io(e.error))?;
    saved.backup = Some(name);
    save(path, saved)
}

pub(super) fn request_control(path: &Path, action: Action) -> Result<()> {
    let _control = ControlLease::acquire(path)?;
    // Control rows are writes too: an unrelated restore/maintenance operation
    // must exclude them under the same administration lease as annotations.
    let _administration = AdministrationLease::shared(path)?;
    let conn = open(path, true)?;
    if version(&conn)? == 9 {
        // Publication won admission. Do not leave a stale command for a later run.
        return Ok(());
    }
    if !exists(&conn, "v9_control")? {
        return Err(Error::Busy(
            "Migration is still preparing; retry control request".into(),
        ));
    }
    conn.execute("INSERT INTO v9_control VALUES(1,?1,strftime('%Y-%m-%dT%H:%M:%fZ','now')) ON CONFLICT(singleton) DO UPDATE SET cmd=excluded.cmd,requested_at=excluded.requested_at", [if action == Action::Cancel { "cancel" } else { "pause" }]).map_err(db_error)?;
    Ok(())
}
pub(super) fn clear_control(conn: &Connection, cancel: &AtomicBool) -> Result<()> {
    let tx = begin_write(conn, cancel)?;
    tx.execute("DELETE FROM v9_control", []).map_err(db_error)?;
    tx.commit().map_err(db_error)
}
fn drop_paths(conn: &Connection) -> Result<()> {
    dir_search::drop_tracking(conn)?;
    conn.execute_batch(DROP_TRACK).map_err(db_error)?;
    conn.execute_batch("DROP TABLE IF EXISTS audio_dir_search; DROP TABLE IF EXISTS audio_dir_runs; DROP TABLE IF EXISTS v9_control;").map_err(db_error)?;
    // Keep the generation monotonic across rollback/re-migration so live readers
    // cannot reuse a cached directory tree from the earlier publication.
    conn.execute(
        "DELETE FROM library_meta WHERE key IN ('search_path_rev','snapshot_format')",
        [],
    )
    .map_err(db_error)?;
    Ok(())
}
pub(super) fn discard(path: &Path) -> Result<()> {
    let mut conn = open(path, true)?;
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(db_error)?;
    if version(&tx)? != 8 {
        return Err(invalid("Cannot cancel a published migration; use rollback"));
    }
    drop_paths(&tx)?;
    put(&tx, "snapshot_format", completed_format(&tx, 8)?)?;
    tx.commit().map_err(db_error)
}
fn put(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute("INSERT INTO library_meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key,value]).map_err(db_error)?;
    Ok(())
}

pub(super) fn publish(
    path: &Path,
    conn: &mut Connection,
    saved: &mut Saved,
    cancel: &AtomicBool,
) -> Result<()> {
    // Drain admitted pause/cancel writers, including their shared admin leases,
    // before checking control and acquiring the exclusive publication lease.
    // Retain admission through commit so no request can enter between these steps.
    let _control = ControlLease::acquire(path)?;
    if stop_requested(path, conn, saved, cancel)? {
        return Ok(());
    }
    saved.report.state = State::Switching;
    save(path, saved)?;
    // Fail closed if a scan/maintenance is active. The checkpoint remains resumable.
    let _scan = LibraryLease::acquire(path)?;
    let _administration = AdministrationLease::exclusive(path)?;
    let started = Instant::now();
    let tx = begin_write(conn, cancel)?;
    if identity(&tx)? != saved.library_id || version(&tx)? != 8 {
        return Err(invalid("Source changed before schema-9 publication"));
    }
    // Replay all committed directory mutations, including insertions behind the cursor.
    let changed = tx
        .prepare("SELECT id FROM v9_dirty_dirs ORDER BY id")
        .map_err(db_error)?
        .query_map([], |r| r.get::<_, i64>(0))
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    for id in changed {
        dir_search::reindex_segment(&tx, id)?;
    }
    dir_search::finish_tracking(&tx)?;
    tx.execute("DELETE FROM audio_dir_runs", [])
        .map_err(db_error)?;
    tx.execute_batch(dir_search::INSERT_RUNS)
        .map_err(db_error)?;
    tx.execute(
        "INSERT INTO audio_dir_search(audio_dir_search) VALUES('integrity-check')",
        [],
    )
    .map_err(db_error)?;
    let folders: u64 = tx
        .query_row("SELECT count(*) FROM folders", [], |r| count(r, 0))
        .map_err(db_error)?;
    let indexed: u64 = tx
        .query_row("SELECT count(*) FROM audio_dir_search", [], |r| count(r, 0))
        .map_err(db_error)?;
    let missing = tx.query_row("SELECT EXISTS(SELECT 1 FROM folders WHERE id NOT IN (SELECT rowid FROM audio_dir_search))", [], |r| r.get::<_,bool>(0)).map_err(db_error)?;
    if folders != indexed || missing {
        return Err(invalid("Directory index count mismatch"));
    }
    crate::vendor_import::ensure_schema(&tx)?;
    put(&tx, "snapshot_format", completed_format(&tx, 9)?)?;
    put(&tx, "search_path_rev", "1")?;
    put(&tx, "source_search_rev", "3")?;
    let generation = tx
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM library_meta WHERE key='dir_index_gen'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .map_err(db_error)?
        .unwrap_or(0)
        .saturating_add(1);
    put(&tx, "dir_index_gen", &generation.to_string())?;
    tx.execute(
        "INSERT INTO audiolib_schema_features VALUES('search_path',1),('vorbis_tags',1),('vendor_metadata',1)",
        [],
    )
    .map_err(db_error)?;
    tx.pragma_update(None, "user_version", 9)
        .map_err(db_error)?;
    tx.execute_batch(DROP_TRACK).map_err(db_error)?;
    #[cfg(feature = "test-hooks")]
    {
        if let Some(ms) = hooks::number("AUDIOLIB_MIGRATION_HOLD_SWITCH_MS") {
            std::thread::sleep(Duration::from_millis(ms));
        }
        hooks::abort("abort:switch_tx");
    }
    if cancel.load(Ordering::Acquire) {
        drop(tx);
        saved.report.state = State::Paused;
        save(path, saved)?;
        return Ok(());
    }
    tx.commit().map_err(db_error)?;
    saved
        .report
        .phases
        .insert("switch".into(), started.elapsed().as_secs_f64() * 1000.0);
    saved.report.state = State::Done;
    saved.report.progress = Progress {
        done: folders,
        total: folders,
    };
    saved.report.eta_s = Some(0.0);
    saved.report.switch = Some(markers(conn)?);
    save(path, saved)
}

pub(super) fn rollback(path: &Path, mut saved: Saved) -> Result<Report> {
    let _scan = LibraryLease::acquire(path)?;
    let _admin = AdministrationLease::exclusive(path)?;
    let mut conn = open(path, true)?;
    if version(&conn)? != 9 {
        saved.report.error_code = Some(Failure::NotMigrated);
        return Ok(saved.report);
    }
    validate_backup(path, &saved)?;
    validate_ready(&conn)?;
    // This migration changes derived objects only. Reverse those changes in one
    // transaction instead of overwriting annotations made since the retained backup.
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(db_error)?;
    drop_paths(&tx)?;
    tx.execute(
        "DELETE FROM audiolib_schema_features WHERE feature IN ('search_path','vorbis_tags','vendor_metadata')",
        [],
    )
    .map_err(db_error)?;
    put(&tx, "source_search_rev", "2")?;
    tx.pragma_update(None, "user_version", 8)
        .map_err(db_error)?;
    put(&tx, "snapshot_format", completed_format(&tx, 8)?)?;
    tx.commit().map_err(db_error)?;
    saved.report.state = State::RolledBack;
    saved.report.error_code = None;
    saved.report.switch = Some(markers(&conn)?);
    save(path, &saved)?;
    Ok(saved.report)
}

pub(super) fn validate_ready(conn: &Connection) -> Result<()> {
    if version(conn)? != 9 || !crate::schema::readable_format_ready(conn)? {
        return Err(invalid("Invalid schema-9 library contract"));
    }
    let format: Option<String> = conn
        .query_row(
            "SELECT value FROM library_meta WHERE key='snapshot_format'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_error)?;
    if format.as_deref() != Some(completed_format(conn, 9)?)
        || !dir_search::ready(conn)?
        || !crate::schema::definition::matches_sql(
            conn,
            "audio_dir_search",
            "table",
            "audio_dir_search",
            dir_search::CREATE_DIR_FTS,
        )?
        || !crate::schema::definition::matches_sql(
            conn,
            "audio_dir_runs",
            "table",
            "audio_dir_runs",
            dir_search::CREATE_RUNS,
        )?
        || exists(conn, "v9_checkpoint")?
    {
        return Err(invalid("Incomplete schema-9 directory index"));
    }
    Ok(())
}
