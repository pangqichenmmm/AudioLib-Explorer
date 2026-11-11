// crates/audiolib-db/tests/migration9.rs
//! Online directory-only migration: retained bytes, restart, and rollback contracts.
use audiolib_core::{DataProvider, MetadataPatch, SearchRequest, SearchScope};
use audiolib_db::{
    SqliteProvider,
    migration9::{self, Action, Options, State},
};
use rusqlite::{Connection, params};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
mod support;

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("library.db");
    drop(SqliteProvider::open(&db).unwrap());
    let c = support::connection(&db).unwrap();
    c.execute_batch("INSERT INTO folders(id,folder_path,folder_name,parent_id,total_file_count) VALUES (1,'root','root',NULL,4),(2,'root/ＳＦＸ','ＳＦＸ',1,1),(3,'root/other','other',1,3); INSERT INTO folder_closure VALUES(1,1,0),(2,2,0),(3,3,0),(1,2,1),(1,3,1);").unwrap();
    for id in 1..=4 {
        c.execute("INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,user_notes,metadata_json) VALUES(?1,?2,?2,?2,?3,'retained', '{\"extension\":\"原文\"}')", params![id,format!("sound-{id}.wav"),if id==1 {2} else {3}]).unwrap();
    }
    (dir, db)
}
fn run(db: &Path, action: Action) -> migration9::Report {
    migration9::execute(db, action, &Options::default(), &AtomicBool::new(false))
}
fn shadow(db: &Path) -> Vec<Vec<rusqlite::types::Value>> {
    let c = Connection::open(db).unwrap();
    let mut result = vec![];
    for table in [
        "audio_search_data",
        "audio_search_idx",
        "audio_search_docsize",
        "audio_search_config",
    ] {
        let mut s = c
            .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
            .unwrap();
        let n = s.column_count();
        result.extend(
            s.query_map([], |r| (0..n).map(|i| r.get(i)).collect())
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap(),
        );
    }
    result
}

#[test]
fn directory_only_switch_reopens_and_rollback_preserves_new_annotations() {
    let (_dir, db) = fixture();
    let before = shadow(&db);
    let report = run(&db, Action::Start);
    assert_eq!(report.state, State::Done, "{report:?}");
    assert_eq!(
        report.switch.unwrap().features,
        ["search_path", "vendor_metadata", "vorbis_tags"]
    );
    assert_eq!(before, shadow(&db));
    let c = Connection::open(&db).unwrap();
    migration9::validate_ready(&c).unwrap();
    assert_eq!(
        c.query_row("SELECT count(*) FROM audio_dir_search", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(c.query_row("SELECT count(*) FROM sqlite_schema WHERE name IN ('audio_path_search','v9_checkpoint')",[],|r| r.get::<_,i64>(0)).unwrap(),0);
    assert_eq!(
        c.query_row("SELECT count(*) FROM vendor_metadata", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(
        SqliteProvider::open(&db).is_err(),
        "ordinary client must not silently become an s9 writer"
    );
    let provider =
        SqliteProvider::open_server_existing(&db, &AtomicBool::new(false), |_| {}).unwrap();
    let results = provider
        .search(&SearchRequest {
            query: "SFX".into(),
            scope: SearchScope::Path,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        results.results.iter().map(|f| f.id).collect::<Vec<_>>(),
        vec![1]
    );
    provider
        .update_metadata(
            1,
            &MetadataPatch {
                notes: Some("after publication".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let after = shadow(&db);
    assert_eq!(run(&db, Action::Rollback).state, State::RolledBack);
    assert_eq!(after, shadow(&db));
    assert_eq!(provider.get_audio(1).unwrap().notes, "after publication");
    c.execute("UPDATE folders SET folder_name='later' WHERE id=2", [])
        .unwrap();
    assert_eq!(run(&db, Action::Start).state, State::Done);
    assert_eq!(
        provider
            .search(&SearchRequest {
                query: "later".into(),
                scope: SearchScope::Path,
                ..Default::default()
            })
            .unwrap()
            .results
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>(),
        vec![1],
        "a live reader must refresh its directory cache after re-migration"
    );
    assert!(run(&db, Action::Start).batches.is_empty());
}

#[test]
fn missing_and_unrelated_databases_are_never_initialized() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("missing.db");
    assert!(run(&db, Action::Start).error_code.is_some());
    assert!(!db.exists());
    Connection::open(&db)
        .unwrap()
        .execute_batch("CREATE TABLE unrelated(value TEXT);")
        .unwrap();
    let before = std::fs::read(&db).unwrap();
    assert!(run(&db, Action::Start).error_code.is_some());
    assert_eq!(before, std::fs::read(&db).unwrap());
}

#[cfg(feature = "test-hooks")]
#[test]
fn slow_directory_batch_reduces_the_next_batch_without_changing_main_fts() {
    let (_dir, db) = fixture();
    let before = shadow(&db);
    let mut c = support::connection(&db).unwrap();
    let tx = c.transaction().unwrap();
    // The t3 batch unit is a directory, so three batches require enough folders,
    // irrespective of how many audio files belong to each folder.
    for id in 4..=150 {
        tx.execute(
            "INSERT INTO folders(id,folder_path,folder_name,parent_id,total_file_count) VALUES(?1,?2,?3,1,0)",
            params![id, format!("root/dir-{id}"), format!("dir-{id}")],
        )
        .unwrap();
        tx.execute("INSERT INTO folder_closure VALUES(?1,?1,0),(1,?1,1)", [id])
            .unwrap();
    }
    tx.commit().unwrap();
    drop(c);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_audiolib-migrate"))
        .args(["start", "--db"])
        .arg(&db)
        .env("AUDIOLIB_MIGRATION_BATCH_ROWS", "50")
        .env("AUDIOLIB_MIGRATION_SLOW_BATCH", "2:1500")
        .output()
        .unwrap();
    let report: migration9::Report = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success(), "{report:?}");
    assert_eq!(report.state, State::Done);
    assert!(report.batches.len() >= 3, "{report:?}");
    assert!(
        report.batches[2].rows < report.batches[1].rows,
        "{report:?}"
    );
    assert_eq!(report.progress.done, 150);
    assert_eq!(before, shadow(&db));
    migration9::validate_ready(&Connection::open(&db).unwrap()).unwrap();
}

#[cfg(feature = "test-hooks")]
#[test]
fn subprocess_faults_resume_directory_batches_without_rebuilding_main() {
    for (fault, done) in [
        ("abort:batch_boundary:1", 1),
        ("abort:mid_batch:1", 0),
        ("abort:mid_batch:2", 1),
        ("abort:optimize", 3),
        ("abort:switch_tx", 3),
        ("sqlite_full:batch:2", 1),
    ] {
        let (_dir, db) = fixture();
        let before = shadow(&db);
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_audiolib-migrate"))
            .args(["start", "--db"])
            .arg(&db)
            .env("AUDIOLIB_MIGRATION_BATCH_ROWS", "1")
            .env("AUDIOLIB_MIGRATION_FAULT", fault)
            .output()
            .unwrap();
        assert!(!output.status.success(), "{fault}");
        if fault.starts_with("abort:") {
            assert!(output.stdout.is_empty(), "{fault}");
        } else {
            let report: migration9::Report = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(report.error_code, Some(migration9::Failure::DiskFull));
        }
        assert_eq!(run(&db, Action::Status).state, State::Paused, "{fault}");
        let c = support::connection(&db).unwrap();
        assert_eq!(
            c.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            8,
            "{fault}: publication must remain atomic"
        );
        assert_eq!(c.query_row("SELECT count(*) FROM audiolib_schema_features WHERE feature IN ('search_path','vorbis_tags','vendor_metadata')", [], |r| r.get::<_,i64>(0)).unwrap(), 0, "{fault}");
        assert_eq!(
            c.query_row("SELECT done FROM v9_checkpoint", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            done,
            "{fault}"
        );
        c.execute("UPDATE folders SET folder_name='renamed' WHERE id=2", [])
            .unwrap();
        let report = run(&db, Action::Resume);
        assert_eq!(report.state, State::Done, "{fault}: {report:?}");
        assert_eq!(before, shadow(&db), "{fault}");
        assert_eq!(
            migration9::search(&db, "renamed".into(), SearchScope::Path, 100, None)
                .unwrap()
                .ids,
            Some(vec![1]),
            "{fault}"
        );
    }
}

#[test]
fn earlier_directory_only_schema9_remains_readable_and_partial_features_fail() {
    let (_dir, db) = fixture();
    assert_eq!(run(&db, Action::Start).state, State::Done);
    let c = Connection::open(&db).unwrap();
    c.execute(
        "DELETE FROM audiolib_schema_features WHERE feature='vendor_metadata'",
        [],
    )
    .unwrap();
    assert!(migration9::validate_ready(&c).is_err());
    c.execute(
        "DELETE FROM audiolib_schema_features WHERE feature='vorbis_tags'",
        [],
    )
    .unwrap();
    migration9::validate_ready(&c).unwrap();
    assert_eq!(
        run(&db, Action::Status).switch.unwrap().features,
        ["search_path"]
    );
    let provider =
        SqliteProvider::open_server_existing(&db, &AtomicBool::new(false), |_| {}).unwrap();
    assert_eq!(
        provider
            .search(&SearchRequest {
                query: "SFX".into(),
                scope: SearchScope::Path,
                ..Default::default()
            })
            .unwrap()
            .results
            .len(),
        1
    );
    assert_eq!(run(&db, Action::Rollback).state, State::RolledBack);
}

#[cfg(feature = "scanner")]
#[test]
fn server_schema9_relinks_real_wav_roots_without_enabling_restore_or_maintenance() {
    use audiolib_core::MaintenanceAction;
    use audiolib_db::{ScanCancellation, ScanOptions, ScanService};
    use std::{fs, sync::Arc};

    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("original");
    let target = temp.path().join("relocated");
    for root in [&source, &target] {
        fs::create_dir_all(root.join("SFX")).unwrap();
    }
    let mut wav = b"RIFF".to_vec();
    wav.extend(38_u32.to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16_u32.to_le_bytes());
    wav.extend(1_u16.to_le_bytes());
    wav.extend(1_u16.to_le_bytes());
    wav.extend(48000_u32.to_le_bytes());
    wav.extend(96000_u32.to_le_bytes());
    wav.extend(2_u16.to_le_bytes());
    wav.extend(16_u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(2_u32.to_le_bytes());
    wav.extend(0_i16.to_le_bytes());
    fs::write(source.join("SFX/clip.wav"), &wav).unwrap();
    fs::write(target.join("SFX/clip.wav"), &wav).unwrap();
    let unrelated = temp.path().join("unrelated");
    fs::create_dir_all(unrelated.join("other")).unwrap();
    for name in ["ambient.wav", "neutral.wav"] {
        fs::write(unrelated.join("other").join(name), &wav).unwrap();
    }
    let db = temp.path().join("library.db");
    let provider = Arc::new(SqliteProvider::open(&db).unwrap());
    ScanService::new(provider.clone())
        .scan(
            &source,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    ScanService::new(provider.clone())
        .scan(
            &unrelated,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let original = provider
        .find_audio_by_path(&source.join("SFX/clip.wav"))
        .unwrap()
        .unwrap();
    provider
        .update_metadata(
            original.id,
            &MetadataPatch {
                notes: Some("retained after relink".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let backup = temp.path().join("schema8-backup.db");
    provider.backup_library(&backup).unwrap();
    drop(provider);
    assert_eq!(run(&db, Action::Start).state, State::Done);
    assert!(SqliteProvider::open_existing(&db).is_err());
    let server =
        SqliteProvider::open_server_existing(&db, &AtomicBool::new(false), |_| {}).unwrap();
    // Neither path has schema-9 restore/rebuild semantics. Relink admission must
    // not expand their existing writer guard as a side effect.
    assert!(server.restore_library(&backup).is_err());
    assert!(
        server
            .maintain_library(MaintenanceAction::Optimize)
            .is_err()
    );
    assert!(
        server
            .maintain_library(MaintenanceAction::RebuildSearch)
            .is_err()
    );
    assert!(
        SqliteProvider::open_read_only(&db)
            .unwrap()
            .relink_folder(&source, &target)
            .is_err()
    );
    let conn = support::connection(&db).unwrap();
    conn.execute(
        "DELETE FROM audiolib_schema_features WHERE feature='vendor_metadata'",
        [],
    )
    .unwrap();
    assert!(
        server.relink_folder(&source, &target).is_err(),
        "incomplete schema 9 must be refused"
    );
    assert_eq!(
        server.get_audio(original.id).unwrap().file_path,
        original.file_path
    );
    conn.execute(
        "INSERT INTO audiolib_schema_features VALUES('vendor_metadata',1)",
        [],
    )
    .unwrap();
    drop(conn);
    assert_eq!(server.relink_folder(&source, &target).unwrap(), 1);
    let moved = server.get_audio(original.id).unwrap();
    assert_eq!(
        moved.file_path,
        audiolib_db::display_path(&target.join("SFX/clip.wav").canonicalize().unwrap())
    );
    assert_eq!(moved.folder_id, original.folder_id);
    assert_eq!(moved.notes, "retained after relink");
    let matches = server
        .search(&SearchRequest {
            query: "SFX".into(),
            scope: SearchScope::Path,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        matches
            .results
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>(),
        vec![original.id]
    );
    assert_eq!(fs::read(source.join("SFX/clip.wav")).unwrap(), wav);
    assert_eq!(fs::read(target.join("SFX/clip.wav")).unwrap(), wav);
    let conn = Connection::open(&db).unwrap();
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        9
    );
    migration9::validate_ready(&conn).unwrap();
}
