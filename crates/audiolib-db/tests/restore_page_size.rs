// crates/audiolib-db/tests/restore_page_size.rs
use audiolib_core::{DataProvider, LibraryExportFormat, MetadataPatch, SearchRequest};
use audiolib_db::{SCHEMA_VERSION, SqliteProvider};
use rusqlite::Connection;
use std::fs;

fn patch(tags: &str) -> MetadataPatch {
    MetadataPatch {
        tags: Some(tags.into()),
        ..Default::default()
    }
}

#[test]
fn official_backups_with_different_page_sizes_restore_into_wal_without_modifying_sources() {
    for page_size in [1024, 8192] {
        let temp = tempfile::tempdir().unwrap();
        let provider = SqliteProvider::open(&temp.path().join("library.db")).unwrap();
        let fixture = Connection::open(provider.database_path()).unwrap();
        audiolib_db::register_library_functions(&fixture).unwrap();
        fixture
            .execute_batch(
                "INSERT INTO audio_files(id,file_path,path_key,filename)
             VALUES(1,'owned.wav','owned.wav','owned.wav');",
            )
            .unwrap();
        drop(fixture);
        provider.update_metadata(1, &patch("saved-state")).unwrap();

        let source = temp.path().join("alternate-page-size.db");
        provider.backup_library(&source).unwrap();
        let source_conn = Connection::open(&source).unwrap();
        source_conn
            .pragma_update(None, "page_size", page_size)
            .unwrap();
        source_conn.execute_batch("VACUUM;").unwrap();
        assert_eq!(
            source_conn
                .pragma_query_value(None, "page_size", |row| row.get::<_, i64>(0))
                .unwrap(),
            page_size
        );
        drop(source_conn);
        let source_provider = SqliteProvider::open_existing(&source).unwrap();
        assert_eq!(
            source_provider.library_info().unwrap().schema_version,
            SCHEMA_VERSION
        );
        assert!(source_provider.check_library().unwrap().is_ok());
        let official_backup = temp.path().join("official-backup.db");
        source_provider.backup_library(&official_backup).unwrap();
        drop(source_provider);
        let source_bytes = fs::read(&source).unwrap();
        let backup_bytes = fs::read(&official_backup).unwrap();
        provider.update_metadata(1, &patch("later-state")).unwrap();
        // Warm an existing pooled reader before replacement, including its FTS5 cursor.
        assert_eq!(
            provider
                .search(&SearchRequest {
                    query: "later".into(),
                    ..Default::default()
                })
                .unwrap()
                .total_count,
            1
        );
        let report = provider.restore_library(&official_backup).unwrap();
        assert_eq!(report.library.schema_version, SCHEMA_VERSION);
        assert!(provider.check_library().unwrap().is_ok());
        assert_eq!(provider.get_audio(1).unwrap().tags, "saved-state");

        assert_eq!(
            provider
                .search(&SearchRequest {
                    query: "saved".into(),
                    ..Default::default()
                })
                .unwrap()
                .total_count,
            1
        );
        assert_eq!(
            provider
                .search(&SearchRequest {
                    query: "later".into(),
                    ..Default::default()
                })
                .unwrap()
                .total_count,
            0
        );
        let target = Connection::open(provider.database_path()).unwrap();
        assert_eq!(
            target
                .pragma_query_value(None, "page_size", |row| row.get::<_, i64>(0))
                .unwrap(),
            4096
        );
        assert_eq!(
            target
                .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert_eq!(fs::read(&official_backup).unwrap(), backup_bytes);
        // The live writer remains usable following page-size normalization and restore.
        provider
            .update_metadata(1, &patch("usable-after-restore"))
            .unwrap();
    }
}

#[test]
fn readonly_check_export_and_backup_do_not_create_administration_sidecars() {
    let temp = tempfile::tempdir().unwrap();
    let original = SqliteProvider::open(&temp.path().join("original.db")).unwrap();
    let source = temp.path().join("readonly.db");
    original.backup_library(&source).unwrap();
    let before = fs::read(&source).unwrap();
    let readonly = SqliteProvider::open_read_only(&source).unwrap();
    assert!(readonly.check_library().unwrap().is_ok());
    readonly
        .backup_library(&temp.path().join("copy.db"))
        .unwrap();
    readonly
        .export_library(
            &temp.path().join("export.jsonl"),
            LibraryExportFormat::JsonLines,
        )
        .unwrap();
    assert!(
        readonly
            .maintain_library(audiolib_core::MaintenanceAction::Optimize)
            .is_err()
    );
    assert!(readonly.update_metadata(1, &patch("blocked")).is_err());
    assert!(!temp.path().join("readonly.db.admin.lock").exists());
    assert!(!temp.path().join("readonly.db.scan.lock").exists());
    assert_eq!(fs::read(source).unwrap(), before);
}

#[test]
fn page_size_adaptation_refuses_to_renumber_implicit_extension_identities() {
    let temp = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&temp.path().join("current.db")).unwrap();
    let conn = Connection::open(provider.database_path()).unwrap();
    conn.execute_batch(
        "CREATE TABLE extension_rows(value TEXT);
         INSERT INTO extension_rows(rowid,value) VALUES(-20,'first'),(100,'second');",
    )
    .unwrap();
    let source = temp.path().join("other-page-size.db");
    provider.backup_library(&source).unwrap();
    let snapshot = Connection::open(&source).unwrap();
    snapshot
        .execute_batch(
            "PRAGMA page_size=8192; VACUUM;
             UPDATE extension_rows SET rowid=-20 WHERE value='first';
             UPDATE extension_rows SET rowid=100 WHERE value='second';",
        )
        .unwrap();
    drop(snapshot);
    let original = fs::read(&source).unwrap();
    let error = provider.restore_library(&source).unwrap_err();
    assert!(error.to_string().contains("identit"), "{error}");
    assert_eq!(fs::read(&source).unwrap(), original);
    let ids = conn
        .prepare("SELECT rowid FROM extension_rows ORDER BY rowid")
        .unwrap()
        .query_map([], |row| row.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(ids, [-20, 100]);
    assert_eq!(
        provider.library_info().unwrap().schema_version,
        SCHEMA_VERSION
    );
    assert!(provider.check_library().unwrap().is_ok());
}
