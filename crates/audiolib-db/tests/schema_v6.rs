// crates/audiolib-db/tests/schema_v6.rs
//! Final v6 playback retirement, compatible p0 reads and atomic owned conversion.
mod support;
use audiolib_core::{
    DataProvider, SearchRequest,
    compatibility::{LibraryAccess, MigrationDisposition},
};
use audiolib_db::{SqliteProvider, migration};
use std::sync::atomic::AtomicBool;

fn assert_retired(conn: &rusqlite::Connection) {
    assert_eq!(conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_xinfo('audio_files') WHERE name COLLATE NOCASE IN ('play_count','last_played_at')", [], |row|row.get::<_,i64>(0),
    ).unwrap(),0);
    assert_eq!(conn.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE name='play_history' COLLATE NOCASE OR name IN ('idx_play_history_audio','idx_play_history_date','idx_play_history_audio_file')", [], |row|row.get::<_,i64>(0),
    ).unwrap(),0);
    assert_eq!(
        conn.query_row(
            "SELECT version FROM audiolib_schema_features WHERE feature='play_history_retired'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
}

#[test]
fn current_schema_has_no_library_playback_storage_and_reopen_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("current.db");
    drop(SqliteProvider::open(&path).unwrap());
    let conn = support::connection(&path).unwrap();
    assert_retired(&conn);
    conn.execute(
        "INSERT INTO audio_files(id,file_path,filename) VALUES(1,'owned.wav','metal.wav')",
        [],
    )
    .unwrap();
    drop(conn);
    let provider = SqliteProvider::open_existing(&path).unwrap();
    let dto = serde_json::to_value(provider.get_audio(1).unwrap()).unwrap();
    assert!(dto.get("play_count").is_none());
    assert!(dto["attributes"].get("last_played_at").is_none());
    assert_eq!(
        provider
            .search(&SearchRequest {
                query: "metal".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        1
    );
    let before =
        migration::inspect(&path, LibraryAccess::Write, true, &AtomicBool::new(false)).unwrap();
    drop(provider);
    let mut upgrading = false;
    let provider = SqliteProvider::open_existing_with_progress(&path, |event| {
        upgrading |= !matches!(event, audiolib_db::SnapshotUpgradeProgress::Preparing);
    })
    .unwrap();
    assert!(!upgrading);
    assert_eq!(
        migration::inspect(&path, LibraryAccess::Write, true, &AtomicBool::new(false)).unwrap(),
        before
    );
    assert!(provider.check_library().unwrap().is_ok());
}

#[test]
fn owned_v5_retirement_can_be_cancelled_and_original_playback_values_rolled_back() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("legacy.db");
    let conn = support::connection(&path).unwrap();
    conn.execute_batch(include_str!("fixtures/schema-v5.sql"))
        .unwrap();
    conn.execute_batch(
        "ALTER TABLE audio_files ADD COLUMN last_played_at_note TEXT;
         INSERT INTO audio_files(id,file_path,filename,play_count,last_played_at,last_played_at_note,user_rating,user_notes)
         VALUES(1,'owned.wav','metal.wav',17,'2026-10-21T09:00:00Z','extension note',4,'retained');
         INSERT INTO play_history(audio_file_id,played_at,filename,file_path)
         VALUES(1,'2026-10-21T09:00:00Z','metal.wav','owned.wav');",
    ).unwrap();
    drop(conn);
    let cancel = AtomicBool::new(false);
    let plan = migration::plan(&path, &cancel).unwrap();
    let mut saved = None;
    assert!(matches!(
        migration::apply(&plan, &cancel, &mut |event| {
            saved = Some(event.directory);
            if event.phase == migration::MigrationPhase::Copying && event.completed > 0 {
                Err(audiolib_core::Error::Cancelled)
            } else {
                Ok(())
            }
        }),
        Err(audiolib_core::Error::Cancelled)
    ));
    let job = saved.unwrap();
    migration::rollback(&job, &cancel, &mut |_| Ok(())).unwrap();
    assert_eq!(
        migration::inspect(&path, LibraryAccess::Write, true, &cancel)
            .unwrap()
            .business,
        plan.source.business
    );
    let conn = support::connection(&path).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT play_count,last_played_at FROM audio_files WHERE id=1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        )
        .unwrap(),
        (17, "2026-10-21T09:00:00Z".into())
    );
    drop(conn);
    let job = migration::apply(
        &migration::plan(&path, &cancel).unwrap(),
        &cancel,
        &mut |_| Ok(()),
    )
    .unwrap();
    migration::verify(&job.directory, &cancel).unwrap();
    let conn = support::connection(&path).unwrap();
    assert_retired(&conn);
    assert_eq!(
        conn.query_row(
            "SELECT last_played_at_note,user_rating,user_notes FROM audio_files WHERE id=1",
            [],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?
            ))
        )
        .unwrap(),
        ("extension note".into(), 4, "retained".into())
    );
    drop(conn);
    migration::rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
    let conn = support::connection(&path).unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM play_history", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn p0_is_readable_but_owned_upgrade_removes_last_timestamp_and_rollback_restores_p0() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("p0.db");
    drop(SqliteProvider::open(&path).unwrap());
    let conn = support::connection(&path).unwrap();
    // Remove every schema-8-only object from this empty owned fixture before
    // inserting historical TEXT rows. v6 had neither album identity nor a name column.
    conn.execute_batch(
        "PRAGMA journal_mode=DELETE;
         DROP TRIGGER audiolib_album_audio_insert;
         DROP TRIGGER audiolib_album_audio_update;
         DROP TRIGGER audiolib_album_folder_delete;
         DROP TRIGGER audiolib_album_id_high_water;
         DROP INDEX idx_audio_visible_album;
         DROP INDEX idx_audio_album_members;
         ALTER TABLE audio_files DROP COLUMN album_id;
         DROP TABLE albums;
         DROP TABLE audiolib_album_id_sequence;
         DELETE FROM audiolib_schema_features WHERE feature IN ('album_entities','metadata_storage');
         DELETE FROM library_meta WHERE key='metadata_storage_policy';
         PRAGMA user_version=6;
         ALTER TABLE audio_files ADD COLUMN Last_Played_At TEXT;
         ALTER TABLE audio_files ADD COLUMN last_played_at_note TEXT;
         UPDATE audiolib_schema_features SET version=1 WHERE feature='play_history_retired';
         INSERT INTO audio_files(id,file_path,filename,last_played_at,last_played_at_note,user_notes)
         VALUES(1,'owned.wav','metal.wav','2026-10-21T10:00:00Z','extension retained','retained');",
    ).unwrap();
    drop(conn);
    let cancel = AtomicBool::new(false);
    let before = migration::inspect(&path, LibraryAccess::Read, true, &cancel).unwrap();
    assert!(before.evidence.search_ready);
    assert_eq!(
        before.decision.disposition,
        Some(MigrationDisposition::Compatible)
    );
    let original = std::fs::read(&path).unwrap();
    let readonly = SqliteProvider::open_read_only(&path).unwrap();
    assert_ne!(readonly.database_path(), path.as_path());
    assert_eq!(
        readonly.library_info().unwrap().schema_version,
        audiolib_db::SCHEMA_VERSION
    );
    assert!(SqliteProvider::has_current_album_state(readonly.database_path()).unwrap());
    let dto = serde_json::to_value(readonly.get_audio(1).unwrap()).unwrap();
    assert!(dto.get("play_count").is_none());
    assert!(dto["attributes"].get("last_played_at").is_none());
    assert_eq!(dto["notes"], "retained");
    assert!(matches!(
        readonly.update_metadata(
            1,
            &audiolib_core::MetadataPatch {
                notes: Some("must not write".into()),
                ..Default::default()
            }
        ),
        Err(audiolib_core::Error::Validation(_))
    ));
    drop(readonly);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(
        support::connection(&path)
            .unwrap()
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        6
    );
    let plan = migration::plan(&path, &cancel).unwrap();
    assert_eq!(
        plan.source.decision.disposition,
        Some(MigrationDisposition::Migratable)
    );
    let job = migration::apply(&plan, &cancel, &mut |_| Ok(())).unwrap();
    migration::verify(&job.directory, &cancel).unwrap();
    let conn = support::connection(&path).unwrap();
    assert_retired(&conn);
    assert_eq!(
        conn.query_row(
            "SELECT last_played_at_note FROM audio_files WHERE id=1",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "extension retained"
    );
    drop(conn);
    migration::rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
    assert_eq!(
        migration::inspect(&path, LibraryAccess::Read, true, &cancel).unwrap(),
        before
    );
    let conn = support::connection(&path).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT last_played_at FROM audio_files WHERE id=1",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "2026-10-21T10:00:00Z"
    );
    conn.execute(
        "UPDATE audiolib_schema_features SET version=3 WHERE feature='play_history_retired'",
        [],
    )
    .unwrap();
    drop(conn);
    let bytes = std::fs::read(&path).unwrap();
    assert!(migration::plan(&path, &cancel).is_err());
    assert!(SqliteProvider::open_read_only(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}
