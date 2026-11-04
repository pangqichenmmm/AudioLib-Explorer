// crates/audiolib-db/src/migration/stage.rs
//! Owned staging copies preserve row identities and commit watermarks with copied data.
use super::{
    BusinessDigest, MigrationJob, MigrationPhase, SchemaChange,
    inspect::{
        business_digest, business_digest_matching, business_digest_retained, check_cancel,
        table_exists,
    },
    job::{Observer, open_read, open_rw},
    plan::invalid,
};
use crate::{db_error, management, schema, search_storage};
use audiolib_core::{Result, compatibility::LibraryFormat};
use rusqlite::{Connection, OpenFlags, params};
use std::{fs::File, path::PathBuf, sync::atomic::AtomicBool};

const BATCH: u32 = 5000;
const STATE: &str = "audiolib_migration_copy";

fn sql_count(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| invalid("Migration row count exceeds SQLite integer range"))
}

fn input_path(job: &MigrationJob) -> Result<PathBuf> {
    job.artifact(if job.plan.space.normalizes_legacy {
        "normalized.db"
    } else {
        "backup.db"
    })
}

pub(super) fn normalize(
    job: &MigrationJob,
    cancel: &AtomicBool,
    observer: &mut Observer<'_>,
) -> Result<BusinessDigest> {
    let path = input_path(job)?;
    if job.plan.space.normalizes_legacy {
        if !path.exists() {
            let mut original = open_read(&job.artifact("backup.db")?)?;
            management::snapshot_to_file_cancellable(&mut original, &path, cancel)?;
        }
        let mut conn = open_rw(&path)?;
        schema::register_tokenizer(&conn, crate::load_tokenizer())?;
        crate::owned_cancellation::run(&mut conn, cancel, |conn| {
            schema::migrate_resumable(conn, &path, cancel, &mut |_| {
                job.notify(observer, MigrationPhase::Normalizing, None, 0)
            })?;
            schema::backfill_source_tags(conn)?;
            Ok(())
        })?;
    }
    let conn = open_read(&path)?;
    let original = job
        .plan
        .source
        .business
        .as_ref()
        .ok_or_else(|| invalid("Missing source digest"))?;
    if business_digest_matching(&conn, Some(original), cancel)? != *original {
        return Err(invalid(
            "Legacy normalization changed authoritative business data",
        ));
    }
    search_storage::blueprint_with(&conn, &job.plan.schema_changes)?;
    // Only the compiled, explicitly declared retirement can leave the target digest.
    let mut projection = business_digest(&conn, cancel)?;
    for change in &job.plan.schema_changes {
        match change {
            SchemaChange::RetireTable { table } => projection
                .tables
                .retain(|entry| !entry.table.eq_ignore_ascii_case(table)),
            SchemaChange::DropDerivedColumn { table, column }
            | SchemaChange::RetireColumn { table, column } => {
                if let Some(entry) = projection
                    .tables
                    .iter_mut()
                    .find(|entry| entry.table.eq_ignore_ascii_case(table))
                {
                    entry
                        .columns
                        .retain(|name| !name.eq_ignore_ascii_case(column));
                }
            }
            SchemaChange::AddColumn { .. } | SchemaChange::CreateAlbumEntities => {}
        }
    }
    let retired = job
        .plan
        .schema_changes
        .iter()
        .filter_map(|change| match change {
            SchemaChange::RetireTable { table } => Some(table.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    business_digest_retained(&conn, Some(&projection), &retired, cancel)
}

fn read_only_uri(path: &std::path::Path) -> String {
    #[cfg(windows)]
    let native = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    let native = native
        .strip_prefix("//?/UNC/")
        .map(|rest| format!("//{rest}"))
        .or_else(|| native.strip_prefix("//?/").map(str::to_owned))
        .unwrap_or(native);
    #[cfg(windows)]
    let bytes = native.as_bytes();
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        // Backslashes and non-UTF-8 bytes are filename data on POSIX filesystems.
        path.as_os_str().as_bytes()
    };
    #[cfg(not(any(windows, unix)))]
    let native = path.to_string_lossy();
    #[cfg(not(any(windows, unix)))]
    let bytes = native.as_bytes();
    let mut encoded = String::from("file:");
    // SQLite's bundled URI parser only accepts an empty or localhost authority.
    // `file:////host/share` retains an empty authority and the complete UNC path
    // `//host/share`; `file://host/share` would reject the host before opening it.
    if bytes.starts_with(b"//") {
        encoded.push_str("//");
    }
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || b"/:._-".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("{encoded}?mode=ro")
}

/// Writable stored columns plus an unshadowed rowid alias preserve implicit identities.
/// Generated columns are recomputed by SQLite and are checked by the business digest.
pub(super) fn copy_columns(conn: &Connection, table: &str) -> Result<(Vec<String>, bool)> {
    let info = conn
        .prepare("SELECT name,hidden FROM pragma_table_xinfo(?1) ORDER BY cid")
        .map_err(db_error)?
        .query_map([table], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    let without_rowid: bool = conn
        .query_row(
            "SELECT wr FROM pragma_table_list WHERE schema='main' AND name=?1",
            [table],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    let mut columns = info
        .iter()
        .filter(|(_, hidden)| *hidden == 0)
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    if !without_rowid {
        let alias = ["rowid", "_rowid_", "oid"]
            .into_iter()
            .find(|alias| {
                !info
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(alias))
            })
            .ok_or_else(|| invalid(format!("Table {table} hides every rowid alias")))?;
        columns.insert(0, alias.into());
    }
    Ok((columns, !without_rowid))
}

fn initialize(job: &MigrationJob, candidate: &mut Connection, input: &Connection) -> Result<()> {
    let count: i64 = candidate
        .query_row("SELECT COUNT(*) FROM sqlite_schema", [], |row| row.get(0))
        .map_err(db_error)?;
    if count != 0 {
        return Ok(());
    }
    let blueprint = search_storage::blueprint_with(input, &job.plan.schema_changes)?;
    let page_size: u32 = input
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .map_err(db_error)?;
    let application_id: i32 = input
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(db_error)?;
    candidate
        .pragma_update(None, "page_size", page_size)
        .map_err(db_error)?;
    candidate
        .pragma_update(None, "application_id", application_id)
        .map_err(db_error)?;
    let tx = candidate.transaction().map_err(db_error)?;
    for (_, sql) in &blueprint.tables {
        search_storage::execute_schema(&tx, sql)?;
    }
    if table_exists(input, "sqlite_sequence")? && !table_exists(&tx, "sqlite_sequence")? {
        // Keep an empty sequence catalog even when its only AUTOINCREMENT table
        // was explicitly retired. SQLite creates this internal table itself.
        tx.execute_batch(
            "CREATE TABLE audiolib_sequence_bootstrap(id INTEGER PRIMARY KEY AUTOINCREMENT);
            DROP TABLE audiolib_sequence_bootstrap;",
        )
        .map_err(db_error)?;
    }
    tx.execute_batch(
        "CREATE TABLE audiolib_migration_copy(
          name TEXT PRIMARY KEY,phase TEXT NOT NULL,last_id INTEGER,completed INTEGER NOT NULL,done INTEGER NOT NULL);
         INSERT INTO audiolib_migration_copy VALUES('', 'copy',NULL,0,0);",
    ).map_err(db_error)?;
    for (name, _) in &blueprint.tables {
        if !table_exists(input, name)? {
            continue;
        }
        tx.execute(
            "INSERT INTO audiolib_migration_copy VALUES(?1,'table',NULL,0,0)",
            [name],
        )
        .map_err(db_error)?;
    }
    tx.commit().map_err(db_error)
}

fn list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| search_storage::quote_identifier(name))
        .collect::<Vec<_>>()
        .join(",")
}

fn copy_table(
    job: &MigrationJob,
    conn: &mut Connection,
    table: &str,
    cancel: &AtomicBool,
    observer: &mut Observer<'_>,
) -> Result<()> {
    let (mut columns, has_rowid) = copy_columns(conn, table)?;
    let input_columns = conn
        .prepare("SELECT name FROM pragma_table_xinfo(?1,'input')")
        .map_err(db_error)?
        .query_map([table], |row| row.get::<_, String>(0))
        .map_err(db_error)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db_error)?;
    // Added fields use their declared defaults. A recipe's postcondition must check
    // the new values; existing columns never get positional SELECT * treatment.
    let implicit = has_rowid.then(|| columns[0].clone());
    columns.retain(|column| {
        input_columns
            .iter()
            .any(|input| input.eq_ignore_ascii_case(column))
            || implicit
                .as_ref()
                .is_some_and(|alias| alias.eq_ignore_ascii_case(column))
    });
    let projection = list(&columns);
    // Recompute this explicitly derived field while copying, before installing
    // extension triggers. A schema-6 repair must not retain stale source-tag keys
    // or fire a user's UPDATE trigger as a side effect of rebuilding search.
    let source_projection = columns
        .iter()
        .map(|column| {
            if table.eq_ignore_ascii_case("audio_files")
                && column.eq_ignore_ascii_case("source_tags_text")
            {
                "audiolib_source_tags_key(metadata_json)".into()
            } else if table.eq_ignore_ascii_case("audio_files")
                && column.eq_ignore_ascii_case("metadata_json")
            {
                // A legacy experimental BLOB is decoded once; every new
                // candidate stores the original UTF-8 bytes as SQLite TEXT.
                "audiolib_metadata_json(metadata_json)".into()
            } else {
                search_storage::quote_identifier(column)
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let quoted = search_storage::quote_identifier(table);
    let order = if has_rowid {
        search_storage::quote_identifier(&columns[0])
    } else {
        let primary = conn
            .prepare("SELECT name FROM pragma_table_xinfo(?1) WHERE pk>0 ORDER BY pk")
            .map_err(db_error)?
            .query_map([table], |row| row.get::<_, String>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        list(&primary)
    };
    loop {
        check_cancel(cancel)?;
        let (last, completed, done): (Option<i64>, u64, bool) = conn
            .query_row(
                "SELECT last_id,completed,done FROM audiolib_migration_copy WHERE name=?1",
                [table],
                |row| Ok((row.get(0)?, crate::unsigned(row, 1)?, row.get(2)?)),
            )
            .map_err(db_error)?;
        if done {
            return Ok(());
        }
        let tx = conn.transaction().map_err(db_error)?;
        let (high, copied) = if has_rowid {
            let lower = if last.is_some() { ">" } else { ">=" };
            let high: Option<i64> = tx.query_row(&format!(
                "SELECT MAX({order}) FROM (SELECT {order} FROM input.{quoted} WHERE {order}{lower}?1 ORDER BY {order} LIMIT ?2)",
            ), params![last.unwrap_or(i64::MIN),BATCH], |row| row.get(0)).map_err(db_error)?;
            let copied = if let Some(high) = high {
                tx.execute(
                    &format!(
                    "INSERT INTO main.{quoted}({projection}) SELECT {source_projection} FROM input.{quoted}
                     WHERE {order}{lower}?1 AND {order}<=?2 ORDER BY {order}",
                ),
                    params![last.unwrap_or(i64::MIN), high],
                )
                .map_err(db_error)?
            } else {
                0
            };
            (high.or(last), copied as u64)
        } else {
            let copied = tx
                .execute(
                    &format!(
                "INSERT INTO main.{quoted}({projection}) SELECT {source_projection} FROM input.{quoted}
                 ORDER BY {order} LIMIT ?1 OFFSET ?2",
            ),
                    params![BATCH, sql_count(completed)?],
                )
                .map_err(db_error)?;
            (None, copied as u64)
        };
        tx.execute(
            "UPDATE audiolib_migration_copy SET last_id=?2,completed=?3,done=?4 WHERE name=?1",
            params![table, high, sql_count(completed + copied)?, copied == 0],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        job.notify(
            observer,
            MigrationPhase::Copying,
            Some(table),
            completed + copied,
        )?;
    }
}

/// Index only the owned candidate during album derivation. The source blueprint's
/// ordinary indexes are installed afterwards; relying on them here makes each
/// member's ancestor lookup scan the complete closure table.
fn backfill_albums(conn: &Connection, cancel: &AtomicBool) -> Result<()> {
    search_storage::transaction_required(conn)?;
    let name = format!("audiolib_album_backfill_{}", uuid::Uuid::new_v4().simple());
    let quoted = search_storage::quote_identifier(&name);
    // No IF NOT EXISTS: an unexpected collision fails before we can remove any
    // pre-existing object. The caller rolls the entire owned transaction back.
    conn.execute(
        &format!(
            "CREATE INDEX main.{quoted} ON folder_closure(descendant_id,depth DESC,ancestor_id)"
        ),
        [],
    )
    .map_err(db_error)?;
    crate::albums::backfill(conn, cancel)?;
    conn.execute(&format!("DROP INDEX main.{quoted}"), [])
        .map_err(db_error)?;
    Ok(())
}

pub(super) fn build(
    job: &mut MigrationJob,
    cancel: &AtomicBool,
    observer: &mut Observer<'_>,
) -> Result<()> {
    let path = job.artifact("candidate.db")?;
    if !path.exists() {
        File::options()
            .create_new(true)
            .write(true)
            .open(&path)?
            .sync_all()?;
    }
    let input_path = input_path(job)?;
    let input = open_read(&input_path)?;
    let mut candidate = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(db_error)?;
    candidate
        .execute_batch(
            "PRAGMA foreign_keys=OFF; PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;",
        )
        .map_err(db_error)?;
    schema::register_tokenizer(&candidate, crate::load_tokenizer())?;
    initialize(job, &mut candidate, &input)?;
    if !table_exists(&candidate, STATE)? {
        return verify_connection(&candidate, job, cancel);
    }
    candidate
        .execute("ATTACH DATABASE ?1 AS input", [read_only_uri(&input_path)])
        .map_err(db_error)?;
    crate::owned_cancellation::run(&mut candidate, cancel, |candidate| {
        super::checkpoint::validate(job, candidate, &input)?;
        let phase: String = candidate
            .query_row(
                "SELECT phase FROM audiolib_migration_copy WHERE name=''",
                [],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if phase == "copy" {
            if job.phase == MigrationPhase::Indexing {
                return Err(invalid(
                    "Candidate copy state regressed after indexing began",
                ));
            }
            let tables = candidate
                .prepare("SELECT name FROM audiolib_migration_copy WHERE name<>'' ORDER BY name")
                .map_err(db_error)?
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(db_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db_error)?;
            for table in tables {
                copy_table(job, candidate, &table, cancel, observer)?;
            }
            let blueprint = search_storage::blueprint_with(&input, &job.plan.schema_changes)?;
            let tx = candidate.transaction().map_err(db_error)?;
            if table_exists(&input, "sqlite_sequence")? {
                // Preserve AUTOINCREMENT high-water values; retired tables have no sequence.
                tx.execute_batch(
                    "DELETE FROM sqlite_sequence;
                    INSERT INTO sqlite_sequence(rowid,name,seq)
                    SELECT rowid,name,seq FROM input.sqlite_sequence
                    WHERE name COLLATE NOCASE IN (SELECT name FROM main.sqlite_schema WHERE type='table');",
                )
                .map_err(db_error)?;
            }
            backfill_albums(&tx, cancel).map_err(|e| match e {
                audiolib_core::Error::Database(message) => {
                    audiolib_core::Error::Database(format!("Album candidate backfill: {message}"))
                }
                other => other,
            })?;
            crate::albums::configure_compression(&tx, false)?;
            if job.plan.source.format.schema_version < 8 && job.plan.local_history.is_some() {
                tx.execute("UPDATE audiolib_album_id_sequence SET legacy_scope=?1 WHERE singleton=1 AND legacy_scope IS NULL",[job.plan.source.source.to_string_lossy().as_ref()]).map_err(db_error)?;
            }
            for (name, sql) in &blueprint.objects {
                if [
                    "audiolib_album_id_high_water",
                    "audiolib_album_audio_insert",
                    "audiolib_album_audio_update",
                    "audiolib_album_folder_delete",
                    "idx_albums_folder",
                    "idx_audio_visible_album",
                    "idx_audio_album_members",
                ]
                .contains(&name.as_str())
                {
                    continue;
                }
                search_storage::execute_schema(&tx, sql)
                    .map_err(|error| invalid(format!("Candidate object {name}: {error}")))?;
            }
            crate::albums::install_triggers(&tx)?;
            search_storage::create_index(&tx)?;
            tx.execute("UPDATE audiolib_migration_copy SET phase='index',last_id=NULL,completed=0 WHERE name=''", []).map_err(db_error)?;
            tx.commit().map_err(db_error)?;
        } else if phase != "index" {
            return Err(invalid("Unknown candidate copy phase"));
        }
        // The candidate commits its own phase/cursor first. If the process exits
        // before this job update, resumption observes that committed candidate
        // state and advances the job without copying any rows again.
        if job.phase != MigrationPhase::Indexing {
            job.phase = MigrationPhase::Indexing;
            job.save()?;
        }
        loop {
            check_cancel(cancel)?;
            let (last, completed): (Option<i64>, u64) = candidate
                .query_row(
                    "SELECT last_id,completed FROM audiolib_migration_copy WHERE name=''",
                    [],
                    |row| Ok((row.get(0)?, crate::unsigned(row, 1)?)),
                )
                .map_err(db_error)?;
            let tx = candidate.transaction().map_err(db_error)?;
            let (high, count) = search_storage::index_batch(&tx, last, BATCH)?;
            tx.execute(
                "UPDATE audiolib_migration_copy SET last_id=?1,completed=?2 WHERE name=''",
                params![high, sql_count(completed + count)?],
            )
            .map_err(db_error)?;
            if count == 0 {
                search_storage::install_triggers(&tx)?;
                schema::mark_current(&tx)?;
                tx.execute_batch("DROP TABLE audiolib_migration_copy;")
                    .map_err(db_error)?;
                tx.commit().map_err(db_error)?;
                return Ok(());
            }
            tx.commit().map_err(db_error)?;
            job.notify(
                observer,
                MigrationPhase::Indexing,
                Some("audio_files"),
                completed + count,
            )?;
        }
    })
}

fn validate_text_metadata(conn: &Connection, cancel: &AtomicBool) -> Result<()> {
    // Bounded primary-key ranges preserve cancellation even for published
    // verification, which does not install the owned-writer interrupt hook.
    let mut last = None;
    loop {
        check_cancel(cancel)?;
        let high: Option<i64> = if let Some(low) = last {
            conn.query_row(
                "SELECT MAX(id) FROM (SELECT id FROM audio_files WHERE id>?1 ORDER BY id LIMIT ?2)",
                params![low, BATCH],
                |row| row.get(0),
            )
            .map_err(db_error)?
        } else {
            conn.query_row(
                "SELECT MAX(id) FROM (SELECT id FROM audio_files ORDER BY id LIMIT ?1)",
                [BATCH],
                |row| row.get(0),
            )
            .map_err(db_error)?
        };
        let Some(high) = high else { return Ok(()) };
        let non_text: bool = if let Some(low) = last {
            conn.query_row("SELECT EXISTS(SELECT 1 FROM audio_files WHERE id>?1 AND id<=?2 AND typeof(metadata_json)<>'text')", params![low, high], |row| row.get(0)).map_err(db_error)?
        } else {
            conn.query_row("SELECT EXISTS(SELECT 1 FROM audio_files WHERE id<=?1 AND typeof(metadata_json)<>'text')", [high], |row| row.get(0)).map_err(db_error)?
        };
        if non_text {
            return Err(invalid("Candidate Metadata must use SQLite TEXT storage"));
        }
        last = Some(high);
    }
}

pub(super) fn verify_connection(
    conn: &Connection,
    job: &MigrationJob,
    cancel: &AtomicBool,
) -> Result<()> {
    // Logical decoded-byte parity alone cannot label mixed legacy storage as a1.
    validate_text_metadata(conn, cancel)?;
    let expected = job
        .target_business
        .as_ref()
        .ok_or_else(|| invalid("Missing candidate business digest"))?;
    let actual = business_digest(conn, cancel)?;
    let mut expected_shape = expected
        .tables
        .iter()
        .map(|entry| (entry.table.clone(), entry.columns.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    for change in &job.plan.schema_changes {
        if let SchemaChange::AddColumn { table, column, .. } = change
            && let Some((_, columns)) = expected_shape
                .iter_mut()
                .find(|(name, _)| name.eq_ignore_ascii_case(table))
            && !columns.iter().any(|name| name.eq_ignore_ascii_case(column))
        {
            columns.push(column.clone());
        }
    }
    if job.target_schema_sha256.is_none()
        && job
            .plan
            .schema_changes
            .contains(&SchemaChange::CreateAlbumEntities)
    {
        for table in &actual.tables {
            if ["albums", "audiolib_album_id_sequence"].contains(&table.table.as_str()) {
                expected_shape
                    .entry(table.table.clone())
                    .or_insert_with(|| table.columns.clone());
            }
        }
    }
    let actual_shape = actual
        .tables
        .iter()
        .map(|entry| (entry.table.clone(), entry.columns.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    if actual_shape != expected_shape
        || business_digest_matching(conn, Some(expected), cancel)? != *expected
    {
        let changed: Vec<_> = actual
            .tables
            .iter()
            .filter(|entry| !expected.tables.contains(entry))
            .map(|entry| entry.table.as_str())
            .collect();
        return Err(invalid(format!(
            "Candidate business projection differs from the reviewed migration (tables: {})",
            changed.join(", "),
        )));
    }
    if let Some(schema_hash) = &job.target_schema_sha256
        && super::plan::schema_digest(conn)? != *schema_hash
    {
        return Err(invalid("Candidate schema changed after construction"));
    }
    // Before saving the final target hash, validate new fields against the
    // declaration. Subsequent verification uses the complete saved projection.
    if job.target_schema_sha256.is_none() {
        for change in &job.plan.schema_changes {
            if let SchemaChange::AddColumn { table, column, .. } = change {
                if table == "audio_files" && column == "album_id" {
                    if !crate::albums::ready(conn)? {
                        return Err(invalid("Shared album backfill is incomplete"));
                    }
                    continue;
                }
                let default: Option<String> = conn
                    .query_row(
                        "SELECT dflt_value FROM pragma_table_xinfo(?1) WHERE name=?2 COLLATE NOCASE",
                        params![table, column],
                        |row| row.get(0),
                    )
                    .map_err(db_error)?;
                let query = format!(
                    "SELECT COUNT(*) FROM {} WHERE NOT ({} IS ({}))",
                    search_storage::quote_identifier(table),
                    search_storage::quote_identifier(column),
                    default.as_deref().unwrap_or("NULL")
                );
                let mismatches: i64 = conn
                    .query_row(&query, [], |row| row.get(0))
                    .map_err(db_error)?;
                if mismatches != 0 {
                    return Err(invalid(format!(
                        "Added field {table}.{column} violates its declared backfill"
                    )));
                }
            }
        }
    }
    if !search_storage::structure_matches(conn)? {
        return Err(invalid(
            "Candidate search structure is not the supported target",
        ));
    }
    let inspection = super::inspect::inspect_connection(
        conn,
        job.plan.source.source.clone(),
        audiolib_core::compatibility::LibraryAccess::Write,
        false,
        cancel,
    )?;
    if inspection.format != LibraryFormat::target() {
        return Err(invalid(
            "Candidate format markers do not match the target catalog",
        ));
    }
    if !schema::current_format_ready(conn)? {
        return Err(invalid("Candidate derived schema is incomplete"));
    }
    let quick: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(db_error)?;
    let foreign_keys: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .map_err(db_error)?;
    let unmatched: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM audio_files af LEFT JOIN audio_search_docsize ds ON af.id=ds.id WHERE ds.id IS NULL)
         +(SELECT COUNT(*) FROM audio_search_docsize ds LEFT JOIN audio_files af ON af.id=ds.id WHERE af.id IS NULL)",
        [], |row| row.get(0),
    ).map_err(db_error)?;
    if quick != "ok" || foreign_keys != 0 || unmatched != 0 {
        return Err(invalid(format!(
            "Candidate integrity failed: quick={quick}, foreign_keys={foreign_keys}, unmatched_documents={unmatched}"
        )));
    }
    if table_exists(conn, "play_history")?
        || search_storage::columns(conn)?.iter().any(|column| {
            column.eq_ignore_ascii_case("play_count")
                || column.eq_ignore_ascii_case("last_played_at")
        })
    {
        return Err(invalid("Authorized playback retirement is incomplete"));
    }
    Ok(())
}

pub(super) fn validate(job: &MigrationJob, cancel: &AtomicBool) -> Result<()> {
    // The immutable candidate precedes a separately proved new-library identity
    // admission; published verification still uses the complete admitted digest.
    let mut original = job.clone();
    if let Some(proof) = &job.import_identity {
        original.target_business = Some(proof.before.clone());
        original.import_identity = None;
    }
    let job = &original;
    let mut conn = open_rw(&job.artifact("candidate.db")?)?;
    schema::register_tokenizer(&conn, crate::load_tokenizer())?;
    crate::owned_cancellation::run(&mut conn, cancel, |conn| {
        verify_connection(conn, job, cancel)?;
        conn.execute(
            "INSERT INTO audio_search(audio_search) VALUES('integrity-check')",
            [],
        )
        .map_err(db_error)?;
        Ok(())
    })
}

#[cfg(test)]
mod album_backfill_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn candidate() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::register_library_functions(&conn).unwrap();
        // The explicit owned index must provide the bound; a planner-generated
        // automatic index must not mask a regression on another SQLite build.
        conn.pragma_update(None, "automatic_index", false).unwrap();
        conn.execute_batch("CREATE TABLE folders(id INTEGER PRIMARY KEY,folder_path TEXT,folder_name TEXT,parent_id INTEGER);
            CREATE TABLE folder_closure(ancestor_id INTEGER,descendant_id INTEGER,depth INTEGER,PRIMARY KEY(ancestor_id,descendant_id));
            CREATE TABLE audio_files(id INTEGER PRIMARY KEY,file_path TEXT,filename TEXT,folder_id INTEGER,metadata_json TEXT NOT NULL DEFAULT '{}',album_id INTEGER,missing INTEGER NOT NULL DEFAULT 0);
            INSERT INTO folders VALUES(1,'owned','owned',NULL);
            WITH RECURSIVE groups(g) AS (VALUES(0) UNION ALL SELECT g+1 FROM groups WHERE g<1023), levels(n) AS (VALUES(0),(1),(2),(3))
            INSERT INTO folders SELECT 2+4*g+n,'owned/library-'||g||'/level-'||n,CASE WHEN n=0 THEN 'library-'||g ELSE 'nested-'||n END,CASE WHEN n=0 THEN 1 ELSE 1+4*g+n END FROM groups CROSS JOIN levels;
            WITH RECURSIVE ancestors(descendant,ancestor,depth) AS (SELECT id,id,0 FROM folders UNION ALL SELECT a.descendant,f.parent_id,a.depth+1 FROM ancestors a JOIN folders f ON f.id=a.ancestor WHERE f.parent_id IS NOT NULL)
            INSERT INTO folder_closure SELECT ancestor,descendant,depth FROM ancestors;
            WITH RECURSIVE ids(id) AS (VALUES(1) UNION ALL SELECT id+1 FROM ids WHERE id<2048)
            INSERT INTO audio_files(id,file_path,filename,folder_id) SELECT id,'owned/'||id||'.wav','sound.wav',5+4*((id-1)%1024) FROM ids;
            INSERT INTO audio_files(id,file_path,filename,folder_id) VALUES(2049,'owned/direct.wav','direct.wav',1);
            CREATE INDEX audiolib_album_backfill_keep_unknown ON folders(folder_name);").unwrap();
        conn
    }

    #[test]
    fn owned_album_backfill_without_descendant_index_has_a_linear_vm_budget() {
        let mut conn = candidate();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM folders", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            4097
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM folder_closure", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            14337
        );
        // More than 29M closure rows would be inspected by one full scan per
        // audio. A progress budget fails that regression promptly and is stable
        // across host speed; it counts native bundled SQLite VM instructions.
        const BUDGET: usize = 4_000_000;
        let steps = Arc::new(AtomicUsize::new(0));
        let observed = steps.clone();
        conn.progress_handler(
            100,
            Some(move || observed.fetch_add(100, Ordering::Relaxed) >= BUDGET),
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        let result = backfill_albums(&tx, &AtomicBool::new(false));
        tx.progress_handler(0, None::<fn() -> bool>).unwrap();
        assert!(
            result.is_ok(),
            "owned album backfill exceeded its VM budget: {result:?}; steps={}",
            steps.load(Ordering::Relaxed)
        );
        assert!(steps.load(Ordering::Relaxed) <= BUDGET);
        assert_eq!(
            tx.query_row("SELECT COUNT(*) FROM albums", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1025
        );
        assert_eq!(tx.query_row("SELECT COUNT(*) FROM audio_files af LEFT JOIN albums al ON al.id=af.album_id WHERE al.id IS NULL OR al.kind<>'folder' OR al.folder_id<>CASE WHEN af.id=2049 THEN 1 ELSE 2+4*((af.id-1)%1024) END",[],|row|row.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(tx.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE type='index' AND name LIKE 'audiolib_album_backfill_%'",[],|row|row.get::<_,i64>(0)).unwrap(),1);
        assert!(tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='audiolib_album_backfill_keep_unknown')",[],|row|row.get::<_,bool>(0)).unwrap());
        tx.commit().unwrap();
    }

    #[test]
    fn cancelled_owned_album_backfill_rolls_back_its_unique_covering_index() {
        let mut conn = candidate();
        {
            let tx = conn.transaction().unwrap();
            assert!(matches!(
                backfill_albums(&tx, &AtomicBool::new(true)),
                Err(audiolib_core::Error::Cancelled)
            ));
        }
        assert!(!conn.table_exists(None, "albums").unwrap());
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM audio_files WHERE album_id IS NULL",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            2049
        );
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE type='index' AND name LIKE 'audiolib_album_backfill_%'",[],|row|row.get::<_,i64>(0)).unwrap(),1);
        assert!(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='audiolib_album_backfill_keep_unknown')",[],|row|row.get::<_,bool>(0)).unwrap());
    }
}

#[cfg(test)]
mod plain_metadata_validation_tests {
    use super::*;
    use std::sync::{Arc, atomic::Ordering};

    fn data() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE audio_files(id INTEGER PRIMARY KEY,metadata_json); WITH RECURSIVE ids(id) AS (VALUES(1) UNION ALL SELECT id+1 FROM ids WHERE id<10001) INSERT INTO audio_files SELECT id,'{}' FROM ids; INSERT INTO audio_files VALUES(-9223372036854775808,'{}'),(9223372036854775807,'{}');").unwrap();
        conn
    }

    #[test]
    fn publication_checks_late_rows_and_extreme_ids_without_rewriting_them() {
        let conn = data();
        let cancel = AtomicBool::new(false);
        validate_text_metadata(&conn, &cancel).unwrap();
        conn.execute(
            "UPDATE audio_files SET metadata_json=x'00' WHERE id=9223372036854775807",
            [],
        )
        .unwrap();
        assert!(validate_text_metadata(&conn, &cancel).is_err());
        assert_eq!(
            conn.query_row(
                "SELECT metadata_json FROM audio_files WHERE id=9223372036854775807",
                [],
                |row| row.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
            [0]
        );
        conn.execute(
            "UPDATE audio_files SET metadata_json='{}' WHERE id=9223372036854775807",
            [],
        )
        .unwrap();
        validate_text_metadata(&conn, &cancel).unwrap();
    }

    #[test]
    fn publication_type_scan_observes_cancellation_between_ranges() {
        let conn = data();
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
        let result = validate_text_metadata(&conn, &cancel);
        conn.progress_handler(0, None::<fn() -> bool>).unwrap();
        assert!(matches!(result, Err(audiolib_core::Error::Cancelled)));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM audio_files", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            10003
        );
    }
}

#[cfg(test)]
mod uri_tests {
    use super::read_only_uri;
    use rusqlite::{Connection, ErrorCode, OpenFlags};
    use std::path::Path;

    #[cfg(windows)]
    #[test]
    fn unc_paths_use_an_empty_authority_and_escape_reserved_uri_bytes() {
        let expected = "file:////host/share/%E4%B8%AD%E6%96%87%20%23%3F%25%26.db?mode=ro";
        for path in [
            r"\\host\share\中文 #?%&.db",
            r"\\?\UNC\host\share\中文 #?%&.db",
        ] {
            assert_eq!(read_only_uri(Path::new(path)), expected);
        }
        assert_eq!(
            read_only_uri(Path::new(r"\\?\C:\中文 #?%&.db")),
            "file:C:/%E4%B8%AD%E6%96%87%20%23%3F%25%26.db?mode=ro"
        );
    }

    fn assert_readonly_attachment(path: &Path) {
        {
            let source = Connection::open(path).unwrap();
            source
                .execute_batch(
                    "CREATE TABLE sentinel(value TEXT); INSERT INTO sentinel VALUES('unchanged');",
                )
                .unwrap();
        }
        let before = std::fs::read(path).unwrap();
        let reader = Connection::open_with_flags(
            ":memory:",
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        reader
            .execute(
                "ATTACH DATABASE ?1 AS input",
                [read_only_uri(&path.canonicalize().unwrap())],
            )
            .unwrap();
        assert_eq!(
            reader
                .query_row("SELECT value FROM input.sentinel", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "unchanged"
        );
        let error = reader
            .execute("UPDATE input.sentinel SET value='changed'", [])
            .unwrap_err();
        assert_eq!(error.sqlite_error_code(), Some(ErrorCode::ReadOnly));
        drop(reader);
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn escaped_paths_attach_the_original_file_without_granting_writes() {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let name = "中文 # % & +.db";
        #[cfg(not(windows))]
        let name = "中文 # ? % & + \\.db";
        assert_readonly_attachment(&directory.path().join(name));
    }

    #[cfg(unix)]
    #[test]
    fn unix_non_utf8_path_bytes_survive_uri_encoding() {
        use std::{ffi::OsString, fs::File, os::unix::ffi::OsStringExt};
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .join(OsString::from_vec(b"raw-\xff#?.db".to_vec()));
        // APFS and other UTF-8-only volumes reject the name. SQLite then
        // reports SQLITE_CANTOPEN before any URI is parsed. The round-trip
        // applies only where those bytes can be stored.
        if File::create(&path).is_err() {
            eprintln!("filesystem rejects non-UTF-8 filenames; not run");
            return;
        }
        std::fs::remove_file(&path).unwrap();
        assert_readonly_attachment(&path);
    }
}
