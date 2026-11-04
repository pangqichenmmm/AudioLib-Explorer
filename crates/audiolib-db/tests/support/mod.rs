// crates/audiolib-db/tests/support/mod.rs
//! Raw connections to owned test files use the production indexing functions.
use rusqlite::Connection;
use std::path::Path;

pub fn connection(path: impl AsRef<Path>) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    audiolib_db::register_library_functions(&conn).unwrap();
    Ok(conn)
}

/// Build a historical test fixture from retained business fields in an owned current file.
/// This is fixture construction, not an application downgrade path.
#[allow(dead_code)]
pub fn legacy_copy(source: &Path, destination: &Path) {
    assert!(!destination.exists());
    let mut conn = connection(destination).unwrap();
    conn.execute_batch(include_str!("../fixtures/schema-v5.sql"))
        .unwrap();
    conn.execute("ATTACH DATABASE ?1 AS input", [source.to_str().unwrap()])
        .unwrap();
    let tables: Vec<String> = conn.prepare("SELECT name FROM main.sqlite_schema WHERE type='table' AND name NOT LIKE 'audio_search%' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .unwrap().query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    let tx = conn.transaction().unwrap();
    for table in tables {
        let columns: Vec<String> = tx.prepare("SELECT name FROM pragma_table_xinfo(?1,'main') WHERE hidden=0 AND name IN (SELECT name FROM pragma_table_xinfo(?1,'input')) ORDER BY cid")
            .unwrap().query_map([&table], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        if columns.is_empty() {
            continue;
        }
        let quote = |value: &str| format!("\"{}\"", value.replace('"', "\"\""));
        let names = columns
            .iter()
            .map(|c| quote(c))
            .collect::<Vec<_>>()
            .join(",");
        // Historical schemas contain ordinary JSON TEXT, not the schema-8 codec.
        // Decode only that known logical column; all other SQLite values copy as-is.
        let source_names = columns
            .iter()
            .map(|column| {
                if table == "audio_files" && column == "metadata_json" {
                    format!("audiolib_metadata_json({})", quote(column))
                } else {
                    quote(column)
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        let table = quote(&table);
        tx.execute_batch(&format!("DELETE FROM main.{table}; INSERT INTO main.{table}({names}) SELECT {source_names} FROM input.{table};")).unwrap();
    }
    tx.execute_batch("DELETE FROM audiolib_schema_features WHERE feature IN ('search_storage','play_history_retired','album_entities','metadata_storage');").unwrap();
    tx.commit().unwrap();
    assert!(
        !conn
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .exists([])
            .unwrap()
    );
}
