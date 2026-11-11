// crates/audiolib-db/src/snapshot_schema9.rs
//! Completed schema-9 snapshot admission; ordinary local writers still target schema 8.
use crate::{SqliteProvider, db_error, migration9, owned_cancellation, schema};
use audiolib_core::{Error, Result};
use rusqlite::{Connection, OpenFlags};
use std::{path::Path, sync::atomic::AtomicBool, time::Duration};

pub(crate) fn validate_read(conn: &Connection) -> Result<()> {
    schema::validate_library_identity(conn)?;
    migration9::validate_ready(conn)?;
    let vendor: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name IN (
         'vendor_metadata','vendor_import_log','audiolib_vendor_baseline','audiolib_vendor_jobs','audiolib_vendor_events'))",
        [], |row| row.get(0),
    ).map_err(db_error)?;
    // Earlier completed directory-only s9 inputs have none of these objects.
    // Once any reserved vendor structure exists, require the entire owned layout.
    if vendor {
        crate::vendor_import::validate_schema(conn)?;
    }
    Ok(())
}

fn invalid(message: &str) -> Error {
    Error::Validation(format!("Invalid schema-9 snapshot: {message}"))
}

fn validate_directory_members(conn: &Connection) -> Result<()> {
    let mismatch: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM folders f LEFT JOIN audio_dir_search_docsize d ON d.id=f.id WHERE d.id IS NULL)
         OR EXISTS(SELECT 1 FROM audio_dir_search_docsize d LEFT JOIN folders f ON f.id=d.id WHERE f.id IS NULL)",
        [], |row| row.get(0),
    ).map_err(db_error)?;
    if mismatch {
        return Err(invalid("directory FTS identities differ from folders"));
    }
    let invalid_runs: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM audio_dir_runs r LEFT JOIN folders f ON f.id=r.dir_id
         WHERE f.id IS NULL OR typeof(r.dir_id)<>'integer' OR typeof(r.lo)<>'integer'
         OR typeof(r.hi)<>'integer' OR typeof(r.n)<>'integer' OR r.lo>r.hi OR r.n<=0
         OR NOT EXISTS(SELECT 1 FROM audio_files a WHERE a.id=r.lo AND a.folder_id=r.dir_id)
         OR NOT EXISTS(SELECT 1 FROM audio_files a WHERE a.id=r.hi AND a.folder_id=r.dir_id)
         OR r.n<>(SELECT count(*) FROM audio_files a WHERE a.id BETWEEN r.lo AND r.hi AND a.folder_id=r.dir_id)
         OR EXISTS(SELECT 1 FROM audio_files a WHERE a.id BETWEEN r.lo AND r.hi
             AND a.folder_id IS NOT r.dir_id))
         OR EXISTS(SELECT 1 FROM (
             SELECT lo,max(hi) OVER (ORDER BY lo,hi,dir_id ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING) AS prior_hi
             FROM audio_dir_runs) WHERE lo<=prior_hi)
         OR (SELECT coalesce(sum(n),0) FROM audio_dir_runs)
             <>(SELECT count(*) FROM audio_files WHERE folder_id IS NOT NULL)",
        [], |row| row.get(0),
    ).map_err(db_error)?;
    if invalid_runs {
        return Err(invalid(
            "directory runs do not partition folder-owned audio IDs",
        ));
    }
    Ok(())
}

impl SqliteProvider {
    /// Validate a supported owned snapshot before cache publication, without conversion.
    ///
    /// Schema 9 additionally requires completed format markers, directory FTS integrity,
    /// exact folder membership and disjoint runs covering every folder-owned audio ID.
    /// Files without a folder have no path-index membership. Legacy snapshots retain
    /// their existing integrity and owned-conversion validation. This never creates a
    /// missing file; the caller must exclusively own the staging snapshot and separately
    /// run `check_owned_file_cancellable` for SQLite, foreign keys and the main FTS.
    pub fn validate_owned_snapshot_format_cancellable(
        path: &Path,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(db_error)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        owned_cancellation::run(&mut conn, cancel, |conn| {
            schema::validate_library_identity(conn)?;
            let version: i64 = conn
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(db_error)?;
            if version > crate::MAX_SNAPSHOT_SCHEMA {
                return Err(Error::NewerDatabase {
                    what: "schema".into(),
                    found: version,
                    supported: crate::MAX_SNAPSHOT_SCHEMA,
                });
            }
            if !crate::snapshot_schema_supported(version) {
                return Err(Error::Validation("Unsupported snapshot schema".into()));
            }
            if version != 9 {
                // Reject unknown required features but permit the existing legacy
                // conversion path to repair its explicitly supported derived state.
                schema::search_text_is_current(conn)?;
                return Ok(());
            }
            let tx = conn.transaction().map_err(db_error)?;
            validate_read(&tx)?;
            tx.execute(
                "INSERT INTO audio_dir_search(audio_dir_search) VALUES('integrity-check')",
                [],
            )
            .map_err(db_error)?;
            validate_directory_members(&tx)?;
            tx.commit().map_err(db_error)
        })
    }
}
