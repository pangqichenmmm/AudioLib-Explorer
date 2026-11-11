// crates/audiolib-db/tests/schema_v5.rs
//! Schema v5 indexes and derived source tags: upgrade behaviour, owned snapshot repair,
//! query plans, and tolerance of legacy rating data. `user_rating` stays.
mod support;
use audiolib_core::{DataProvider, LibraryExportFormat, SearchFilters, SearchRequest};
use audiolib_db::{MIN_SNAPSHOT_SCHEMA_VERSION, SCHEMA_VERSION, SqliteProvider};
use rusqlite::Connection;
use std::path::Path;

/// Indexes v4 added to v3 that v5 keeps (the two rating ones are gone).
const V4_KEPT_INDEXES: &[&str] = &[
    "idx_audio_visible_stats",
    "idx_playlist_items_audio",
    "idx_project_references_audio",
    "idx_waveform_cache_audio",
    "idx_audio_visible_modified",
    "idx_audio_visible_modified_desc",
    "idx_audio_visible_format",
    "idx_audio_visible_format_name",
    "idx_audio_visible_color",
];
/// v5 replaces the user-only tag index by one that also covers the derived column.
const NEW_TAG_INDEX: &str = "idx_audio_visible_tagged_any";
const OLD_TAG_INDEX: &str = "idx_audio_visible_tagged";
const RATING_INDEXES: &[&str] = &[
    "idx_audio_visible_rating",
    "idx_audio_visible_rating_desc",
    "idx_user_rating",
];

fn index_sql(conn: &Connection, name: &str) -> Option<String> {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='index' AND name=?1",
        [name],
        |r| r.get::<_, String>(0),
    )
    .ok()
}

fn index_exists(conn: &Connection, name: &str) -> bool {
    index_sql(conn, name).is_some()
}

fn make_library(path: &Path) {
    let conn = support::connection(path).unwrap();
    conn.execute_batch(include_str!("fixtures/schema-v5.sql"))
        .unwrap();
    for id in 1..=40i64 {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename,folder_id,duration,file_size,user_tags,user_color,user_rating,bit_depth,channels,missing,favorite)
             VALUES(?1,?2,?3,NULL,1.5,100,?4,?5,?6,24,2,?7,?8)",
            rusqlite::params![
                id,
                format!("{id}.wav"),
                format!("f{id}.wav"),
                if id % 3 == 0 { "tag" } else { " " },
                if id % 5 == 0 { Some("#FF0000") } else { None },
                id % 6,
                id % 10 == 0,
                id % 4 == 0
            ],
        )
        .unwrap();
        if id % 7 == 0 {
            conn.execute(
                "UPDATE audio_files SET metadata_json=?2 WHERE id=?1",
                rusqlite::params![
                    id,
                    r#"{"wav_metadata":{"fields":{"keywords":"Engine; Rattle","category":"Air"}}}"#
                ],
            )
            .unwrap();
        }
    }
}

fn source_tags(path: &Path) -> Vec<String> {
    let conn = support::connection(path).unwrap();
    let mut s = conn
        .prepare("SELECT source_tags_text FROM audio_files ORDER BY id")
        .unwrap();
    s.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn ratings(path: &Path) -> Vec<i64> {
    let conn = support::connection(path).unwrap();
    let mut s = conn
        .prepare("SELECT user_rating FROM audio_files ORDER BY id")
        .unwrap();
    s.query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

/// Remove additive v5 fields/indexes from the captured historical layout.
/// This is a version-band fixture, not an archived original v3/v4 database.
fn downgrade(path: &Path, version: i64) {
    let conn = support::connection(path).unwrap();
    if version == 3 {
        for name in V4_KEPT_INDEXES {
            conn.execute_batch(&format!("DROP INDEX IF EXISTS {name};"))
                .unwrap();
        }
    }
    // Pre-v5 libraries have neither the derived column nor its marker/index.
    conn.execute_batch(&format!(
        "DROP INDEX IF EXISTS {NEW_TAG_INDEX};
         DROP INDEX IF EXISTS idx_audio_visible_tagged_folders;
         ALTER TABLE audio_files DROP COLUMN source_tags_text;
         DELETE FROM library_meta WHERE key LIKE 'source_tags%';
         CREATE INDEX {OLD_TAG_INDEX} ON audio_files(filename COLLATE NOCASE,id) WHERE missing=0 AND COALESCE(trim(user_tags),'')<>'';"
    ))
    .unwrap();
    conn.execute_batch(
        "DROP INDEX IF EXISTS idx_audio_visible_favorite;
         CREATE INDEX idx_audio_visible_favorite ON audio_files(user_rating,filename COLLATE NOCASE,id) WHERE missing=0 AND favorite=1;
         CREATE INDEX idx_user_rating ON audio_files(user_rating);
         CREATE INDEX idx_audio_visible_rating_desc ON audio_files(user_rating DESC,id) WHERE missing=0;
         CREATE INDEX idx_audio_visible_rating ON audio_files(user_rating,id) WHERE missing=0;",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", version).unwrap();
}

fn definitions(path: &Path) -> Vec<String> {
    let conn = support::connection(path).unwrap();
    // Exclude only the explicitly managed objects added by later schemas;
    // every unrelated table and trigger must still retain its original SQL.
    let mut s = conn
        .prepare("SELECT sql FROM sqlite_master WHERE type IN ('table','trigger') AND name<>'audio_files' AND name NOT LIKE 'audio_search%' AND name NOT IN ('audio_files_ai','audio_files_au','audio_files_ad','play_history','audiolib_migration_receipts','albums','audiolib_album_id_sequence','audiolib_album_audio_insert','audiolib_album_audio_update','audiolib_album_folder_delete','audiolib_album_id_high_water') ORDER BY name")
        .unwrap();
    s.query_map([], |r| r.get::<_, Option<String>>(0))
        .unwrap()
        .map(|r| r.unwrap().unwrap_or_default())
        .collect()
}

#[test]
fn database_target_matches_core_catalog_with_legacy_snapshots_accepted() {
    assert_eq!(
        SCHEMA_VERSION,
        i64::from(audiolib_core::compatibility::TARGET_SCHEMA)
    );
    assert_eq!(MIN_SNAPSHOT_SCHEMA_VERSION, 3);
}

#[test]
fn v3_and_v4_libraries_upgrade_indexes_and_derived_tags_and_keep_rating_data() {
    for from in [3, 4] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        make_library(&path);
        downgrade(&path, from);
        let before = ratings(&path);
        assert!(before.iter().any(|r| *r > 0));
        let tables_before = definitions(&path);
        let provider = SqliteProvider::open_existing(&path).unwrap();
        assert_eq!(
            provider.library_info().unwrap().schema_version,
            SCHEMA_VERSION
        );
        drop(provider);
        let conn = support::connection(&path).unwrap();
        for name in RATING_INDEXES {
            assert!(
                !index_exists(&conn, name),
                "v{from}: {name} must be dropped"
            );
        }
        for name in V4_KEPT_INDEXES {
            assert!(index_exists(&conn, name), "v{from}: {name} missing");
        }
        let favorite = index_sql(&conn, "idx_audio_visible_favorite").unwrap();
        assert!(!favorite.contains("user_rating"), "{favorite}");
        assert_eq!(
            definitions(&path),
            tables_before,
            "migration must preserve unrelated tables and triggers"
        );
        assert!(index_exists(&conn, NEW_TAG_INDEX));
        assert!(!index_exists(&conn, OLD_TAG_INDEX));
        drop(conn);
        // The derived column is backfilled from metadata_json, only for rows with tags.
        let derived = source_tags(&path);
        for (i, text) in derived.iter().enumerate() {
            let id = i + 1;
            let expected = if id % 7 == 0 {
                "engine, rattle, air"
            } else {
                ""
            };
            assert_eq!(text, expected, "v{from} row {id}");
        }
        let conn = support::connection(&path).unwrap();
        let marker: String = conn
            .query_row(
                "SELECT value FROM library_meta WHERE key='source_tags_rev'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(marker, "2");
        drop(conn);
        assert_eq!(ratings(&path), before, "v{from}: rating data is untouched");
        // Re-running is a no-op.
        SqliteProvider::open_existing(&path).unwrap();
        assert_eq!(ratings(&path), before);
        assert_eq!(source_tags(&path), derived);
        let version: i64 = support::connection(&path)
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }
}

#[test]
fn v3_v4_v5_snapshots_repair_owned_copies_without_migrating_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    make_library(&path);
    downgrade(&path, 3);
    let original = std::fs::read(&path).unwrap();
    let provider = SqliteProvider::open_read_only(&path).unwrap();
    assert_eq!(
        provider.library_info().unwrap().schema_version,
        SCHEMA_VERSION
    );
    assert_ne!(provider.database_path(), path.as_path());
    assert!(SqliteProvider::has_current_search_state(provider.database_path()).unwrap());
    drop(provider);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    // Still v3 with its old indexes: only the exclusively owned copy is repaired.
    let conn = support::connection(&path).unwrap();
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3);
    assert!(!index_exists(&conn, "idx_audio_visible_stats"));
    assert!(index_exists(&conn, "idx_audio_visible_rating"));
    for bad in [2, SCHEMA_VERSION + 1] {
        conn.pragma_update(None, "user_version", bad).unwrap();
        assert!(
            SqliteProvider::open_read_only(&path).is_err(),
            "schema {bad}"
        );
    }
    for good in [4, 5] {
        conn.pragma_update(None, "user_version", good).unwrap();
        let original = std::fs::read(&path).unwrap();
        let provider = SqliteProvider::open_read_only(&path).unwrap();
        assert_eq!(
            provider.library_info().unwrap().schema_version,
            SCHEMA_VERSION,
            "schema {good}"
        );
        assert_ne!(provider.database_path(), path.as_path());
        drop(provider);
        assert_eq!(std::fs::read(&path).unwrap(), original, "schema {good}");
    }
}

#[test]
fn client_selects_never_reference_user_rating() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    make_library(&path);
    downgrade(&path, 4);
    // A snapshot from a server that has no rating column at all must still be readable.
    {
        let conn = support::connection(&path).unwrap();
        conn.execute_batch(
            "DROP INDEX idx_audio_visible_favorite; DROP INDEX idx_user_rating;
             DROP INDEX idx_audio_visible_rating_desc; DROP INDEX idx_audio_visible_rating;
             ALTER TABLE audio_files DROP COLUMN user_rating;",
        )
        .unwrap();
    }
    let provider = SqliteProvider::open_read_only(&path).unwrap();
    let page = provider
        .search(&SearchRequest {
            filters: SearchFilters {
                favorites_only: true,
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
    assert!(page.total_count > 0);
    assert_eq!(provider.get_audio(1).unwrap().filename, "f1.wav");
}

fn plan(conn: &Connection, sql: &str) -> String {
    let mut s = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    s.query_map([], |r| r.get::<_, String>(3))
        .unwrap()
        .map(|r| r.unwrap())
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn query_plans_use_the_remaining_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    make_library(&path);
    let conn = support::connection(&path).unwrap();
    let cases = [
        (
            "SELECT af.id FROM audio_files af WHERE af.missing=0 AND (COALESCE(trim(af.user_tags),'')<>'' OR af.source_tags_text<>'') ORDER BY af.filename COLLATE NOCASE ASC,af.id ASC LIMIT 50",
            NEW_TAG_INDEX,
        ),
        (
            "SELECT af.id FROM audio_files af WHERE af.missing=0 AND af.favorite=1 ORDER BY af.filename COLLATE NOCASE ASC,af.id ASC LIMIT 50",
            "idx_audio_visible_favorite",
        ),
        (
            "SELECT af.id FROM audio_files af WHERE af.missing=0 AND af.user_color COLLATE NOCASE='#FF0000' ORDER BY af.id LIMIT 50",
            "idx_audio_visible_color",
        ),
        (
            "SELECT af.id,af.modified_at AS v FROM audio_files af WHERE af.missing=0 ORDER BY v DESC,af.id ASC LIMIT 50",
            "idx_audio_visible_modified_desc",
        ),
        (
            "SELECT af.id FROM audio_files af WHERE af.missing=0 AND af.bit_depth=24 ORDER BY af.filename COLLATE NOCASE ASC,af.id ASC LIMIT 50",
            "idx_audio_visible_format_name",
        ),
        (
            "SELECT af.id,af.modified_at AS v FROM audio_files af WHERE af.missing=0 ORDER BY v ASC,af.id ASC LIMIT 50",
            "idx_audio_visible_modified",
        ),
    ];
    for (sql, index) in cases {
        let p = plan(&conn, sql);
        assert!(p.contains(index), "expected {index} in plan: {p}\n{sql}");
        assert!(
            !p.contains("USE TEMP B-TREE FOR ORDER BY"),
            "sorter in plan: {p}"
        );
    }
}

#[test]
fn imports_tolerate_and_ignore_legacy_rating_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    make_library(&path);
    let provider = SqliteProvider::open(&path).unwrap();
    let before = ratings(&path);

    // Exports no longer carry a rating column.
    let csv = dir.path().join("export.csv");
    provider
        .export_library(&csv, LibraryExportFormat::Csv)
        .unwrap();
    let header = std::fs::read_to_string(&csv).unwrap();
    assert!(
        !header.lines().next().unwrap().contains("rating"),
        "{header}"
    );

    // JSON Lines from an older client: rating is accepted (even out of range) and ignored.
    let jsonl = dir.path().join("legacy.jsonl");
    let line = |id: i64, rating: &str| {
        format!(r#"{{"file_path":"{id}.wav","tags":"legacy {id}","rating":{rating}}}"#)
    };
    std::fs::write(&jsonl, format!("{}\n{}\n", line(1, "5"), line(2, "99"))).unwrap();
    let report = provider.import_annotations(&jsonl).unwrap();
    assert_eq!(report.records + report.skipped, 2);

    // CSV with a legacy rating column (including a non-numeric cell).
    let legacy_csv = dir.path().join("legacy.csv");
    std::fs::write(
        &legacy_csv,
        "\"file_path\",\"rating\",\"tags\"\r\n\"3.wav\",\"4\",\"legacy 3\"\r\n\"4.wav\",\"n/a\",\"legacy 4\"\r\n",
    )
    .unwrap();
    let report = provider.import_annotations(&legacy_csv).unwrap();
    assert_eq!(report.records + report.skipped, 2);
    drop(provider);
    assert_eq!(ratings(&path), before, "imports never write user_rating");
}
