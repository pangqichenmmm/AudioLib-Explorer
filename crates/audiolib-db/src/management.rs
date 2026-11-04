// crates/audiolib-db/src/management.rs
//! Transactional library administration. These operations never delete source audio.
use super::*;
use audiolib_core::{ScanIssue, ScanState, TagEditMode};
use std::{
    collections::HashSet,
    io::{BufRead, BufReader, BufWriter, Read, Write},
};

// Stored POSIX paths retain case even when read by a Windows process. This
// comparison deliberately does not resolve links or native case aliases: a
// case-only relocation must still update the recorded spelling.
#[cfg(feature = "scanner")]
fn relocation_path_matches(left: &str, right: &str) -> bool {
    path_identity::comparison_key(left) == path_identity::comparison_key(right)
        || (path_identity::windows_path(left)
            && path_identity::windows_path(right)
            && path_identity::matches(left, right, true))
}

/// Persistent lock identity shared with scanning, including builds without scanner support.
pub(crate) struct LibraryLease(fs::File);
impl LibraryLease {
    pub(crate) fn acquire(database: &Path) -> Result<Self> {
        let database = database.canonicalize()?;
        let mut name = database.file_name().unwrap_or_default().to_os_string();
        name.push(".scan.lock");
        let file = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(database.with_file_name(name))?;
        match file.try_lock() {
            Ok(()) => Ok(Self(file)),
            Err(fs::TryLockError::WouldBlock) => Err(Error::Busy(
                "Another scan or library administration operation is running for this library"
                    .into(),
            )),
            Err(fs::TryLockError::Error(error)) => Err(Error::Io(error)),
        }
    }
}
impl Drop for LibraryLease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Cross-process isolation for all writers against whole-library replacement.
/// Scans and normal edits share this lock; maintenance and restore own it exclusively.
pub(crate) struct AdministrationLease(fs::File);

/// Search upgrades commit incomplete derived state between batches. Ordinary
/// maintenance keeps its established read-snapshot behavior and does not take this lock.
pub(crate) struct SearchUpgradeLease(fs::File);
impl SearchUpgradeLease {
    pub(crate) fn acquire(database: &Path, exclusive: bool) -> Result<Self> {
        let database = database.canonicalize()?;
        let mut name = database.file_name().unwrap_or_default().to_os_string();
        name.push(".upgrade.lock");
        let file = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(database.with_file_name(name))?;
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match result {
            Ok(()) => Ok(Self(file)),
            Err(fs::TryLockError::WouldBlock) => Err(Error::Busy(
                "Library search upgrade conflicts with an active reader".into(),
            )),
            Err(fs::TryLockError::Error(error)) => Err(Error::Io(error)),
        }
    }
}
impl Drop for SearchUpgradeLease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
fn administration_lock_path(database: &Path) -> Result<PathBuf> {
    let database = database.canonicalize()?;
    let mut name = database.file_name().unwrap_or_default().to_os_string();
    name.push(".admin.lock");
    Ok(database.with_file_name(name))
}

/// Only a uniquely owned temporary database may remove its otherwise persistent lock.
pub(crate) struct OwnedAdministrationSidecar(PathBuf);
impl OwnedAdministrationSidecar {
    pub(crate) fn new(staging: &Path) -> Result<Self> {
        Ok(Self(administration_lock_path(staging)?))
    }
}
impl Drop for OwnedAdministrationSidecar {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
impl AdministrationLease {
    fn acquire(database: &Path, exclusive: bool) -> Result<Self> {
        let file = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(administration_lock_path(database)?)?;
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match result {
            Ok(()) => Ok(Self(file)),
            Err(fs::TryLockError::WouldBlock) => Err(Error::Busy(
                "Library maintenance, restore, or schema upgrade conflicts with an active operation"
                    .into(),
            )),
            Err(fs::TryLockError::Error(error)) => Err(Error::Io(error)),
        }
    }
    pub(crate) fn shared(database: &Path) -> Result<Self> {
        Self::acquire(database, false)
    }
    pub(crate) fn exclusive(database: &Path) -> Result<Self> {
        Self::acquire(database, true)
    }
}
impl Drop for AdministrationLease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn validation(error: impl std::fmt::Display) -> Error {
    Error::Validation(error.to_string())
}
fn check_page(limit: usize, offset: usize) -> Result<(i64, i64)> {
    if limit == 0 || limit > 10_000 {
        return Err(validation("History limit must be 1..=10000"));
    }
    Ok((limit as i64, i64::try_from(offset).map_err(validation)?))
}
fn unique_ids(ids: &[i64]) -> Result<Vec<i64>> {
    if ids.is_empty() || ids.len() > 10_000 || ids.iter().any(|id| *id <= 0) {
        return Err(validation("Operation requires 1..=10000 positive IDs"));
    }
    let mut seen = HashSet::new();
    Ok(ids.iter().copied().filter(|id| seen.insert(*id)).collect())
}
fn output_stage(destination: &Path, prefix: &str) -> Result<tempfile::NamedTempFile> {
    if destination.exists() {
        return Err(validation("Output already exists"));
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    Ok(tempfile::Builder::new()
        .prefix(prefix)
        .tempfile_in(parent)?)
}
fn copy_database(source: &Connection, destination: &mut Connection) -> Result<()> {
    rusqlite::backup::Backup::new(source, destination)
        .map_err(db_error)?
        .run_to_completion(256, Duration::from_millis(10), None)
        .map_err(db_error)
}
fn normalize_page_size(staging: &Connection, target_page_size: i64) -> Result<()> {
    let source_page_size: i64 = staging
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .map_err(db_error)?;
    if source_page_size != target_page_size {
        let before = migration::digest_business(staging, &AtomicBool::new(false))?;
        // SQLite backup cannot change the page size of a WAL destination. Rewrite only
        // the owned staging snapshot in DELETE mode, leaving the source backup intact.
        staging
            .execute_batch("PRAGMA journal_mode=DELETE;")
            .map_err(db_error)?;
        staging
            .pragma_update(None, "page_size", target_page_size)
            .map_err(db_error)?;
        staging.execute_batch("VACUUM;").map_err(db_error)?;
        if migration::digest_business(staging, &AtomicBool::new(false))? != before {
            return Err(validation(
                "Page-size adaptation changed implicit business identities; restore is blocked and the working library is intact",
            ));
        }
    }
    Ok(())
}
pub(crate) fn snapshot_to_file(
    source: &mut Connection,
    destination: &Path,
) -> Result<BackupReport> {
    snapshot_to_file_cancellable(source, destination, &AtomicBool::new(false))
}

pub(crate) fn snapshot_to_file_cancellable(
    source: &mut Connection,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<BackupReport> {
    if cancel.load(Ordering::Acquire) {
        return Err(Error::Cancelled);
    }
    let stage = output_stage(destination, "audiolib-backup-")?;
    // The copy is as large as the live database (WAL content included); fail before
    // writing anything instead of with a half-written copy and a generic I/O error.
    let used: u64 = source
        .query_row(
            "SELECT (SELECT page_count FROM pragma_page_count)*(SELECT page_size FROM pragma_page_size)",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(db_error)?
        .try_into()
        .unwrap_or(0);
    ensure_free_space(stage.path().parent().unwrap_or(Path::new(".")), used)?;
    let tx = source.transaction().map_err(db_error)?;
    let count: u64 = tx
        .query_row("SELECT COUNT(*) FROM audio_files", [], |r| unsigned(r, 0))
        .map_err(db_error)?;
    {
        let mut target = Connection::open(stage.path()).map_err(db_error)?;
        {
            let backup = rusqlite::backup::Backup::new(&tx, &mut target).map_err(db_error)?;
            // The source transaction is pinned; pauses between steps cannot improve
            // concurrency, but each bounded step gives cancellation a chance.
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Err(Error::Cancelled);
                }
                match backup.step(4096).map_err(db_error)? {
                    rusqlite::backup::StepResult::Done => break,
                    rusqlite::backup::StepResult::More => {}
                    // The source transaction is pinned and the destination is exclusively
                    // owned. A lock conflict cannot be fixed by endlessly retrying here.
                    _ => {
                        return Err(Error::Busy(
                            "Snapshot copy encountered a database lock".into(),
                        ));
                    }
                }
            }
        }
        target
            .execute_batch("PRAGMA journal_mode=DELETE;")
            .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)?;
    if cancel.load(Ordering::Acquire) {
        return Err(Error::Cancelled);
    }
    stage.as_file().sync_all()?;
    let bytes = stage.as_file().metadata()?.len();
    stage
        .persist_noclobber(destination)
        .map_err(|e| Error::Io(e.error))?;
    Ok(BackupReport {
        destination: destination.to_path_buf(),
        total_records: count,
        bytes,
    })
}
fn check_connection(conn: &Connection) -> Result<IntegrityReport> {
    let mut report = IntegrityReport::default();
    let quick: Vec<String> = conn
        .prepare("PRAGMA quick_check")
        .map_err(db_error)?
        .query_map([], |r| r.get(0))
        .map_err(db_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_error)?;
    report.sqlite_ok = quick.len() == 1 && quick[0] == "ok";
    if !report.sqlite_ok {
        report.errors.extend(quick);
    }
    report.foreign_key_violations = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            unsigned(r, 0)
        })
        .map_err(db_error)?;
    if report.foreign_key_violations > 0 {
        report.errors.push(format!(
            "{} foreign key violations",
            report.foreign_key_violations
        ));
    }
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)?;
    let current = version >= 6;
    match conn.execute(
        if current {
            "INSERT INTO audio_search(audio_search) VALUES('integrity-check')"
        } else {
            "INSERT INTO audio_search(audio_search,rank) VALUES('integrity-check',1)"
        },
        [],
    ) {
        Ok(_) => report.fts_ok = true,
        Err(error) => report.errors.push(format!("FTS5 consistency: {error}")),
    }
    let structure = if current {
        // Missing performance indexes are repairable derived state, not FTS
        // corruption. Keep this pre-upgrade integrity gate focused on the FTS
        // table and synchronization triggers; publication checks full readiness.
        crate::search_storage::structure_matches(conn)?
    } else {
        schema::triggers_match(conn)?
    };
    if !structure {
        report.fts_ok = false;
        report.errors.push(
            "FTS5 table or synchronization triggers are missing or differ from the supported schema".into(),
        );
    }
    if current {
        let unmatched:i64=conn.query_row(
            "SELECT (SELECT COUNT(*) FROM audio_files af LEFT JOIN audio_search_docsize ds ON af.id=ds.id WHERE ds.id IS NULL)
             +(SELECT COUNT(*) FROM audio_search_docsize ds LEFT JOIN audio_files af ON af.id=ds.id WHERE af.id IS NULL)",
            [],|row|row.get(0),
        ).map_err(db_error)?;
        if unmatched != 0 {
            report.fts_ok = false;
            report
                .errors
                .push(format!("FTS5 document identity mismatch: {unmatched}"));
        }
    }
    Ok(report)
}
fn checked(report: &IntegrityReport) -> Result<()> {
    if report.is_ok() {
        Ok(())
    } else {
        Err(Error::Database(report.errors.join("; ")))
    }
}
pub(crate) fn rollback_path(database: &Path) -> Result<PathBuf> {
    let parent = database
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stage = tempfile::Builder::new()
        .prefix("audiolib-rollback-")
        .suffix(".db")
        .tempfile_in(parent)?;
    let path = stage.path().to_path_buf();
    drop(stage);
    Ok(path)
}
fn patch_row(conn: &Connection, id: i64, patch: &MetadataPatch, mode: TagEditMode) -> Result<()> {
    patch.validate()?;
    let current: Option<String> = conn
        .query_row(
            "SELECT COALESCE(user_tags,'') FROM audio_files WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(db_error)?;
    let current = current.ok_or_else(|| Error::NotFound(format!("Audio {id}")))?;
    let mut assignments = vec!["modified_at=strftime('%Y-%m-%d %H:%M:%f','now')".to_string()];
    let mut values = Vec::<Value>::new();
    if let Some(tags) = &patch.tags {
        let tags = match mode {
            TagEditMode::Replace => tags.clone(),
            TagEditMode::Append | TagEditMode::Remove => {
                let split = |value: &str| {
                    value
                        .split([',', ';', '\n'])
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                };
                let mut old = split(&current);
                let requested = split(tags);
                if mode == TagEditMode::Append {
                    let mut seen: HashSet<String> =
                        old.iter().map(|s| schema::fold_case(s)).collect();
                    for tag in requested {
                        if seen.insert(schema::fold_case(&tag)) {
                            old.push(tag);
                        }
                    }
                } else {
                    let remove: HashSet<String> =
                        requested.iter().map(|s| schema::fold_case(s)).collect();
                    old.retain(|tag| !remove.contains(&schema::fold_case(tag)));
                }
                old.join(", ")
            }
        };
        if tags.len() > 16_384 {
            return Err(validation("Combined tags exceed the allowed size"));
        }
        assignments.push("user_tags=?".into());
        values.push(Value::Text(tags));
    }
    if let Some(notes) = &patch.notes {
        assignments.push("user_notes=?".into());
        values.push(Value::Text(notes.clone()));
    }
    if let Some(favorite) = patch.favorite {
        assignments.push("favorite=?".into());
        values.push(Value::Integer(i64::from(favorite)));
    }
    if let Some(color) = &patch.color {
        assignments.push("user_color=?".into());
        values.push(if color.is_empty() {
            Value::Null
        } else {
            Value::Text(color.clone())
        });
    }
    values.push(Value::Integer(id));
    conn.execute(
        &format!(
            "UPDATE audio_files SET {} WHERE id=?",
            assignments.join(",")
        ),
        params_from_iter(values),
    )
    .map_err(db_error)?;
    Ok(())
}
fn remove_one(conn: &Connection, id: i64) -> Result<u64> {
    for table in ["playlist_items", "project_references", "waveform_cache"] {
        conn.execute(&format!("DELETE FROM {table} WHERE audio_file_id=?1"), [id])
            .map_err(db_error)?;
    }
    Ok(conn
        .execute("DELETE FROM audio_files WHERE id=?1", [id])
        .map_err(db_error)? as u64)
}

impl SqliteProvider {
    fn require_writable(&self) -> Result<()> {
        schema::ensure_ready(&*self.writer_unlocked()?)?;
        if self
            .writer_unlocked()?
            .is_readonly("main")
            .map_err(db_error)?
        {
            return Err(validation("Library was opened read-only"));
        }
        let version: i64 = self
            .writer_unlocked()?
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(db_error)?;
        if version != schema::SCHEMA_VERSION {
            return Err(validation(
                "Reopen the library before writing an older format",
            ));
        }
        Ok(())
    }
    pub(crate) fn management_info(&self) -> Result<LibraryInfo> {
        let stats = self.stats()?;
        self.management_info_with(stats)
    }
    /// [`Self::management_info`] with caller-supplied statistics (`stats()` aggregates the
    /// whole `audio_files` table, which takes seconds on a million-row library).
    pub(crate) fn management_info_with(
        &self,
        stats: audiolib_core::LibraryStats,
    ) -> Result<LibraryInfo> {
        let conn = self.reader()?;
        let schema_version = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(db_error)?;
        let sqlite_version = conn
            .query_row("SELECT sqlite_version()", [], |r| r.get(0))
            .map_err(db_error)?;
        drop(conn);
        let mut wal = self.path.as_os_str().to_os_string();
        wal.push("-wal");
        Ok(LibraryInfo {
            database: self.path.clone(),
            schema_version,
            sqlite_version,
            database_bytes: fs::metadata(&self.path)?.len(),
            wal_bytes: fs::metadata(PathBuf::from(wal))
                .map(|m| m.len())
                .unwrap_or_default(),
            stats,
        })
    }
    pub(crate) fn management_check(&self) -> Result<IntegrityReport> {
        let _upgrade = if self.read_only {
            None
        } else {
            Some(SearchUpgradeLease::acquire(&self.path, false)?)
        };
        let _gate = self
            .read_gate
            .read()
            .map_err(|_| Error::Database("Library reader gate was poisoned".into()))?;
        // A full backup touches every source page. A mapped pooled search reader
        // can retain the whole library's working set after this check returns.
        // Keep its snapshot/restore gate, but give validation a separate ordinary
        // I/O connection whose page cache is released with this operation.
        let mut source = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(db_error)?;
        source
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_error)?;
        source
            .pragma_update(None, "mmap_size", 0)
            .map_err(db_error)?;
        schema::ensure_ready(&source)?;
        let tx = source.transaction().map_err(db_error)?;
        tx.query_row("SELECT COUNT(*) FROM sqlite_schema", [], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(db_error)?;
        // Keep validation memory bounded even for multi-million-record libraries.
        let stage = tempfile::NamedTempFile::new()?;
        let mut copy = Connection::open(stage.path()).map_err(db_error)?;
        copy.pragma_update(None, "mmap_size", 0).map_err(db_error)?;
        copy_database(&tx, &mut copy)?;
        copy.execute_batch("PRAGMA journal_mode=DELETE;")
            .map_err(db_error)?;
        check_connection(&copy)
    }
    /// The checks of `check_library` (quick_check, foreign keys, FTS5 consistency and the
    /// supported triggers) run in place on a database file the caller owns exclusively,
    /// such as a downloaded server snapshot before it is installed. `check_library` checks
    /// a temporary copy instead, because a live library may be written meanwhile; for a
    /// file of several hundred MB that copy doubles the disk use and the time. The FTS5
    /// integrity-check statement needs a writable connection but changes nothing.
    pub fn check_owned_file(path: &Path) -> Result<IntegrityReport> {
        Self::check_owned_file_cancellable(path, &AtomicBool::new(false))
    }

    /// Check an exclusively owned snapshot, interrupting integrity SQL on cancellation.
    pub fn check_owned_file_cancellable(
        path: &Path,
        cancel: &AtomicBool,
    ) -> Result<IntegrityReport> {
        let mut conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(db_error)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        owned_cancellation::run(&mut conn, cancel, |conn| check_connection(conn))
    }

    /// Read statistics from a validated, upgraded owned snapshot with cancellable SQL.
    /// This never opens a live provider or creates a second derived-state copy.
    pub fn owned_snapshot_info_cancellable(
        path: &Path,
        cancel: &AtomicBool,
    ) -> Result<LibraryInfo> {
        let mut conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(db_error)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        owned_cancellation::run(&mut conn, cancel, |conn| {
            schema::validate_library_identity(conn)?;
            let stats = conn
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
                .map_err(db_error)?;
            let schema_version = conn
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(db_error)?;
            let sqlite_version = conn
                .query_row("SELECT sqlite_version()", [], |row| row.get(0))
                .map_err(db_error)?;
            Ok(LibraryInfo {
                database: path.to_path_buf(),
                schema_version,
                sqlite_version,
                database_bytes: fs::metadata(path)?.len(),
                wal_bytes: 0,
                stats,
            })
        })
    }
    pub(crate) fn management_backup(&self, destination: &Path) -> Result<BackupReport> {
        let mut reader = self.reader()?;
        snapshot_to_file(&mut reader, destination)
    }
    /// Copy a consistent library snapshot, cancelling between bounded SQLite backup
    /// steps. Cancellation removes only this operation's unpublished staging file.
    pub fn backup_library_cancellable(
        &self,
        destination: &Path,
        cancel: &AtomicBool,
    ) -> Result<BackupReport> {
        let mut reader = self.reader()?;
        snapshot_to_file_cancellable(&mut reader, destination, cancel)
    }
    pub(crate) fn management_restore(&self, source: &Path) -> Result<RestoreReport> {
        self.require_writable()?;
        let _lease = LibraryLease::acquire(&self.path)?;
        let _isolation = AdministrationLease::exclusive(&self.path)?;
        if !source.is_file() {
            return Err(Error::NotFound(source.display().to_string()));
        }
        if source.canonicalize()? == self.path.canonicalize()? {
            return Err(validation("Restore source is the active library"));
        }
        // Validate and migrate only an owned snapshot; malformed/future sources never touch the current library.
        let owned = tempfile::Builder::new()
            .prefix("audiolib-restore-owned-")
            .tempdir_in(
                self.path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
            )?;
        let stage = tempfile::Builder::new()
            .prefix("audiolib-restore-")
            .suffix(".db")
            .tempfile_in(owned.path())?;
        {
            let src = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(db_error)?;
            schema::validate_library_identity(&src)?;
            let mut target = Connection::open(stage.path()).map_err(db_error)?;
            copy_database(&src, &mut target)?;
            schema::register_tokenizer(&target, self.tokenizer.clone())?;
            // A derived-state upgrade recreates the FTS table and triggers.
            // Validate the original owned snapshot first so that rebuilding
            // cannot conceal damaged postings or synchronization triggers.
            checked(&check_connection(&target)?)?;
            drop(target);
            Self::upgrade_owned_snapshot(stage.path())?;
            let target = Connection::open(stage.path()).map_err(db_error)?;
            schema::register_tokenizer(&target, self.tokenizer.clone())?;
            // A restore may bring back fewer/lower IDs than this installation has
            // already issued. Keep those identities retired even after replacement.
            let high_water = {
                let writer = self.writer_unlocked()?;
                schema::audio_id_high_water(&writer)?
            };
            schema::retain_audio_id_high_water(&target, high_water)?;
            let album_high_water = {
                let writer = self.writer_unlocked()?;
                crate::albums::high_water(&writer)?
            };
            crate::albums::retain_high_water(&target, album_high_water)?;
            let page_size = self
                .writer_unlocked()?
                .pragma_query_value(None, "page_size", |row| row.get(0))
                .map_err(db_error)?;
            normalize_page_size(&target, page_size)?;
            checked(&check_connection(&target)?)?;
        }
        let rollback_backup = rollback_path(&self.path)?;
        {
            let _gate = self
                .read_gate
                .write()
                .map_err(|_| Error::Database("Library reader gate was poisoned".into()))?;
            let mut writer = self.writer_unlocked()?;
            self.read_pool
                .lock()
                .map_err(|_| Error::Database("Reader pool lock was poisoned".into()))?
                .clear();
            snapshot_to_file(&mut writer, &rollback_backup)?;
            let replacement =
                Connection::open_with_flags(stage.path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(db_error)?;
            if let Err(error) = copy_database(&replacement, &mut writer)
                .and_then(|()| checked(&check_connection(&writer)?))
            {
                let rollback =
                    Connection::open_with_flags(&rollback_backup, OpenFlags::SQLITE_OPEN_READ_ONLY)
                        .map_err(db_error)?;
                copy_database(&rollback, &mut writer).map_err(|repair| {
                    Error::Database(format!(
                        "Restore failed: {error}; rollback failed: {repair}; backup: {}",
                        rollback_backup.display()
                    ))
                })?;
                return Err(error);
            }
            schema::register_tokenizer(&writer, self.tokenizer.clone())?;
            self.derived_tags.store(
                schema::source_tags_ready(&writer)?,
                std::sync::atomic::Ordering::Release,
            );
        }
        let identity = {
            let writer = self.writer_unlocked()?;
            (
                crate::albums::namespace(&writer)?,
                crate::albums::legacy_scope(&writer)?,
            )
        };
        *self
            .album_namespace
            .write()
            .map_err(|_| Error::Database("Album namespace lock was poisoned".into()))? = identity.0;
        *self
            .legacy_album_scope
            .write()
            .map_err(|_| Error::Database("Legacy album scope lock was poisoned".into()))? =
            identity.1;
        Ok(RestoreReport {
            source: source.to_path_buf(),
            rollback_backup,
            library: self.management_info()?,
        })
    }
    pub(crate) fn management_maintain(
        &self,
        action: MaintenanceAction,
    ) -> Result<MaintenanceReport> {
        self.require_writable()?;
        let _lease = LibraryLease::acquire(&self.path)?;
        let _isolation = AdministrationLease::exclusive(&self.path)?;
        let rollback_backup = rollback_path(&self.path)?;
        let integrity;
        {
            let _gate = self
                .read_gate
                .write()
                .map_err(|_| Error::Database("Library reader gate was poisoned".into()))?;
            let mut writer = self.writer_unlocked()?;
            self.read_pool
                .lock()
                .map_err(|_| Error::Database("Reader pool lock was poisoned".into()))?
                .clear();
            snapshot_to_file(&mut writer, &rollback_backup)?;
            let operation = (|| -> Result<IntegrityReport> {
                match action {
                    MaintenanceAction::Optimize => {
                        writer
                            .execute(
                                "INSERT INTO audio_search(audio_search) VALUES('optimize')",
                                [],
                            )
                            .map_err(db_error)?;
                        writer
                            .execute_batch("REINDEX; PRAGMA optimize;")
                            .map_err(db_error)?;
                    }
                    MaintenanceAction::RebuildSearch => schema::rebuild_search(&mut writer)?,
                }
                let report = check_connection(&writer)?;
                checked(&report)?;
                Ok(report)
            })();
            integrity = match operation {
                Ok(report) => report,
                Err(error) => {
                    let rollback = Connection::open_with_flags(
                        &rollback_backup,
                        OpenFlags::SQLITE_OPEN_READ_ONLY,
                    )
                    .map_err(db_error)?;
                    copy_database(&rollback, &mut writer).map_err(|repair| {
                        Error::Database(format!(
                            "Maintenance failed: {error}; rollback failed: {repair}; backup: {}",
                            rollback_backup.display()
                        ))
                    })?;
                    return Err(error);
                }
            };
            self.derived_tags.store(
                schema::source_tags_ready(&writer)?,
                std::sync::atomic::Ordering::Release,
            );
        }
        Ok(MaintenanceReport {
            action,
            rollback_backup,
            integrity,
        })
    }
    pub(crate) fn management_batch(
        &self,
        request: &BatchMetadataRequest,
    ) -> Result<BatchMetadataReport> {
        request.validate()?;
        let _lease = LibraryLease::acquire(&self.path)?;
        let ids = unique_ids(&request.ids)?;
        let mut writer = self.writer()?;
        let tx = writer.transaction().map_err(db_error)?;
        for id in &ids {
            patch_row(&tx, *id, &request.patch, request.tag_mode)?;
        }
        tx.commit().map_err(db_error)?;
        Ok(BatchMetadataReport {
            updated: ids.len() as u64,
        })
    }
    pub(crate) fn management_lookup(&self, path: &Path) -> Result<Option<AudioFile>> {
        let resolved = if self.read_only {
            path.to_path_buf()
        } else {
            path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
        };
        let reader = self.reader()?;
        // Keep the resolved identity and projected row in one read snapshot;
        // another writer may otherwise reuse the numeric ID between statements.
        let transaction = reader.unchecked_transaction().map_err(db_error)?;
        let Some(id) =
            path_identity::audio_id(&transaction, &display_path(&resolved), !self.read_only)?
        else {
            return Ok(None);
        };
        let projection = audio_projection(&transaction)?;
        let audio = transaction
            .query_row(
                &format!("SELECT {projection} FROM audio_files af WHERE id=?1"),
                [id],
                audio_from_row,
            )
            .optional()
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
        Ok(audio)
    }
    pub(crate) fn management_remove(&self, ids: &[i64]) -> Result<u64> {
        let _lease = LibraryLease::acquire(&self.path)?;
        let ids = unique_ids(ids)?;
        let mut writer = self.writer()?;
        let tx = writer.transaction().map_err(db_error)?;
        let mut removed = 0;
        for id in ids {
            removed += remove_one(&tx, id)?;
        }
        refresh_folder_statistics(&tx)?;
        tx.commit().map_err(db_error)?;
        Ok(removed)
    }
    pub(crate) fn management_remove_folder(&self, id: i64) -> Result<u64> {
        let _lease = LibraryLease::acquire(&self.path)?;
        let mut writer = self.writer()?;
        let tx = writer.transaction().map_err(db_error)?;
        let exists = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM folders WHERE id=?1)",
                [id],
                |r| r.get::<_, bool>(0),
            )
            .map_err(db_error)?;
        if !exists {
            return Err(Error::NotFound(format!("Folder {id}")));
        }
        let folder_ids:Vec<i64>=tx.prepare("WITH RECURSIVE tree(id) AS (SELECT ?1 UNION SELECT f.id FROM folders f JOIN tree ON f.parent_id=tree.id) SELECT id FROM tree").map_err(db_error)?
            .query_map([id],|r|r.get(0)).map_err(db_error)?.collect::<rusqlite::Result<_>>().map_err(db_error)?;
        let mut removed = 0;
        for folder in &folder_ids {
            let audio: Vec<i64> = tx
                .prepare("SELECT id FROM audio_files WHERE folder_id=?1")
                .map_err(db_error)?
                .query_map([folder], |r| r.get(0))
                .map_err(db_error)?
                .collect::<rusqlite::Result<_>>()
                .map_err(db_error)?;
            for id in audio {
                removed += remove_one(&tx, id)?;
            }
        }
        // Explicit cleanup also supports legacy schemas without foreign-key cascades.
        for folder in &folder_ids {
            tx.execute(
                "DELETE FROM folder_closure WHERE ancestor_id=?1 OR descendant_id=?1",
                [folder],
            )
            .map_err(db_error)?;
        }
        for folder in folder_ids.iter().rev() {
            tx.execute("DELETE FROM folders WHERE id=?1", [folder])
                .map_err(db_error)?;
        }
        refresh_folder_statistics(&tx)?;
        tx.commit().map_err(db_error)?;
        Ok(removed)
    }
    pub(crate) fn management_scans(&self, limit: usize, offset: usize) -> Result<Vec<ScanSession>> {
        let (limit, offset) = check_page(limit, offset)?;
        self.reader()?.prepare("SELECT id,COALESCE(start_time,''),end_time,COALESCE(scan_path,''),COALESCE(scan_type,''),status,
            COALESCE(files_scanned,0),COALESCE(files_added,0),COALESCE(files_updated,0),COALESCE(files_removed,0),COALESCE(files_skipped,0),COALESCE(errors,0),error_details,errors_truncated
            FROM scan_sessions ORDER BY id DESC LIMIT ?1 OFFSET ?2").map_err(db_error)?
            .query_map(params![limit,offset],|row|{
                let raw:Option<String>=row.get(12)?;
                let details=raw.and_then(|raw|serde_json::from_str::<serde_json::Value>(&raw).ok()).and_then(|value|value.as_array().cloned()).unwrap_or_default()
                    .into_iter().filter_map(|value|if let Some(message)=value.as_str(){Some(ScanIssue{path:String::new(),message:message.into()})}else{serde_json::from_value(value).ok()}).collect();
                let status:Option<String>=row.get(5)?;
                Ok(ScanSession{id:row.get(0)?,start_time:row.get(1)?,end_time:row.get(2)?,scan_path:row.get(3)?,scan_type:row.get(4)?,
                    state:match status.as_deref(){Some("completed")=>ScanState::Completed,Some("cancelled")=>ScanState::Cancelled,Some("failed")=>ScanState::Failed,_=>ScanState::Running},
                    scanned:unsigned(row,6)?,added:unsigned(row,7)?,updated:unsigned(row,8)?,removed:unsigned(row,9)?,unchanged:unsigned(row,10)?,errors:unsigned(row,11)?,error_details:details,errors_truncated:unsigned(row,13)?})
            }).map_err(db_error)?.collect::<rusqlite::Result<_>>().map_err(db_error)
    }

    /// Relocate an indexed root in one transaction. Validation and the rollback
    /// backup happen under both OS leases, so scans and other writers cannot alter
    /// the records between validation and commit. Source audio is never modified.
    #[cfg(feature = "scanner")]
    pub(crate) fn management_relink_folder(
        &self,
        source: &Path,
        target: &Path,
        cancel: &AtomicBool,
    ) -> Result<u64> {
        self.require_writable()?;
        if !source.is_absolute() || !target.is_absolute() {
            return Err(validation("Folder relocation requires absolute paths"));
        }
        if source.file_name().is_none() || target.file_name().is_none() {
            return Err(validation("File system or drive roots cannot be relocated"));
        }
        let check_cancel = || -> Result<()> {
            if cancel.load(Ordering::Acquire) {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        };
        // A cancelled request must not acquire leases or turn into a successful
        // no-op merely because the source is an alias or is not indexed.
        check_cancel()?;
        let _scan = LibraryLease::acquire(&self.path)?;
        let _administration = AdministrationLease::exclusive(&self.path)?;
        let target = audiolib_core::validate_scan_root(target, &[])?;
        if target.file_name().is_none() {
            return Err(validation("File system or drive roots cannot be relocated"));
        }
        // Scanning stores canonical paths. Resolve existing source aliases
        // (including macOS /var -> /private/var and Windows short names), but
        // retain a missing old path after a physical move or case-only rename.
        // Other I/O failures are errors.
        let requested_source_text = folder_path_text(source).trim_end_matches('/').to_string();
        let resolved_source = match source.canonicalize() {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => source.to_path_buf(),
            Err(error) => return Err(error.into()),
        };
        // Keep the requested basename when only its parent is an alias. Full
        // realpath may return a renamed Unicode basename that SQLite's ASCII
        // NOCASE cannot match to the old indexed spelling.
        let parent_resolved_source = match source
            .parent()
            .expect("absolute source has a parent")
            .canonicalize()
        {
            Ok(parent) => parent.join(source.file_name().expect("source filename validated above")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => source.to_path_buf(),
            Err(error) => return Err(error.into()),
        };
        let parent_resolved_source_text = folder_path_text(&parent_resolved_source)
            .trim_end_matches('/')
            .to_string();
        let source = resolved_source.as_path();
        if source.file_name().is_none() {
            return Err(validation("File system or drive roots cannot be relocated"));
        }
        let target_text = folder_path_text(&target);
        let resolved_source_text = folder_path_text(source).trim_end_matches('/').to_string();
        let source_key = path_identity::comparison_key(&resolved_source_text);
        let target_key = path_identity::comparison_key(&target_text);
        // Windows source aliases retain their established no-op behavior. On
        // case-insensitive POSIX volumes, realpath can change the source spelling
        // after a case-only rename, so compare its stored spelling below instead.
        #[cfg(windows)]
        if relocation_path_matches(&resolved_source_text, &target_text) {
            return Ok(0);
        }
        if source_key.starts_with(&format!("{target_key}/"))
            || target_key.starts_with(&format!("{source_key}/"))
        {
            return Err(validation("Relocation paths must not overlap"));
        }
        let mut writer = self.writer_unlocked()?;
        let lookup = |sql: &str| -> Result<Vec<(i64, Option<i64>, String)>> {
            writer
                .prepare(sql)
                .map_err(db_error)?
                .query_map(
                    params![
                        requested_source_text,
                        resolved_source_text,
                        parent_resolved_source_text
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .map_err(db_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db_error)
        };
        // Prefer exact stored identities: case-sensitive filesystems can index
        // distinct "audio" and "Audio" roots. Only fall back to NOCASE when no
        // exact candidate exists, and never pick an arbitrary matching identity.
        let mut roots = lookup(
            "SELECT id,parent_id,folder_path FROM folders
             WHERE folder_path=?1 COLLATE BINARY OR folder_path=?2 COLLATE BINARY
                OR folder_path=?3 COLLATE BINARY LIMIT 2",
        )?;
        if roots.is_empty() {
            let mut seen = HashSet::new();
            for path in [
                &requested_source_text,
                &resolved_source_text,
                &parent_resolved_source_text,
            ] {
                let candidate = path_identity::folder_id(&writer, path).map_err(|error| {
                    if matches!(&error, Error::Validation(message) if message.starts_with("Path matches multiple library records:")) {
                        validation("Source path matches more than one indexed folder")
                    } else {
                        error
                    }
                })?;
                if let Some(id) = candidate
                    && seen.insert(id)
                {
                    roots.push(
                        writer
                            .query_row(
                                "SELECT id,parent_id,folder_path FROM folders WHERE id=?1",
                                [id],
                                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                            )
                            .map_err(db_error)?,
                    );
                }
            }
        }
        if roots.len() > 1 {
            return Err(validation(
                "Source path matches more than one indexed folder",
            ));
        }
        let Some((root, parent, source_text)) = roots.pop() else {
            return Ok(0);
        };
        // Use the indexed spelling for suffixes and POSIX case-only relocation.
        // The canonical path above serves as a lookup alias, not a replacement
        // for the identity recorded before the directory moved or was renamed.
        if relocation_path_matches(&source_text, &target_text) {
            return Ok(0);
        }
        let source_key = path_identity::comparison_key(&source_text);
        if source_key.starts_with(&format!("{target_key}/"))
            || target_key.starts_with(&format!("{source_key}/"))
        {
            return Err(validation("Relocation paths must not overlap"));
        }
        if parent.is_some() {
            return Err(validation(
                "Only an indexed top-level folder may be relocated",
            ));
        }
        let folders: Vec<(i64, String)> = writer
            .prepare("WITH RECURSIVE tree(id) AS (SELECT ?1 UNION SELECT f.id FROM folders f JOIN tree ON f.parent_id=tree.id) SELECT id,folder_path FROM folders WHERE id IN (SELECT id FROM tree)")
            .map_err(db_error)?
            .query_map([root], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(db_error)?
            .collect::<rusqlite::Result<_>>()
            .map_err(db_error)?;
        let suffix = |text: &str| -> Result<String> {
            let text = if path_identity::windows_path(text) {
                text.replace('\\', "/")
            } else {
                text.to_owned()
            };
            let prefix = text
                .get(..source_text.len())
                .ok_or_else(|| validation("Stored folder path is outside the relocation root"))?;
            if !relocation_path_matches(prefix, &source_text)
                || (text.len() > source_text.len() && text.as_bytes()[source_text.len()] != b'/')
            {
                return Err(validation(
                    "Stored folder path is outside the relocation root",
                ));
            }
            let relative = text[source_text.len()..].trim_start_matches('/');
            if Path::new(relative)
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                return Err(validation(
                    "Stored relocation path contains invalid components",
                ));
            }
            Ok(relative.to_string())
        };
        let mut moved_folders = Vec::with_capacity(folders.len());
        for (id, path) in &folders {
            check_cancel()?;
            let desired = target.join(suffix(path)?);
            let real = audiolib_core::validate_scan_root(&desired, std::slice::from_ref(&target))
                .map_err(|error| {
                validation(format!(
                    "Invalid corresponding relocation folder {}: {error}",
                    desired.display()
                ))
            })?;
            if !relocation_path_matches(&display_path(&real), &display_path(&desired)) {
                return Err(validation(
                    "Relocation folders must use real paths without directory links",
                ));
            }
            let new_path = folder_path_text(&real);
            let occupied =
                path_identity::folder_id(&writer, &new_path)?.is_some_and(|found| found != *id);
            if occupied {
                return Err(validation("Relocation target contains an indexed folder"));
            }
            moved_folders.push((*id, new_path));
        }
        let projection = audio_projection(&writer)?;
        let mut statement = writer
            .prepare(&format!("WITH RECURSIVE tree(id) AS (SELECT ?1 UNION SELECT f.id FROM folders f JOIN tree ON f.parent_id=tree.id) SELECT {projection} FROM audio_files af WHERE af.folder_id IN (SELECT id FROM tree)"))
            .map_err(db_error)?;
        let files = statement
            .query_map([root], audio_from_row)
            .map_err(db_error)?;
        let mut moved_files = Vec::new();
        let mut keys = HashSet::new();
        for audio in files {
            let audio = audio.map_err(db_error)?;
            check_cancel()?;
            let desired = target.join(suffix(&audio.file_path)?);
            let real = audiolib_core::validate_wav_path(&desired, std::slice::from_ref(&target))
                .map_err(|error| {
                    validation(format!(
                        "Invalid corresponding relocation WAV {}: {error}",
                        desired.display()
                    ))
                })?;
            // A directory link must not move the record out of its preserved tree.
            if !relocation_path_matches(&display_path(&real), &display_path(&desired)) {
                return Err(validation(
                    "Relocation targets must use real paths without directory links",
                ));
            }
            let before = fs::metadata(&real)?;
            let metadata = audiolib_audio::probe(&real).map_err(validation)?;
            let after = fs::metadata(&real)?;
            if before.len() != after.len() || before.modified()? != after.modified()? {
                return Err(validation(
                    "Relocation target changed while reading Metadata; retry",
                ));
            }
            if after.len() != audio.file_size
                || metadata.sample_rate != audio.sample_rate
                || metadata.channels != audio.channels
                || metadata.bit_depth != audio.bit_depth
                || (metadata.duration - audio.duration).abs() > 0.000_001
            {
                return Err(validation(format!(
                    "Relocation target does not match indexed WAV Metadata: {}",
                    desired.display()
                )));
            }
            let text = display_path(&real);
            let key = path_key(&text);
            let occupied = path_identity::audio_id(&writer, &text, true)?
                .is_some_and(|found| found != audio.id);
            if occupied || !keys.insert(path_identity::comparison_key(&text)) {
                return Err(validation(
                    "Relocation target is occupied by another audio record",
                ));
            }
            let mtime_ns = i64::try_from(
                after
                    .modified()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(validation)?
                    .as_nanos(),
            )
            .map_err(validation)?;
            moved_files.push((audio.id, text, key, mtime_ns));
        }
        drop(statement);
        check_cancel()?;
        let backup_dir = self.path.parent().unwrap_or(Path::new(".")).join("backups");
        fs::create_dir_all(&backup_dir)?;
        let mut backup = tempfile::Builder::new()
            .prefix("before-root-relink-")
            .suffix(".db")
            .tempfile_in(&backup_dir)?;
        let used = writer.query_row("SELECT (SELECT page_count FROM pragma_page_count)*(SELECT page_size FROM pragma_page_size)", [], |row| row.get::<_, i64>(0)).map_err(db_error)?;
        ensure_free_space(&backup_dir, used.try_into().unwrap_or(0))?;
        let mut backup_conn = Connection::open(backup.path()).map_err(db_error)?;
        copy_database(&writer, &mut backup_conn)?;
        drop(backup_conn);
        backup.as_file_mut().sync_all()?;
        // Keep the complete rollback copy even if a later transaction is cancelled.
        backup.keep().map_err(|error| Error::Io(error.error))?;
        check_cancel()?;
        let tx = writer.transaction().map_err(db_error)?;
        for (id, path) in &moved_folders {
            tx.execute("UPDATE folders SET folder_path=?2,updated_at=strftime('%Y-%m-%d %H:%M:%f','now') WHERE id=?1", params![id,path]).map_err(db_error)?;
        }
        tx.execute(
            "UPDATE folders SET folder_name=?2 WHERE id=?1",
            params![
                root,
                target
                    .file_name()
                    .unwrap_or(target.as_os_str())
                    .to_string_lossy()
            ],
        )
        .map_err(db_error)?;
        for (id, text, key, mtime) in &moved_files {
            check_cancel()?;
            tx.execute("UPDATE audio_files SET file_path=?2,path_key=?3,source_mtime_ns=?4,missing=0,modified_at=strftime('%Y-%m-%d %H:%M:%f','now') WHERE id=?1", params![id,text,key,mtime]).map_err(db_error)?;
            tx.execute("DELETE FROM waveform_cache WHERE audio_file_id=?1", [id])
                .map_err(db_error)?;
        }
        refresh_folder_statistics(&tx)?;
        tx.execute("UPDATE albums SET name=COALESCE((SELECT folder_name FROM folders WHERE id=albums.folder_id),name) WHERE kind='folder'",[]).map_err(db_error)?;
        check_cancel()?;
        tx.commit().map_err(db_error)?;
        Ok(moved_files.len() as u64)
    }

    #[cfg(not(feature = "scanner"))]
    pub(crate) fn management_relink_folder(
        &self,
        _source: &Path,
        _target: &Path,
        _cancel: &AtomicBool,
    ) -> Result<u64> {
        Err(validation("Folder relocation requires the scanner feature"))
    }

    #[cfg(feature = "scanner")]
    pub(crate) fn management_relink(&self, id: i64, path: &Path) -> Result<AudioFile> {
        let _lease = LibraryLease::acquire(&self.path)?;
        let path = path.canonicalize()?;
        if !path.is_file()
            || !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
        {
            return Err(validation("Relink target must be an existing WAV file"));
        }
        let before = fs::metadata(&path)?;
        let metadata = audiolib_audio::probe(&path)?;
        let after = fs::metadata(&path)?;
        if before.len() != after.len() || before.modified()? != after.modified()? {
            return Err(validation(
                "Relink target changed while reading Metadata; retry",
            ));
        }
        let mtime_ns = i64::try_from(
            after
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(validation)?
                .as_nanos(),
        )
        .map_err(validation)?;
        let key = path_key(&display_path(&path));
        let parent = path
            .parent()
            .ok_or_else(|| validation("Relink target has no parent"))?;
        let mut writer = self.writer()?;
        let tx = writer.transaction().map_err(db_error)?;
        let exists = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM audio_files WHERE id=?1)",
                [id],
                |r| r.get::<_, bool>(0),
            )
            .map_err(db_error)?;
        if !exists {
            return Err(Error::NotFound(format!("Audio {id}")));
        }
        let occupied = path_identity::audio_id(&tx, &display_path(&path), true)?
            .is_some_and(|found| found != id);
        if occupied {
            return Err(validation(
                "Relink target is already occupied by another library record",
            ));
        }
        // Reuse a registered ancestor when possible so the target remains browsable in its tree.
        let mut root = parent.to_path_buf();
        for ancestor in parent.ancestors() {
            let found = path_identity::folder_id(&tx, &folder_path_text(ancestor))?.is_some();
            if found {
                root = ancestor.to_path_buf();
                break;
            }
        }
        tx.execute(
            "UPDATE audio_files SET file_path=?2,path_key=?3 WHERE id=?1",
            params![id, display_path(&path), key],
        )
        .map_err(db_error)?;
        tx.execute("DELETE FROM waveform_cache WHERE audio_file_id=?1", [id])
            .map_err(db_error)?;
        self.apply_scan_items(
            &tx,
            &[ScannedAudio {
                path,
                root,
                size: after.len(),
                mtime_ns,
                metadata,
            }],
            None,
        )?;
        refresh_folder_statistics(&tx)?;
        let projection = audio_projection(&tx)?;
        let result = tx
            .query_row(
                &format!("SELECT {projection} FROM audio_files af WHERE af.id=?1"),
                [id],
                audio_from_row,
            )
            .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(result)
    }
    #[cfg(not(feature = "scanner"))]
    pub(crate) fn management_relink(&self, _id: i64, _path: &Path) -> Result<AudioFile> {
        Err(validation(
            "Relinking WAV Metadata requires the scanner feature",
        ))
    }

    pub(crate) fn management_export(
        &self,
        destination: &Path,
        format: LibraryExportFormat,
    ) -> Result<TransferReport> {
        let mut stage = output_stage(destination, "audiolib-export-")?;
        let mut reader = self.reader()?;
        let tx = reader.transaction().map_err(db_error)?;
        let projection = audio_projection(&tx)?;
        let mut statement = tx
            .prepare(&format!(
                "SELECT {projection} FROM audio_files af ORDER BY af.id"
            ))
            .map_err(db_error)?;
        let mut rows = statement.query([]).map_err(db_error)?;
        let mut records = 0;
        {
            let mut output = BufWriter::new(stage.as_file_mut());
            if format == LibraryExportFormat::Csv {
                csv_write(
                    &mut output,
                    &CSV_COLUMNS
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect::<Vec<_>>(),
                )?;
            }
            while let Some(row) = rows.next().map_err(db_error)? {
                let audio = audio_from_row(row).map_err(db_error)?;
                match format {
                    LibraryExportFormat::JsonLines => {
                        serde_json::to_writer(&mut output, &audio).map_err(validation)?;
                        output.write_all(b"\n")?;
                    }
                    LibraryExportFormat::Csv => {
                        let value = serde_json::to_value(&audio).map_err(validation)?;
                        let fields = CSV_COLUMNS
                            .iter()
                            .map(|key| match &value[*key] {
                                serde_json::Value::Null => String::new(),
                                serde_json::Value::String(s) => s.clone(),
                                value => value.to_string(),
                            })
                            .collect::<Vec<_>>();
                        csv_write(&mut output, &fields)?;
                    }
                }
                records += 1;
            }
            output.flush()?;
        }
        drop(rows);
        drop(statement);
        tx.commit().map_err(db_error)?;
        stage.as_file().sync_all()?;
        let bytes = stage.as_file().metadata()?.len();
        stage
            .persist_noclobber(destination)
            .map_err(|e| Error::Io(e.error))?;
        Ok(TransferReport {
            path: destination.to_path_buf(),
            records,
            skipped: 0,
            bytes,
        })
    }
    pub(crate) fn management_import(&self, source: &Path) -> Result<TransferReport> {
        let _lease = LibraryLease::acquire(&self.path)?;
        let file = fs::File::open(source)?;
        let bytes = file.metadata()?.len();
        let mut input = BufReader::new(file);
        // Detect the actual stream, since export format is independent of its filename.
        if input.fill_buf()?.starts_with(&[0xef, 0xbb, 0xbf]) {
            input.consume(3);
        }
        let first = loop {
            let buffer = input.fill_buf()?;
            let whitespace = buffer
                .iter()
                .take_while(|byte| byte.is_ascii_whitespace())
                .count();
            if whitespace < buffer.len() {
                break Some(buffer[whitespace]);
            }
            if buffer.is_empty() {
                break None;
            }
            input.consume(whitespace);
        };
        let is_csv = first.is_some_and(|byte| byte != b'{');
        let mut writer = self.writer()?;
        let tx = writer.transaction().map_err(db_error)?;
        let mut records = 0;
        let mut skipped = 0;
        let mut apply = |value: serde_json::Value| -> Result<()> {
            let (path, patch) = annotation_patch(&value)?;
            if let Some(id) = path_identity::audio_id(&tx, &path, false)? {
                patch_row(&tx, id, &patch, TagEditMode::Replace)?;
                records += 1;
            } else {
                skipped += 1;
            }
            Ok(())
        };
        if is_csv {
            let columns =
                csv_read(&mut input)?.ok_or_else(|| validation("CSV is missing its header"))?;
            let unique: HashSet<&str> = columns.iter().map(String::as_str).collect();
            if unique.len() != columns.len() || !unique.contains("file_path") {
                return Err(validation("CSV requires unique column names and file_path"));
            }
            while let Some(fields) = csv_read(&mut input)? {
                if fields.len() != columns.len() {
                    return Err(validation("CSV record width differs from header"));
                }
                let mut object = serde_json::Map::new();
                for (column, field) in columns.iter().zip(fields) {
                    let value = match column.as_str() {
                        // Legacy exports carry a rating column; it is tolerated and ignored.
                        "rating" => continue,
                        "favorite" => serde_json::json!(field.parse::<bool>().map_err(validation)?),
                        _ => serde_json::Value::String(field),
                    };
                    object.insert(column.clone(), value);
                }
                apply(serde_json::Value::Object(object))?;
            }
        } else {
            loop {
                let mut line = Vec::new();
                let read = (&mut input)
                    .take(16 * 1024 * 1024 + 1)
                    .read_until(b'\n', &mut line)?;
                if read == 0 {
                    break;
                }
                if read > 16 * 1024 * 1024 {
                    return Err(validation("JSONL record exceeds 16 MiB"));
                }
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                apply(serde_json::from_slice(&line).map_err(validation)?)?;
            }
        }
        tx.commit().map_err(db_error)?;
        Ok(TransferReport {
            path: source.to_path_buf(),
            records,
            skipped,
            bytes,
        })
    }
}

const CSV_COLUMNS: &[&str] = &[
    "id",
    "file_path",
    "filename",
    "folder_id",
    "file_size",
    "sample_rate",
    "bit_depth",
    "channels",
    "duration",
    "tags",
    "color",
    "notes",
    "favorite",
    "bext_description",
    "attributes",
    "missing",
];
fn csv_write(output: &mut impl Write, fields: &[String]) -> Result<()> {
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            output.write_all(b",")?;
        }
        output.write_all(b"\"")?;
        output.write_all(field.replace('"', "\"\"").as_bytes())?;
        output.write_all(b"\"")?;
    }
    output.write_all(b"\r\n")?;
    Ok(())
}
fn csv_read(input: &mut impl BufRead) -> Result<Option<Vec<String>>> {
    let mut fields = Vec::new();
    let mut field = Vec::new();
    let mut quoted = false;
    let mut closed = false;
    let mut any = false;
    let mut bytes = 0usize;
    loop {
        let available = input.fill_buf()?;
        let next = available.first().copied();
        let Some(byte) = next else {
            if quoted {
                return Err(validation("CSV has an unterminated quoted field"));
            }
            if !any {
                return Ok(None);
            }
            fields.push(String::from_utf8(field).map_err(validation)?);
            return Ok(Some(fields));
        };
        input.consume(1);
        any = true;
        bytes += 1;
        if bytes > 32 * 1024 * 1024 {
            return Err(validation("CSV record exceeds 32 MiB"));
        }
        if quoted {
            if byte == b'"' {
                if input.fill_buf()?.first() == Some(&b'"') {
                    input.consume(1);
                    field.push(b'"');
                } else {
                    quoted = false;
                    closed = true;
                }
            } else {
                field.push(byte);
            }
        } else {
            match byte {
                b'"' if field.is_empty() && !closed => quoted = true,
                b',' => {
                    fields.push(String::from_utf8(std::mem::take(&mut field)).map_err(validation)?);
                    closed = false;
                }
                b'\n' | b'\r' => {
                    if byte == b'\r' && input.fill_buf()?.first() == Some(&b'\n') {
                        input.consume(1);
                    }
                    fields.push(String::from_utf8(field).map_err(validation)?);
                    return Ok(Some(fields));
                }
                _ if closed => return Err(validation("CSV has text after a quoted field")),
                b'"' => return Err(validation("CSV quote occurs inside an unquoted field")),
                _ => field.push(byte),
            }
        }
    }
}
fn annotation_patch(value: &serde_json::Value) -> Result<(String, MetadataPatch)> {
    let object = value
        .as_object()
        .ok_or_else(|| validation("Annotation record must be an object"))?;
    let path = object
        .get("file_path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && !s.contains('\0'))
        .ok_or_else(|| validation("Annotation record requires file_path"))?
        .to_string();
    let text = |name: &str| -> Result<Option<String>> {
        match object.get(name) {
            None => Ok(None),
            Some(serde_json::Value::Null) if name == "color" => Ok(Some(String::new())),
            Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
            _ => Err(validation(format!("Annotation {name} must be text"))),
        }
    };
    // A legacy `rating` field in imported annotations is tolerated and ignored.
    let favorite = object
        .get("favorite")
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| validation("Annotation favorite must be a boolean"))
        })
        .transpose()?;
    let patch = MetadataPatch {
        tags: text("tags")?,
        color: text("color")?,
        notes: text("notes")?,
        favorite,
    };
    patch.validate()?;
    Ok((path, patch))
}
