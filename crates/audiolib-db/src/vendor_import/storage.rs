// crates/audiolib-db/src/vendor_import/storage.rs
//! Durable business provenance and incremental vendor projection. No source audio is modified.
use super::{Result, db_error, invalid, json};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const TABLES: &[(&str, &[&str])] = &[
    (
        "vendor_metadata",
        &[
            "audio_file_id",
            "import_id",
            "field",
            "value",
            "source_file",
            "sheet",
            "row",
            "language",
        ],
    ),
    (
        "vendor_import_log",
        &[
            "import_id",
            "audio_file_id",
            "before_rows",
            "before_vendor_json",
            "before_derived",
            "before_source_json",
        ],
    ),
    (
        "audiolib_vendor_baseline",
        &["audio_file_id", "rows_json", "vendor_json"],
    ),
    (
        "audiolib_vendor_jobs",
        &[
            "job_order",
            "import_id",
            "idempotency_key",
            "options_hash",
            "state",
            "checkpoint",
            "written",
            "restored",
            "created",
            "preview_json",
            "options_json",
            "commit_result",
            "last_error",
            "operation",
        ],
    ),
    (
        "audiolib_vendor_events",
        &[
            "import_id",
            "audio_file_id",
            "field",
            "value",
            "source_file",
            "sheet",
            "row",
            "overwrite",
            "language",
        ],
    ),
];

pub(super) fn schema_exists(conn: &Connection) -> Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='audiolib_vendor_jobs')", [], |r| r.get(0)).map_err(db_error)
}

fn validate_existing(conn: &Connection, required: bool) -> Result<()> {
    for (table, columns) in TABLES {
        let object: Option<String> = conn
            .query_row(
                "SELECT type FROM sqlite_schema WHERE name=?1",
                [table],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_error)?;
        match object.as_deref() {
            None if !required => continue,
            Some("table") => {}
            _ => {
                return Err(invalid(format!(
                    "Vendor storage object {table} is missing or not a table"
                )));
            }
        }
        // Names are compile-time constants, never user supplied SQL identifiers.
        let mut statement = conn
            .prepare(&format!("PRAGMA table_info(\"{table}\")"))
            .map_err(db_error)?;
        let actual = statement
            .query_map([], |r| r.get::<_, String>(1))
            .map_err(db_error)?
            .collect::<rusqlite::Result<BTreeSet<_>>>()
            .map_err(db_error)?;
        if actual.len() != columns.len() || columns.iter().any(|name| !actual.contains(*name)) {
            return Err(invalid(format!(
                "Unknown vendor storage layout for {table}"
            )));
        }
        let primary: Vec<(String, i64)> = conn
            .prepare(&format!("PRAGMA table_info(\"{table}\")"))
            .map_err(db_error)?
            .query_map([], |r| Ok((r.get(1)?, r.get(5)?)))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?
            .into_iter()
            .filter(|(_, position)| *position > 0)
            .collect();
        let expected: &[(&str, i64)] = match *table {
            "vendor_metadata" => &[("audio_file_id", 1), ("field", 2), ("value", 3)],
            "vendor_import_log" => &[("import_id", 1), ("audio_file_id", 2)],
            "audiolib_vendor_baseline" => &[("audio_file_id", 1)],
            "audiolib_vendor_jobs" => &[("job_order", 1)],
            "audiolib_vendor_events" => &[
                ("import_id", 1),
                ("audio_file_id", 2),
                ("field", 3),
                ("value", 4),
                ("language", 5),
                ("source_file", 6),
                ("sheet", 7),
                ("row", 8),
            ],
            _ => return Err(invalid("Unregistered vendor table")),
        };
        if primary.len() != expected.len()
            || expected.iter().any(|(name, position)| {
                !primary
                    .iter()
                    .any(|(actual, p)| actual.as_str() == *name && p == position)
            })
        {
            return Err(invalid(format!(
                "Unknown vendor primary-key contract for {table}"
            )));
        }
    }
    let index: Option<(String, String)> = conn
        .query_row(
            "SELECT type,tbl_name FROM sqlite_schema WHERE name='audiolib_vendor_events_audio'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(db_error)?;
    match index {
        Some((kind, table)) if kind == "index" && table == "audiolib_vendor_events" => {
            let columns = conn
                .prepare("PRAGMA index_info(audiolib_vendor_events_audio)")
                .map_err(db_error)?
                .query_map([], |r| r.get::<_, String>(2))
                .map_err(db_error)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db_error)?;
            if columns != ["audio_file_id"] {
                return Err(invalid("Unknown vendor event index layout"));
            }
        }
        None if !required => {}
        _ => {
            return Err(invalid(
                "Vendor event index is missing or occupied by an unknown object",
            ));
        }
    }
    Ok(())
}

/// Create additive owned tables only on a validated writable library. Unknown layouts are refused.
pub(crate) fn ensure_schema(conn: &Connection) -> Result<()> {
    validate_existing(conn, false)?;
    conn.execute_batch(
        "SAVEPOINT audiolib_vendor_schema;
        CREATE TABLE IF NOT EXISTS vendor_metadata(
          audio_file_id INTEGER NOT NULL REFERENCES audio_files(id) ON DELETE CASCADE,
          import_id TEXT NOT NULL, field TEXT NOT NULL, value TEXT NOT NULL,
          source_file TEXT NOT NULL, sheet TEXT NOT NULL, row INTEGER NOT NULL,language TEXT NOT NULL,
          PRIMARY KEY(audio_file_id,field,value));
        CREATE TABLE IF NOT EXISTS vendor_import_log(
          import_id TEXT NOT NULL,audio_file_id INTEGER NOT NULL,
          before_rows TEXT NOT NULL,before_vendor_json TEXT NOT NULL,before_derived TEXT NOT NULL,before_source_json TEXT NOT NULL,
          PRIMARY KEY(import_id,audio_file_id));
        CREATE TABLE IF NOT EXISTS audiolib_vendor_baseline(
          audio_file_id INTEGER PRIMARY KEY, rows_json TEXT NOT NULL,vendor_json TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS audiolib_vendor_jobs(
          job_order INTEGER PRIMARY KEY AUTOINCREMENT,import_id TEXT NOT NULL UNIQUE,
          idempotency_key TEXT NOT NULL UNIQUE,options_hash TEXT NOT NULL,state TEXT NOT NULL,
          checkpoint INTEGER NOT NULL DEFAULT 0,written INTEGER NOT NULL DEFAULT 0,
          restored INTEGER NOT NULL DEFAULT 0,created INTEGER NOT NULL,
          preview_json TEXT NOT NULL,options_json TEXT NOT NULL,commit_result TEXT,last_error TEXT,operation TEXT);
        CREATE TABLE IF NOT EXISTS audiolib_vendor_events(
          import_id TEXT NOT NULL REFERENCES audiolib_vendor_jobs(import_id),
          audio_file_id INTEGER NOT NULL REFERENCES audio_files(id) ON DELETE CASCADE,
          field TEXT NOT NULL,value TEXT NOT NULL,source_file TEXT NOT NULL,sheet TEXT NOT NULL,
          row INTEGER NOT NULL,overwrite INTEGER NOT NULL,language TEXT NOT NULL,
          PRIMARY KEY(import_id,audio_file_id,field,value,language,source_file,sheet,row));
        CREATE INDEX IF NOT EXISTS audiolib_vendor_events_audio ON audiolib_vendor_events(audio_file_id);
        RELEASE audiolib_vendor_schema;",
    )
    .map_err(|error| {
        let _ = conn
            .execute_batch("ROLLBACK TO audiolib_vendor_schema; RELEASE audiolib_vendor_schema;");
        db_error(error)
    })?;
    validate_schema(conn)
}

/// Read-only validation for a format that declares vendor storage.
pub(crate) fn validate_schema(conn: &Connection) -> Result<()> {
    validate_existing(conn, true)
}

/// Remove this module's additive structures only inside a caller-owned snapshot transaction.
/// The caller must prove exclusive ownership of the copy. Retain the unknown Metadata
/// mirror and already indexed words: old read-only clients can search all WAV sources.
pub(crate) fn drop_legacy_snapshot_storage(conn: &Connection) -> Result<()> {
    if conn.is_autocommit() {
        return Err(invalid(
            "Legacy vendor stripping requires an owned snapshot transaction",
        ));
    }
    validate_existing(conn, false)?;
    conn.execute_batch(
        "DROP TABLE IF EXISTS audiolib_vendor_events;
      DROP TABLE IF EXISTS vendor_import_log;
      DROP TABLE IF EXISTS audiolib_vendor_baseline;
      DROP TABLE IF EXISTS audiolib_vendor_jobs;
      DROP TABLE IF EXISTS vendor_metadata;",
    )
    .map_err(db_error)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    import_id: String,
    field: String,
    value: String,
    source_file: String,
    sheet: String,
    row: u64,
    #[serde(default)]
    language: String,
}

fn rows(conn: &Connection, id: i64) -> Result<Vec<Entry>> {
    let mut query = conn.prepare_cached("SELECT import_id,field,value,source_file,sheet,row,language FROM vendor_metadata WHERE audio_file_id=?1 ORDER BY field,value,import_id").map_err(db_error)?;
    query
        .query_map([id], |row| {
            Ok(Entry {
                import_id: row.get(0)?,
                field: row.get(1)?,
                value: row.get(2)?,
                source_file: row.get(3)?,
                sheet: row.get(4)?,
                row: crate::unsigned(row, 5)?,
                language: row.get(6)?,
            })
        })
        .map_err(db_error)?
        .collect::<rusqlite::Result<_>>()
        .map_err(db_error)
}

#[derive(Serialize, Deserialize)]
struct SourceBeforeImage {
    bwf: String,
    metadata: Value,
    rows_sha256: String,
    vendor_sha256: String,
}

/// Verify every saved source receipt before using its provenance. Never restore its
/// embedded values: later rescans must survive rollback of only the vendor source.
pub(super) fn verify_before(conn: &Connection, import: &str, id: i64) -> Result<()> {
    let saved:Option<(String,String,String,String)>=conn.prepare_cached("SELECT before_rows,before_vendor_json,before_derived,before_source_json FROM vendor_import_log WHERE import_id=?1 AND audio_file_id=?2").map_err(db_error)?.query_row(params![import,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(db_error)?;
    let (rows, vendor, derived, source) = saved
        .ok_or_else(|| invalid("Vendor before-image is missing; automatic rollback refused"))?;
    let entries: Vec<Entry> = serde_json::from_str(&rows).map_err(invalid)?;
    let vendor: Value = serde_json::from_str(&vendor).map_err(invalid)?;
    let source: SourceBeforeImage = serde_json::from_str(&source).map_err(invalid)?;
    if format!("{:x}", Sha256::digest(rows.as_bytes())) != source.rows_sha256
        || format!("{:x}", Sha256::digest(json(&vendor)?.as_bytes())) != source.vendor_sha256
    {
        return Err(invalid(
            "Vendor before-image receipt checksum is inconsistent",
        ));
    }
    let mut unique = BTreeSet::new();
    if entries
        .iter()
        .any(|e| e.field.is_empty() || !unique.insert((&e.field, &e.value)))
        || source
            .metadata
            .get("vendor_metadata")
            .unwrap_or(&Value::Null)
            != &vendor
    {
        return Err(invalid(
            "Vendor before-image source/provenance is inconsistent",
        ));
    }
    let recomputed: String = conn
        .prepare_cached("SELECT audiolib_tokenize(audiolib_source_text(?1,?2))")
        .map_err(db_error)?
        .query_row(params![source.bwf, json(&source.metadata)?], |r| r.get(0))
        .map_err(db_error)?;
    if recomputed != derived {
        return Err(invalid("Vendor before-image derived text is inconsistent"));
    }
    Ok(())
}

pub(super) fn save_before(conn: &Connection, import: &str, id: i64) -> Result<()> {
    let captured:bool=conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM vendor_import_log WHERE import_id=?1 AND audio_file_id=?2)").map_err(db_error)?.query_row(params![import,id],|r|r.get(0)).map_err(db_error)?;
    if captured {
        let baseline: bool = conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM audiolib_vendor_baseline WHERE audio_file_id=?1)",
            )
            .map_err(db_error)?
            .query_row([id], |r| r.get(0))
            .map_err(db_error)?;
        if !baseline {
            return Err(invalid(
                "Vendor baseline is missing for a persisted before-image",
            ));
        }
        return verify_before(conn, import, id);
    }
    let (metadata, derived,bwf): (String, String,String) = conn.prepare_cached("SELECT metadata_json,audiolib_tokenize(audiolib_source_text(bwf_description,metadata_json)),coalesce(bwf_description,'') FROM audio_files WHERE id=?1").map_err(db_error)?.query_row([id], |r| Ok((r.get(0)?, r.get(1)?,r.get(2)?))).map_err(db_error)?;
    let metadata: Value = serde_json::from_str(&metadata).map_err(invalid)?;
    let vendor = metadata
        .get("vendor_metadata")
        .cloned()
        .unwrap_or(Value::Null);
    let original = json(&rows(conn, id)?)?;
    let vendor = json(&vendor)?;
    let source = json(&SourceBeforeImage {
        bwf,
        metadata,
        rows_sha256: format!("{:x}", Sha256::digest(original.as_bytes())),
        vendor_sha256: format!("{:x}", Sha256::digest(vendor.as_bytes())),
    })?;
    conn.prepare_cached("INSERT OR IGNORE INTO vendor_import_log(import_id,audio_file_id,before_rows,before_vendor_json,before_derived,before_source_json) VALUES(?1,?2,?3,?4,?5,?6)").map_err(db_error)?.execute(params![import,id,original,vendor,derived,source]).map_err(db_error)?;
    // The baseline is taken from the first persisted before-image, never reconstructed
    // from max IDs or the current post-import projection during recovery.
    conn.prepare_cached("INSERT OR IGNORE INTO audiolib_vendor_baseline(audio_file_id,rows_json,vendor_json) SELECT audio_file_id,before_rows,before_vendor_json FROM vendor_import_log WHERE import_id=?1 AND audio_file_id=?2").map_err(db_error)?.execute(params![import,id]).map_err(db_error)?;
    Ok(())
}

pub(super) struct EventRow<'a> {
    pub import: &'a str,
    pub id: i64,
    pub field: &'a str,
    pub value: &'a str,
    pub source_file: &'a str,
    pub sheet: &'a str,
    pub row: u64,
    pub overwrite: bool,
    pub language: &'a str,
}

pub(super) fn add_event(conn: &Connection, event: EventRow<'_>) -> Result<()> {
    conn.prepare_cached("INSERT OR IGNORE INTO audiolib_vendor_events(import_id,audio_file_id,field,value,source_file,sheet,row,overwrite,language) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)").map_err(db_error)?.execute(params![event.import,event.id,event.field,event.value,event.source_file,event.sheet,super::sql_u64(event.row)?,event.overwrite,event.language]).map_err(db_error)?;
    Ok(())
}

fn language(value: &str, text: &str) -> String {
    let folded = value.trim().to_lowercase().replace('_', "-");
    let language = folded.split('-').next().unwrap_or_default();
    match language {
        "zh" | "chinese" | "中文" | "汉语" => "zh".into(),
        "en" | "english" | "英文" | "英语" => "en".into(),
        "fr" | "french" | "法语" => "fr".into(),
        "de" | "german" | "德语" => "de".into(),
        "ja" | "japanese" | "日语" => "ja".into(),
        "" => {
            if text.chars().any(|c| ('\u{3400}'..='\u{9fff}').contains(&c)) {
                "zh".into()
            } else if text.is_ascii() {
                "en".into()
            } else {
                "und".into()
            }
        }
        other => other.to_owned(),
    }
}
fn language_rank(declared: &str, text: &str, preferred: &[String]) -> usize {
    let actual = language(declared, text);
    preferred
        .iter()
        .position(|p| language(p, "") == actual)
        .unwrap_or(preferred.len())
}

/// Recompute the materialized table and metadata mirror from active immutable events.
/// Distinct source batches can own the same value without duplicating searchable text.
/// Automatic jobs are source-owned refreshes, not immutable manual previews.
pub(super) fn automatic_job(conn: &Connection, id: &str) -> Result<bool> {
    conn.query_row(
        "SELECT CASE WHEN json_valid(preview_json) THEN json_extract(preview_json,'$._automatic.kind')='sidecar-v1' ELSE 0 END FROM audiolib_vendor_jobs WHERE import_id=?1",
        [id],
        |row| row.get::<_, Option<bool>>(0),
    )
    .optional()
    .map(|value| value.flatten().unwrap_or(false))
    .map_err(db_error)
}

fn automatic_key(value: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    crate::schema::fold_case(&value.nfc().collect::<String>())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn field_values(field: &str, value: &str) -> Vec<String> {
    if field == "keywords" {
        value
            .split([',', ';', '|', '\r', '\n', '，', '；', '、'])
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        vec![value.trim().to_owned()]
    }
}

fn known_automatic_values(field: &str, bwf: &str, metadata: &Value) -> BTreeSet<String> {
    let mut values = BTreeSet::new();
    let mut add = |value: &Value| {
        let texts = match value {
            Value::String(text) => vec![text.as_str()],
            Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        for text in texts {
            for text in field_values(field, text) {
                values.insert(automatic_key(&text));
            }
        }
    };
    add(&metadata["wav_metadata"]["fields"][field]);
    for group in ["riff_info", "ixml", "axml", "xmp", "id3", "smed", "vorbis"] {
        if let Some(fields) = metadata["wav_metadata"][group].as_object() {
            for (key, value) in fields {
                if super::parser::canonical(key) == Some(field) {
                    add(value);
                }
            }
        }
    }
    if field == "description" && !bwf.trim().is_empty() {
        values.insert(automatic_key(bwf));
    }
    values
}

pub(super) fn project(conn: &Connection, id: i64) -> Result<()> {
    let (original, bwf): (String, String) = conn
        .query_row(
            "SELECT metadata_json,coalesce(bwf_description,'') FROM audio_files WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(db_error)?;
    let mut metadata: Value = serde_json::from_str(&original).map_err(invalid)?;
    let baseline: (String, String) = conn
        .prepare_cached(
            "SELECT rows_json,vendor_json FROM audiolib_vendor_baseline WHERE audio_file_id=?1",
        )
        .map_err(db_error)?
        .query_row([id], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(db_error)?;
    let originals: Vec<Entry> = serde_json::from_str(&baseline.0).map_err(invalid)?;
    let baseline_vendor = serde_json::from_str::<Value>(&baseline.1).map_err(invalid)?;
    let preferred:Option<String>=conn.prepare_cached("SELECT j.options_json FROM audiolib_vendor_jobs j WHERE EXISTS(SELECT 1 FROM audiolib_vendor_events e WHERE e.import_id=j.import_id AND e.audio_file_id=?1) AND (CASE WHEN json_valid(j.preview_json) THEN json_extract(j.preview_json,'$._automatic.kind') IS NOT 'sidecar-v1' ELSE 1 END) ORDER BY j.job_order DESC LIMIT 1").map_err(db_error)?.query_row([id],|r|r.get(0)).optional().map_err(db_error)?;
    let preferred = preferred
        .as_deref()
        .map(serde_json::from_str::<audiolib_core::vendor_import::VendorImportOptions>)
        .transpose()
        .map_err(invalid)?
        .map(|o| o.preferred_languages)
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| vec!["zh".into(), "en".into()]);
    let mut active = BTreeMap::<(String, String), (Entry, bool)>::new();
    for entry in originals {
        active.insert((entry.field.clone(), entry.value.clone()), (entry, false));
    }
    // Older compatibility copies intentionally retain this unknown mirror while
    // stripping formal tables. Treat its text as a baseline source, never discard it.
    if let Some(fields) = baseline_vendor.get("fields").and_then(Value::as_object) {
        for (field, value) in fields {
            let values = match value {
                Value::String(s) => vec![s.as_str()],
                Value::Array(v) => v.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            for value in values.into_iter().filter(|s| !s.trim().is_empty()) {
                active
                    .entry((field.clone(), value.into()))
                    .or_insert_with(|| {
                        (
                            Entry {
                                import_id: "legacy-mirror".into(),
                                field: field.clone(),
                                value: value.into(),
                                source_file: "legacy-mirror".into(),
                                sheet: String::new(),
                                row: 0,
                                language: String::new(),
                            },
                            false,
                        )
                    });
            }
        }
    }
    let mut query = conn.prepare_cached("SELECT e.import_id,e.field,e.value,e.source_file,e.sheet,e.row,e.overwrite,e.language,CASE WHEN json_valid(j.preview_json) THEN json_extract(j.preview_json,'$._automatic.kind')='sidecar-v1' ELSE 0 END,j.job_order FROM audiolib_vendor_events e JOIN audiolib_vendor_jobs j USING(import_id) WHERE e.audio_file_id=?1 ORDER BY j.job_order,e.field,e.value").map_err(db_error)?;
    let events = query
        .query_map([id], |r| {
            Ok((
                Entry {
                    import_id: r.get(0)?,
                    field: r.get(1)?,
                    value: r.get(2)?,
                    source_file: r.get(3)?,
                    sheet: r.get(4)?,
                    row: crate::unsigned(r, 5)?,
                    language: r.get(7)?,
                },
                r.get::<_, bool>(6)?,
                r.get::<_, Option<bool>>(8)?.unwrap_or(false),
                r.get::<_, i64>(9)?,
            ))
        })
        .map_err(db_error)?;
    let mut overwritten = BTreeSet::new();
    let mut source_order = BTreeMap::new();
    let mut has_events = false;
    for event in events {
        let (entry, overwrite, automatic, order) = event.map_err(db_error)?;
        source_order.insert(entry.import_id.clone(), order);
        has_events = true;
        if overwrite && overwritten.insert((entry.import_id.clone(), entry.field.clone())) {
            active.retain(|(field, _), _| field != &entry.field);
        }
        let key = (entry.field.clone(), entry.value.clone());
        match active.entry(key) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert((entry, automatic));
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                let (current, current_automatic) = slot.get();
                if (*current_automatic && !automatic)
                    || (*current_automatic == automatic
                        && language_rank(&entry.language, &entry.value, &preferred)
                            < language_rank(&current.language, &current.value, &preferred))
                {
                    slot.insert((entry, automatic));
                }
            }
        }
    }
    // Raw events remain immutable provenance. Only the automatic projection is
    // supplementary: current embedded/manual values suppress duplicates. User
    // tags remain independent; removing one must not remove source Metadata.
    let mut known = BTreeMap::<String, BTreeSet<String>>::new();
    for (entry, automatic) in active.values() {
        let values = known
            .entry(entry.field.clone())
            .or_insert_with(|| known_automatic_values(&entry.field, &bwf, &metadata));
        if !automatic {
            for value in field_values(&entry.field, &entry.value) {
                values.insert(automatic_key(&value));
            }
        }
    }
    let mut projected = BTreeMap::new();
    // Prefer the earliest active automatic source's original spelling. Adding a
    // canonically equal later table must not churn an existing projection.
    let mut ordered = active.into_values().collect::<Vec<_>>();
    ordered.sort_by(|(left, left_auto), (right, right_auto)| {
        (
            left_auto,
            source_order.get(&left.import_id),
            &left.field,
            &left.value,
        )
            .cmp(&(
                right_auto,
                source_order.get(&right.import_id),
                &right.field,
                &right.value,
            ))
    });
    for (mut entry, automatic) in ordered {
        if automatic {
            let mut seen = BTreeSet::new();
            let values = field_values(&entry.field, &entry.value)
                .into_iter()
                .filter(|value| {
                    let key = automatic_key(value);
                    !key.is_empty()
                        && !known
                            .get(&entry.field)
                            .is_some_and(|values| values.contains(&key))
                        && seen.insert(key)
                })
                .collect::<Vec<_>>();
            if values.is_empty() {
                continue;
            }
            known.entry(entry.field.clone()).or_default().extend(seen);
            entry.value = if entry.field == "keywords" {
                values.join(", ")
            } else {
                values[0].clone()
            };
        }
        projected
            .entry((entry.field.clone(), entry.value.clone()))
            .or_insert(entry);
    }
    let active = projected;
    let desired: Vec<_> = active
        .values()
        .filter(|entry| has_events || entry.import_id != "legacy-mirror")
        .cloned()
        .collect();
    if rows(conn, id)? != desired {
        conn.prepare_cached("DELETE FROM vendor_metadata WHERE audio_file_id=?1")
            .map_err(db_error)?
            .execute([id])
            .map_err(db_error)?;
        for entry in &desired {
            conn.prepare_cached("INSERT INTO vendor_metadata(audio_file_id,import_id,field,value,source_file,sheet,row,language) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)").map_err(db_error)?.execute(params![id,entry.import_id,entry.field,entry.value,entry.source_file,entry.sheet,super::sql_u64(entry.row)?,entry.language]).map_err(db_error)?;
        }
    }
    let mut fields = BTreeMap::<String, Vec<String>>::new();
    for entry in active.values() {
        fields
            .entry(entry.field.clone())
            .or_default()
            .push(entry.value.clone());
    }
    if let Some(values) = fields.get_mut("keywords") {
        let mut keywords = BTreeMap::new();
        for value in values.iter() {
            for term in value
                .split([',', ';', '|', '，', '；'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                keywords
                    .entry(term.to_lowercase())
                    .or_insert_with(|| term.to_owned());
            }
        }
        *values = keywords.into_values().collect();
    }
    for (field, values) in &mut fields {
        values.sort_by_key(|s| {
            (
                language_rank(
                    active
                        .get(&(field.clone(), s.clone()))
                        .map_or("", |e| e.language.as_str()),
                    s,
                    &preferred,
                ),
                s.clone(),
            )
        });
        values.dedup();
    }
    let mirror = if !has_events || fields.is_empty() {
        baseline_vendor
    } else {
        let mut object = baseline_vendor.as_object().cloned().unwrap_or_default();
        object.insert(
            "fields".into(),
            serde_json::to_value(fields).map_err(invalid)?,
        );
        Value::Object(object)
    };
    let object = metadata
        .as_object_mut()
        .ok_or_else(|| invalid("Audio Metadata is not a JSON object"))?;
    // Raw events, normalized provenance and job accounting above still persist.
    // Equal projected values do not need an audio row write or FTS rebuild.
    if (mirror.is_null() && !object.contains_key("vendor_metadata"))
        || (!mirror.is_null() && object.get("vendor_metadata") == Some(&mirror))
    {
        return Ok(());
    }
    if mirror.is_null() {
        object.remove("vendor_metadata");
    } else {
        object.insert("vendor_metadata".into(), mirror);
    }
    let updated = json(&metadata)?;
    // Existing production triggers update contentless FTS in this same transaction.
    // User annotations and embedded fields are never part of this SET clause.
    conn.prepare_cached("UPDATE audio_files SET metadata_json=?1 WHERE id=?2")
        .map_err(db_error)?
        .execute(params![updated, id])
        .map_err(db_error)?;
    Ok(())
}
