// crates/audiolib-db/tests/schema_snapshot_merge.rs
//! Supported old snapshots repair derived state only in owned copies.
use audiolib_core::{DataProvider, Error, SearchFilters, SearchRequest, SortOrder};
use audiolib_db::{SCHEMA_VERSION, SqliteProvider};
use rusqlite::Connection;
use std::{fs, path::Path};

fn fixture(path: &Path) {
    let conn = Connection::open(path).unwrap();
    // This suite exercises the supported external-content snapshot family.
    // A freshly created contentless database cannot represent those historical inputs.
    conn.execute_batch(include_str!("fixtures/schema-v5.sql"))
        .unwrap();
    conn.execute_batch(
        "INSERT INTO audio_files(id,file_path,filename,filename_tokenized,
         user_tags,user_tags_tokenized,user_notes,user_rating,favorite,metadata_json)
         VALUES(42,'first.wav','café.wav','caf é wav','private','private','retain notes',5,1,
         '{\"private\":\"keep\",\"wav_metadata\":{\"fields\":{\"keywords\":\"Metal; Rattle\"}}}'),
         (43,'second.wav','café.wav','caf é wav','private','private','other notes',4,0,
         '{\"wav_metadata\":{\"fields\":{\"keywords\":\"Metal; Rattle\"}}}');
         UPDATE audiolib_audio_id_sequence SET high_water=900;
         PRAGMA journal_mode=DELETE;",
    )
    .unwrap();
}

#[test]
fn obsolete_v3_v4_v5_derived_snapshots_keep_source_bytes_annotations_and_identities() {
    let directory = tempfile::tempdir().unwrap();
    for version in [3, 4, 5] {
        for retained_sequence in [false, true] {
            let path = directory
                .path()
                .join(format!("v{version}-{retained_sequence}.db"));
            fixture(&path);
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "DROP INDEX idx_audio_visible_tagged_any;
                 DROP INDEX idx_audio_visible_tagged_folders;
                 ALTER TABLE audio_files DROP COLUMN source_tags_text;
                 DROP TABLE library_meta;
                 DELETE FROM audiolib_schema_features WHERE feature='tokenization';",
            )
            .unwrap();
            if !retained_sequence {
                conn.execute_batch(
                    "DROP TRIGGER audiolib_audio_id_high_water;
                     DROP TABLE audiolib_audio_id_sequence;
                     DROP TABLE audiolib_schema_features;",
                )
                .unwrap();
            }
            conn.pragma_update(None, "user_version", version).unwrap();
            drop(conn);
            let original = fs::read(&path).unwrap();
            assert!(!SqliteProvider::has_current_search_state(&path).unwrap());
            let provider = SqliteProvider::open_read_only(&path).unwrap();
            assert_ne!(provider.database_path(), path.as_path());
            assert_eq!(
                provider.library_info().unwrap().schema_version,
                SCHEMA_VERSION
            );
            assert!(SqliteProvider::has_current_search_state(provider.database_path()).unwrap());
            let page = provider
                .search(&SearchRequest {
                    query: "cafe metal".into(),
                    order_by: SortOrder::Filename,
                    filters: SearchFilters {
                        tagged_only: true,
                        tag_contains: Some("RATTLE".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(
                page.results.iter().map(|row| row.id).collect::<Vec<_>>(),
                [42, 43]
            );
            let file = provider.get_audio(42).unwrap();
            assert_eq!((&*file.tags, &*file.notes), ("private", "retain notes"));
            assert!(file.favorite);
            assert_eq!(file.attributes["private"], "keep");
            let copy = Connection::open(provider.database_path()).unwrap();
            assert_eq!(
                copy.query_row("SELECT user_rating FROM audio_files WHERE id=42", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                5
            );
            let high_water: i64 = copy
                .query_row(
                    "SELECT high_water FROM audiolib_audio_id_sequence",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            if retained_sequence {
                assert_eq!(high_water, 900);
            } else {
                assert!(high_water > 900);
            }
            assert!(copy.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='idx_audio_visible_tagged_any')", [], |r| r.get::<_, bool>(0)).unwrap());
            drop(copy);
            let owned_directory = provider.database_path().parent().unwrap().to_path_buf();
            drop(provider);
            assert!(!owned_directory.exists());
            assert_eq!(fs::read(&path).unwrap(), original);
        }
    }
}

#[test]
fn current_derived_v3_v4_v5_snapshots_use_owned_album_projection_without_source_writes() {
    let directory = tempfile::tempdir().unwrap();
    for version in [3, 4, 5] {
        let path = directory.path().join(format!("current-v{version}.db"));
        fixture(&path);
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", version)
            .unwrap();
        let original = fs::read(&path).unwrap();
        let provider = SqliteProvider::open_read_only(&path).unwrap();
        assert_ne!(provider.database_path(), path.as_path());
        assert_eq!(
            provider.library_info().unwrap().schema_version,
            SCHEMA_VERSION
        );
        assert!(SqliteProvider::has_current_album_state(provider.database_path()).unwrap());
        let file = provider.get_audio(42).unwrap();
        assert_eq!((&*file.tags, &*file.notes), ("private", "retain notes"));
        assert!(file.favorite);
        assert_eq!(file.attributes["private"], "keep");
        assert_eq!(file.album_id, Some(file.album_entity.unwrap().id));
        assert!(matches!(
            provider.update_metadata(
                42,
                &audiolib_core::MetadataPatch {
                    favorite: Some(false),
                    ..Default::default()
                }
            ),
            Err(Error::Validation(_))
        ));
        assert!(provider.get_audio(42).unwrap().favorite);
        let source =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(
            source
                .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            version
        );
        drop(source);
        assert_eq!(fs::read(&path).unwrap(), original);
        let owned_directory = provider.database_path().parent().unwrap().to_path_buf();
        drop(provider);
        assert!(!owned_directory.exists());
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}

#[test]
fn future_source_derived_revisions_are_rejected_before_owned_or_live_writes() {
    let directory = tempfile::tempdir().unwrap();
    for (key, future) in [
        ("source_search_rev", "3"),
        ("source_tags_rev", "3"),
        ("source_tags_progress_rev", "3"),
    ] {
        let path = directory.path().join(format!("future-{key}.db"));
        fixture(&path);
        Connection::open(&path)
            .unwrap()
            .execute(
                "INSERT OR REPLACE INTO library_meta(value,key) VALUES(?1,?2)",
                [future, key],
            )
            .unwrap();
        let original = fs::read(&path).unwrap();
        for result in [
            SqliteProvider::has_current_search_state(&path).map(|_| ()),
            SqliteProvider::upgrade_owned_snapshot(&path).map(|_| ()),
            SqliteProvider::open_read_only(&path).map(|_| ()),
            SqliteProvider::open_existing(&path).map(|_| ()),
        ] {
            assert!(
                matches!(result, Err(Error::NewerDatabase { ref what, found: 3, supported: 2 }) if what.contains(key)),
                "{key}: {result:?}"
            );
            assert_eq!(fs::read(&path).unwrap(), original);
        }
    }
}
