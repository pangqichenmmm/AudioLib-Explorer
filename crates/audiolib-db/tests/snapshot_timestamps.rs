// crates/audiolib-db/tests/snapshot_timestamps.rs
//! Incremental Metadata replies preserve the server's timestamp and ordering.
mod support;
use audiolib_core::{DataProvider, SearchRequest, SortOrder};
use audiolib_db::SqliteProvider;
use rusqlite::{Connection, OpenFlags};
use std::{fs, path::Path};

fn historical_plan_b(conn: &Connection, version: i64) {
    // Construct the published v6/v7 layout from captured v5, independently of
    // schema-8 album tables, compressed storage and its current feature map.
    conn.execute_batch(
        "DROP TRIGGER audio_files_ai; DROP TRIGGER audio_files_au;
         DROP TRIGGER audio_files_ad; DROP TABLE audio_search;
         DROP TABLE play_history;
         ALTER TABLE audio_files DROP COLUMN play_count;
         ALTER TABLE audio_files DROP COLUMN last_played_at;
         ALTER TABLE audio_files DROP COLUMN filename_tokenized;
         ALTER TABLE audio_files DROP COLUMN user_tags_tokenized;
         ALTER TABLE audio_files DROP COLUMN bwf_description_tokenized;
         ALTER TABLE audio_files DROP COLUMN ixml_scene_tokenized;
         ALTER TABLE audio_files DROP COLUMN ixml_take_tokenized;
         ALTER TABLE audio_files DROP COLUMN user_notes_tokenized;
         INSERT INTO audiolib_schema_features(feature,version) VALUES
           ('search_storage',1),('play_history_retired',2);
         CREATE VIRTUAL TABLE audio_search USING fts5(
           filename,filename_tokenized,user_tags,user_tags_tokenized,
           bwf_description,bwf_description_tokenized,ixml_scene,ixml_scene_tokenized,
           ixml_take,ixml_take_tokenized,user_notes,user_notes_tokenized,
           content='',contentless_delete=1,detail=full,columnsize=1,
           tokenize='unicode61 remove_diacritics 2');",
    )
    .unwrap();
    let columns = "filename,filename_tokenized,user_tags,user_tags_tokenized,bwf_description,bwf_description_tokenized,ixml_scene,ixml_scene_tokenized,ixml_take,ixml_take_tokenized,user_notes,user_notes_tokenized";
    let values = "new.filename,audiolib_tokenize(new.filename),new.user_tags,audiolib_tokenize(new.user_tags),new.bwf_description,audiolib_tokenize(audiolib_source_text(new.bwf_description,new.metadata_json)),new.ixml_scene,audiolib_tokenize(new.ixml_scene),new.ixml_take,audiolib_tokenize(new.ixml_take),new.user_notes,audiolib_tokenize(new.user_notes)";
    conn.execute_batch(&format!(
        "CREATE TRIGGER audio_files_ai AFTER INSERT ON audio_files BEGIN
           INSERT INTO audio_search(rowid,{columns}) VALUES(new.id,{values}); END;
         CREATE TRIGGER audio_files_au AFTER UPDATE OF
           id,filename,user_tags,bwf_description,metadata_json,ixml_scene,ixml_take,user_notes
           ON audio_files BEGIN
           DELETE FROM audio_search WHERE rowid=old.id;
           INSERT INTO audio_search(rowid,{columns}) VALUES(new.id,{values}); END;
         CREATE TRIGGER audio_files_ad AFTER DELETE ON audio_files BEGIN
           DELETE FROM audio_search WHERE rowid=old.id; END;"
    ))
    .unwrap();
    if version == 7 {
        conn.execute_batch(
            "ALTER TABLE audio_files ADD COLUMN album TEXT NOT NULL DEFAULT '';
             CREATE INDEX idx_audio_visible_album ON audio_files(album,id) WHERE missing=0;",
        )
        .unwrap();
    }
}

fn fixture(path: &Path, version: i64) {
    if version < audiolib_db::SCHEMA_VERSION {
        let conn = support::connection(path).unwrap();
        conn.execute_batch(include_str!("fixtures/schema-v5.sql"))
            .unwrap();
        if version >= 6 {
            historical_plan_b(&conn, version);
        }
        conn.pragma_update(None, "user_version", version).unwrap();
    } else {
        drop(SqliteProvider::open(path).unwrap());
    }
    support::connection(path).unwrap().execute_batch(
        "INSERT INTO audio_files(id,file_path,path_key,filename,modified_at,metadata_json)
         VALUES(1,'/remote/one.wav','/remote/one.wav','one.wav','2020-01-01 01:02:03','{\"private\":\"retain\"}'),
         (2,'/remote/two.wav','/remote/two.wav','two.wav','2024-01-01 01:02:03','{}');"
    ).unwrap();
}

fn source_rows(path: &Path) -> Vec<(i64, String, String, String)> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    let rows = conn.prepare("SELECT id,COALESCE(user_notes,''),modified_at,audiolib_metadata_json(metadata_json) FROM audio_files ORDER BY id").unwrap()
        .query_map([], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))
        .unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    drop(conn);
    rows
}

fn source_version(path: &Path) -> i64 {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

#[test]
fn authoritative_timestamp_and_modified_sort_match_for_all_snapshot_schemas() {
    let root = tempfile::tempdir().unwrap();
    for version in 3..=audiolib_db::SCHEMA_VERSION {
        let path = root.path().join(format!("snapshot-v{version}.db"));
        fixture(&path, version);
        let original_bytes = fs::read(&path).unwrap();
        assert_eq!(source_version(&path), version);
        let provider = SqliteProvider::open_read_only(&path).unwrap();
        if version < audiolib_db::SCHEMA_VERSION {
            assert_ne!(provider.database_path(), path);
        } else {
            assert_eq!(provider.database_path(), path);
        }
        assert_eq!(fs::read(&path).unwrap(), original_bytes);
        assert_eq!(source_version(&path), version);
        let mut reply = provider.get_audio(1).unwrap();
        reply.tags = "updated tag".into();
        reply.notes = "server note".into();
        // Deliberately unlike the client's wall clock, and keep exact precision.
        reply.attributes["modified_at"] = "2031-04-05 06:07:08.123456".into();
        SqliteProvider::apply_snapshot_records(&path, &[reply]).unwrap();
        if version < audiolib_db::SCHEMA_VERSION {
            assert_eq!(
                provider.get_audio(1).unwrap().attributes["modified_at"],
                "2020-01-01 01:02:03"
            );
        }
        drop(provider);
        let provider = SqliteProvider::open_read_only(&path).unwrap();
        assert_eq!(source_version(&path), version);
        let row = provider.get_audio(1).unwrap();
        assert_eq!(row.attributes["modified_at"], "2031-04-05 06:07:08.123456");
        assert_eq!(row.attributes["private"], "retain");
        assert_eq!(row.notes, "server note");
        for (descending, expected) in [(true, vec![1, 2]), (false, vec![2, 1])] {
            let page = provider
                .search(&SearchRequest {
                    order_by: SortOrder::Modified,
                    descending,
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(
                page.results.iter().map(|row| row.id).collect::<Vec<_>>(),
                expected
            );
        }
        assert_eq!(
            provider
                .search(&SearchRequest {
                    query: "updated".into(),
                    ..Default::default()
                })
                .unwrap()
                .total_count,
            1
        );
    }
}

#[test]
fn unsupported_or_missing_timestamps_require_full_sync_without_partial_writes() {
    let root = tempfile::tempdir().unwrap();
    for version in [5, audiolib_db::SCHEMA_VERSION] {
        let path = root.path().join(format!("invalid-v{version}.db"));
        fixture(&path, version);
        let provider = SqliteProvider::open_read_only(&path).unwrap();
        let original = provider.get_audio(1).unwrap();
        let original_rows = source_rows(&path);
        let original_bytes = fs::read(&path).unwrap();
        for bad in [
            None,
            Some(serde_json::Value::Null),
            Some(17.into()),
            Some("tomorrow".into()),
            Some("2026-02-30 12:00:00".into()),
            Some("2026-10-24T12:00:00Z".into()),
        ] {
            let mut first = original.clone();
            first.notes = "must roll back".into();
            let mut second = provider.get_audio(2).unwrap();
            if let Some(bad) = bad {
                second.attributes["modified_at"] = bad;
            } else {
                second
                    .attributes
                    .as_object_mut()
                    .unwrap()
                    .remove("modified_at");
            }
            assert!(SqliteProvider::apply_snapshot_records(&path, &[first, second]).is_err());
            assert_eq!(source_rows(&path), original_rows);
            assert_eq!(source_version(&path), version);
            assert_eq!(fs::read(&path).unwrap(), original_bytes);
            assert_eq!(provider.get_audio(1).unwrap().notes, original.notes);
            assert_eq!(
                provider.get_audio(1).unwrap().attributes["modified_at"],
                original.attributes["modified_at"]
            );
        }
    }
}
