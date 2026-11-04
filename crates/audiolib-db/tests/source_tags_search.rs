// crates/audiolib-db/tests/source_tags_search.rs
//! Extracted (source) tags take part in tagged-only / tag-contains filters and full-text
//! search; the one-time local search-text migration is marker-guarded and idempotent.
mod support;
use audiolib_core::{DataProvider, SearchFilters, SearchRequest};
use audiolib_db::SqliteProvider;
use rusqlite::Connection;
use std::path::Path;

fn tags_json(keywords: &str, category: &str) -> String {
    serde_json::json!({"wav_metadata": {"fields": {"keywords": keywords, "category": category}}})
        .to_string()
}

fn make_library(path: &Path) {
    seed_library(path, false);
}

fn seed_library(path: &Path, historical: bool) {
    if !historical {
        drop(SqliteProvider::open(path).unwrap());
    }
    let conn = support::connection(path).unwrap();
    if historical {
        conn.execute_batch(include_str!("fixtures/schema-v5.sql"))
            .unwrap();
    }
    let rows = [
        (1, "plain.wav", "hand", "{}".to_string()),
        (
            2,
            "extracted.wav",
            "",
            tags_json("Engine; Rattle", "Aircraft"),
        ),
        (3, "nothing.wav", " ", "{}".to_string()),
        (
            4,
            "other.wav",
            "",
            r#"{"wav_metadata":{"fields":{"description":"d"}}}"#.to_string(),
        ),
    ];
    for (id, name, tags, json) in rows {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename,user_tags,metadata_json,missing,source_tags_text)
             VALUES(?1,?2,?3,?4,?5,0,?6)",
            // A scan keeps the derived column in step with metadata_json; mirror that here.
            rusqlite::params![
                id,
                format!("{id}.wav"),
                name,
                tags,
                json,
                if id == 2 { "engine, rattle, aircraft" } else { "" }
            ],
        )
        .unwrap();
    }
}

fn ids(provider: &SqliteProvider, query: &str, filters: SearchFilters) -> Vec<i64> {
    let page = provider
        .search(&SearchRequest {
            query: query.into(),
            filters,
            ..Default::default()
        })
        .unwrap();
    let mut ids: Vec<i64> = page.results.iter().map(|file| file.id).collect();
    ids.sort_unstable();
    ids
}

fn marker(path: &Path) -> Option<String> {
    let conn = support::connection(path).unwrap();
    conn.query_row(
        "SELECT value FROM library_meta WHERE key='source_search_rev'",
        [],
        |row| row.get(0),
    )
    .ok()
}

#[test]
fn filters_include_extracted_tags_without_touching_user_tags() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    make_library(&path);
    let provider = SqliteProvider::open(&path).unwrap();
    let tagged = SearchFilters {
        tagged_only: true,
        ..Default::default()
    };
    assert_eq!(ids(&provider, "", tagged), [1, 2]);
    let contains = |text: &str| SearchFilters {
        tag_contains: Some(text.into()),
        ..Default::default()
    };
    assert_eq!(ids(&provider, "", contains("AIRCRAFT")), [2]);
    assert_eq!(ids(&provider, "", contains("rattle")), [2]);
    assert_eq!(ids(&provider, "", contains("hand")), [1]);
    assert!(ids(&provider, "", contains("%")).is_empty());
    assert!(ids(&provider, "", contains("d")).contains(&1));
    // Description is not a tag field; the library tag column keeps its meaning.
    assert!(!ids(&provider, "", contains("description")).contains(&4));
    let file = provider
        .search(&SearchRequest::default())
        .unwrap()
        .results
        .into_iter()
        .find(|file| file.id == 2)
        .unwrap();
    assert_eq!(file.tags, "");
}

#[test]
fn search_text_migration_is_marked_and_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    make_library(&path);
    let provider = SqliteProvider::open(&path).unwrap();
    // A fresh library is built with the current revision.
    assert_eq!(marker(&path).as_deref(), Some("2"));
    assert!(!provider.ensure_source_search_current().unwrap());
    // Simulate a library created before the extracted fields were indexed.
    {
        let conn = support::connection(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE library_meta; INSERT INTO audio_search(audio_search) VALUES('delete-all');",
        )
        .unwrap();
    }
    assert!(ids(&provider, "Rattle", Default::default()).is_empty());
    assert!(provider.ensure_source_search_current().unwrap());
    assert_eq!(marker(&path).as_deref(), Some("2"));
    assert_eq!(ids(&provider, "Rattle", Default::default()), [2]);
    assert_eq!(ids(&provider, "aircraft", Default::default()), [2]);
    assert!(!provider.ensure_source_search_current().unwrap());
    // Reopen: still marked, nothing rebuilt, results unchanged.
    drop(provider);
    let provider = SqliteProvider::open_existing(&path).unwrap();
    assert!(!provider.ensure_source_search_current().unwrap());
    assert_eq!(ids(&provider, "Rattle", Default::default()), [2]);
    let rows: i64 = support::connection(&path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM audio_files", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 4);
}

/// Ready-made provider-independent plan check for the exact SQL `search_sql` emits.
fn plan(conn: &Connection, filter: &str) -> String {
    let sql = format!(
        "EXPLAIN QUERY PLAN SELECT af.id FROM audio_files af WHERE af.missing=0 AND {filter} ORDER BY af.filename COLLATE NOCASE ASC,af.id ASC LIMIT 50"
    );
    let mut statement = conn.prepare(&sql).unwrap();
    statement
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .map(|row| row.unwrap())
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn derived_tag_filters_use_the_partial_index_without_sorter_or_scan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    make_library(&path);
    let conn = support::connection(&path).unwrap();
    conn.create_scalar_function(
        "audiolib_casefold",
        1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |_| Ok(String::new()),
    )
    .unwrap();
    let any = "(COALESCE(trim(af.user_tags),'')<>'' OR af.source_tags_text<>'')";
    let contains = "(instr(audiolib_casefold(COALESCE(af.user_tags,'')),'x')>0 OR instr(af.source_tags_text,'x')>0)";
    for filter in [any.to_string(), format!("{any} AND {contains}")] {
        let plan = plan(&conn, &filter);
        assert!(plan.contains("idx_audio_visible_tagged_any"), "{plan}");
        assert!(!plan.contains("USE TEMP B-TREE"), "{plan}");
        assert!(
            !plan.contains("SCAN af |") && !plan.ends_with("SCAN af"),
            "{plan}"
        );
    }
}

fn source_tags_column(path: &Path) -> Vec<String> {
    let conn = support::connection(path).unwrap();
    let mut statement = conn
        .prepare("SELECT source_tags_text FROM audio_files ORDER BY id")
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
}

#[test]
fn derived_column_is_maintained_by_scans_and_matches_search_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    make_library(&path);
    {
        // Library from before the column was populated.
        let conn = support::connection(&path).unwrap();
        conn.execute_batch(
            "UPDATE audio_files SET source_tags_text='';
             DELETE FROM library_meta WHERE key='source_tags_rev';",
        )
        .unwrap();
    }
    let provider = SqliteProvider::open(&path).unwrap();
    // `open` backfilled it.
    assert_eq!(
        source_tags_column(&path),
        ["", "engine, rattle, aircraft", "", ""]
    );
    drop(provider);
    // Reopen is a no-op for an already marked library.
    let provider = SqliteProvider::open_existing(&path).unwrap();
    assert_eq!(provider.ensure_source_tags_current().unwrap(), 0);
}

#[test]
fn backfill_resumes_from_progress_and_filters_fall_back_until_done() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    make_library(&path);
    let provider = SqliteProvider::open(&path).unwrap();
    // Unfinished backfill: no marker, column still empty.
    {
        let conn = support::connection(&path).unwrap();
        conn.execute_batch(
            "UPDATE audio_files SET source_tags_text='';
             DELETE FROM library_meta WHERE key='source_tags_rev';
             INSERT OR REPLACE INTO library_meta(key,value) VALUES('source_tags_progress','2');
             INSERT OR REPLACE INTO library_meta(key,value) VALUES('source_tags_progress_rev','2');",
        )
        .unwrap();
    }
    // Resumed after id 2: row 2 is skipped (already "done"), nothing after it has tags.
    assert_eq!(provider.ensure_source_tags_current().unwrap(), 0);
    assert_eq!(source_tags_column(&path), ["", "", "", ""]);
    // Rows 1..=2 were claimed done, so reset progress to simulate a fresh interrupted start.
    {
        let conn = support::connection(&path).unwrap();
        conn.execute_batch(
            "DELETE FROM library_meta WHERE key IN ('source_tags_rev','source_tags_progress');",
        )
        .unwrap();
    }
    assert_eq!(provider.ensure_source_tags_current().unwrap(), 1);
    assert_eq!(
        source_tags_column(&path),
        ["", "engine, rattle, aircraft", "", ""]
    );
    let contains = SearchFilters {
        tag_contains: Some("RATTLE".into()),
        ..Default::default()
    };
    assert_eq!(ids(&provider, "", contains), [2]);
}

#[test]
fn escaped_source_keys_repair_completed_and_partial_old_backfills() {
    for completed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        make_library(&path);
        let escaped = r#"{"wav_metadata":{"fields":{"\u006beywords":"OnlySource"}},"private":{"retain":true}}"#;
        let conn = support::connection(&path).unwrap();
        conn.execute(
            "UPDATE audio_files SET metadata_json=?1,source_tags_text='' WHERE id=2",
            [escaped],
        )
        .unwrap();
        conn.execute_batch(
            "DELETE FROM library_meta WHERE key LIKE 'source_tags%';
             INSERT INTO library_meta(key,value) VALUES('source_tags_progress','4');",
        )
        .unwrap();
        if completed {
            conn.execute_batch(
                "INSERT INTO library_meta(key,value) VALUES('source_tags_rev','1');",
            )
            .unwrap();
        }
        drop(conn);
        // The prior revision's cursor must not skip id=2 under the new algorithm.
        let provider = SqliteProvider::open_existing(&path).unwrap();
        assert_eq!(
            ids(
                &provider,
                "",
                SearchFilters {
                    tagged_only: true,
                    ..Default::default()
                }
            ),
            [1, 2],
        );
        assert_eq!(
            ids(
                &provider,
                "",
                SearchFilters {
                    tag_contains: Some("OnlySource".into()),
                    ..Default::default()
                }
            ),
            [2],
        );
        let file = provider.get_audio(2).unwrap();
        assert_eq!(file.tags, "");
        assert_eq!(file.attributes["private"]["retain"], true);
        let conn = support::connection(&path).unwrap();
        let stored: String = conn
            .query_row(
                "SELECT audiolib_metadata_json(metadata_json) FROM audio_files WHERE id=2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, escaped);
        let revision: String = conn
            .query_row(
                "SELECT value FROM library_meta WHERE key='source_tags_rev'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(revision, "2");
        let remaining_progress: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM library_meta WHERE key LIKE 'source_tags_progress%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining_progress, 0);
        assert_eq!(provider.ensure_source_tags_current().unwrap(), 0);
    }
}

#[test]
fn escaped_source_key_repairs_owned_snapshot_without_changing_original() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    seed_library(&path, true);
    let conn = support::connection(&path).unwrap();
    conn.execute(
        "UPDATE audio_files SET metadata_json=?1,source_tags_text='',
         bwf_description_tokenized='OnlySource' WHERE id=2",
        [r#"{"wav_metadata":{"fields":{"\u006beywords":"OnlySource"}}}"#],
    )
    .unwrap();
    // This fixture predates positional fragment text: its ordinary derived
    // word and revision marker must agree so the owned copy rebuilds it.
    conn.execute_batch(
        "UPDATE library_meta SET value='1' WHERE key='source_tags_rev';
         UPDATE audiolib_schema_features SET version=1 WHERE feature='tokenization';",
    )
    .unwrap();
    drop(conn);
    let original = std::fs::read(&path).unwrap();
    let provider = SqliteProvider::open_read_only(&path).unwrap();
    assert_ne!(provider.database_path(), path.as_path());
    assert_eq!(ids(&provider, "OnlySource", Default::default()), [2]);
    assert_eq!(ids(&provider, "lySou", Default::default()), [2]);
    let file = provider.get_audio(2).unwrap();
    assert_eq!(file.tags, "");
    assert_eq!(
        file.attributes["wav_metadata"]["fields"]["keywords"],
        "OnlySource"
    );
    assert_eq!(
        ids(
            &provider,
            "",
            SearchFilters {
                tag_contains: Some("OnlySource".into()),
                ..Default::default()
            }
        ),
        [2]
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[test]
fn extracted_compound_codes_keep_numeric_and_single_letter_components_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    make_library(&path);
    let keywords = "FX_123 A.wav 1_AB_2 4_5";
    let metadata = tags_json(keywords, "");
    let conn = support::connection(&path).unwrap();
    conn.execute(
        "UPDATE audio_files SET metadata_json=?1 WHERE id=2",
        [&metadata],
    )
    .unwrap();
    drop(conn);
    let provider = SqliteProvider::open_existing(&path).unwrap();
    provider
        .maintain_library(audiolib_core::MaintenanceAction::RebuildSearch)
        .unwrap();
    for query in ["123", "fx 123", "a wav", "A.wav", "4.5", "x_12"] {
        assert_eq!(ids(&provider, query, Default::default()), [2], "{query}");
    }
    for query in ["1.2", "fx123"] {
        assert!(
            ids(&provider, query, Default::default()).is_empty(),
            "{query}"
        );
    }
    let file = provider.get_audio(2).unwrap();
    assert_eq!(file.tags, "");
    assert_eq!(
        file.attributes["wav_metadata"]["fields"]["keywords"],
        keywords
    );
    let stored: String = support::connection(&path)
        .unwrap()
        .query_row(
            "SELECT audiolib_metadata_json(metadata_json) FROM audio_files WHERE id=2",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, metadata);
    let readonly = SqliteProvider::open_read_only(&path).unwrap();
    assert_eq!(ids(&readonly, "123", Default::default()), [2]);
    assert_eq!(ids(&readonly, "a wav", Default::default()), [2]);
    assert!(ids(&readonly, "1.2", Default::default()).is_empty());
}

#[test]
fn source_tag_backfill_keeps_the_full_signed_legacy_id_range() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    drop(SqliteProvider::open(&path).unwrap());
    let conn = support::connection(&path).unwrap();
    for id in [i64::MIN, -5, 0] {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename,metadata_json,missing)
             VALUES(?1,?2,'legacy.wav',?3,0)",
            rusqlite::params![id, format!("{id}.wav"), tags_json("OnlySource", "")],
        )
        .unwrap();
    }
    conn.execute_batch("UPDATE library_meta SET value='1' WHERE key='source_tags_rev'")
        .unwrap();
    drop(conn);
    let provider = SqliteProvider::open_existing(&path).unwrap();
    assert_eq!(
        ids(
            &provider,
            "",
            SearchFilters {
                tag_contains: Some("OnlySource".into()),
                ..Default::default()
            }
        ),
        [i64::MIN, -5, 0],
    );
    assert_eq!(provider.ensure_source_tags_current().unwrap(), 0);
}

#[test]
fn old_snapshots_without_the_column_repair_owned_tags_without_changing_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    seed_library(&path, true);
    {
        let conn = support::connection(&path).unwrap();
        conn.execute_batch(
            "DROP INDEX idx_audio_visible_tagged_any;
             DROP INDEX idx_audio_visible_tagged_folders;
             ALTER TABLE audio_files DROP COLUMN source_tags_text;
             DELETE FROM library_meta WHERE key LIKE 'source_tags%';",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 4).unwrap();
    }
    let original = std::fs::read(&path).unwrap();
    let provider = SqliteProvider::open_read_only(&path).unwrap();
    assert_ne!(provider.database_path(), path.as_path());
    assert_eq!(
        provider.library_info().unwrap().schema_version,
        audiolib_db::SCHEMA_VERSION
    );
    let tagged = SearchFilters {
        tagged_only: true,
        ..Default::default()
    };
    assert_eq!(ids(&provider, "", tagged), [1, 2]);
    let contains = SearchFilters {
        tag_contains: Some("Aircraft".into()),
        ..Default::default()
    };
    assert_eq!(ids(&provider, "", contains), [2]);
    drop(provider);
    assert_eq!(std::fs::read(&path).unwrap(), original);
}
