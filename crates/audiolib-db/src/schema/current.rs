// crates/audiolib-db/src/schema/current.rs
//! Current shared-album storage entry points, separate from legacy normalization.
use super::*;
use crate::{migration, search_storage};
use audiolib_core::compatibility::LibraryFormat;
use rusqlite::{params, params_from_iter, types::Value};
use std::collections::BTreeMap;

pub(crate) fn current_format_ready(conn: &Connection) -> Result<bool> {
    format_ready(conn, false)
}

/// Completed p0 snapshots remain readable; all new library writes require p1.
pub(crate) fn readable_format_ready(conn: &Connection) -> Result<bool> {
    format_ready(conn, true)
}

fn format_ready(conn: &Connection, allow_p0: bool) -> Result<bool> {
    if !format_ready_at_version(conn, allow_p0, SCHEMA_VERSION)? {
        return Ok(false);
    }
    if !crate::albums::ready(conn)? {
        return Ok(false);
    }
    // A recognized experimental snapshot remains readable without rewriting it.
    // Current writers require the plain target and its retired policy set to zero.
    if allow_p0 && feature_version(conn, "metadata_storage")? == Some(1) {
        return Ok(true);
    }
    conn.query_row(
        "SELECT compression=0 FROM audiolib_album_id_sequence WHERE singleton=1",
        [],
        |row| row.get(0),
    )
    .map_err(db_error)
}

fn format_ready_at_version(
    conn: &Connection,
    allow_p0: bool,
    expected_version: i64,
) -> Result<bool> {
    ensure_ready(conn)?;
    validate_supported_feature_revisions(conn)?;
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)?;
    if version != expected_version || !feature_table_exists(conn)? {
        return Ok(false);
    }
    let features = conn
        .prepare("SELECT feature,version FROM audiolib_schema_features ORDER BY feature")
        .map_err(db_error)?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .map_err(db_error)?;
    let mut expected = LibraryFormat::target().features;
    if allow_p0
        && features.get("metadata_storage") == Some(&1)
        && features.get("play_history_retired") == Some(&2)
    {
        expected.insert("metadata_storage".into(), 1);
    }
    let p0 = allow_p0 && features.get("play_history_retired") == Some(&1);
    if p0 {
        expected.insert("play_history_retired".into(), 1);
    }
    if features != expected
        || !source_tags_ready(conn)?
        || !source_search_marked(conn)?
        || !search_storage::structure_matches(conn)?
        || search_storage::columns(conn)?.iter().any(|column| {
            column.eq_ignore_ascii_case("play_count")
                || (!p0 && column.eq_ignore_ascii_case("last_played_at"))
        })
    {
        return Ok(false);
    }
    let history: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='play_history' COLLATE NOCASE)",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if history {
        return Ok(false);
    }
    definition::required_match(conn)
}

pub(crate) fn needs_upgrade(conn: &Connection) -> Result<bool> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)?;
    if version > SCHEMA_VERSION {
        return Err(Error::NewerDatabase {
            what: "schema".into(),
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    if version == SCHEMA_VERSION {
        return current_format_ready(conn).map(|ready| !ready);
    }
    if !pending(conn)? {
        validate_supported_feature_revisions(conn)?;
    }
    Ok(true)
}

pub(crate) fn mark_current(conn: &Connection) -> Result<()> {
    search_storage::transaction_required(conn)?;
    let retired: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo('audio_files')
          WHERE name COLLATE NOCASE IN ('play_count','last_played_at'))
          OR EXISTS(SELECT 1 FROM sqlite_schema WHERE name='play_history' COLLATE NOCASE)",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if retired {
        return Err(Error::Validation(
            "Playback retirement must complete in the owned migration before publishing p1".into(),
        ));
    }
    // Repair only explicitly owned derived objects, preserving ID watermark data.
    definition::repair_owned(conn)?;
    conn.execute(
        "DELETE FROM audiolib_schema_features WHERE feature='metadata_storage'",
        [],
    )
    .map_err(db_error)?;
    crate::albums::configure_compression(conn, false)?;
    for (name, version) in LibraryFormat::target().features {
        conn.execute(
            "INSERT INTO audiolib_schema_features(feature,version) VALUES(?1,?2)
            ON CONFLICT(feature) DO UPDATE SET version=excluded.version",
            params![name, version],
        )
        .map_err(db_error)?;
    }
    conn.execute_batch(MARK_SOURCE_SEARCH).map_err(db_error)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(db_error)
}

/// Build only a genuinely empty database, using the same reviewed schema recipe.
pub(crate) fn create_current(conn: &mut Connection) -> Result<()> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sqlite_schema", [], |row| row.get(0))
        .map_err(db_error)?;
    if count != 0 {
        return Err(Error::Validation(
            "New library creation requires an empty SQLite file".into(),
        ));
    }
    let mut template = Connection::open_in_memory().map_err(db_error)?;
    register_tokenizer(&template, crate::load_tokenizer())?;
    // Build the current business template directly. No playback table/column is
    // created, even transiently; old-source normalization is isolated in legacy.rs.
    let seed = template.transaction().map_err(db_error)?;
    seed.execute_batch(BASE_SCHEMA).map_err(db_error)?;
    crate::albums::initialize(&seed)?;
    for (name, definition) in AUDIO_COLUMNS {
        if !search_storage::is_derived_column(name) {
            ensure_column(&seed, "audio_files", name, definition)?;
        }
    }
    for (name, definition) in FOLDER_COLUMNS {
        ensure_column(&seed, "folders", name, definition)?;
    }
    seed.execute_batch(BASE_INDEXES).map_err(db_error)?;
    add_management_schema(&seed)?;
    apply_indexes(&seed)?;
    seed.execute_batch(ALBUM_INDEX).map_err(db_error)?;
    crate::albums::configure_compression(&seed, false)?;
    add_current_features(&seed)?;
    set_feature_version(&seed, "album_entities", 1)?;
    seed.execute_batch(MARK_SOURCE_SEARCH).map_err(db_error)?;
    seed.commit().map_err(db_error)?;
    let blueprint = search_storage::blueprint_with(&template, &migration::schema_changes())?;
    let tx = conn.transaction().map_err(db_error)?;
    for (_, sql) in &blueprint.tables {
        search_storage::execute_schema(&tx, sql)?;
    }
    // Empty-library defaults (identity sequence and feature/meta records) are copied
    // explicitly. No schema-5 file is ever published or opened by a writer.
    for (table, _) in &blueprint.tables {
        let columns = tx
            .prepare("SELECT name FROM pragma_table_xinfo(?1) WHERE hidden=0 ORDER BY cid")
            .map_err(db_error)?
            .query_map([table], |row| row.get::<_, String>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        let quote = search_storage::quote_identifier(table);
        let selected = columns
            .iter()
            .map(|column| search_storage::quote_identifier(column))
            .collect::<Vec<_>>()
            .join(",");
        let mut source = template
            .prepare(&format!("SELECT {selected} FROM {quote}"))
            .map_err(db_error)?;
        let mut rows = source.query([]).map_err(db_error)?;
        let placeholders = std::iter::repeat_n("?", columns.len())
            .collect::<Vec<_>>()
            .join(",");
        while let Some(row) = rows.next().map_err(db_error)? {
            let values = (0..columns.len())
                .map(|index| row.get::<_, Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db_error)?;
            tx.execute(
                &format!("INSERT INTO {quote}({selected}) VALUES({placeholders})"),
                params_from_iter(values),
            )
            .map_err(db_error)?;
        }
    }
    for (_, sql) in &blueprint.objects {
        search_storage::execute_schema(&tx, sql)?;
    }
    search_storage::create_index(&tx)?;
    search_storage::install_triggers(&tx)?;
    crate::albums::install_triggers(&tx)?;
    if !source_tags_ready(&tx)? {
        tx.execute(
            "UPDATE audio_files SET source_tags_text=audiolib_source_tags_key(metadata_json)",
            [],
        )
        .map_err(db_error)?;
    }
    mark_current(&tx)?;
    tx.commit().map_err(db_error)
}

pub(super) fn rebuild_search(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction().map_err(db_error)?;
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS audio_files_ai; DROP TRIGGER IF EXISTS audio_files_au;
        DROP TRIGGER IF EXISTS audio_files_ad; DROP TABLE IF EXISTS audio_search;",
    )
    .map_err(db_error)?;
    search_storage::create_index(&tx)?;
    let mut after = None;
    loop {
        let (next, count) = search_storage::index_batch(&tx, after, 5000)?;
        if count == 0 {
            break;
        }
        after = next;
    }
    search_storage::install_triggers(&tx)?;
    if !source_tags_ready(&tx)? {
        tx.execute(
            "UPDATE audio_files SET source_tags_text=audiolib_source_tags_key(metadata_json)",
            [],
        )
        .map_err(db_error)?;
    }
    mark_current(&tx)?;
    tx.commit().map_err(db_error)
}
