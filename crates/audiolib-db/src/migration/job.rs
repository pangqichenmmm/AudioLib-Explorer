// crates/audiolib-db/src/migration/job.rs
//! Durable offline jobs. The source is guarded until atomic publication or rollback.
use super::{
    BusinessDigest, MigrationPlan,
    inspect::{business_digest, check_cancel, ordinary_tables, table_exists},
    plan::{check_source, invalid, tokenizer_identity},
    stage,
};
use crate::{db_error, management, search_storage::quote_identifier, wal};
use audiolib_core::{Error, Result, compatibility::CATALOG_REVISION};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

/// Committed boundary. Copy/index cursors live in the same transaction as their rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MigrationPhase {
    /// Persist an immutable rollback snapshot.
    Backup,
    /// Block legacy writers with durable source guards.
    Guarding,
    /// Complete supported old schema/derived migrations in an owned copy.
    Normalizing,
    /// Copy business records without firing user triggers.
    Copying,
    /// Regenerate FTS from authoritative Metadata.
    Indexing,
    /// Verify complete business data and integrity.
    Validating,
    /// Merge the owned candidate's FTS segments; preserve implicit business rowids.
    Compacting,
    /// Atomically publish through SQLite's backup transaction.
    Publishing,
    /// Publish committed; finish the WAL checkpoint.
    Checkpoint,
    /// Candidate is installed and verified.
    Complete,
    /// Restore a validated backup atomically.
    RollingBack,
    /// Original restored; only a new explicit plan may migrate it again.
    RolledBack,
}

/// Local management result; never send its filesystem paths in an HTTP health response.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationJob {
    /// Owned persistent directory. No cleanup occurs implicitly.
    pub directory: PathBuf,
    /// Opaque identity stored both here and in the source marker/target receipt.
    pub id: String,
    /// Reviewed source-bound plan.
    pub plan: MigrationPlan,
    /// Last durable outer boundary.
    pub phase: MigrationPhase,
    /// Immutable original backup checksum, set before source guards.
    pub backup_sha256: Option<String>,
    /// Complete normalized business projection to verify before publication.
    pub target_business: Option<BusinessDigest>,
    /// Target DDL identity prevents rollback from discarding later schema changes.
    pub target_schema_sha256: Option<String>,
    /// Explicit new-library identity admission after protected publication.
    #[serde(default)]
    pub import_identity: Option<ImportIdentityAdmission>,
}

/// A restricted, recoverable new-library admission. Stored atomically with the
/// new identity in the library receipt, then mirrored in the owned job record.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportIdentityAdmission {
    protocol: u32,
    job_id: String,
    pub(super) before: BusinessDigest,
    after: BusinessDigest,
    previous_namespace: String,
    previous_legacy_scope: Option<String>,
    namespace: String,
}

/// Owned progress DTO; observers may request cancellation by returning an error.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationProgress {
    /// Resumable local job location.
    pub directory: PathBuf,
    /// Current phase, independently from downloads or application startup.
    pub phase: MigrationPhase,
    /// Table currently being copied/indexed, if any.
    pub table: Option<String>,
    /// Rows durably committed in this table/phase.
    pub completed: u64,
}

pub(super) type Observer<'a> = dyn FnMut(MigrationProgress) -> Result<()> + 'a;

impl MigrationJob {
    pub(super) fn artifact(&self, name: &str) -> Result<PathBuf> {
        if !["job.sqlite", "backup.db", "normalized.db", "candidate.db"].contains(&name) {
            return Err(invalid("Unknown migration artifact"));
        }
        let path = self.directory.join(name);
        if path.exists() {
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.is_file()
                || is_link(&metadata)
                || path.canonicalize()?.parent() != Some(self.directory.as_path())
            {
                return Err(invalid("Migration artifact escaped its owned directory"));
            }
        }
        Ok(path)
    }

    pub(super) fn notify(
        &self,
        observer: &mut Observer<'_>,
        phase: MigrationPhase,
        table: Option<&str>,
        completed: u64,
    ) -> Result<()> {
        observer(MigrationProgress {
            directory: self.directory.clone(),
            phase,
            table: table.map(str::to_owned),
            completed,
        })
    }

    pub(super) fn save(&self) -> Result<()> {
        let mut conn = open_rw(&self.artifact("job.sqlite")?)?;
        let tx = conn.transaction().map_err(db_error)?;
        tx.execute(
            "UPDATE job SET payload=?1 WHERE singleton=1",
            [encode(self)?],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)
    }

    fn advance(&mut self, phase: MigrationPhase) -> Result<()> {
        self.phase = phase;
        self.save()
    }
}

fn encode(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|error| invalid(error.to_string()))
}

pub(super) fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

pub(super) fn open_rw(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).map_err(db_error)?;
    conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA busy_timeout=1000;")
        .map_err(db_error)?;
    Ok(conn)
}

pub(super) fn open_read(path: &Path) -> Result<Connection> {
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(db_error)?;
    conn.execute_batch("PRAGMA query_only=ON;")
        .map_err(db_error)?;
    Ok(conn)
}

pub(super) fn file_hash(path: &Path, cancel: &AtomicBool) -> Result<String> {
    let mut input = File::open(path)?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut hash = Sha256::new();
    loop {
        check_cancel(cancel)?;
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn validate_plan_identity(plan: &MigrationPlan) -> Result<()> {
    if let Some(local) = &plan.local_history
        && (!local.data_root.is_absolute()
            || local.library_id
                != crate::local_play_history::local_library_id(&plan.source.source)?)
    {
        return Err(invalid(
            "Local history migration destination/library identity is invalid",
        ));
    }
    if plan.protocol != 1
        || plan.catalog_revision != CATALOG_REVISION
        || plan.application_version != env!("CARGO_PKG_VERSION")
        || plan.tokenizer_identity != tokenizer_identity()
        || plan.recipe_identity != super::recipe::identity()
        || plan.schema_changes != super::recipe::changes()
    {
        return Err(invalid(
            "Migration job requires its original supported tool/catalog/tokenizer",
        ));
    }
    Ok(())
}

fn validate_local_history_destination(plan: &MigrationPlan) -> Result<()> {
    if let Some(local) = &plan.local_history
        && (!local.data_root.is_dir() || local.data_root.canonicalize()? != local.data_root)
    {
        return Err(invalid(
            "Local history migration data root changed; preserve the original destination before resuming",
        ));
    }
    Ok(())
}

pub(super) fn load(directory: &Path) -> Result<MigrationJob> {
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || is_link(&metadata) {
        return Err(invalid(
            "Migration directory must be an owned real directory",
        ));
    }
    let directory = directory.canonicalize()?;
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("Invalid migration directory"))?;
    if !name.starts_with(".audiolib-migrate-") {
        return Err(invalid("Unrecognized migration directory"));
    }
    let state_path = directory.join("job.sqlite");
    let state_meta = fs::symlink_metadata(&state_path)?;
    if !state_meta.is_file() || is_link(&state_meta) {
        return Err(invalid("Invalid migration manifest file"));
    }
    // A hot rollback journal after power loss needs an owned writable connection
    // to recover the manifest before reading its last committed state.
    let conn = open_rw(&state_path)?;
    let payload: String = conn
        .query_row("SELECT payload FROM job WHERE singleton=1", [], |row| {
            row.get(0)
        })
        .map_err(db_error)?;
    let job: MigrationJob =
        serde_json::from_str(&payload).map_err(|error| invalid(error.to_string()))?;
    if job.directory != directory
        || job.id != name
        || job.plan.source.source.canonicalize()? != job.plan.source.source
        || job.plan.source.source.parent() != directory.parent()
    {
        return Err(invalid("Migration source/directory identity mismatch"));
    }
    Ok(job)
}

pub(super) struct Leases {
    _scan: management::LibraryLease,
    _admin: management::AdministrationLease,
    _search: management::SearchUpgradeLease,
}
impl Leases {
    pub(super) fn acquire(source: &Path) -> Result<Self> {
        Ok(Self {
            _scan: management::LibraryLease::acquire(source)?,
            _admin: management::AdministrationLease::exclusive(source)?,
            _search: management::SearchUpgradeLease::acquire(source, true)?,
        })
    }
}

fn marker_matches(source: &Connection, job: &MigrationJob) -> Result<bool> {
    if !table_exists(source, "audiolib_storage_migration")? {
        return Ok(false);
    }
    let marker: (String, String, u32, u32) = source
        .query_row(
            "SELECT job_id,plan,protocol,target FROM audiolib_storage_migration WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(db_error)?;
    // A job started by the previous target must stay resumable, and a job started by this
    // build records the current target.
    let target = i64::from(marker.3);
    let current = crate::schema::SCHEMA_VERSION;
    let accepted_target = (current - 1..=current).contains(&target);
    if marker.0 != job.id || marker.1 != encode(&job.plan)? || marker.2 != 1 || !accepted_target {
        return Err(invalid(
            "Source belongs to a different or unsupported migration",
        ));
    }
    Ok(true)
}

fn install_guards(source: &mut Connection, job: &MigrationJob, cancel: &AtomicBool) -> Result<()> {
    if marker_matches(source, job)? {
        return check_source(source, &job.plan, cancel);
    }
    let tx = source
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(db_error)?;
    check_source(&tx, &job.plan, cancel)?;
    tx.execute_batch(
        "CREATE TABLE audiolib_storage_migration(
          singleton INTEGER PRIMARY KEY CHECK(singleton=1),protocol INTEGER NOT NULL,
          target INTEGER NOT NULL,job_id TEXT NOT NULL,directory TEXT NOT NULL,plan TEXT NOT NULL);",
    ).map_err(db_error)?;
    tx.execute(
        // Record the format this build publishes; the compatibility check also accepts the
        // previous target, so an in-flight job from an older build can still resume.
        &format!(
            "INSERT INTO audiolib_storage_migration VALUES(1,1,{},?1,?2,?3)",
            crate::schema::SCHEMA_VERSION
        ),
        params![job.id, job.directory.to_string_lossy(), encode(&job.plan)?],
    )
    .map_err(db_error)?;
    for (ordinal, table) in ordinary_tables(&tx)?.into_iter().enumerate() {
        for operation in ["INSERT", "UPDATE", "DELETE"] {
            let name = format!("audiolib_storage_guard_{ordinal}_{operation}");
            // RAISE is unconditional: this executor never writes source business
            // rows until SQLite backup atomically replaces the whole database.
            tx.execute_batch(&format!(
                "CREATE TRIGGER {} BEFORE {operation} ON {} BEGIN
                 SELECT RAISE(ABORT,'Library storage migration is incomplete'); END;",
                quote_identifier(&name),
                quote_identifier(&table),
            ))
            .map_err(db_error)?;
        }
    }
    tx.commit().map_err(db_error)
}

fn backup_valid(job: &MigrationJob, cancel: &AtomicBool) -> Result<()> {
    let path = job.artifact("backup.db")?;
    if job.backup_sha256.as_deref() != Some(file_hash(&path, cancel)?.as_str()) {
        return Err(invalid("Rollback backup checksum mismatch"));
    }
    let conn = open_read(&path)?;
    check_source(&conn, &job.plan, cancel)
}

fn receipt_matches(source: &Connection, job: &MigrationJob) -> Result<bool> {
    if !table_exists(source, "audiolib_migration_receipts")? {
        return Ok(false);
    }
    let saved: Option<String> = source
        .query_row(
            "SELECT plan FROM audiolib_migration_receipts WHERE job_id=?1",
            [&job.id],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_error)?;
    Ok(saved.as_deref() == Some(encode(&job.plan)?.as_str()))
}

fn import_receipt_key(job: &MigrationJob) -> String {
    format!("import-identity:{}", job.id)
}

/// Finish the crash window between the library transaction and owned job save.
/// Only the recorded identity substitution may reconstruct the old full digest.
fn reconcile_import_identity(
    source: &Connection,
    job: &mut MigrationJob,
    cancel: &AtomicBool,
    allow_later_business: bool,
) -> Result<Option<String>> {
    if job.phase != MigrationPhase::Complete {
        return Ok(None);
    }
    let saved: Option<String> = if table_exists(source, "audiolib_migration_receipts")? {
        source
            .query_row(
                "SELECT plan FROM audiolib_migration_receipts WHERE job_id=?1",
                [import_receipt_key(job)],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?
    } else {
        None
    };
    let Some(saved) = saved else {
        if job.import_identity.is_some() {
            return Err(invalid(
                "Published import identity lost its admission receipt",
            ));
        }
        return Ok(None);
    };
    let proof: ImportIdentityAdmission = serde_json::from_str(&saved)
        .map_err(|error| invalid(format!("Invalid import identity receipt: {error}")))?;
    if proof.protocol != 1
        || proof.job_id != job.id
        || !receipt_matches(source, job)?
        || uuid::Uuid::parse_str(&proof.namespace).is_err()
        || uuid::Uuid::parse_str(&proof.previous_namespace).is_err()
        || crate::albums::namespace(source)? != proof.namespace
        || crate::albums::legacy_scope(source)?.is_some()
        || (job.target_business.as_ref() != Some(&proof.before)
            && job.target_business.as_ref() != Some(&proof.after))
        || job
            .import_identity
            .as_ref()
            .is_some_and(|saved| saved != &proof)
    {
        return Err(invalid(
            "Import identity admission does not match its bound migration",
        ));
    }
    // An already mirrored proof is idempotent startup admission, not a new
    // acceptance of later annotations. Verify/rollback still compare the saved
    // complete digest and refuse to discard those later values.
    if allow_later_business
        && job.import_identity.as_ref() == Some(&proof)
        && job.target_business.as_ref() == Some(&proof.after)
    {
        return Ok(Some(proof.namespace.clone()));
    }
    if super::inspect::business_digest_before_import(
        source,
        &proof.before,
        &proof.previous_namespace,
        proof.previous_legacy_scope.as_deref(),
        cancel,
    )? != proof.before
        || business_digest(source, cancel)? != proof.after
    {
        return Err(invalid(
            "Library changed outside the recorded new-library identity admission",
        ));
    }
    let namespace = proof.namespace.clone();
    if job.import_identity.as_ref() != Some(&proof)
        || job.target_business.as_ref() != Some(&proof.after)
    {
        job.target_business = Some(proof.after.clone());
        job.import_identity = Some(proof);
        job.save()?;
    }
    Ok(Some(namespace))
}

/// Called only by explicit import admission while its destination owner is held.
/// All source writers/read generations are excluded; unknown-trigger side effects
/// abort the library transaction rather than becoming part of the accepted proof.
pub(crate) fn admit_import_identity(
    path: &Path,
    namespace: &str,
    cancel: &AtomicBool,
) -> Result<String> {
    if uuid::Uuid::parse_str(namespace).is_err() {
        return Err(invalid("Invalid imported library namespace"));
    }
    let _leases = Leases::acquire(path)?;
    let mut source = open_rw(path)?;
    crate::register_library_functions(&source)?;
    let mut completed = super::runtime::discover(path)?;
    if let Some(job) = &mut completed {
        validate_plan_identity(&job.plan)?;
        if job.phase != MigrationPhase::Complete {
            return Err(invalid(
                "Complete protected migration before admitting a new library identity",
            ));
        }
        backup_valid(job, cancel)?;
        if !receipt_matches(&source, job)? {
            return Err(invalid(
                "Published migration lost its receipt before import admission",
            ));
        }
        if let Some(namespace) = reconcile_import_identity(&source, job, cancel, true)? {
            return Ok(namespace);
        }
        stage::verify_connection(&source, job, cancel)?;
    }
    let tx = source
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(db_error)?;
    let before_projection = super::digest_import_admission(&tx, cancel)?;
    let before = business_digest(&tx, cancel)?;
    let previous_namespace = crate::albums::namespace(&tx)?;
    let previous_legacy_scope = crate::albums::legacy_scope(&tx)?;
    if previous_namespace == namespace && previous_legacy_scope.is_none() {
        return Ok(namespace.to_owned());
    }
    tx.execute(
        "UPDATE audiolib_album_id_sequence SET namespace=?1,legacy_scope=NULL WHERE singleton=1",
        [namespace],
    )
    .map_err(db_error)?;
    if super::digest_import_admission(&tx, cancel)? != before_projection {
        return Err(invalid(
            "New-library identity admission changed unrelated business data",
        ));
    }
    let after = business_digest(&tx, cancel)?;
    let proof = completed.as_ref().map(|job| ImportIdentityAdmission {
        protocol: 1,
        job_id: job.id.clone(),
        before,
        after: after.clone(),
        previous_namespace,
        previous_legacy_scope,
        namespace: namespace.to_owned(),
    });
    if let Some(proof) = &proof {
        let job = completed.as_ref().expect("proof has a migration");
        tx.execute(
            "INSERT INTO audiolib_migration_receipts(job_id,plan) VALUES(?1,?2)",
            params![import_receipt_key(job), encode(proof)?],
        )
        .map_err(db_error)?;
        if business_digest(&tx, cancel)? != after {
            return Err(invalid(
                "Import identity receipt changed unrelated business data",
            ));
        }
    }
    tx.commit().map_err(db_error)?;
    if let (Some(job), Some(proof)) = (&mut completed, proof) {
        job.target_business = Some(proof.after.clone());
        job.import_identity = Some(proof);
        job.save()?;
    }
    Ok(namespace.to_owned())
}

/// Create a durable job and execute it. Observer events expose the recovery directory
/// even if a later cancellation/error prevents returning a completed result.
pub fn apply(
    plan: &MigrationPlan,
    cancel: &AtomicBool,
    observer: &mut Observer<'_>,
) -> Result<MigrationJob> {
    validate_plan_identity(plan)?;
    validate_local_history_destination(plan)?;
    check_cancel(cancel)?;
    let _leases = Leases::acquire(&plan.source.source)?;
    let source = open_read(&plan.source.source)?;
    check_source(&source, plan, cancel)?;
    if table_exists(&source, "audiolib_storage_migration")? {
        return Err(Error::Busy(
            "An existing storage job must be resumed or rolled back".into(),
        ));
    }
    let parent = plan
        .source
        .source
        .parent()
        .ok_or_else(|| invalid("Missing source parent"))?;
    if crate::space::free_space(parent).is_none() {
        return Err(invalid(
            "Migration cannot determine available disk space; execution is blocked",
        ));
    }
    crate::space::ensure_free_space(parent, plan.space.additional_bytes)?;
    let directory = tempfile::Builder::new()
        .prefix(".audiolib-migrate-")
        .tempdir_in(parent)?
        .keep()
        .canonicalize()?;
    let id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("Invalid job name"))?
        .to_owned();
    let job = MigrationJob {
        directory,
        id,
        plan: plan.clone(),
        phase: MigrationPhase::Backup,
        backup_sha256: None,
        target_business: None,
        target_schema_sha256: None,
        import_identity: None,
    };
    let state_path = job.artifact("job.sqlite")?;
    File::options()
        .write(true)
        .create_new(true)
        .open(&state_path)?
        .sync_all()?;
    let mut state = open_rw(&state_path)?;
    let tx = state.transaction().map_err(db_error)?;
    tx.execute_batch(
        "CREATE TABLE job(singleton INTEGER PRIMARY KEY CHECK(singleton=1),payload TEXT NOT NULL);",
    )
    .map_err(db_error)?;
    tx.execute("INSERT INTO job VALUES(1,?1)", [encode(&job)?])
        .map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    drop(state);
    drop(source);
    super::runtime::register(&job)?;
    run(job, cancel, observer)
}

/// Resume exactly the original job. Changed source, backup, catalog or tokenizer blocks.
pub fn resume(
    directory: &Path,
    cancel: &AtomicBool,
    observer: &mut Observer<'_>,
) -> Result<MigrationJob> {
    let job = load(directory)?;
    let _leases = Leases::acquire(&job.plan.source.source)?;
    run(job, cancel, observer)
}

fn import_local_history(job: &MigrationJob, cancel: &AtomicBool) -> Result<()> {
    let Some(local) = &job.plan.local_history else {
        return Ok(());
    };
    check_cancel(cancel)?;
    validate_local_history_destination(&job.plan)?;
    // A p0-to-p1 upgrade has no remaining history table and must not create an
    // empty import receipt which could hide a later real legacy restore.
    let backup_path = job.artifact("backup.db")?;
    let backup = open_read(&backup_path)?;
    if !table_exists(&backup, "play_history")? {
        return Ok(());
    }
    let history = crate::local_play_history::LocalPlayHistory::open(
        &local.data_root,
        &local.library_id,
        local.limit,
    )?;
    history.import_v5_local(
        &backup,
        crate::local_play_history::LocalHistoryImportSource {
            library_id: &local.library_id,
            verified_backup_path: &backup_path,
            verified_backup_sha256: job.backup_sha256.as_deref().ok_or_else(|| {
                invalid("Local history import requires a verified rollback backup")
            })?,
        },
    )?;
    check_cancel(cancel)
}

fn run(
    mut job: MigrationJob,
    cancel: &AtomicBool,
    observer: &mut Observer<'_>,
) -> Result<MigrationJob> {
    validate_plan_identity(&job.plan)?;
    check_cancel(cancel)?;
    let mut source = open_rw(&job.plan.source.source)?;
    reconcile_import_identity(&source, &mut job, cancel, false)?;
    if job.phase == MigrationPhase::RolledBack {
        return Ok(job);
    }
    if job.phase == MigrationPhase::RollingBack {
        return rollback_locked(job, &mut source, cancel, observer);
    }
    if job.phase != MigrationPhase::Complete {
        validate_local_history_destination(&job.plan)?;
    }
    if !matches!(job.phase, MigrationPhase::Backup | MigrationPhase::Guarding) {
        backup_valid(&job, cancel)?;
        if !receipt_matches(&source, &job)? {
            if !marker_matches(&source, &job)? {
                return Err(invalid("Migration source lost its maintenance marker"));
            }
            check_source(&source, &job.plan, cancel)?;
        }
    }
    loop {
        check_cancel(cancel)?;
        job.notify(observer, job.phase, None, 0)?;
        match job.phase {
            MigrationPhase::Backup => {
                let backup = job.artifact("backup.db")?;
                if !backup.exists() {
                    management::snapshot_to_file_cancellable(&mut source, &backup, cancel)?;
                }
                check_source(&open_read(&backup)?, &job.plan, cancel)?;
                job.backup_sha256 = Some(file_hash(&backup, cancel)?);
                // Persist the verified rollback identity before an external local
                // history store is touched. A failed import leaves the source's
                // schema/data untouched and remains idempotently resumable here.
                job.save()?;
                import_local_history(&job, cancel)?;
                job.advance(MigrationPhase::Guarding)?;
            }
            MigrationPhase::Guarding => {
                backup_valid(&job, cancel)?;
                install_guards(&mut source, &job, cancel)?;
                job.advance(MigrationPhase::Normalizing)?;
            }
            MigrationPhase::Normalizing => {
                job.target_business = Some(stage::normalize(&job, cancel, observer)?);
                job.advance(MigrationPhase::Copying)?;
            }
            MigrationPhase::Copying | MigrationPhase::Indexing => {
                stage::build(&mut job, cancel, observer)?;
                let candidate = open_read(&job.artifact("candidate.db")?)?;
                stage::verify_connection(&candidate, &job, cancel)?;
                job.target_business = Some(business_digest(&candidate, cancel)?);
                job.target_schema_sha256 = Some(super::plan::schema_digest(&candidate)?);
                job.advance(MigrationPhase::Validating)?;
            }
            MigrationPhase::Validating => {
                stage::validate(&job, cancel)?;
                job.advance(MigrationPhase::Compacting)?;
            }
            MigrationPhase::Compacting => {
                let path = job.artifact("candidate.db")?;
                let mut candidate = open_rw(&path)?;
                crate::space::ensure_free_space(
                    &job.directory,
                    fs::metadata(&path)?.len().saturating_mul(2),
                )?;
                crate::owned_cancellation::run(&mut candidate, cancel, |conn| {
                    // Business tables were copied into a fresh file. VACUUM may
                    // renumber implicit rowids in unknown extension tables, so
                    // only merge FTS here; full digest verification follows.
                    conn.execute(
                        "INSERT INTO audio_search(audio_search) VALUES('optimize')",
                        [],
                    )
                    .map(drop)
                    .map_err(db_error)
                })?;
                drop(candidate);
                File::options()
                    .read(true)
                    .write(true)
                    .open(&path)?
                    .sync_all()?;
                job.advance(MigrationPhase::Publishing)?;
            }
            MigrationPhase::Publishing => {
                if !receipt_matches(&source, &job)? {
                    check_source(&source, &job.plan, cancel)?;
                    backup_valid(&job, cancel)?;
                    stage::validate(&job, cancel)?;
                    let mut candidate = open_rw(&job.artifact("candidate.db")?)?;
                    let tx = candidate.transaction().map_err(db_error)?;
                    tx.execute_batch("CREATE TABLE IF NOT EXISTS audiolib_migration_receipts(job_id TEXT PRIMARY KEY,plan TEXT NOT NULL);").map_err(db_error)?;
                    tx.execute("INSERT INTO audiolib_migration_receipts VALUES(?1,?2) ON CONFLICT(job_id) DO UPDATE SET plan=excluded.plan",
                        params![job.id,encode(&job.plan)?]).map_err(db_error)?;
                    tx.commit().map_err(db_error)?;
                    copy_atomic(&candidate, &mut source, cancel)?;
                }
                job.advance(MigrationPhase::Checkpoint)?;
            }
            MigrationPhase::Checkpoint => {
                wal::truncate(&source)?;
                stage::verify_connection(&source, &job, cancel)?;
                job.advance(MigrationPhase::Complete)?;
            }
            MigrationPhase::Complete => return Ok(job),
            MigrationPhase::RollingBack | MigrationPhase::RolledBack => unreachable!(),
        }
    }
}

pub(super) fn copy_atomic(
    from: &Connection,
    to: &mut Connection,
    cancel: &AtomicBool,
) -> Result<()> {
    let source_size: u64 = from
        .query_row(
            "SELECT page_count*page_size FROM pragma_page_count,pragma_page_size",
            [],
            |row| crate::unsigned(row, 0),
        )
        .map_err(db_error)?;
    if let Some(path) = to.path() {
        crate::space::ensure_free_space(
            Path::new(path).parent().unwrap_or(Path::new(".")),
            source_size.saturating_mul(2),
        )?;
    }
    let page_from: u32 = from
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .map_err(db_error)?;
    let page_to: u32 = to
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .map_err(db_error)?;
    if page_from != page_to {
        return Err(invalid(
            "Publication page-size mismatch; adapt only the owned candidate",
        ));
    }
    let backup = rusqlite::backup::Backup::new(from, to).map_err(db_error)?;
    loop {
        check_cancel(cancel)?;
        match backup.step(4096).map_err(db_error)? {
            rusqlite::backup::StepResult::Done => return Ok(()),
            rusqlite::backup::StepResult::More => {}
            _ => {
                return Err(Error::Busy(
                    "Migration publication is blocked by another connection".into(),
                ));
            }
        }
    }
    // Dropping an unfinished backup rolls back its destination transaction.
}

/// Verify a published result or owned candidate without changing the working library.
pub fn verify(directory: &Path, cancel: &AtomicBool) -> Result<BusinessDigest> {
    let mut job = load(directory)?;
    validate_plan_identity(&job.plan)?;
    let _leases = Leases::acquire(&job.plan.source.source)?;
    let source = open_read(&job.plan.source.source)?;
    reconcile_import_identity(&source, &mut job, cancel, false)?;
    if job.phase == MigrationPhase::RolledBack && job.backup_sha256.is_none() {
        check_source(&source, &job.plan, cancel)?;
        return business_digest(&source, cancel);
    }
    backup_valid(&job, cancel)?;
    if receipt_matches(&source, &job)? {
        // FTS integrity-check is a write-like command; run it in an in-memory
        // read-only-independent check only for staging. Published read verification
        // covers the full business projection and actual format without writes.
        stage::verify_connection(&source, &job, cancel)?;
    } else if job.phase == MigrationPhase::RolledBack {
        check_source(&source, &job.plan, cancel)?;
        return business_digest(&source, cancel);
    } else {
        stage::validate(&job, cancel)?;
    }
    job.target_business
        .ok_or_else(|| invalid("No completed candidate"))
}

/// Restore only if no business writes have occurred since publication. Never discards
/// later annotations/play history or clears source WAVs. Artifacts remain for audit.
pub fn rollback(
    directory: &Path,
    cancel: &AtomicBool,
    observer: &mut Observer<'_>,
) -> Result<MigrationJob> {
    let mut job = load(directory)?;
    validate_plan_identity(&job.plan)?;
    let _leases = Leases::acquire(&job.plan.source.source)?;
    let mut source = open_rw(&job.plan.source.source)?;
    reconcile_import_identity(&source, &mut job, cancel, false)?;
    rollback_locked(job, &mut source, cancel, observer)
}

fn rollback_locked(
    mut job: MigrationJob,
    source: &mut Connection,
    cancel: &AtomicBool,
    observer: &mut Observer<'_>,
) -> Result<MigrationJob> {
    if matches!(
        job.phase,
        MigrationPhase::Backup | MigrationPhase::RolledBack
    ) && job.backup_sha256.is_none()
    {
        check_source(source, &job.plan, cancel)?;
        if marker_matches(source, &job)? {
            return Err(invalid("Source was guarded without a recorded backup"));
        }
        job.advance(MigrationPhase::RolledBack)?;
        job.notify(observer, job.phase, None, 0)?;
        return Ok(job);
    }
    backup_valid(&job, cancel)?;
    if job.phase == MigrationPhase::RolledBack {
        check_source(source, &job.plan, cancel)?;
        return Ok(job);
    }
    if receipt_matches(source, &job)? {
        stage::verify_connection(source, &job, cancel).map_err(|_| invalid(
            "Library changed after publication; rollback would discard later data and is blocked",
        ))?;
    } else {
        // Also recognizes a crash after restoring the original but before job save.
        check_source(source, &job.plan, cancel)?;
        if !marker_matches(source, &job)? && job.phase == MigrationPhase::RollingBack {
            wal::truncate(source)?;
            job.advance(MigrationPhase::RolledBack)?;
            return Ok(job);
        }
    }
    job.advance(MigrationPhase::RollingBack)?;
    job.notify(observer, job.phase, None, 0)?;
    copy_atomic(&open_read(&job.artifact("backup.db")?)?, source, cancel)?;
    wal::truncate(source)?;
    check_source(source, &job.plan, cancel)?;
    job.advance(MigrationPhase::RolledBack)?;
    job.notify(observer, job.phase, None, 0)?;
    Ok(job)
}

#[cfg(test)]
mod import_identity_tests {
    use super::*;
    use audiolib_core::DataProvider;

    #[test]
    fn local_schema7_import_verifies_recovers_admission_commit_and_rolls_back() {
        let (root, original) = crate::album_tests::previous_fixture();
        let destination = root.path().join("imported.db");
        let data = root.path().join("client");
        fs::create_dir_all(&data).unwrap();
        let original_bytes = fs::read(&original).unwrap();
        crate::import_legacy_local(&original, &destination, &data, 0).unwrap();
        let cancel = AtomicBool::new(false);
        let job = super::super::runtime::discover(&destination)
            .unwrap()
            .unwrap();
        let proof = job
            .import_identity
            .clone()
            .expect("explicit admission proof");
        assert_ne!(proof.previous_namespace, proof.namespace);
        assert_eq!(verify(&job.directory, &cancel).unwrap(), proof.after);
        stage::validate(&job, &cancel).unwrap();
        // Simulate process loss after the atomic library+receipt commit but before
        // mirroring that proof to job.sqlite. Verification must finish only it.
        let mut interrupted = job.clone();
        interrupted.target_business = Some(proof.before.clone());
        interrupted.import_identity = None;
        interrupted.save().unwrap();
        assert_eq!(verify(&job.directory, &cancel).unwrap(), proof.after);
        let repaired = load(&job.directory).unwrap();
        assert_eq!(repaired.import_identity.as_ref(), Some(&proof));
        let provider = crate::SqliteProvider::open_existing_local_with_progress_cancellable(
            &destination,
            &data,
            0,
            &cancel,
            |_| {},
        )
        .unwrap();
        assert_eq!(provider.album_namespace(), proof.namespace);
        assert_eq!(provider.legacy_album_scope(), None);
        assert_eq!(provider.get_audio(2).unwrap().notes, "second note");
        drop(provider);
        let conn = open_rw(&destination).unwrap();
        crate::register_library_functions(&conn).unwrap();
        conn.execute(
            "UPDATE audio_files SET user_notes='later user edit' WHERE id=2",
            [],
        )
        .unwrap();
        assert!(verify(&job.directory, &cancel).is_err());
        assert!(rollback(&job.directory, &cancel, &mut |_| Ok(())).is_err());
        conn.execute(
            "UPDATE audio_files SET user_notes='second note' WHERE id=2",
            [],
        )
        .unwrap();
        drop(conn);
        assert_eq!(verify(&job.directory, &cancel).unwrap(), proof.after);
        let reverted = rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
        assert_eq!(reverted.phase, MigrationPhase::RolledBack);
        verify(&job.directory, &cancel).unwrap();
        let conn = open_read(&destination).unwrap();
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            7
        );
        assert_eq!(fs::read(&original).unwrap(), original_bytes);
    }

    #[test]
    fn import_identity_rejects_unknown_trigger_business_side_effects() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("source.db");
        let destination = root.path().join("new.db");
        let source = crate::SqliteProvider::open(&original).unwrap();
        source.writer().unwrap().execute_batch("CREATE TABLE vendor_audit(value INTEGER); INSERT INTO vendor_audit VALUES(0); CREATE TRIGGER vendor_identity_audit AFTER UPDATE OF namespace ON audiolib_album_id_sequence BEGIN UPDATE vendor_audit SET value=value+1; END;").unwrap();
        drop(source);
        let before = fs::read(&original).unwrap();
        assert!(
            matches!(crate::import_legacy(&original,&destination),Err(Error::Validation(message)) if message.contains("unrelated business data"))
        );
        assert!(!destination.exists());
        assert_eq!(fs::read(&original).unwrap(), before);
    }
}
