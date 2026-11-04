// crates/audiolib-db/src/metadata_plain_tests.rs
//! Plain target storage and owned conversion of experimental compressed snapshots.
use crate::*;
use audiolib_core::{DataProvider, compatibility::LibraryAccess};

fn legacy_fixture() -> (tempfile::TempDir, PathBuf, String, String, i64) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("experimental.db");
    let provider = SqliteProvider::open(&path).unwrap();
    let namespace = provider.album_namespace();
    let raw = format!(
        " {{\n \"wav_metadata\":{{\"fields\":{{\"library\":\"Shared library\",\"keywords\":\"Metal\"}}}},\n \"number\":1.00,\"unknown\":\"{}\" }}\n",
        "exact 🦉 <XML> whitespace ".repeat(512)
    );
    let compressed = metadata_storage::encode_legacy_fixture(&raw).unwrap();
    assert!(matches!(&compressed, Value::Blob(_)));
    let conn = Connection::open(&path).unwrap();
    register_library_functions(&conn).unwrap();
    for (id, value) in [(41, Value::Text(raw.clone())), (42, compressed)] {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename,metadata_json,user_tags,user_notes,favorite)
             VALUES(?1,?2,'sound.wav',?3,'private tag','retain note',1)",
            params![id, format!("offline/{id}.wav"), value],
        )
        .unwrap();
    }
    let album = provider.get_audio(42).unwrap().album_id.unwrap();
    conn.execute_batch(
        "INSERT INTO audiolib_schema_features VALUES('metadata_storage',1);
         UPDATE audiolib_album_id_sequence SET compression=1,high_water=9001,legacy_scope='C:/prior/library.db';
         UPDATE audiolib_audio_id_sequence SET high_water=12001;",
    )
    .unwrap();
    drop(conn);
    drop(provider);
    (root, path, raw, namespace, album)
}

fn assert_plain(conn: &Connection) {
    let state: (i64, i64, i64) = conn
        .query_row(
            "SELECT (SELECT compression FROM audiolib_album_id_sequence WHERE singleton=1),
             (SELECT COUNT(*) FROM audiolib_schema_features WHERE feature='metadata_storage'),
             (SELECT COUNT(*) FROM audio_files WHERE typeof(metadata_json)<>'text')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, (0, 0, 0));
    assert!(schema::current_format_ready(conn).unwrap());
}

#[test]
fn new_library_and_metadata_patches_keep_the_plain_target() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("plain.db");
    let provider = SqliteProvider::open(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    register_library_functions(&conn).unwrap();
    let raw = format!(
        " {{\"private\":\"{}\",\"number\":1.00}} \n",
        "exact 🦉 ".repeat(512)
    );
    let value = metadata_storage::encode(&raw, true).unwrap();
    assert_eq!(value, Value::Text(raw.clone()));
    conn.execute("INSERT INTO audio_files(id,file_path,filename,metadata_json) VALUES(1,'owned.wav','owned.wav',?1)", [value]).unwrap();
    provider
        .update_metadata(
            1,
            &MetadataPatch {
                notes: Some("saved note".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_plain(&conn);
    assert_eq!(
        conn.query_row(
            "SELECT metadata_json FROM audio_files WHERE id=1",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap()
        .as_bytes(),
        raw.as_bytes()
    );
    assert_eq!(provider.get_audio(1).unwrap().notes, "saved note");
}

#[test]
fn experimental_compressed_snapshot_remains_read_only_without_source_rewrites() {
    let (_root, path, raw, namespace, album) = legacy_fixture();
    let original = fs::read(&path).unwrap();
    let source = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert!(!schema::current_format_ready(&source).unwrap());
    assert!(schema::readable_format_ready(&source).unwrap());
    drop(source);
    let snapshot = SqliteProvider::open_read_only(&path).unwrap();
    assert_eq!(snapshot.database_path(), path);
    assert_eq!(snapshot.album_namespace(), namespace);
    for id in [41, 42] {
        let file = snapshot.get_audio(id).unwrap();
        assert_eq!(file.album_id, Some(album));
        assert_eq!(file.notes, "retain note");
        assert!(file.favorite);
    }
    let reader = snapshot.reader().unwrap();
    assert_eq!(
        reader
            .query_row(
                "SELECT audiolib_metadata_json(metadata_json) FROM audio_files WHERE id=42",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap()
            .as_bytes(),
        raw.as_bytes()
    );
    drop(reader);
    assert!(matches!(
        snapshot.update_metadata(
            42,
            &MetadataPatch {
                notes: Some("refused".into()),
                ..Default::default()
            }
        ),
        Err(Error::Validation(_))
    ));
    drop(snapshot);
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn experimental_conversion_preserves_exact_metadata_identity_and_unknown_extensions() {
    let (_root, path, raw, namespace, album) = legacy_fixture();
    let conn = Connection::open(&path).unwrap();
    register_library_functions(&conn).unwrap();
    conn.execute_batch(
        "CREATE TABLE vendor_events(payload BLOB);
         INSERT INTO vendor_events(rowid,payload) VALUES(-99,x'00ff7f');
         CREATE TRIGGER vendor_metadata_audit AFTER UPDATE OF metadata_json ON audio_files BEGIN
           INSERT INTO vendor_events VALUES(x'01'); END;",
    )
    .unwrap();
    drop(conn);
    let cancel = AtomicBool::new(false);
    let before = migration::inspect(&path, LibraryAccess::Write, true, &cancel)
        .unwrap()
        .business;
    let provider = SqliteProvider::open_existing(&path).unwrap();
    assert_eq!(provider.album_namespace(), namespace);
    assert_eq!(
        provider.legacy_album_scope().as_deref(),
        Some("C:/prior/library.db")
    );
    for id in [41, 42] {
        assert_eq!(provider.get_audio(id).unwrap().album_id, Some(album));
    }
    let conn = Connection::open(&path).unwrap();
    register_library_functions(&conn).unwrap();
    assert_plain(&conn);
    for id in [41, 42] {
        let stored: String = conn
            .query_row(
                "SELECT metadata_json FROM audio_files WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored.as_bytes(), raw.as_bytes());
    }
    assert_eq!(
        conn.query_row(
            "SELECT high_water FROM audiolib_album_id_sequence",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        9001
    );
    assert_eq!(
        conn.query_row(
            "SELECT high_water FROM audiolib_audio_id_sequence",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        12001
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM vendor_events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT payload FROM vendor_events WHERE rowid=-99",
            [],
            |row| row.get::<_, Vec<u8>>(0)
        )
        .unwrap(),
        vec![0, 255, 127]
    );
    drop(conn);
    assert_eq!(
        migration::inspect(&path, LibraryAccess::Write, true, &cancel)
            .unwrap()
            .business,
        before
    );
}

#[test]
fn cancelled_experimental_conversion_can_roll_back_without_losing_original_bytes() {
    let (_root, path, raw, namespace, album) = legacy_fixture();
    let cancel = AtomicBool::new(false);
    let plan = migration::plan(&path, &cancel).unwrap();
    let mut directory = None;
    let result = migration::apply(&plan, &cancel, &mut |event| {
        directory = Some(event.directory);
        if event.phase == migration::MigrationPhase::Copying && event.completed > 0 {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    });
    assert!(matches!(result, Err(Error::Cancelled)));
    migration::rollback(&directory.unwrap(), &cancel, &mut |_| Ok(())).unwrap();
    assert_eq!(
        migration::inspect(&path, LibraryAccess::Read, true, &cancel)
            .unwrap()
            .business,
        plan.source.business
    );
    let provider = SqliteProvider::open_read_only(&path).unwrap();
    assert_eq!(provider.album_namespace(), namespace);
    assert_eq!(provider.get_audio(42).unwrap().album_id, Some(album));
    let reader = provider.reader().unwrap();
    let stored: Value = reader
        .query_row(
            "SELECT metadata_json FROM audio_files WHERE id=42",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(matches!(stored, Value::Blob(_)));
    assert_eq!(
        reader
            .query_row(
                "SELECT audiolib_metadata_json(metadata_json) FROM audio_files WHERE id=42",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap()
            .as_bytes(),
        raw.as_bytes()
    );
}

#[test]
fn malformed_experimental_metadata_blocks_conversion_without_changing_its_source() {
    let (_root, path, _, _, _) = legacy_fixture();
    let mut conn = Connection::open(&path).unwrap();
    register_library_functions(&conn).unwrap();
    let definitions = |conn: &Connection| {
        conn.prepare("SELECT sql FROM sqlite_schema WHERE type='trigger' AND name IN ('audio_files_au','audiolib_album_audio_update') ORDER BY name")
            .unwrap().query_map([], |row| row.get::<_,String>(0)).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap()
    };
    let original_triggers = definitions(&conn);
    assert_eq!(original_triggers.len(), 2);
    // Simulate bytes already damaged on disk, without asking ordinary Metadata
    // consumers to accept them. Restore every owned trigger before admission.
    let tx = conn.transaction().unwrap();
    tx.execute_batch(
        "DROP TRIGGER audio_files_au; DROP TRIGGER audiolib_album_audio_update;
        UPDATE audio_files SET metadata_json=x'00ff' WHERE id=42;",
    )
    .unwrap();
    for sql in &original_triggers {
        tx.execute_batch(sql).unwrap();
    }
    tx.commit().unwrap();
    assert_eq!(definitions(&conn), original_triggers);
    drop(conn);
    let original = fs::read(&path).unwrap();
    assert!(SqliteProvider::open_existing(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
}
