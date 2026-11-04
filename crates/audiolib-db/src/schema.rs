// crates/audiolib-db/src/schema.rs
use crate::{SnapshotUpgradeProgress, db_error, path_key, tokenize};
use audiolib_core::{Error, Result};
use jieba_rs::Jieba;
use rusqlite::{Connection, OptionalExtension, functions::FunctionFlags};
use std::sync::Arc;
mod current;
pub(crate) mod definition;
mod legacy;
mod search_upgrade;
pub(crate) use current::{
    create_current, current_format_ready, mark_current, needs_upgrade, readable_format_ready,
};
pub(crate) use search_upgrade::{migrate_resumable, pending};

/// Writable target format; legacy normalization remains pinned to v5 below.
pub const SCHEMA_VERSION: i64 = 8;
pub const LEGACY_SCHEMA_VERSION: i64 = 5;
/// Read-only source snapshots remain compatible across schemas 3..=5. Obsolete
/// derived search state is repaired only in an owned staging or temporary copy.
pub const MIN_SNAPSHOT_SCHEMA_VERSION: i64 = 3;
// Revision 3 also indexes multi-letter folds of a single source grapheme, while
// preserving its exact single-grapheme token for multi-term queries.
const SEARCH_TEXT_VERSION: i64 = 3;

fn feature_table_exists(conn: &Connection) -> Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='audiolib_schema_features')",
        [],|row|row.get(0)).map_err(db_error)
}

pub(crate) fn ensure_ready(conn: &Connection) -> Result<()> {
    search_upgrade::ensure_ready(conn)?;
    let pending: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='audiolib_storage_migration')",
        [], |row| row.get(0),
    ).map_err(db_error)?;
    if pending {
        return Err(Error::Busy(
            "Storage migration is incomplete; resume or roll back its job".into(),
        ));
    }
    Ok(())
}

pub(crate) fn feature_version(conn: &Connection, feature: &str) -> Result<Option<i64>> {
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='audiolib_schema_features')",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    if !exists {
        return Ok(None);
    }
    conn.query_row(
        "SELECT version FROM audiolib_schema_features WHERE feature=?1",
        [feature],
        |row| row.get(0),
    )
    .optional()
    .map_err(db_error)
}

/// Refuse to reinterpret a known newer implementation within a supported schema.
/// Both writable migrations and read-only revision probes use this guard.
fn validate_supported_feature_revisions(conn: &Connection) -> Result<()> {
    validate_feature_revisions(conn, SEARCH_TEXT_VERSION)
}

fn validate_feature_revisions(conn: &Connection, tokenization: i64) -> Result<()> {
    if feature_table_exists(conn)? {
        let names = conn
            .prepare("SELECT feature FROM audiolib_schema_features")
            .map_err(db_error)?
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        for name in names {
            if ![
                "album_entities",
                "metadata_storage",
                "stable_audio_ids",
                "foreign_key_indexes",
                "tokenization",
                "search_indexes",
                "search_storage",
                "play_history_retired",
            ]
            .contains(&name.as_str())
            {
                return Err(Error::Database(format!(
                    "Unknown required database feature {name}"
                )));
            }
        }
    }
    for (feature, supported) in [
        ("album_entities", 1),
        ("metadata_storage", 1),
        ("stable_audio_ids", 1),
        ("foreign_key_indexes", 1),
        ("tokenization", tokenization),
        ("search_indexes", 1),
        ("search_storage", 1),
        ("play_history_retired", 2),
    ] {
        if let Some(version) = feature_version(conn, feature)?
            && version > supported
        {
            return Err(Error::NewerDatabase {
                what: format!("feature {feature} revision"),
                found: version,
                supported,
            });
        }
    }
    for (key, supported) in [
        (SOURCE_SEARCH_REV_KEY, 2),
        ("source_tags_rev", 2),
        ("source_tags_progress_rev", 2),
    ] {
        if let Some(version) = meta_value(conn, key)?.and_then(|value| value.parse::<i64>().ok())
            && version > supported
        {
            return Err(Error::NewerDatabase {
                what: format!("feature {key} revision"),
                found: version,
                supported,
            });
        }
    }
    Ok(())
}

fn set_feature_version(conn: &Connection, feature: &str, version: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO audiolib_schema_features(feature,version) VALUES(?1,?2)
         ON CONFLICT(feature) DO UPDATE SET version=excluded.version",
        rusqlite::params![feature, version],
    )
    .map(drop)
    .map_err(db_error)
}

/// Probe additive implementation revisions and indexes before upgrading a library.
/// Read-only connections can probe this without creating tables or changing the source.
pub(crate) fn legacy_needs_upgrade(conn: &Connection) -> Result<bool> {
    if pending(conn)? {
        return Ok(true);
    }
    validate_supported_feature_revisions(conn)?;
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)?;
    if version != LEGACY_SCHEMA_VERSION {
        return Ok(true);
    }
    if feature_version(conn, "stable_audio_ids")? != Some(1)
        || feature_version(conn, "foreign_key_indexes")? != Some(1)
        || feature_version(conn, "search_indexes")? != Some(1)
        || !search_text_is_current(conn)?
        || !source_tags_ready(conn)?
    {
        return Ok(true);
    }
    let objects: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE
         (type='table' AND name='audiolib_audio_id_sequence') OR
         (type='trigger' AND name='audiolib_audio_id_high_water') OR
         (type='index' AND name IN ('idx_playlist_items_audio_file','idx_project_references_audio_file',
          'idx_waveform_cache_audio_file','idx_play_history_audio_file','idx_closure_descendant',
          'idx_audio_files_not_present','idx_audio_search_facets','idx_audio_visible_tagged_folders'))",
        [],
        |row| row.get(0),
    ).map_err(db_error)?;
    if objects != 10 {
        return Ok(true);
    }
    let valid_sequence: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM audiolib_audio_id_sequence WHERE singleton=1
             AND high_water>=COALESCE((SELECT MAX(id) FROM audio_files),0))",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    Ok(!valid_sequence)
}

/// Whether this snapshot's derived text agrees with the plain-text query compiler.
pub(crate) fn search_text_is_current(conn: &Connection) -> Result<bool> {
    if pending(conn)? {
        return Ok(false);
    }
    validate_supported_feature_revisions(conn)?;
    Ok(
        feature_version(conn, "tokenization")? == Some(SEARCH_TEXT_VERSION)
            && source_search_marked(conn)?,
    )
}

/// Derived text and the required directory/visibility indexes must all be readable.
pub(crate) fn search_state_is_current(conn: &Connection) -> Result<bool> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)?;
    if version == SCHEMA_VERSION {
        return readable_format_ready(conn);
    }
    if !search_text_is_current(conn)?
        || !source_tags_ready(conn)?
        || feature_version(conn, "search_indexes")? != Some(1)
    {
        return Ok(false);
    }
    conn.query_row(
        "SELECT COUNT(*)=4 FROM sqlite_schema WHERE type='index'
         AND name IN ('idx_closure_descendant','idx_audio_files_not_present',
           'idx_audio_search_facets','idx_audio_visible_tagged_folders')",
        [],
        |row| row.get(0),
    )
    .map_err(db_error)
}

/// Highest identity already issued, including records removed from this library.
pub(crate) fn audio_id_high_water(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT MAX(high_water,COALESCE((SELECT MAX(id) FROM audio_files),0))
         FROM audiolib_audio_id_sequence WHERE singleton=1",
        [],
        |row| row.get(0),
    )
    .map_err(db_error)
}

/// Preserve the destination's issued identities when restoring an older owned backup.
pub(crate) fn retain_audio_id_high_water(conn: &Connection, high_water: i64) -> Result<()> {
    conn.execute(
        "UPDATE audiolib_audio_id_sequence SET high_water=MAX(high_water,?1) WHERE singleton=1",
        [high_water],
    )
    .map(drop)
    .map_err(db_error)
}

/// Allocate within the caller's write transaction; rollback also rolls back allocation.
#[cfg(feature = "scanner")]
pub(crate) fn allocate_audio_id(conn: &Connection) -> Result<i64> {
    conn.prepare_cached(
        "UPDATE audiolib_audio_id_sequence SET high_water=high_water+1
         WHERE singleton=1 AND high_water<9223372036854775807 RETURNING high_water",
    )
    .map_err(db_error)?
    .query_row([], |row| row.get(0))
    .optional()
    .map_err(db_error)?
    .ok_or_else(|| Error::Validation("Audio file identity space is exhausted".into()))
}

fn add_current_features(conn: &Connection) -> Result<()> {
    // A legacy AUTOINCREMENT table can remember IDs that no longer have a row.
    // Read its watermark without creating sqlite_sequence in non-AUTOINCREMENT DBs.
    let has_sqlite_sequence: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='sqlite_sequence')",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    let legacy_high_water: i64 = if has_sqlite_sequence {
        conn.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM sqlite_sequence WHERE name='audio_files'",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?
    } else {
        0
    };
    let has_audio_sequence: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='audiolib_audio_id_sequence')",
            [],
            |row| row.get(0),
        )
        .map_err(db_error)?;
    let existing_sequence: Option<i64> = if has_audio_sequence {
        conn.query_row(
            "SELECT high_water FROM audiolib_audio_id_sequence WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_error)?
    } else {
        None
    };
    let (existing_max, version): (i64, i64) = conn
        .query_row(
            "SELECT MAX(0,COALESCE(MAX(id),0)),(SELECT user_version FROM pragma_user_version)
             FROM audio_files",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(db_error)?;
    // The old format cannot reveal a deleted maximum ID. On its first upgrade,
    // start new allocations in a large namespace rather than the old small row IDs.
    // This is a conservative baseline, not recovery of arbitrary deleted i64 IDs.
    // Fresh empty libraries start at 1; existing sequence rows never follow the clock.
    let initial_high_water = if let Some(high_water) = existing_sequence {
        high_water.max(existing_max).max(legacy_high_water)
    } else if version > 0 || existing_max > 0 || legacy_high_water > 0 {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| {
                Error::Validation(format!("Cannot initialize audio identities: {error}"))
            })?
            .as_millis();
        let baseline = millis
            .checked_mul(1024)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or_else(|| Error::Validation("Audio identity baseline exceeds i64".into()))?;
        baseline.max(existing_max).max(legacy_high_water)
    } else {
        0
    };
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS audiolib_schema_features (
         feature TEXT PRIMARY KEY, version INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS audiolib_audio_id_sequence (
         singleton INTEGER PRIMARY KEY CHECK(singleton=1),
         high_water INTEGER NOT NULL CHECK(high_water>=0));
         CREATE TRIGGER IF NOT EXISTS audiolib_audio_id_high_water AFTER INSERT ON audio_files BEGIN
          UPDATE audiolib_audio_id_sequence SET high_water=MAX(high_water,new.id) WHERE singleton=1;
         END;
         CREATE INDEX IF NOT EXISTS idx_playlist_items_audio_file ON playlist_items(audio_file_id);
         CREATE INDEX IF NOT EXISTS idx_project_references_audio_file ON project_references(audio_file_id);
         CREATE INDEX IF NOT EXISTS idx_waveform_cache_audio_file ON waveform_cache(audio_file_id);
         CREATE INDEX IF NOT EXISTS idx_closure_descendant ON folder_closure(descendant_id);
         CREATE INDEX IF NOT EXISTS idx_audio_files_not_present ON audio_files(id) WHERE missing IS NOT 0;
         CREATE INDEX IF NOT EXISTS idx_audio_search_facets ON audio_files(id,missing,folder_id);
         CREATE INDEX IF NOT EXISTS idx_audio_visible_tagged_folders
          ON audio_files(folder_id,id,user_tags,source_tags_text)
          WHERE missing=0 AND (COALESCE(trim(user_tags),'')<>'' OR source_tags_text<>'');",
    )
    .map_err(db_error)?;
    conn.execute(
        "INSERT OR IGNORE INTO audiolib_audio_id_sequence(singleton,high_water) VALUES(1,?1)",
        [initial_high_water],
    )
    .map_err(db_error)?;
    retain_audio_id_high_water(conn, existing_max.max(legacy_high_water))?;
    set_feature_version(conn, "stable_audio_ids", 1)?;
    set_feature_version(conn, "foreign_key_indexes", 1)?;
    set_feature_version(conn, "search_indexes", 1)
}

/// Identify an existing AudioLib source before migrations can create missing tables.
pub(crate) fn validate_library_identity(conn: &Connection) -> Result<()> {
    let mut columns = conn
        .prepare("PRAGMA table_info(audio_files)")
        .map_err(db_error)?;
    let names: Vec<String> = columns
        .query_map([], |row| row.get(1))
        .map_err(db_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_error)?;
    if ["id", "file_path", "filename"]
        .iter()
        .any(|name| !names.iter().any(|actual| actual == name))
    {
        return Err(Error::Validation(
            "Source is not an AudioLib library: audio_files requires id, file_path and filename"
                .into(),
        ));
    }
    Ok(())
}

pub(crate) fn fold_case(value: &str) -> String {
    icu_casemap::CaseMapper::new()
        .fold_string(value)
        .into_owned()
}

/// Marker (in `library_meta`) of the derived-search-text revision a library was built with.
/// Revision 2 = `source_metadata_search_text` also indexes the extracted tag fields.
pub(crate) const SOURCE_SEARCH_REV_KEY: &str = "source_search_rev";
pub(crate) const SOURCE_SEARCH_REV: &str = "2";

const CREATE_LIBRARY_META: &str = "CREATE TABLE IF NOT EXISTS library_meta(
        key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;";

/// Set whenever search text AND the derived tag column were both recomputed for every row
/// (fresh library, legacy import, full search rebuild).
const MARK_SOURCE_SEARCH: &str = "CREATE TABLE IF NOT EXISTS library_meta(
        key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
    INSERT OR REPLACE INTO library_meta(key,value) VALUES('source_search_rev','2');
    INSERT OR REPLACE INTO library_meta(key,value) VALUES('source_tags_rev','2');
    DELETE FROM library_meta WHERE key IN ('source_tags_progress','source_tags_progress_rev');";

/// Revision of the derived `audio_files.source_tags_text` column (casefolded, ', '-joined
/// extracted tags; '' when none). Revision 2 accepts Unicode-escaped JSON keys and
/// backfills the full signed range of retained legacy IDs.
/// An absent or older marker means the column needs a backfill.
pub(crate) const SOURCE_TAGS_REV: &str = "2";
const SOURCE_TAGS_BATCH: i64 = 20_000;

/// Derived column text for one `metadata_json`: the extracted tags joined with ', ' and
/// casefolded (so `tag_contains` needs no per-row folding or JSON parsing).
pub(crate) fn source_tags_key(json: &str) -> String {
    if !json_has_source_tags(json) {
        return String::new();
    }
    let value = serde_json::from_str(json).unwrap_or_default();
    fold_case(&audiolib_core::source_tags(&value).join(", "))
}

pub(crate) fn has_source_tags_column(conn: &Connection) -> Result<bool> {
    let mut statement = conn
        .prepare("PRAGMA table_info(audio_files)")
        .map_err(db_error)?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(db_error)?;
    for name in names {
        if name.map_err(db_error)? == "source_tags_text" {
            return Ok(true);
        }
    }
    Ok(false)
}

fn meta_value(conn: &Connection, key: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    let table: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type='table' AND name='library_meta'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_error)?;
    if table.is_none() {
        return Ok(None);
    }
    conn.query_row(
        "SELECT value FROM library_meta WHERE key=?1",
        [key],
        |row| row.get(0),
    )
    .optional()
    .map_err(db_error)
}

/// True when the derived tag column exists and is fully populated (queries may use it).
pub(crate) fn source_tags_ready(conn: &Connection) -> Result<bool> {
    Ok(has_source_tags_column(conn)?
        && meta_value(conn, "source_tags_rev")?.as_deref() == Some(SOURCE_TAGS_REV))
}

/// Populate `source_tags_text` in id-ordered batches of 20 000 rows, one short transaction
/// each, recording the last finished id in `library_meta` so an interrupted run resumes
/// where it stopped. Idempotent; sets the `source_tags_rev` marker when complete. Returns
/// the number of rows that received a non-empty value in this call.
pub(crate) fn backfill_source_tags(conn: &mut Connection) -> Result<u64> {
    if source_tags_ready(conn)? {
        return Ok(0);
    }
    conn.execute_batch(CREATE_LIBRARY_META).map_err(db_error)?;
    // Progress belongs to its extraction revision. An older partial backfill may have
    // skipped escaped keys before its high-water ID, so it must restart at zero.
    if meta_value(conn, "source_tags_progress_rev")?.as_deref() != Some(SOURCE_TAGS_REV) {
        let tx = conn.transaction().map_err(db_error)?;
        tx.execute(
            "DELETE FROM library_meta WHERE key='source_tags_progress'",
            [],
        )
        .map_err(db_error)?;
        tx.execute(
            "INSERT OR REPLACE INTO library_meta(key,value) VALUES('source_tags_progress_rev',?1)",
            [SOURCE_TAGS_REV],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
    }
    let progress = meta_value(conn, "source_tags_progress")?.and_then(|value| value.parse().ok());
    let mut low: i64 = progress.unwrap_or(i64::MIN);
    let mut inclusive = progress.is_none();
    let mut filled = 0u64;
    loop {
        // Keep a primary-key range scan for each batch, including i64::MIN on the
        // first pass. Imported IDs are retained and need not be positive.
        let lower_bound = if inclusive { "id>=?1" } else { "id>?1" };
        let high: Option<i64> = conn
            .query_row(
                &format!("SELECT MAX(id) FROM (SELECT id FROM audio_files WHERE {lower_bound} ORDER BY id LIMIT ?2)"),
                rusqlite::params![low, SOURCE_TAGS_BATCH],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        let Some(high) = high else { break };
        let tx = conn.transaction().map_err(db_error)?;
        filled += tx
            .execute(
                &format!("UPDATE audio_files SET source_tags_text=audiolib_source_tags_key(metadata_json)
                 WHERE {lower_bound} AND id<=?2 AND audiolib_has_source_tags(metadata_json)"),
                rusqlite::params![low, high],
            )
            .map_err(db_error)? as u64;
        tx.execute(
            "INSERT OR REPLACE INTO library_meta(key,value) VALUES('source_tags_progress',?1)",
            [high.to_string()],
        )
        .map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        low = high;
        inclusive = false;
    }
    let tx = conn.transaction().map_err(db_error)?;
    tx.execute(
        "INSERT OR REPLACE INTO library_meta(key,value) VALUES('source_tags_rev',?1)",
        [SOURCE_TAGS_REV],
    )
    .map_err(db_error)?;
    tx.execute(
        "DELETE FROM library_meta WHERE key IN ('source_tags_progress','source_tags_progress_rev')",
        [],
    )
    .map_err(db_error)?;
    tx.commit().map_err(db_error)?;
    Ok(filled)
}

/// True when `metadata_json` yields at least one extracted tag (cheap text pre-check first,
/// so rows without any of the tag keys are never JSON-parsed).
fn json_has_source_tags(json: &str) -> bool {
    // Quoted forms of audiolib_core::SOURCE_TAG_KEYS (kept in sync by a unit test).
    const QUOTED_KEYS: [&str; 5] = [
        "\"keywords\"",
        "\"category\"",
        "\"subcategory\"",
        "\"vendor_category\"",
        "\"cat_id\"",
    ];
    // JSON permits every character in a property name to be Unicode-escaped. Keep
    // the cheap canonical-key rejection only when there are no Unicode escapes.
    if !QUOTED_KEYS.iter().any(|key| json.contains(key)) && !json.contains("\\u") {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(json)
        .map(|value| !audiolib_core::source_tags(&value).is_empty())
        .unwrap_or(false)
}

pub(crate) fn source_search_marked(conn: &Connection) -> Result<bool> {
    Ok(meta_value(conn, SOURCE_SEARCH_REV_KEY)?.as_deref() == Some(SOURCE_SEARCH_REV))
}

fn register_metadata_storage(conn: &Connection) -> Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    conn.create_scalar_function("audiolib_metadata_json", 1, flags, |context| {
        crate::metadata_storage::decode_value(context.get_raw(0))
            .map_err(|error| rusqlite::Error::UserFunctionError(error.into()))
    })
    .map_err(db_error)?;
    conn.create_scalar_function("audiolib_metadata_store", 2, flags, |context| {
        let raw = crate::metadata_storage::decode_value(context.get_raw(0))
            .map_err(|error| rusqlite::Error::UserFunctionError(error.into()))?;
        crate::metadata_storage::encode(&raw, context.get::<bool>(1)?)
            .map_err(|error| rusqlite::Error::UserFunctionError(error.into()))
    })
    .map_err(db_error)
}

/// Register deterministic folding, tag-query and source-path identity functions
/// without rewriting the stored annotations or original Metadata.
pub(crate) fn register_casefold(conn: &Connection) -> Result<()> {
    // Search-only readers register this subset, so codec functions must be available
    // before either a reader projection or a writer trigger can reference Metadata.
    register_metadata_storage(conn)?;
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    // Directory queries run on the lightweight read pool as well as on writers.
    // This identity is query-only, never persisted in a schema/index or feature map.
    conn.create_scalar_function("audiolib_path_identity", 1, flags, |context| {
        Ok(crate::path_identity::comparison_key(
            &context.get::<String>(0)?,
        ))
    })
    .map_err(db_error)?;
    let tags_mapper = icu_casemap::CaseMapper::new();
    conn.create_scalar_function("audiolib_user_tags_key", 1, flags, move |context| {
        let mut value = context.get::<Option<String>>(0)?.unwrap_or_default();
        // Match display_tags' Rust Unicode trim, then use the exact same folding
        // as source_tags_key. NULL lets blank annotations fall back to source tags.
        let leading = value.len() - value.trim_start().len();
        if leading == value.len() {
            return Ok(None);
        }
        value.truncate(value.trim_end().len());
        if leading != 0 {
            drop(value.drain(..leading));
        }
        // Reuse the owned SQL argument for ASCII keys, including Unicode-trimmed
        // boundaries, so the common English case needs no second allocation.
        if value.is_ascii() {
            value.make_ascii_lowercase();
            return Ok(Some(value));
        }
        let key = match tags_mapper.fold_string(&value) {
            std::borrow::Cow::Borrowed(_) => value,
            std::borrow::Cow::Owned(folded) => folded,
        };
        Ok(Some(key))
    })
    .map_err(db_error)?;
    conn.create_scalar_function("audiolib_has_source_tags", 1, flags, |context| {
        let json = crate::metadata_storage::decode_value(context.get_raw(0))
            .map_err(|error| rusqlite::Error::UserFunctionError(error.into()))?;
        Ok(i64::from(json_has_source_tags(&json)))
    })
    .map_err(db_error)?;
    conn.create_scalar_function("audiolib_source_tags_key", 1, flags, |context| {
        let json = crate::metadata_storage::decode_value(context.get_raw(0))
            .map_err(|error| rusqlite::Error::UserFunctionError(error.into()))?;
        Ok(source_tags_key(&json))
    })
    .map_err(db_error)?;
    conn.create_scalar_function("audiolib_source_tags", 1, flags, |context| {
        let json = crate::metadata_storage::decode_value(context.get_raw(0))
            .map_err(|error| rusqlite::Error::UserFunctionError(error.into()))?;
        if !json_has_source_tags(&json) {
            return Ok(String::new());
        }
        let value = serde_json::from_str(&json).unwrap_or_default();
        Ok(audiolib_core::source_tags(&value).join(", "))
    })
    .map_err(db_error)?;
    // Library/album name exactly as the file declares it; empty when it declares none.
    conn.create_scalar_function("audiolib_album_key", 1, flags, |context| {
        let json = crate::metadata_storage::decode_value(context.get_raw(0))
            .map_err(|error| rusqlite::Error::UserFunctionError(error.into()))?;
        if !json.contains("library") && !json.contains("\\u") {
            return Ok(String::new());
        }
        let value: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
        Ok(audiolib_core::album(&value))
    })
    .map_err(db_error)?;
    // Include/exclude tag terms in one call per row: the library tags are folded once and
    // every term is tested here, rather than one folding call per SQL term. The third
    // argument is a JSON array of [exclude, folded term] pairs, parsed once per statement.
    // A term matches like `tag_contains`: a substring of the folded library tags or of the
    // already folded source tag text.
    let terms_mapper = icu_casemap::CaseMapper::new();
    conn.create_scalar_function("audiolib_tag_terms", 3, flags, move |context| {
        type Terms = Vec<(bool, String)>;
        let terms = context.get_or_create_aux(
            2,
            |value| -> std::result::Result<Terms, Box<dyn std::error::Error + Send + Sync>> {
                Ok(serde_json::from_str(value.as_str()?)?)
            },
        )?;
        let text = |index| {
            context
                .get_raw(index)
                .as_str_or_null()
                .map_err(|error| rusqlite::Error::UserFunctionError(error.into()))
                .map(Option::unwrap_or_default)
        };
        let user = text(0)?;
        let source = text(1)?;
        // ASCII tags fold to ASCII lowercase, so compare them in place. This runs for
        // every tagged file of a filter scan; only non-ASCII tags allocate a folded copy.
        let folded = (!user.is_ascii()).then(|| terms_mapper.fold_string(user));
        Ok(terms.iter().all(|(exclude, term)| {
            let in_user = match &folded {
                Some(folded) => folded.contains(term.as_str()),
                None => contains_ascii_folded(user.as_bytes(), term.as_bytes()),
            };
            (in_user || source.contains(term.as_str())) != *exclude
        }))
    })
    .map_err(db_error)?;
    let mapper = icu_casemap::CaseMapper::new();
    conn.create_scalar_function(
        "audiolib_casefold",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        move |context| {
            let mut value = context.get::<Option<String>>(0)?.unwrap_or_default();
            // ASCII folding is identical to full Unicode folding and avoids ICU
            // traversal for the common English tag case without another allocation.
            if value.is_ascii() {
                value.make_ascii_lowercase();
                return Ok(value);
            }
            match mapper.fold_string(&value) {
                std::borrow::Cow::Borrowed(_) => Ok(value),
                std::borrow::Cow::Owned(folded) => Ok(folded),
            }
        },
    )
    .map_err(db_error)
}

#[cfg(test)]
mod metadata_sql_tests {
    use super::*;

    #[test]
    fn text_and_compressed_metadata_share_sql_projections_and_lossless_storage() {
        let conn = Connection::open_in_memory().unwrap();
        register_tokenizer(&conn, crate::load_tokenizer()).unwrap();
        let raw = serde_json::json!({
            "wav_metadata": {
                "version": 2,
                "fields": {"library": "Library", "keywords": ["Metal", "门"]},
                "ixml": {"SCENE": "scene"},
            },
            "unknown": "exact whitespace 🦉 ".repeat(400),
        })
        .to_string();
        let compressed = crate::metadata_storage::encode_legacy_fixture(&raw).unwrap();
        assert!(matches!(&compressed, rusqlite::types::Value::Blob(_)));
        let read = |value: &dyn rusqlite::ToSql| {
            conn.query_row(
                "SELECT audiolib_metadata_json(?1),audiolib_source_tags_key(?1),
                 audiolib_source_tags(?1),audiolib_has_source_tags(?1),
                 audiolib_album_key(?1),audiolib_source_text('',?1)",
                [value],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .unwrap()
        };
        assert_eq!(read(&raw), read(&compressed));
        let stored: (String, String, String) = conn
            .query_row(
                "SELECT typeof(audiolib_metadata_store(?1,1)),
                 typeof(audiolib_metadata_store(?1,0)),
                 audiolib_metadata_json(audiolib_metadata_store(?1,1))",
                [&raw],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored, ("text".into(), "text".into(), raw.clone()));
        let restored: (String, String) = conn
            .query_row(
                "SELECT typeof(audiolib_metadata_store(?1,1)),audiolib_metadata_store(?1,1)",
                [&compressed],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(restored, ("text".into(), raw));
    }

    #[test]
    fn corrupt_metadata_fails_all_sql_consumers_instead_of_becoming_empty() {
        let conn = Connection::open_in_memory().unwrap();
        register_tokenizer(&conn, crate::load_tokenizer()).unwrap();
        for projection in [
            "audiolib_metadata_json(?1)",
            "audiolib_metadata_store(?1,1)",
            "audiolib_source_tags_key(?1)",
            "audiolib_source_tags(?1)",
            "audiolib_has_source_tags(?1)",
            "audiolib_album_key(?1)",
            "audiolib_source_text('',?1)",
        ] {
            assert!(
                conn.query_row(
                    &format!("SELECT {projection}"),
                    [b"invalid".as_slice()],
                    |row| { row.get::<_, rusqlite::types::Value>(0) }
                )
                .is_err(),
                "{projection}"
            );
        }
        let legacy: String = conn
            .query_row("SELECT audiolib_source_tags_key('not-json')", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(legacy.is_empty());
    }

    #[test]
    fn search_only_readers_register_the_lossless_codec() {
        let conn = Connection::open_in_memory().unwrap();
        register_casefold(&conn).unwrap();
        let raw = "original raw text ".repeat(100);
        let restored: String = conn
            .query_row(
                "SELECT audiolib_metadata_json(audiolib_metadata_store(?1,1))",
                [&raw],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(restored, raw);
    }

    #[test]
    fn search_only_readers_register_source_path_identity_without_writes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA query_only=ON;").unwrap();
        register_casefold(&conn).unwrap();
        for path in [
            r"C:\FX\Designed\one.wav",
            r"\\NAS\SHARE\Designed\one.wav",
            r"/FX/Back\Slash/one.wav",
            "relative.wav",
        ] {
            let actual: String = conn
                .query_row("SELECT audiolib_path_identity(?1)", [path], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(actual, crate::normalize_source_path_identity(path));
        }
        assert_eq!(conn.total_changes(), 0);
        assert_eq!(
            conn.query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

/// Whether ASCII `haystack`, folded to lowercase, contains the folded `needle`. Exactly
/// `haystack.to_ascii_lowercase().contains(needle)`: a folded term has no uppercase
/// ASCII, and its non-ASCII bytes never equal a folded ASCII byte.
fn contains_ascii_folded(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack.windows(needle.len()).any(|window| {
            window
                .iter()
                .zip(needle)
                .all(|(byte, expected)| byte.to_ascii_lowercase() == *expected)
        })
}

pub fn register_tokenizer(conn: &Connection, jieba: Arc<Jieba>) -> Result<()> {
    register_casefold(conn)?;
    conn.create_scalar_function(
        "audiolib_source_text",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| {
            let description = context.get::<Option<String>>(0)?.unwrap_or_default();
            let json = crate::metadata_storage::decode_value(context.get_raw(1))
                .map_err(|error| rusqlite::Error::UserFunctionError(error.into()))?;
            let attributes = serde_json::from_str(&json).unwrap_or_default();
            Ok(audiolib_core::source_metadata_search_text(
                &description,
                &attributes,
            ))
        },
    )
    .map_err(db_error)?;
    conn.create_scalar_function(
        "audiolib_tokenize",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        move |context| {
            let value = context.get::<Option<String>>(0)?.unwrap_or_default();
            Ok(tokenize(&jieba, &value))
        },
    )
    .map_err(db_error)?;
    conn.create_scalar_function(
        "audiolib_path_key",
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |context| Ok(path_key(&context.get::<String>(0)?)),
    )
    .map_err(db_error)
}

fn ensure_column(conn: &Connection, table: &str, name: &str, definition: &str) -> Result<()> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(db_error)?;
    let names = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(db_error)?;
    for item in names {
        if item.map_err(db_error)? == name {
            return Ok(());
        }
    }
    conn.execute_batch(&format!(
        "ALTER TABLE {table} ADD COLUMN {name} {definition};"
    ))
    .map_err(db_error)
}

/// Extend legacy tables in place; never reconstruct or discard their user columns.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    migrate_with_progress(conn, &mut |_| Ok(()))
}

pub(crate) fn migrate_with_progress(
    conn: &mut Connection,
    progress: &mut dyn FnMut(SnapshotUpgradeProgress) -> Result<()>,
) -> Result<()> {
    validate_supported_feature_revisions(conn)?;
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
    if version > LEGACY_SCHEMA_VERSION {
        return Err(Error::Database(format!(
            "Legacy normalization supports schema up to {LEGACY_SCHEMA_VERSION}, found {version}"
        )));
    }
    if version == LEGACY_SCHEMA_VERSION && !legacy_needs_upgrade(conn)? {
        return Ok(());
    }
    let tx = conn.transaction().map_err(db_error)?;
    if (1..=LEGACY_SCHEMA_VERSION).contains(&version) {
        add_management_schema(&tx)?;
        legacy::add_playback_schema(&tx)?;
        apply_indexes(&tx)?;
        add_current_features(&tx)?;
        if !search_text_is_current(&tx)? {
            rebuild_search_in_transaction_with_progress(&tx, progress)?;
        }
        tx.pragma_update(None, "user_version", LEGACY_SCHEMA_VERSION)
            .map_err(db_error)?;
        return tx.commit().map_err(db_error);
    }
    tx.execute_batch(BASE_SCHEMA).map_err(db_error)?;
    // Legacy prototypes have slightly different columns. Preserve every original column.
    for (name, definition) in AUDIO_COLUMNS {
        if *name == "album_id" {
            continue;
        }
        ensure_column(&tx, "audio_files", name, definition)?;
    }
    for (name, definition) in FOLDER_COLUMNS {
        ensure_column(&tx, "folders", name, definition)?;
    }
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS audio_files_ai; DROP TRIGGER IF EXISTS audio_files_au;
        DROP TRIGGER IF EXISTS audio_files_ad; DROP TABLE IF EXISTS audio_search;",
    )
    .map_err(db_error)?;
    retokenize_audio_files(&tx, true, progress)?;
    tx.execute_batch(BASE_INDEXES).map_err(db_error)?;
    progress(SnapshotUpgradeProgress::BuildingFts)?;
    tx.execute_batch(FTS_SCHEMA).map_err(db_error)?;
    add_management_schema(&tx)?;
    legacy::add_playback_schema(&tx)?;
    apply_indexes(&tx)?;
    add_current_features(&tx)?;
    tx.execute(
        "INSERT INTO audio_search(audio_search) VALUES ('rebuild')",
        [],
    )
    .map_err(db_error)?;
    set_feature_version(&tx, "tokenization", SEARCH_TEXT_VERSION)?;
    tx.execute_batch(MARK_SOURCE_SEARCH).map_err(db_error)?;
    tx.pragma_update(None, "user_version", LEGACY_SCHEMA_VERSION)
        .map_err(db_error)?;
    tx.commit().map_err(db_error)
}

fn add_management_schema(conn: &Connection) -> Result<()> {
    ensure_column(
        conn,
        "scan_sessions",
        "errors_truncated",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    Ok(())
}

/// Recreate authoritative triggers and rebuild derived search text transactionally.
pub(crate) fn rebuild_search(conn: &mut Connection) -> Result<()> {
    validate_supported_feature_revisions(conn)?;
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_error)?;
    if version == SCHEMA_VERSION {
        return current::rebuild_search(conn);
    }
    let tx = conn.transaction().map_err(db_error)?;
    rebuild_search_in_transaction(&tx)?;
    tx.commit().map_err(db_error)
}

fn rebuild_search_in_transaction(conn: &Connection) -> Result<()> {
    rebuild_search_in_transaction_with_progress(conn, &mut |_| Ok(()))
}

fn rebuild_search_in_transaction_with_progress(
    conn: &Connection,
    progress: &mut dyn FnMut(SnapshotUpgradeProgress) -> Result<()>,
) -> Result<()> {
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS audio_files_ai; DROP TRIGGER IF EXISTS audio_files_au;
        DROP TRIGGER IF EXISTS audio_files_ad; DROP TABLE IF EXISTS audio_search;",
    )
    .map_err(db_error)?;
    retokenize_audio_files(conn, false, progress)?;
    progress(SnapshotUpgradeProgress::BuildingFts)?;
    conn.execute_batch(FTS_SCHEMA).map_err(db_error)?;
    conn.execute(
        "INSERT INTO audio_search(audio_search) VALUES('rebuild')",
        [],
    )
    .map_err(db_error)?;
    set_feature_version(conn, "tokenization", SEARCH_TEXT_VERSION)?;
    conn.execute_batch(MARK_SOURCE_SEARCH).map_err(db_error)
}

/// Report actual completed rows in bounded statements while retaining the caller's
/// outer transaction. Inclusive first bounds preserve imported i64::MIN identities;
/// exclusive subsequent bounds need no overflow-prone increment at i64::MAX.
fn retokenize_audio_files(
    conn: &Connection,
    update_path_key: bool,
    progress: &mut dyn FnMut(SnapshotUpgradeProgress) -> Result<()>,
) -> Result<()> {
    const BATCH: i64 = 5_000;
    let total = conn
        .query_row("SELECT COUNT(*) FROM audio_files", [], |row| {
            crate::unsigned(row, 0)
        })
        .map_err(db_error)?;
    let mut completed = 0;
    progress(SnapshotUpgradeProgress::Tokenizing { completed, total })?;
    let mut low = i64::MIN;
    let mut inclusive = true;
    loop {
        let lower_bound = if inclusive { "id>=?1" } else { "id>?1" };
        let high: Option<i64> = conn
            .query_row(
                &format!("SELECT MAX(id) FROM (SELECT id FROM audio_files WHERE {lower_bound} ORDER BY id LIMIT ?2)"),
                rusqlite::params![low, BATCH],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        let Some(high) = high else { break };
        let path_key = if update_path_key {
            "path_key=audiolib_path_key(file_path),"
        } else {
            ""
        };
        completed += conn
            .execute(
                &format!("UPDATE audio_files SET {path_key}
                    filename_tokenized=audiolib_tokenize(filename),
                    user_tags_tokenized=audiolib_tokenize(user_tags),
                    user_notes_tokenized=audiolib_tokenize(user_notes),
                    bwf_description_tokenized=audiolib_tokenize(audiolib_source_text(bwf_description,metadata_json)),
                    ixml_scene_tokenized=audiolib_tokenize(ixml_scene),
                    ixml_take_tokenized=audiolib_tokenize(ixml_take),
                    source_tags_text=audiolib_source_tags_key(metadata_json)
                    WHERE {lower_bound} AND id<=?2"),
                rusqlite::params![low, high],
            )
            .map_err(db_error)? as u64;
        progress(SnapshotUpgradeProgress::Tokenizing { completed, total })?;
        low = high;
        inclusive = false;
    }
    Ok(())
}

pub(crate) fn triggers_match(conn: &Connection) -> Result<bool> {
    // Compare normalized sqlite_schema definitions to this version's known-good triggers.
    let expected = Connection::open_in_memory().map_err(db_error)?;
    expected.execute_batch(BASE_SCHEMA).map_err(db_error)?;
    for (name, definition) in AUDIO_COLUMNS {
        if *name == "album_id" {
            continue;
        }
        ensure_column(&expected, "audio_files", name, definition)?;
    }
    expected.execute_batch(FTS_SCHEMA).map_err(db_error)?;
    let normalize = |sql: String| {
        // ALTER TABLE on the resumable staging FTS quotes the published table name.
        sql.replace("\"audio_search\"", "audio_search")
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>()
            .trim_end_matches(';')
            .to_string()
    };
    for (kind, name) in [
        ("table", "audio_search"),
        ("trigger", "audio_files_ai"),
        ("trigger", "audio_files_au"),
        ("trigger", "audio_files_ad"),
    ] {
        use rusqlite::OptionalExtension;
        let actual: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type=?1 AND name=?2",
                [kind, name],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_error)?;
        let correct: String = expected
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type=?1 AND name=?2",
                [kind, name],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        // Old snapshots are validated before their owned-copy upgrade. Accept
        // only the exact previous table definition for an older revision; never
        // relax synchronization-trigger validation or current-revision tables.
        let legacy_table = if name == "audio_search"
            && feature_version(conn, "tokenization")?.unwrap_or(0) < SEARCH_TEXT_VERSION
        {
            Some(normalize(correct.replace(
                "tokenize='unicode61 remove_diacritics 2'",
                "tokenize='porter unicode61 remove_diacritics 2'",
            )))
        } else {
            None
        };
        if actual.is_none_or(|sql| {
            let sql = normalize(sql);
            sql != normalize(correct) && legacy_table.as_ref() != Some(&sql)
        }) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// v5: ratings are gone from the product. Only indexes change (the `user_rating` column and
/// its data stay untouched): drop the three rating indexes and the old favorite index that
/// still carried `user_rating`; PERFORMANCE_INDEXES recreates the favorite index without it.
/// Every statement is IF EXISTS / IF NOT EXISTS, so a repeated run is harmless.
const DROP_RATING_INDEXES: &str = "
 DROP INDEX IF EXISTS idx_audio_visible_tagged;
 DROP INDEX IF EXISTS idx_audio_visible_rating;
 DROP INDEX IF EXISTS idx_audio_visible_rating_desc;
 DROP INDEX IF EXISTS idx_user_rating;
 DROP INDEX IF EXISTS idx_audio_visible_favorite;";

fn apply_indexes(conn: &Connection) -> Result<()> {
    // The tag index below needs the derived column; adding it is idempotent.
    ensure_column(
        conn,
        "audio_files",
        "source_tags_text",
        "TEXT NOT NULL DEFAULT ''",
    )?;
    // v7 album column; the album index in PERFORMANCE_INDEXES depends on it.
    ensure_column(conn, "audio_files", "album", "TEXT NOT NULL DEFAULT ''")?;
    conn.execute_batch(DROP_RATING_INDEXES).map_err(db_error)?;
    conn.execute_batch(PERFORMANCE_INDEXES).map_err(db_error)
}

const PERFORMANCE_INDEXES:&str="
 CREATE INDEX IF NOT EXISTS idx_audio_visible_id ON audio_files(id) WHERE missing=0;
 CREATE INDEX IF NOT EXISTS idx_audio_visible_name ON audio_files(filename COLLATE NOCASE,id) WHERE missing=0;
 CREATE INDEX IF NOT EXISTS idx_audio_visible_folder_name ON audio_files(folder_id,filename COLLATE NOCASE,id) WHERE missing=0;
 CREATE INDEX IF NOT EXISTS idx_audio_visible_filter ON audio_files(sample_rate,duration,id) WHERE missing=0;
 CREATE INDEX IF NOT EXISTS idx_audio_visible_favorite ON audio_files(filename COLLATE NOCASE,id) WHERE missing=0 AND favorite=1;
 CREATE INDEX IF NOT EXISTS idx_audio_files_missing ON audio_files(missing) WHERE missing=1;
 CREATE INDEX IF NOT EXISTS idx_audio_visible_stats ON audio_files(folder_id,duration,file_size) WHERE missing=0;
 CREATE INDEX IF NOT EXISTS idx_playlist_items_audio ON playlist_items(audio_file_id);
 CREATE INDEX IF NOT EXISTS idx_project_references_audio ON project_references(audio_file_id);
 CREATE INDEX IF NOT EXISTS idx_waveform_cache_audio ON waveform_cache(audio_file_id);
 CREATE INDEX IF NOT EXISTS idx_audio_visible_modified ON audio_files(modified_at,id) WHERE missing=0;
 CREATE INDEX IF NOT EXISTS idx_audio_visible_modified_desc ON audio_files(modified_at DESC,id) WHERE missing=0;
 CREATE INDEX IF NOT EXISTS idx_audio_visible_format ON audio_files(bit_depth,channels) WHERE missing=0;
 CREATE INDEX IF NOT EXISTS idx_audio_visible_format_name ON audio_files(bit_depth,filename COLLATE NOCASE,id) WHERE missing=0;
 CREATE INDEX IF NOT EXISTS idx_audio_visible_tagged_any ON audio_files(filename COLLATE NOCASE,id,user_tags,source_tags_text) WHERE missing=0 AND (COALESCE(trim(user_tags),'')<>'' OR source_tags_text<>'');
 CREATE INDEX IF NOT EXISTS idx_audio_visible_color ON audio_files(user_color COLLATE NOCASE,id) WHERE missing=0 AND user_color IS NOT NULL;";

/// Shared-entity indexes for visible facets and all-member preference migration.
pub(crate) const ALBUM_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_audio_visible_album ON audio_files(album_id,id) WHERE missing=0; CREATE INDEX IF NOT EXISTS idx_audio_album_members ON audio_files(album_id,id)";

const AUDIO_COLUMNS: &[(&str, &str)] = &[
    ("file_path", "TEXT"),
    ("filename", "TEXT"),
    ("file_size", "INTEGER"),
    ("sample_rate", "INTEGER"),
    ("bit_depth", "INTEGER"),
    ("channels", "INTEGER"),
    ("duration", "REAL"),
    ("codec", "TEXT"),
    ("bwf_description", "TEXT"),
    ("bwf_originator", "TEXT"),
    ("bwf_originator_reference", "TEXT"),
    ("bwf_origination_date", "TEXT"),
    ("bwf_origination_time", "TEXT"),
    ("bwf_time_reference", "INTEGER"),
    ("bwf_version", "INTEGER"),
    ("bwf_umid", "TEXT"),
    ("ixml_scene", "TEXT"),
    ("ixml_take", "TEXT"),
    ("ixml_tape", "TEXT"),
    ("ixml_circled", "INTEGER"),
    ("ixml_wild_track", "INTEGER"),
    ("ixml_no_good", "INTEGER"),
    ("peak_amplitude", "REAL"),
    ("rms_level", "REAL"),
    ("spectral_centroid", "REAL"),
    ("zero_crossing_rate", "REAL"),
    ("user_tags", "TEXT"),
    ("source_tags_text", "TEXT NOT NULL DEFAULT ''"),
    // v8: shared album identity; the declared name remains in authoritative Metadata.
    ("album_id", "INTEGER REFERENCES albums(id)"),
    ("user_rating", "INTEGER"),
    ("user_color", "TEXT"),
    ("user_notes", "TEXT"),
    ("created_at", "TEXT"),
    ("modified_at", "TEXT"),
    ("filename_tokenized", "TEXT"),
    ("user_tags_tokenized", "TEXT"),
    ("folder_id", "INTEGER"),
    ("user_notes_tokenized", "TEXT"),
    ("bwf_description_tokenized", "TEXT"),
    ("ixml_scene_tokenized", "TEXT"),
    ("ixml_take_tokenized", "TEXT"),
    ("favorite", "INTEGER NOT NULL DEFAULT 0"),
    ("source_mtime_ns", "INTEGER"),
    ("metadata_json", "TEXT NOT NULL DEFAULT '{}'"),
    ("missing", "INTEGER NOT NULL DEFAULT 0"),
    ("seen_scan_id", "INTEGER"),
    ("path_key", "TEXT"),
];
const FOLDER_COLUMNS: &[(&str, &str)] = &[
    ("folder_path", "TEXT"),
    ("folder_name", "TEXT"),
    ("parent_id", "INTEGER"),
    ("depth", "INTEGER NOT NULL DEFAULT 0"),
    ("file_count", "INTEGER NOT NULL DEFAULT 0"),
    ("total_file_count", "INTEGER NOT NULL DEFAULT 0"),
    ("total_duration", "REAL DEFAULT 0"),
    ("total_size", "INTEGER DEFAULT 0"),
    ("created_at", "TEXT"),
    ("updated_at", "TEXT"),
    ("last_scanned", "TEXT"),
];

const BASE_INDEXES: &str = "
 CREATE INDEX IF NOT EXISTS idx_audio_files_folder ON audio_files(folder_id);
 CREATE INDEX IF NOT EXISTS idx_audio_files_path_key ON audio_files(path_key);
 CREATE INDEX IF NOT EXISTS idx_sample_rate ON audio_files(sample_rate);
 CREATE INDEX IF NOT EXISTS idx_duration ON audio_files(duration);
 CREATE INDEX IF NOT EXISTS idx_audio_files_missing ON audio_files(missing) WHERE missing=1;
 CREATE INDEX IF NOT EXISTS idx_folders_parent ON folders(parent_id);
 CREATE INDEX IF NOT EXISTS idx_closure_descendant ON folder_closure(descendant_id);";

const BASE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS audio_files (
 id INTEGER PRIMARY KEY, file_path TEXT NOT NULL UNIQUE, filename TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS folders (
 id INTEGER PRIMARY KEY, folder_path TEXT NOT NULL UNIQUE, folder_name TEXT NOT NULL,
 parent_id INTEGER REFERENCES folders(id) ON DELETE CASCADE,
 depth INTEGER NOT NULL DEFAULT 0, file_count INTEGER NOT NULL DEFAULT 0,
 total_file_count INTEGER NOT NULL DEFAULT 0, total_duration REAL DEFAULT 0,
 total_size INTEGER DEFAULT 0, created_at TEXT, updated_at TEXT, last_scanned TEXT);
CREATE TABLE IF NOT EXISTS folder_closure (
 ancestor_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
 descendant_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
 depth INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(ancestor_id,descendant_id));
CREATE TABLE IF NOT EXISTS playlists (
 id INTEGER PRIMARY KEY, name TEXT NOT NULL, description TEXT, created_at TEXT, modified_at TEXT);
CREATE TABLE IF NOT EXISTS playlist_items (
 id INTEGER PRIMARY KEY, playlist_id INTEGER REFERENCES playlists(id),
 audio_file_id INTEGER REFERENCES audio_files(id), position INTEGER, added_at TEXT, notes TEXT);
CREATE TABLE IF NOT EXISTS project_references (
 id INTEGER PRIMARY KEY, audio_file_id INTEGER REFERENCES audio_files(id),
 project_path TEXT NOT NULL, project_type TEXT, reference_count INTEGER DEFAULT 1, last_seen TEXT);
CREATE TABLE IF NOT EXISTS waveform_cache (
 id INTEGER PRIMARY KEY, audio_file_id INTEGER REFERENCES audio_files(id), zoom_level TEXT,
 samples_per_pixel INTEGER, min_values BLOB, max_values BLOB, rms_values BLOB,
 color_values BLOB, created_at TEXT);
CREATE TABLE IF NOT EXISTS scan_sessions (
 id INTEGER PRIMARY KEY, start_time TEXT, end_time TEXT, scan_path TEXT, scan_type TEXT,
 files_scanned INTEGER DEFAULT 0, files_added INTEGER DEFAULT 0,
 files_updated INTEGER DEFAULT 0, files_removed INTEGER DEFAULT 0,
 files_skipped INTEGER DEFAULT 0, errors INTEGER DEFAULT 0, error_details TEXT,
 status TEXT);";

const FTS_SCHEMA: &str = "
CREATE VIRTUAL TABLE audio_search USING fts5(
 filename,filename_tokenized,user_tags,user_tags_tokenized,
 bwf_description,bwf_description_tokenized,ixml_scene,ixml_scene_tokenized,
 ixml_take,ixml_take_tokenized,user_notes,user_notes_tokenized,
 content='audio_files',content_rowid='id',tokenize='unicode61 remove_diacritics 2');
CREATE TRIGGER audio_files_ai AFTER INSERT ON audio_files BEGIN
 INSERT INTO audio_search(rowid,filename,filename_tokenized,user_tags,user_tags_tokenized,
 bwf_description,bwf_description_tokenized,ixml_scene,ixml_scene_tokenized,
 ixml_take,ixml_take_tokenized,user_notes,user_notes_tokenized)
 VALUES(new.id,new.filename,new.filename_tokenized,new.user_tags,new.user_tags_tokenized,
 new.bwf_description,new.bwf_description_tokenized,new.ixml_scene,new.ixml_scene_tokenized,
 new.ixml_take,new.ixml_take_tokenized,new.user_notes,new.user_notes_tokenized);
END;
CREATE TRIGGER audio_files_au AFTER UPDATE OF filename,filename_tokenized,user_tags,user_tags_tokenized,
 bwf_description,bwf_description_tokenized,ixml_scene,ixml_scene_tokenized,
 ixml_take,ixml_take_tokenized,user_notes,user_notes_tokenized ON audio_files BEGIN
 INSERT INTO audio_search(audio_search,rowid,filename,filename_tokenized,user_tags,user_tags_tokenized,
 bwf_description,bwf_description_tokenized,ixml_scene,ixml_scene_tokenized,
 ixml_take,ixml_take_tokenized,user_notes,user_notes_tokenized)
 VALUES('delete',old.id,old.filename,old.filename_tokenized,old.user_tags,old.user_tags_tokenized,
 old.bwf_description,old.bwf_description_tokenized,old.ixml_scene,old.ixml_scene_tokenized,
 old.ixml_take,old.ixml_take_tokenized,old.user_notes,old.user_notes_tokenized);
 INSERT INTO audio_search(rowid,filename,filename_tokenized,user_tags,user_tags_tokenized,
 bwf_description,bwf_description_tokenized,ixml_scene,ixml_scene_tokenized,
 ixml_take,ixml_take_tokenized,user_notes,user_notes_tokenized)
 VALUES(new.id,new.filename,new.filename_tokenized,new.user_tags,new.user_tags_tokenized,
 new.bwf_description,new.bwf_description_tokenized,new.ixml_scene,new.ixml_scene_tokenized,
 new.ixml_take,new.ixml_take_tokenized,new.user_notes,new.user_notes_tokenized);
END;
CREATE TRIGGER audio_files_ad AFTER DELETE ON audio_files BEGIN
 INSERT INTO audio_search(audio_search,rowid,filename,filename_tokenized,user_tags,user_tags_tokenized,
 bwf_description,bwf_description_tokenized,ixml_scene,ixml_scene_tokenized,
 ixml_take,ixml_take_tokenized,user_notes,user_notes_tokenized)
 VALUES('delete',old.id,old.filename,old.filename_tokenized,old.user_tags,old.user_tags_tokenized,
 old.bwf_description,old.bwf_description_tokenized,old.ixml_scene,old.ixml_scene_tokenized,
 old.ixml_take,old.ixml_take_tokenized,old.user_notes,old.user_notes_tokenized);
END;";

#[cfg(test)]
mod source_tag_tests {
    #[test]
    fn ascii_tag_matching_equals_lowercasing_then_searching() {
        let haystacks = [
            "",
            " ",
            "door",
            "Door",
            "DOOR, Exterior",
            "explosion metal outdoor",
            "a-B_c%d",
        ];
        let needles = [
            "",
            "d",
            "door",
            "or",
            "r, e",
            "exterior",
            "metal outdoor",
            "doors",
            "%d",
            "b_c",
            "straße",
            "ü",
            "DOOR",
            "outdoor ",
        ];
        for haystack in haystacks {
            for needle in needles {
                assert_eq!(
                    super::contains_ascii_folded(haystack.as_bytes(), needle.as_bytes()),
                    haystack.to_ascii_lowercase().contains(needle),
                    "{haystack:?} {needle:?}"
                );
            }
        }
    }

    #[test]
    fn escaped_property_names_match_decoded_source_tags() {
        for key in audiolib_core::SOURCE_TAG_KEYS {
            let escaped = key
                .chars()
                .map(|character| format!("\\u{:04x}", character as u32))
                .collect::<String>();
            let json = format!(r#"{{"wav_metadata":{{"fields":{{"{escaped}":"OnlySource"}}}}}}"#);
            assert!(super::json_has_source_tags(&json), "{key}");
            assert_eq!(super::source_tags_key(&json), "onlysource", "{key}");
        }
        assert!(!super::json_has_source_tags(r#"{"other":"\u006beywords"}"#));
    }

    #[test]
    fn quoted_keys_match_core_keys() {
        let mut json = String::new();
        for key in audiolib_core::SOURCE_TAG_KEYS {
            json = format!("{{\"wav_metadata\":{{\"fields\":{{\"{key}\":\"x\"}}}}}}");
            assert!(super::json_has_source_tags(&json), "{key}");
        }
        assert!(!super::json_has_source_tags(
            &json.replace("cat_id", "other")
        ));
    }
}
