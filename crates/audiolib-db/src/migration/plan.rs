// crates/audiolib-db/src/migration/plan.rs
//! Read-only planning. Format and business evidence bind every later mutation.
use super::inspect::{business_digest, check_cancel, inspect_connection, table_exists};
use crate::{db_error, schema, search_storage};
use audiolib_core::{
    Error, Result,
    compatibility::{CATALOG_REVISION, ExecutionState, LibraryAccess, MigrationDisposition},
};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

/// Conservative additional free-space estimate, never a promised final size.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpaceBudget {
    /// Logical SQLite pages, including committed WAL content.
    pub source_bytes: u64,
    /// Backup, staging, compaction, publication journal and optional legacy copies.
    pub additional_bytes: u64,
    /// Whether a separate owned legacy normalization copy is required.
    pub normalizes_legacy: bool,
}

/// Legacy z1 inputs can expand far beyond their physical page count. Inspect
/// only bounded codec headers here; the pinned source business inspection and
/// candidate copy still validate the complete streams and original UTF-8 bytes.
fn metadata_expansion_bytes(conn: &Connection, cancel: &AtomicBool) -> Result<u64> {
    check_cancel(cancel)?;
    let mut statement = conn
        .prepare("SELECT length(metadata_json),substr(metadata_json,1,16) FROM audio_files WHERE typeof(metadata_json)='blob'")
        .map_err(db_error)?;
    let mut rows = statement.query([]).map_err(db_error)?;
    let mut growth = 0_u64;
    while let Some(row) = rows.next().map_err(db_error)? {
        check_cancel(cancel)?;
        let stored = crate::unsigned(row, 0).map_err(db_error)?;
        let header: Vec<u8> = row.get(1).map_err(db_error)?;
        let decoded = crate::metadata_storage::decoded_blob_length(&header)?;
        growth = growth
            .checked_add(decoded.saturating_sub(stored))
            .ok_or_else(|| invalid("Legacy Metadata expansion estimate overflow"))?;
    }
    // Filtering can consume an all-TEXT source without yielding a single row.
    check_cancel(cancel)?;
    Ok(growth)
}

/// Explicit client-local legacy history destination; never inferred for server/snapshot jobs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LocalHistoryMigration {
    /// Client data root, independent of the library and snapshot cache directories.
    pub data_root: PathBuf,
    /// Canonical local library identity shared with desktop and CLI history reads.
    pub library_id: String,
    /// Retention policy captured when this reviewed job was created.
    pub limit: u32,
}

/// Reviewable, source-bound request. Apply rechecks every field under write exclusion.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MigrationPlan {
    /// Protocol revision understood by the shared executor.
    pub protocol: u32,
    /// Exact producing application identity.
    pub application_version: String,
    /// Compatibility catalog used to choose the route.
    pub catalog_revision: u32,
    /// Tokenizer/projection sources, dictionary and locked dependency identity for resume.
    pub tokenizer_identity: String,
    /// Stable compiled route identity; changing a step cannot silently resume an old job.
    pub recipe_identity: String,
    /// Exact reviewed additions/removals, including authorized historical-data retirement.
    pub schema_changes: Vec<super::SchemaChange>,
    /// Only an explicit local client opens a history store. None means no history import.
    #[serde(default)]
    pub local_history: Option<LocalHistoryMigration>,
    /// Fully traversed source evidence.
    pub source: super::LibraryInspection,
    /// Actual SQL schema identity, excluding only this executor's internal objects.
    pub schema_sha256: String,
    /// Route-specific disk-space estimate.
    pub space: SpaceBudget,
}

pub(super) fn invalid(message: impl Into<String>) -> Error {
    Error::Validation(message.into())
}

pub(super) fn tokenizer_identity() -> String {
    let mut hash = Sha256::new();
    hash.update(b"ale-tokenizer-projection-sources-v1;tokenization=3;source_metadata=2;");
    // Bind the actual loader, Unicode/token/query implementation, SQL-function
    // adapters and Metadata projection, not only their manually maintained revs.
    // Full source files deliberately fail closed on unrelated edits to these few
    // modules. include_bytes! embeds text; it never invokes their identity functions.
    // Manifests bind dependency features/patch selection; Cargo.lock binds versions
    // and registry checksums, including Jieba's compiled dictionary dependencies.
    const INPUTS: &[(&str, &[u8])] = &[
        (
            "dictionary",
            include_bytes!("../../../../resources/audio_terms.txt"),
        ),
        ("db/lib.rs", include_bytes!("../lib.rs")),
        ("db/search_text.rs", include_bytes!("../search_text.rs")),
        ("db/schema.rs", include_bytes!("../schema.rs")),
        (
            "core/lib.rs",
            include_bytes!("../../../audiolib-core/src/lib.rs"),
        ),
        (
            "core/metadata.rs",
            include_bytes!("../../../audiolib-core/src/metadata.rs"),
        ),
        (
            "workspace/Cargo.toml",
            include_bytes!("../../../../Cargo.toml"),
        ),
        ("db/Cargo.toml", include_bytes!("../../Cargo.toml")),
        (
            "core/Cargo.toml",
            include_bytes!("../../../audiolib-core/Cargo.toml"),
        ),
        (
            "workspace/Cargo.lock",
            include_bytes!("../../../../Cargo.lock"),
        ),
    ];
    for &(name, bytes) in INPUTS {
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    format!("{:x}", hash.finalize())
}

pub(super) fn schema_digest(conn: &Connection) -> Result<String> {
    let objects = conn
        .prepare(
            "SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE sql IS NOT NULL
             AND name NOT IN ('audiolib_storage_migration','audiolib_migration_receipts')
             AND name NOT GLOB 'audiolib_storage_guard_*' ORDER BY type,name",
        )
        .map_err(db_error)?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    let bytes = serde_json::to_vec(&objects).map_err(|error| invalid(error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub(super) fn check_source(
    conn: &Connection,
    plan: &MigrationPlan,
    cancel: &AtomicBool,
) -> Result<()> {
    check_cancel(cancel)?;
    let version: u32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)?;
    if version != plan.source.format.schema_version
        || schema_digest(conn)? != plan.schema_sha256
        || Some(business_digest(conn, cancel)?) != plan.source.business
    {
        return Err(invalid(
            "Migration source changed after planning; inspect and plan again",
        ));
    }
    // Feature/revision values are deliberately excluded from the business hash.
    let actual = inspect_connection(
        conn,
        plan.source.source.clone(),
        LibraryAccess::Write,
        false,
        cancel,
    )?;
    if actual.format != plan.source.format {
        return Err(invalid("Migration source format changed after planning"));
    }
    Ok(())
}

/// Plan a supported derived-state migration without opening the source for writing.
/// Legacy v1/v2 imports and unknown/corrupt formats remain explicit blocked decisions.
pub fn plan(path: &Path, cancel: &AtomicBool) -> Result<MigrationPlan> {
    check_cancel(cancel)?;
    crate::sqlite_engine::require_plan_b()?;
    let source_path = path.canonicalize()?;
    let conn = Connection::open_with_flags(&source_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(db_error)?;
    conn.execute_batch("PRAGMA query_only=ON; BEGIN; SELECT COUNT(*) FROM sqlite_schema;")
        .map_err(db_error)?;
    if table_exists(&conn, "audiolib_storage_migration")? {
        return Err(Error::Busy(
            "Storage migration already exists; use resume or rollback".into(),
        ));
    }
    let diagnosis = super::diagnose::diagnose_connection(&conn, source_path, cancel)?;
    if diagnosis
        .findings
        .iter()
        .any(|finding| finding.code != super::DiagnosticCode::DerivedDataInvalid)
    {
        return Err(invalid(format!(
            "Migration diagnosis blocked execution: {}",
            serde_json::to_string(&diagnosis).map_err(|error| invalid(error.to_string()))?
        )));
    }
    let source = diagnosis.inspection.ok_or_else(|| {
        invalid(format!(
            "Migration diagnosis requires attention: {:?}",
            diagnosis.decision
        ))
    })?;
    if source.decision.execution_state != ExecutionState::Ready
        || source.decision.disposition != Some(MigrationDisposition::Migratable)
        || source.business.is_none()
    {
        return Err(invalid(format!(
            "No executable storage migration: {:?}",
            source.decision
        )));
    }
    let normalizes_legacy = source.format.schema_version < 5
        || schema::pending(&conn)?
        || (source.format.schema_version == 5 && schema::legacy_needs_upgrade(&conn)?);
    if !normalizes_legacy {
        // Reject unsupported extension dependencies before creating a job or backup.
        search_storage::blueprint_with(&conn, &super::recipe::changes())?;
    }
    let source_bytes: u64 = conn.query_row(
        "SELECT (SELECT page_count FROM pragma_page_count)*(SELECT page_size FROM pragma_page_size)",
        [], |row| crate::unsigned(row, 0),
    ).map_err(db_error)?;
    let copies = if normalizes_legacy { 8 } else { 5 };
    let metadata_growth = if source.format.features.get("metadata_storage") == Some(&1) {
        metadata_expansion_bytes(&conn, cancel)?
    } else {
        0
    };
    let additional_bytes = source_bytes
        .checked_add(metadata_growth)
        .and_then(|expanded| expanded.checked_mul(copies))
        .and_then(|value| value.checked_add(1 << 30))
        .ok_or_else(|| invalid("Migration space estimate overflow"))?;
    Ok(MigrationPlan {
        protocol: 1,
        application_version: env!("CARGO_PKG_VERSION").into(),
        catalog_revision: CATALOG_REVISION,
        tokenizer_identity: tokenizer_identity(),
        recipe_identity: super::recipe::identity(),
        schema_changes: super::recipe::changes(),
        local_history: None,
        schema_sha256: schema_digest(&conn)?,
        source,
        space: SpaceBudget {
            source_bytes,
            additional_bytes,
            normalizes_legacy,
        },
    })
}

/// Plan the same protected conversion for a client-owned local library. The plan
/// records its history destination so a restart cannot silently skip the import.
pub fn plan_local(
    path: &Path,
    data_root: &Path,
    limit: u32,
    cancel: &AtomicBool,
) -> Result<MigrationPlan> {
    let mut reviewed = plan(path, cancel)?;
    // Planning remains read-only: the client owns and creates its data root
    // before this API. Persist the real directory, never a re-pointable alias.
    let data_root = data_root.canonicalize()?;
    if !data_root.is_dir() {
        return Err(invalid("History data root must be an existing directory"));
    }
    reviewed.local_history = Some(LocalHistoryMigration {
        library_id: crate::local_play_history::local_library_id(&reviewed.source.source)?,
        data_root,
        limit,
    });
    Ok(reviewed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::{MigrationPhase, apply, discover, resume, rollback, verify};

    #[test]
    fn legacy_metadata_budget_observes_cancellation_when_no_blob_row_is_returned() {
        use std::sync::{Arc, atomic::Ordering};

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE audio_files(metadata_json); WITH RECURSIVE ids(id) AS (VALUES(1) UNION ALL SELECT id+1 FROM ids WHERE id<10001) INSERT INTO audio_files SELECT '{}' FROM ids;").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let observed = cancel.clone();
        conn.progress_handler(
            500,
            Some(move || {
                observed.store(true, Ordering::Release);
                false
            }),
        )
        .unwrap();
        let result = metadata_expansion_bytes(&conn, &cancel);
        conn.progress_handler(0, None::<fn() -> bool>).unwrap();
        assert!(matches!(result, Err(Error::Cancelled)));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM audio_files", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            10001
        );
    }

    #[test]
    fn legacy_metadata_budget_accounts_for_text_expansion_without_mutating_input() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE audio_files(metadata_json)")
            .unwrap();
        let raw = format!(" {{\"vendor\":\"{}\"}}\n", "音效 ".repeat(5000));
        let rusqlite::types::Value::Blob(blob) =
            crate::metadata_storage::encode_legacy_fixture(&raw).unwrap()
        else {
            panic!("legacy fixture must be compressed");
        };
        conn.execute(
            "INSERT INTO audio_files VALUES(?1),(?2)",
            rusqlite::params![raw, blob],
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        assert_eq!(
            metadata_expansion_bytes(&conn, &cancel).unwrap(),
            raw.len() as u64 - blob.len() as u64
        );
        let retained: Vec<u8> = conn
            .query_row(
                "SELECT metadata_json FROM audio_files WHERE rowid=2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, blob);
        assert!(matches!(
            metadata_expansion_bytes(&conn, &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        conn.execute("INSERT INTO audio_files VALUES(?1)", [vec![0_u8; 16]])
            .unwrap();
        assert!(metadata_expansion_bytes(&conn, &cancel).is_err());
    }

    #[test]
    fn old_tokenizer_identity_in_same_version_job_blocks_before_touching_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("library.db");
        let mut conn = Connection::open(&path).unwrap();
        schema::register_tokenizer(&conn, crate::load_tokenizer()).unwrap();
        schema::migrate(&mut conn).unwrap();
        conn.execute_batch(
            "WITH RECURSIVE ids(id) AS (VALUES(1) UNION ALL SELECT id+1 FROM ids WHERE id<5001)
             INSERT INTO audio_files(id,file_path,filename,metadata_json)
             SELECT id,'owned/'||id||'.wav','metal.wav',
                    '{\"wav_metadata\":{\"fields\":{\"keywords\":\"木头\"}}}' FROM ids;",
        )
        .unwrap();
        drop(conn);
        let cancel = AtomicBool::new(false);
        let reviewed = plan(&path, &cancel).unwrap();
        assert!(matches!(
            apply(&reviewed, &cancel, &mut |event| {
                if event.phase == MigrationPhase::Indexing && event.completed > 0 {
                    Err(Error::Cancelled)
                } else {
                    Ok(())
                }
            }),
            Err(Error::Cancelled)
        ));
        let mut job = discover(&path).unwrap().unwrap();
        assert_eq!(job.phase, MigrationPhase::Indexing);
        assert_eq!(job.plan.application_version, env!("CARGO_PKG_VERSION"));

        // A real pre-fix identity: same application/version and dictionary, but no
        // binding to the executable tokenizer or Metadata implementation. Persist
        // it in an already partially indexed job, rather than only comparing hashes.
        let mut old_identity = Sha256::new();
        old_identity.update(b"jieba-rs=0.11.0;tokenization=3;source_metadata=2;");
        old_identity.update(include_bytes!("../../../../resources/audio_terms.txt"));
        job.plan.tokenizer_identity = format!("{:x}", old_identity.finalize());
        job.save().unwrap();
        let artifacts = [
            path,
            job.artifact("backup.db").unwrap(),
            job.artifact("candidate.db").unwrap(),
        ];
        let before = artifacts
            .each_ref()
            .map(|path| std::fs::read(path).unwrap());
        for result in [
            resume(&job.directory, &cancel, &mut |_| Ok(())).map(|_| ()),
            verify(&job.directory, &cancel).map(|_| ()),
            rollback(&job.directory, &cancel, &mut |_| Ok(())).map(|_| ()),
        ] {
            assert!(matches!(result, Err(Error::Validation(message))
                if message.contains("original supported tool/catalog/tokenizer")));
        }
        for (path, bytes) in artifacts.iter().zip(before) {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        assert_eq!(
            discover(&artifacts[0]).unwrap().unwrap().phase,
            MigrationPhase::Indexing
        );
    }
}
