// crates/audiolib-db/tests/snapshot_schema9.rs
//! Real migrated snapshots retain their format, path search and incremental annotations.
use audiolib_core::{DataProvider, Error, MetadataPatch, SearchRequest, SearchScope};
use audiolib_db::{
    SqliteProvider,
    migration9::{self, Action, Options, State},
};
use rusqlite::{Connection, OpenFlags, params};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};
mod support;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("server.db");
    drop(SqliteProvider::open(&source).unwrap());
    let conn = support::connection(&source).unwrap();
    conn.execute_batch("INSERT INTO folders(id,folder_path,folder_name,parent_id) VALUES (1,'root','root',NULL),(2,'root/ＳＦＸ','ＳＦＸ',1),(3,'root/other','other',1); INSERT INTO folder_closure VALUES(1,1,0),(2,2,0),(3,3,0),(1,2,1),(1,3,1);").unwrap();
    for (id, folder) in [
        (1, Some(2)),
        (2, Some(3)),
        (3, Some(2)),
        (4, None),
        (5, Some(3)),
        (6, Some(3)),
    ] {
        conn.execute("INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,user_notes,metadata_json) VALUES(?1,?2,?2,?2,?3,'original', '{\"unknown\":\"原文\"}')", params![id,format!("sound-{id}.wav"),folder]).unwrap();
    }
    // Incremental replies preserve the server's authoritative SQLite timestamp.
    conn.execute(
        "UPDATE audio_files SET modified_at='2026-11-04 00:00:00.000000'",
        [],
    )
    .unwrap();
    drop(conn);
    let report = migration9::execute(
        &source,
        Action::Start,
        &Options::default(),
        &AtomicBool::new(false),
    );
    assert_eq!(report.state, State::Done, "{report:?}");
    let snapshot = dir.path().join("snapshot.db");
    let conn = Connection::open_with_flags(&source, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut copy = Connection::open(&snapshot).unwrap();
    rusqlite::backup::Backup::new(&conn, &mut copy)
        .unwrap()
        .run_to_completion(4096, Duration::ZERO, None)
        .unwrap();
    copy.pragma_update(None, "journal_mode", "DELETE").unwrap();
    drop(copy);
    (dir, snapshot)
}

fn validate(path: &Path) -> audiolib_core::Result<()> {
    SqliteProvider::validate_owned_snapshot_format_cancellable(path, &AtomicBool::new(false))
}

#[test]
fn completed_snapshot_opens_without_conversion_and_patches_annotations() {
    let (_dir, snapshot) = fixture();
    let before = std::fs::read(&snapshot).unwrap();
    validate(&snapshot).unwrap();
    assert!(!SqliteProvider::upgrade_owned_snapshot(&snapshot).unwrap());
    let provider = SqliteProvider::open_read_only(&snapshot).unwrap();
    assert_eq!(provider.database_path(), snapshot);
    assert_eq!(std::fs::read(&snapshot).unwrap(), before);
    let page = provider
        .search(&SearchRequest {
            query: "SFX".into(),
            scope: SearchScope::Path,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        page.results.iter().map(|file| file.id).collect::<Vec<_>>(),
        vec![1, 3]
    );
    assert!(
        provider
            .update_metadata(1, &MetadataPatch::default())
            .is_err()
    );
    assert!(SqliteProvider::open(&snapshot).is_err());
    let mut file = provider.get_audio(1).unwrap();
    file.notes = "patchedannotation".into();
    SqliteProvider::apply_snapshot_records(&snapshot, &[file]).unwrap();
    let reopened = SqliteProvider::open_read_only(&snapshot).unwrap();
    assert_eq!(reopened.get_audio(1).unwrap().notes, "patchedannotation");
    assert_eq!(
        reopened
            .search(&SearchRequest {
                query: "patchedannotation".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        1
    );
    validate(&snapshot).unwrap();
    assert_eq!(
        Connection::open(&snapshot)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        9
    );
    assert_eq!(audiolib_db::SCHEMA_VERSION, 8);
}

#[test]
fn malformed_directory_snapshots_are_rejected_without_repair() {
    for sql in [
        "DELETE FROM audio_dir_search WHERE rowid=2",
        "INSERT INTO audio_dir_search(rowid,path_tokens,path_tokens_tokenized) VALUES(99,'orphan','orphan')",
        "DELETE FROM audio_dir_search_data WHERE id>10",
        "DELETE FROM audio_dir_runs WHERE lo=1",
        "UPDATE audio_dir_runs SET n=n+1 WHERE lo=1",
        "UPDATE audio_dir_runs SET dir_id=99 WHERE lo=1",
        "UPDATE audio_dir_runs SET hi=3,n=2 WHERE lo=1",
        "INSERT INTO audiolib_dir_dirty VALUES(2,1,0)",
        "UPDATE audio_files SET id=7 WHERE id=6;
         UPDATE audiolib_audio_id_sequence SET high_water=7 WHERE singleton=1;
         UPDATE audio_dir_runs SET hi=7 WHERE dir_id=3 AND lo=5;
         INSERT INTO audio_files(id,file_path,path_key,filename,folder_id) VALUES(6,'unassigned.wav','unassigned.wav','unassigned.wav',NULL);
         DELETE FROM audiolib_dir_dirty",
        "UPDATE library_meta SET value='2' WHERE key='search_path_rev'",
        "DELETE FROM audiolib_schema_features WHERE feature='vendor_metadata'",
        "CREATE TABLE v9_checkpoint(id INTEGER)",
        "DROP TABLE audio_dir_runs; CREATE TABLE audio_dir_runs(dir_id INTEGER,lo INTEGER,hi INTEGER,n INTEGER)",
    ] {
        let (_dir, snapshot) = fixture();
        support::connection(&snapshot)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        let before = std::fs::read(&snapshot).unwrap();
        assert!(validate(&snapshot).is_err(), "{sql}");
        assert_eq!(std::fs::read(&snapshot).unwrap(), before, "{sql}");
    }
}

#[test]
fn earlier_directory_only_schema9_snapshot_retains_its_read_contract() {
    let (_dir, snapshot) = fixture();
    support::connection(&snapshot).unwrap().execute_batch(
        "DELETE FROM audiolib_schema_features WHERE feature IN ('vorbis_tags','vendor_metadata');
         DROP TRIGGER audiolib_dir_folder_insert; DROP TRIGGER audiolib_dir_folder_delete;
         DROP TRIGGER audiolib_dir_folder_update; DROP TRIGGER audiolib_dir_folder_stats;
         DROP TRIGGER audiolib_dir_audio_insert; DROP TRIGGER audiolib_dir_audio_delete;
         DROP TRIGGER audiolib_dir_audio_update; DROP TABLE audiolib_dir_dirty;"
    ).unwrap();
    let before = std::fs::read(&snapshot).unwrap();
    validate(&snapshot).unwrap();
    assert!(!SqliteProvider::upgrade_owned_snapshot(&snapshot).unwrap());
    let provider = SqliteProvider::open_read_only(&snapshot).unwrap();
    assert_eq!(provider.database_path(), snapshot);
    assert_eq!(
        provider
            .search(&SearchRequest {
                query: "SFX".into(),
                scope: SearchScope::Path,
                ..Default::default()
            })
            .unwrap()
            .total_count,
        2
    );
    assert_eq!(std::fs::read(&snapshot).unwrap(), before);
}

#[test]
fn unsupported_or_partial_formats_never_enter_schema8_owned_migration() {
    for sql in [
        "DELETE FROM audiolib_schema_features WHERE feature='vendor_metadata'",
        "UPDATE audiolib_schema_features SET version=2 WHERE feature='search_path'",
        "UPDATE library_meta SET value='4' WHERE key='source_search_rev'",
        "CREATE TABLE v9_checkpoint(id INTEGER)",
        "PRAGMA user_version=10",
    ] {
        let (_dir, snapshot) = fixture();
        let file = SqliteProvider::open_read_only(&snapshot)
            .unwrap()
            .get_audio(1)
            .unwrap();
        support::connection(&snapshot)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        let before = std::fs::read(&snapshot).unwrap();
        assert!(SqliteProvider::open_read_only(&snapshot).is_err(), "{sql}");
        assert!(
            SqliteProvider::upgrade_owned_snapshot(&snapshot).is_err(),
            "{sql}"
        );
        assert!(
            SqliteProvider::apply_snapshot_records(&snapshot, &[file]).is_err(),
            "{sql}"
        );
        assert_eq!(std::fs::read(&snapshot).unwrap(), before, "{sql}");
    }
}

#[test]
fn owned_validation_is_cancellable_and_does_not_create_missing_files() {
    let (_dir, snapshot) = fixture();
    let before = std::fs::read(&snapshot).unwrap();
    let cancelled = AtomicBool::new(true);
    assert!(matches!(
        SqliteProvider::validate_owned_snapshot_format_cancellable(&snapshot, &cancelled),
        Err(Error::Cancelled)
    ));
    assert_eq!(std::fs::read(&snapshot).unwrap(), before);
    let missing = snapshot.with_file_name("missing.db");
    assert!(validate(&missing).is_err());
    assert!(!missing.exists());
}
