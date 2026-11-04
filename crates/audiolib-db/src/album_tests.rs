// crates/audiolib-db/src/album_tests.rs
//! Album upgrades, old snapshots and scan preservation use only owned fixtures.
use crate::*;
use audiolib_core::{AlbumKind, DataProvider, SearchFilters};

#[test]
fn strict_schema7_text_metadata_upgrades_without_changing_its_storage_constraint() {
    let (_root, path) = previous_fixture();
    let conn = Connection::open(&path).unwrap();
    register_library_functions(&conn).unwrap();
    let raw = format!(
        " {{\"wav_metadata\":{{\"fields\":{{\"library\":\"Strict library\"}}}},\"private\":\"{}\"}} ",
        "retained-".repeat(1024)
    );
    assert!(matches!(
        metadata_storage::encode(&raw, true).unwrap(),
        Value::Text(stored) if stored == raw
    ));
    conn.execute("UPDATE audio_files SET metadata_json=?1 WHERE id=2", [&raw])
        .unwrap();
    let create: String = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name='audio_files'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let objects=conn.prepare("SELECT sql FROM sqlite_schema WHERE tbl_name='audio_files' AND type IN ('index','trigger') AND sql IS NOT NULL ORDER BY type,name").unwrap().query_map([],|row|row.get::<_,String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA legacy_alter_table=ON; BEGIN; ALTER TABLE audio_files RENAME TO strict_source;").unwrap();
    conn.execute(&format!("{create} STRICT"), []).unwrap();
    conn.execute_batch(
        "INSERT INTO audio_files SELECT * FROM strict_source; DROP TABLE strict_source;",
    )
    .unwrap();
    for sql in objects {
        conn.execute(&sql, []).unwrap();
    }
    conn.execute_batch("COMMIT; PRAGMA legacy_alter_table=OFF; PRAGMA foreign_keys=ON;")
        .unwrap();
    assert!(!albums::dependency_safe(&conn).unwrap());
    drop(conn);
    let before = fs::read(&path).unwrap();
    let provider = SqliteProvider::open_read_only(&path).unwrap();
    let reader = provider.reader().unwrap();
    let strict: bool = reader
        .query_row(
            "SELECT strict FROM pragma_table_list WHERE schema='main' AND name='audio_files'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let (kind, stored): (String, String) = reader
        .query_row(
            "SELECT typeof(metadata_json),metadata_json FROM audio_files WHERE id=2",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(strict);
    assert_eq!(kind, "text");
    assert_eq!(stored, raw);
    assert_eq!(
        reader
            .query_row(
                "SELECT compression FROM audiolib_album_id_sequence",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn schema7_metadata_autoindex_constraints_keep_text_and_original_collation() {
    for nocase in [false, true] {
        let (_root, path) = previous_fixture();
        let conn = Connection::open(&path).unwrap();
        register_library_functions(&conn).unwrap();
        let raw = format!(
            " {{\"wav_metadata\":{{\"fields\":{{\"library\":\"Collation Library\"}}}},\"private\":\"{}\"}} \n",
            "retain original whitespace ".repeat(512)
        );
        assert!(matches!(
            metadata_storage::encode(&raw, true).unwrap(),
            Value::Text(stored) if stored == raw
        ));
        conn.execute("UPDATE audio_files SET metadata_json=?1 WHERE id=2", [&raw])
            .unwrap();
        let create: String = conn
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name='audio_files'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let declaration = if nocase {
            "metadata_json TEXT COLLATE NOCASE UNIQUE"
        } else {
            "metadata_json TEXT UNIQUE"
        };
        let constrained = create.replacen("metadata_json TEXT", declaration, 1);
        assert_ne!(create, constrained);
        let objects = conn.prepare("SELECT sql FROM sqlite_schema WHERE tbl_name='audio_files' AND type IN ('index','trigger') AND sql IS NOT NULL ORDER BY type,name").unwrap()
            .query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA legacy_alter_table=ON; BEGIN; ALTER TABLE audio_files RENAME TO constrained_source;").unwrap();
        conn.execute(&constrained, []).unwrap();
        conn.execute_batch("INSERT INTO audio_files SELECT * FROM constrained_source; DROP TABLE constrained_source;").unwrap();
        for sql in objects {
            conn.execute(&sql, []).unwrap();
        }
        conn.execute_batch("COMMIT; PRAGMA legacy_alter_table=OFF; PRAGMA foreign_keys=ON;")
            .unwrap();
        let autoindex_sql: Option<String> = conn
            .query_row(
                "SELECT s.sql FROM pragma_index_list('audio_files') i
             JOIN pragma_index_xinfo(i.name) c JOIN sqlite_schema s ON s.name=i.name
             WHERE i.\"unique\"=1 AND c.name='metadata_json'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(autoindex_sql.is_none());
        assert!(!albums::dependency_safe(&conn).unwrap());
        drop(conn);
        let source_bytes = fs::read(&path).unwrap();
        let provider = SqliteProvider::open_read_only(&path).unwrap();
        let target = Connection::open(provider.database_path()).unwrap();
        register_library_functions(&target).unwrap();
        let (storage, stored, policy): (String, String, i64) = target
            .query_row(
                "SELECT typeof(metadata_json),metadata_json,
             (SELECT compression FROM audiolib_album_id_sequence WHERE singleton=1)
             FROM audio_files WHERE id=2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(storage, "text");
        assert_eq!(stored.as_bytes(), raw.as_bytes());
        assert_eq!(policy, 0);
        let duplicate = if nocase {
            raw.replace("Collation Library", "collation library")
        } else {
            raw.clone()
        };
        let equivalent: bool = target
            .query_row(
                "SELECT metadata_json=?1 FROM audio_files WHERE id=2",
                [&duplicate],
                |row| row.get(0),
            )
            .unwrap();
        assert!(equivalent);
        let error = target.execute(
            "INSERT INTO audio_files(id,file_path,filename,metadata_json) VALUES(3,'duplicate.wav','duplicate.wav',?1)",
            [&duplicate],
        ).unwrap_err();
        assert_eq!(
            error.sqlite_error().unwrap().code,
            rusqlite::ErrorCode::ConstraintViolation
        );
        assert_eq!(provider.get_audio(2).unwrap().album, "Collation Library");
        assert_eq!(fs::read(&path).unwrap(), source_bytes);
    }
}

#[test]
fn semantic_album_lookup_uses_metadata_keys_and_live_indexed_folder_paths() {
    let root = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&root.path().join("lookup.db")).unwrap();
    provider.writer().unwrap().execute_batch("INSERT INTO folders(id,folder_path,folder_name) VALUES(1,'C:/owned/Shared','Shared'); INSERT INTO folder_closure VALUES(1,1,0); INSERT INTO audio_files(id,file_path,filename,folder_id) VALUES(1,'C:/owned/Shared/one.wav','one.wav',1); INSERT INTO audio_files(id,file_path,filename) VALUES(3,'unassigned.wav','unassigned.wav');").unwrap();
    provider.writer().unwrap().execute("INSERT INTO audio_files(id,file_path,filename,metadata_json) VALUES(2,'metadata.wav','metadata.wav',?1)",[r#"{"wav_metadata":{"fields":{"library":"Shared"}}}"#]).unwrap();
    let metadata = provider
        .find_album_by_semantic(AlbumKind::Metadata, Some("Shared"), None)
        .unwrap()
        .unwrap();
    let folder = provider
        .find_album_by_semantic(AlbumKind::Folder, None, Some(r"c:\owned\Shared"))
        .unwrap()
        .unwrap();
    let unassigned = provider
        .find_album_by_semantic(AlbumKind::Folder, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(
        metadata.id,
        provider.get_audio(2).unwrap().album_id.unwrap()
    );
    assert_eq!(folder.id, provider.get_audio(1).unwrap().album_id.unwrap());
    assert_eq!(
        unassigned.id,
        provider.get_audio(3).unwrap().album_id.unwrap()
    );
    assert!(
        provider
            .find_album_by_semantic(AlbumKind::Metadata, Some("shared"), None)
            .unwrap()
            .is_none()
    );
    provider.remove_folder(1).unwrap();
    assert!(
        provider
            .find_album_by_semantic(AlbumKind::Folder, None, Some("C:/owned/Shared"))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        provider
            .find_album_by_semantic(AlbumKind::Folder, None, None)
            .unwrap()
            .unwrap()
            .id,
        unassigned.id
    );
}

#[test]
fn declared_name_filters_keep_empty_library_distinct_from_folder_display_names() {
    let root = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&root.path().join("names.db")).unwrap();
    provider.writer().unwrap().execute_batch("INSERT INTO folders(id,folder_path,folder_name) VALUES(1,'owned/Folder','Folder'); INSERT INTO folder_closure VALUES(1,1,0); INSERT INTO audio_files(id,file_path,filename,folder_id) VALUES(1,'owned/Folder/one.wav','one.wav',1);").unwrap();
    let request = SearchRequest {
        filters: SearchFilters {
            album: Some(String::new()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(provider.search(&request).unwrap().total_count, 1);
    let named = SearchRequest {
        filters: SearchFilters {
            album: Some("Folder".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(provider.search(&named).unwrap().total_count, 0);
    let file = provider.get_audio(1).unwrap();
    assert!(file.album.is_empty());
    assert_eq!(file.album_entity.unwrap().name, "Folder");
}

#[test]
fn explicit_import_is_a_new_album_namespace_and_does_not_inherit_legacy_preferences() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("original.db");
    let imported = root.path().join("imported.db");
    let backup = root.path().join("backup.db");
    let provider = SqliteProvider::open(&source).unwrap();
    provider.writer().unwrap().execute("INSERT INTO audio_files(id,file_path,filename,metadata_json) VALUES(1,'one.wav','one.wav',?1)",[r#"{"wav_metadata":{"fields":{"library":"Shared"}},"private":"retained"}"#]).unwrap();
    provider.writer().unwrap().execute("UPDATE audiolib_album_id_sequence SET legacy_scope='owned-old-source.db' WHERE singleton=1",[]).unwrap();
    let namespace = provider.album_namespace();
    let album = provider.get_audio(1).unwrap().album_id;
    provider.backup_library(&backup).unwrap();
    drop(provider);
    let before = fs::read(&source).unwrap();
    import_legacy(&source, &imported).unwrap();
    let target = SqliteProvider::open_existing(&imported).unwrap();
    assert_ne!(target.album_namespace(), namespace);
    assert_eq!(target.legacy_album_scope(), None);
    assert_eq!(target.get_audio(1).unwrap().album_id, album);
    assert_eq!(
        target.get_audio(1).unwrap().attributes["private"],
        "retained"
    );
    assert_eq!(fs::read(&source).unwrap(), before);
    target.restore_library(&backup).unwrap();
    assert_eq!(target.album_namespace(), namespace);
    assert_eq!(
        target.legacy_album_scope().as_deref(),
        Some("owned-old-source.db")
    );
}

#[test]
fn only_an_explicit_local_upgrade_captures_the_old_database_preference_scope() {
    let (root, path) = previous_fixture();
    let data = root.path().join("client-data");
    fs::create_dir_all(&data).unwrap();
    let provider = SqliteProvider::open_existing_local_with_progress_cancellable(
        &path,
        &data,
        0,
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    assert_eq!(
        provider.legacy_album_scope(),
        Some(path.canonicalize().unwrap().to_string_lossy().into_owned())
    );
    let namespace = provider.album_namespace();
    drop(provider);
    let reopened = SqliteProvider::open_existing(&path).unwrap();
    assert_eq!(reopened.album_namespace(), namespace);
    assert!(reopened.legacy_album_scope().is_some());
    let (_other, old_backup) = previous_fixture();
    reopened.restore_library(&old_backup).unwrap();
    assert_eq!(reopened.legacy_album_scope(), None);
}

pub(crate) fn previous_fixture() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("previous.db");
    drop(SqliteProvider::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    register_library_functions(&conn).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=DELETE;
         DROP TRIGGER IF EXISTS audiolib_album_audio_insert;
         DROP TRIGGER IF EXISTS audiolib_album_audio_update;
         DROP TRIGGER IF EXISTS audiolib_album_folder_delete;
         DROP TRIGGER IF EXISTS audiolib_album_id_high_water;
         DROP INDEX idx_audio_visible_album;
         DROP INDEX IF EXISTS idx_audio_album_members;
         ALTER TABLE audio_files DROP COLUMN album_id;
         DROP TABLE albums;
         DROP TABLE audiolib_album_id_sequence;
         ALTER TABLE audio_files ADD COLUMN album TEXT NOT NULL DEFAULT '';
         CREATE INDEX idx_audio_visible_album ON audio_files(album,id) WHERE missing=0;
         DELETE FROM audiolib_schema_features WHERE feature IN ('album_entities','metadata_storage');
         DELETE FROM library_meta WHERE key='metadata_storage_policy';
         INSERT INTO audio_files(id,file_path,path_key,filename,metadata_json,user_notes,modified_at,album)
         VALUES(-9223372036854775808,'lowest.wav','lowest.wav','metal.wav','{\"wav_metadata\":{\"fields\":{\"library\":\"Library A\"}},\"private\":42}','keep note','2020-01-02 03:04:05','Library A'),
               (2,'two.wav','two.wav','water.wav','{\"wav_metadata\":{\"fields\":{\"library\":\"Library B\"}}}','second note','2020-01-02 03:04:05','Library B');
         PRAGMA user_version=7;",
    ).unwrap();
    drop(conn);
    (root, path)
}

fn facet_counts(provider: &SqliteProvider, request: &SearchRequest) -> Vec<(String, u64)> {
    provider
        .album_facets(request)
        .unwrap()
        .into_iter()
        .map(|facet| (facet.album.name, facet.matching_count))
        .collect()
}

fn insert_current(conn: &Connection, id: i64, path: &str, library: &str) {
    let metadata = serde_json::json!({"wav_metadata":{"fields":{"library":library}}});
    conn.execute(
        "INSERT INTO audio_files(id,file_path,path_key,filename,metadata_json,modified_at)
         VALUES(?1,?2,?2,?3,?4,'2020-01-02 03:04:05')",
        params![
            id,
            path,
            Path::new(path).file_name().unwrap().to_string_lossy(),
            metadata.to_string()
        ],
    )
    .unwrap();
}

fn insert_folder_member(conn: &Connection, id: i64, folder: i64, library: &str) {
    insert_current(conn, id, &format!("folder-{folder}/{id}.wav"), library);
    conn.execute(
        "UPDATE audio_files SET folder_id=?1 WHERE id=?2",
        params![folder, id],
    )
    .unwrap();
}

fn insert_unassigned_member(conn: &Connection, id: i64, path: &str, library: &str) {
    insert_current(conn, id, path, library);
    conn.execute(
        "UPDATE audio_files SET path_key=?1 WHERE id=?2",
        params![path_key(path), id],
    )
    .unwrap();
}

#[test]
fn source_parent_identities_preserve_portable_path_boundaries() {
    for (path, expected) in [
        ("relative.wav", ""),
        (r"literal\backslash.wav", ""),
        ("relative/one.wav", "relative"),
        ("/one.wav", "/"),
        ("/FX/%_[x]/one.wav", "/FX/%_[x]"),
        (r"/FX/Back\Slash/one.wav", r"/FX/Back\Slash"),
        (r"/FX/Trailing\/one.wav", "/FX/Trailing\\"),
        (r"C:\FX\Designed\one.wav", "c:/FX/Designed"),
        (r"\\?\C:\FX\Designed\one.wav", "c:/FX/Designed"),
        (r"C:\one.wav", "c:"),
        (r"\\NAS\SHARE\Designed\one.wav", "//nas/share/Designed"),
        (
            r"\\?\UNC\NAS\SHARE\Designed\one.wav",
            "//nas/share/Designed",
        ),
    ] {
        assert_eq!(source_parent_path_identity(path), expected, "{path}");
        assert_eq!(normalize_source_path_identity(expected), expected);
    }
    assert_eq!(
        normalize_source_path_identity(r"C:\FX\Designed\"),
        "c:/FX/Designed"
    );
    assert_eq!(
        normalize_source_path_identity(r"\\NAS\SHARE\Designed\"),
        "//nas/share/Designed"
    );
    assert_eq!(normalize_source_path_identity("/"), "/");
    assert_eq!(normalize_source_path_identity("C:/"), "c:");
    assert_ne!(
        normalize_source_path_identity("/FX/Designed"),
        normalize_source_path_identity("/FX/designed")
    );
    assert_ne!(
        normalize_source_path_identity(r"/FX/Back\Slash"),
        normalize_source_path_identity("/FX/Back/Slash")
    );
}

#[test]
fn album_directory_functions_survive_writable_readonly_and_reused_pool_connections() {
    let root = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&root.path().join("connection-functions.db")).unwrap();
    let conn = Connection::open(provider.database_path()).unwrap();
    register_library_functions(&conn).unwrap();
    insert_unassigned_member(&conn, 1, "/owned/%_[set]/one.wav", "Connection library");
    conn.execute("UPDATE audio_files SET path_key=NULL WHERE id=1", [])
        .unwrap();
    let album = provider.get_audio(1).unwrap().album_id.unwrap();
    drop(conn);
    let snapshot = root.path().join("readonly-connection-functions.db");
    provider.backup_library(&snapshot).unwrap();
    let before = fs::read(&snapshot).unwrap();
    let modified = fs::metadata(&snapshot).unwrap().modified().unwrap();
    let readonly = SqliteProvider::open_read_only(&snapshot).unwrap();
    for current in [&provider, &readonly] {
        let reference = current.writer_unlocked().unwrap();
        let identity: String = reference
            .query_row(
                "SELECT audiolib_path_identity(?1)",
                [r"C:\FX\Designed\one.wav"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(identity, "c:/FX/Designed/one.wav");
        drop(reference);
        // Keeping both leases alive requires two independent read connections.
        let first = current.reader().unwrap();
        let second = current.reader().unwrap();
        for reader in [&first, &second] {
            let identity: String = reader
                .query_row(
                    "SELECT audiolib_path_identity(?1)",
                    [r"/FX/Back\Slash/one.wav"],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(identity, r"/FX/Back\Slash/one.wav");
        }
        drop(first);
        drop(second);
        assert_eq!(current.read_pool.lock().unwrap().len(), 2);
        for _ in 0..2 {
            let members = current
                .album_members_in_unassigned_directory(
                    album,
                    "/owned/%_[set]",
                    64,
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap();
            assert_eq!(
                members.iter().map(|file| file.id).collect::<Vec<_>>(),
                vec![1]
            );
        }
    }
    drop(readonly);
    assert_eq!(fs::read(&snapshot).unwrap(), before);
    assert_eq!(
        fs::metadata(&snapshot).unwrap().modified().unwrap(),
        modified
    );
}

#[test]
fn unassigned_album_directory_candidates_filter_before_limit_without_cross_directory_fallback() {
    let root = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&root.path().join("unassigned-members.db")).unwrap();
    let mut conn = Connection::open(provider.database_path()).unwrap();
    register_library_functions(&conn).unwrap();
    let tx = conn.transaction().unwrap();
    for id in 1..=80 {
        insert_unassigned_member(
            &tx,
            id,
            &format!("/owned/CK/{id}.wav"),
            "Unassigned shared album",
        );
    }
    for id in 200..270 {
        insert_unassigned_member(
            &tx,
            id,
            &format!("/owned/Designed/{id}.wav"),
            "Unassigned shared album",
        );
    }
    for (id, path, library) in [
        (150, "/owned/Designed/other.wav", "Other album"),
        (
            151,
            "/owned/Designed/child/nested.wav",
            "Unassigned shared album",
        ),
        (
            152,
            "/owned/Designed2/sibling.wav",
            "Unassigned shared album",
        ),
        (
            153,
            "/owned/designed/different-case.wav",
            "Unassigned shared album",
        ),
    ] {
        insert_unassigned_member(&tx, id, path, library);
    }
    tx.execute_batch("INSERT INTO folders(id,folder_path,folder_name) VALUES(-7,'/owned/Designed','Designed'); INSERT INTO folder_closure VALUES(-7,-7,0);").unwrap();
    insert_unassigned_member(
        &tx,
        154,
        "/owned/Designed/assigned.wav",
        "Unassigned shared album",
    );
    tx.execute("UPDATE audio_files SET folder_id=-7 WHERE id=154", [])
        .unwrap();
    tx.execute("UPDATE audio_files SET missing=1 WHERE id=200", [])
        .unwrap();
    tx.commit().unwrap();
    let album = provider.get_audio(201).unwrap().album_id.unwrap();
    let data: &dyn DataProvider = &provider;
    let members = data
        .album_members_in_unassigned_directory(
            album,
            "/owned/Designed/",
            64,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert_eq!(
        members.iter().map(|file| file.id).collect::<Vec<_>>(),
        (201..=264).collect::<Vec<_>>()
    );
    assert!(
        members
            .iter()
            .all(|file| file.folder_id.is_none() && file.album_id == Some(album) && !file.missing)
    );
    assert!(
        data.album_members_in_unassigned_directory(
            album,
            "/owned/Empty",
            64,
            Arc::new(AtomicBool::new(false))
        )
        .unwrap()
        .is_empty()
    );
    let sql = albums::unassigned_directory_members_sql(&conn, "/owned/Designed").unwrap();
    let plan = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(
            params![
                "/owned/designed/",
                "/owned/designed0",
                album,
                "/owned/Designed/",
                64
            ],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|detail| detail.contains("idx_audio_files_path_key")
                && detail.contains("path_key>?")),
        "{plan:?}"
    );

    let source = root.path().join("NoFolder").join("retained.wav");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, b"owned unassigned source remains untouched").unwrap();
    insert_unassigned_member(
        &conn,
        300,
        &display_path(&source),
        "Unassigned shared album",
    );
    let bytes = fs::read(&source).unwrap();
    let modified = fs::metadata(&source).unwrap().modified().unwrap();
    let directory = source_parent_path_identity(&display_path(&source));
    assert_eq!(
        data.album_members_in_unassigned_directory(
            album,
            &directory,
            64,
            Arc::new(AtomicBool::new(false))
        )
        .unwrap()
        .iter()
        .map(|file| file.id)
        .collect::<Vec<_>>(),
        vec![300]
    );
    assert_eq!(fs::read(&source).unwrap(), bytes);
    assert_eq!(fs::metadata(&source).unwrap().modified().unwrap(), modified);
}

#[test]
fn unassigned_album_directory_candidates_preserve_literals_and_source_casing() {
    let root = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&root.path().join("unassigned-paths.db")).unwrap();
    let conn = Connection::open(provider.database_path()).unwrap();
    register_library_functions(&conn).unwrap();
    for (id, path) in [
        (10, "/FX/%_[x]/one.wav"),
        (11, "/FX/%_[x]/child/nested.wav"),
        (12, "/FX/anythingx/other.wav"),
        (20, r"/FX/Back\Slash/one.wav"),
        (21, "/FX/Back/Slash/other.wav"),
        (22, r"/FX/Trailing\/one.wav"),
        (30, r"C:\FX\Designed\one.wav"),
        (31, "c:/FX/Designed/two.wav"),
        (32, "c:/FX/designed/other.wav"),
        (33, r"\\?\C:\FX\Designed\three.wav"),
        (40, r"\\nas\share\Designed\one.wav"),
        (41, "//NAS/SHARE/Designed/two.wav"),
        (42, r"\\?\UNC\NAS\Share\Designed\three.wav"),
        (43, "//nas/share/designed/other.wav"),
        (50, "relative.wav"),
        (51, r"literal\backslash.wav"),
        (52, "relative/one.wav"),
        (60, "/root-file.wav"),
        (61, r"C:\root-file.wav"),
    ] {
        insert_unassigned_member(&conn, id, path, "Portable shared album");
    }
    let album = provider.get_audio(10).unwrap().album_id.unwrap();
    for null_keys in [false, true] {
        if null_keys {
            conn.execute("UPDATE audio_files SET path_key=NULL", [])
                .unwrap();
        }
        for (directory, expected) in [
            ("/FX/%_[x]", vec![10]),
            (r"/FX/Back\Slash", vec![20]),
            ("/FX/Trailing\\", vec![22]),
            (r"C:\FX\Designed\", vec![30, 31, 33]),
            (r"\\NAS\SHARE\Designed\", vec![40, 41, 42]),
            ("", vec![50, 51]),
            ("relative", vec![52]),
            ("/", vec![60]),
            ("C:/", vec![61]),
        ] {
            let members = provider
                .album_members_in_unassigned_directory(
                    album,
                    directory,
                    64,
                    Arc::new(AtomicBool::new(false)),
                )
                .unwrap();
            assert_eq!(
                members.iter().map(|file| file.id).collect::<Vec<_>>(),
                expected,
                "{directory:?}; null_keys={null_keys}"
            );
        }
    }
    for directory in ["x".repeat(4097), "界".repeat(1366)] {
        assert!(matches!(
            provider.album_members_in_unassigned_directory(
                album,
                &directory,
                64,
                Arc::new(AtomicBool::new(false))
            ),
            Err(Error::Validation(_))
        ));
    }
    assert!(
        provider
            .album_members_in_unassigned_directory(
                album,
                &"x".repeat(4096),
                64,
                Arc::new(AtomicBool::new(false))
            )
            .unwrap()
            .is_empty()
    );
    for (invalid_album, invalid_limit) in [(0, 64), (-1, 64), (album, 0), (album, 65)] {
        assert!(matches!(
            provider.album_members_in_unassigned_directory(
                invalid_album,
                "",
                invalid_limit,
                Arc::new(AtomicBool::new(false))
            ),
            Err(Error::Validation(_))
        ));
    }
    assert!(matches!(
        provider.album_members_in_unassigned_directory(
            album,
            "",
            64,
            Arc::new(AtomicBool::new(true))
        ),
        Err(Error::Cancelled)
    ));
    assert!(matches!(
        provider.album_members_in_unassigned_directory(
            album,
            "bad\0directory",
            64,
            Arc::new(AtomicBool::new(false))
        ),
        Err(Error::Validation(_))
    ));
}

#[test]
fn unassigned_album_directory_candidates_merge_legal_null_path_keys_before_the_limit() {
    fn insert_without_key(conn: &Connection, id: i64, path: &str, library: &str) {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename,metadata_json) VALUES(?1,?2,?2,?3)",
            params![
                id,
                path,
                serde_json::json!({"wav_metadata":{"fields":{"library":library}}}).to_string()
            ],
        )
        .unwrap();
        assert!(
            conn.query_row(
                "SELECT path_key IS NULL FROM audio_files WHERE id=?1",
                [id],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
        );
    }

    let root = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&root.path().join("nullable-path-keys.db")).unwrap();
    let mut conn = Connection::open(provider.database_path()).unwrap();
    register_library_functions(&conn).unwrap();
    let tx = conn.transaction().unwrap();
    for id in 1..=80 {
        insert_without_key(
            &tx,
            id,
            &format!("/owned/CK/{id}.wav"),
            "Nullable shared album",
        );
    }
    for id in 200..340 {
        let path = format!("/owned/Designed/{id}.wav");
        if id % 2 == 0 {
            insert_without_key(&tx, id, &path, "Nullable shared album");
        } else {
            insert_unassigned_member(&tx, id, &path, "Nullable shared album");
        }
    }
    for (id, path, library) in [
        (190, "/owned/Designed/other-album.wav", "Other album"),
        (
            191,
            "/owned/Designed/child/nested.wav",
            "Nullable shared album",
        ),
        (192, "/owned/Designed2/sibling.wav", "Nullable shared album"),
        (
            193,
            "/owned/designed/different-case.wav",
            "Nullable shared album",
        ),
    ] {
        insert_without_key(&tx, id, path, library);
    }
    tx.execute_batch("INSERT INTO folders(id,folder_path,folder_name) VALUES(-7,'/owned/Designed','Designed'); INSERT INTO folder_closure VALUES(-7,-7,0);").unwrap();
    insert_without_key(
        &tx,
        194,
        "/owned/Designed/assigned.wav",
        "Nullable shared album",
    );
    tx.execute("UPDATE audio_files SET folder_id=-7 WHERE id=194", [])
        .unwrap();
    tx.execute("UPDATE audio_files SET missing=1 WHERE id=200", [])
        .unwrap();
    tx.commit().unwrap();
    let album = provider.get_audio(201).unwrap().album_id.unwrap();
    let snapshot = root.path().join("readonly-nullable-path-keys.db");
    provider.backup_library(&snapshot).unwrap();
    let before = fs::read(&snapshot).unwrap();
    let modified = fs::metadata(&snapshot).unwrap().modified().unwrap();
    let readonly = SqliteProvider::open_read_only(&snapshot).unwrap();
    let members = readonly
        .album_members_in_unassigned_directory(
            album,
            "/owned/Designed",
            64,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert_eq!(
        members.iter().map(|file| file.id).collect::<Vec<_>>(),
        (201..=264).collect::<Vec<_>>()
    );
    assert!(
        members
            .iter()
            .all(|file| file.folder_id.is_none() && file.album_id == Some(album) && !file.missing)
    );
    assert!(
        readonly
            .album_members_in_unassigned_directory(
                album,
                "/owned/Empty",
                64,
                Arc::new(AtomicBool::new(false))
            )
            .unwrap()
            .is_empty()
    );
    let source = readonly.reader().unwrap();
    assert_eq!(source.query_row("SELECT COUNT(*) FROM audio_files WHERE path_key IS NULL AND id BETWEEN 200 AND 339", [], |row| row.get::<_, i64>(0)).unwrap(), 70);
    let sql = albums::unassigned_directory_members_sql(&source, "/owned/Designed").unwrap();
    let plan = source
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(
            params![
                "/owned/designed/",
                "/owned/designed0",
                album,
                "/owned/Designed/",
                64
            ],
            |row| row.get::<_, String>(3),
        )
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|detail| detail.contains("idx_audio_files_path_key")
                && detail.contains("path_key>?")),
        "{plan:?}"
    );
    assert!(
        plan.iter()
            .any(|detail| detail.contains("idx_audio_files_folder")
                && detail.contains("folder_id=?")),
        "{plan:?}"
    );
    drop(source);
    drop(readonly);
    assert_eq!(fs::read(&snapshot).unwrap(), before);
    assert_eq!(
        fs::metadata(&snapshot).unwrap().modified().unwrap(),
        modified
    );
}

#[test]
fn shared_album_folder_candidates_filter_before_limit_and_preserve_readonly_sources() {
    let root = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&root.path().join("folder-members.db")).unwrap();
    let mut conn = Connection::open(provider.database_path()).unwrap();
    register_library_functions(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO folders(id,folder_path,folder_name,parent_id) VALUES
           (11,'owned/CK','CK',NULL),(-7,'owned/Designed','Designed',NULL),
           (0,'owned/Empty','Empty',NULL),
           (-9223372036854775808,'owned/Designed/child','child',-7);
         INSERT INTO folder_closure(ancestor_id,descendant_id,depth) VALUES
           (11,11,0),(-7,-7,0),(0,0,0),
           (-9223372036854775808,-9223372036854775808,0),
           (-7,-9223372036854775808,1);",
    )
    .unwrap();
    let tx = conn.transaction().unwrap();
    for id in 1..=80 {
        insert_folder_member(&tx, id, 11, "Shared Alien Library");
    }
    for id in 200..270 {
        insert_folder_member(&tx, id, -7, "Shared Alien Library");
    }
    insert_folder_member(&tx, 150, -7, "Other library in the same directory");
    insert_current(&tx, 151, "unassigned.wav", "Shared Alien Library");
    insert_folder_member(&tx, 190, i64::MIN, "Shared Alien Library");
    tx.execute("UPDATE audio_files SET missing=1 WHERE id=200", [])
        .unwrap();
    tx.commit().unwrap();
    let album = provider.get_audio(201).unwrap().album_id.unwrap();
    assert_eq!(provider.get_audio(1).unwrap().album_id, Some(album));
    assert_eq!(provider.get_audio(151).unwrap().folder_id, None);
    assert_eq!(
        provider
            .album_members(album, 64, Arc::new(AtomicBool::new(false)))
            .unwrap()
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>(),
        (1..=64).collect::<Vec<_>>()
    );
    let source = root.path().join("must-not-open-or-change.wav");
    fs::write(&source, b"owned source WAV bytes stay untouched").unwrap();
    conn.execute(
        "UPDATE audio_files SET file_path=?1,path_key=?2 WHERE id=201",
        params![display_path(&source), path_key(&display_path(&source))],
    )
    .unwrap();
    let source_bytes = fs::read(&source).unwrap();
    let source_modified = fs::metadata(&source).unwrap().modified().unwrap();
    let snapshot = root.path().join("readonly-folder-members.db");
    provider.backup_library(&snapshot).unwrap();
    let snapshot_bytes = fs::read(&snapshot).unwrap();
    let snapshot_modified = fs::metadata(&snapshot).unwrap().modified().unwrap();
    let readonly = SqliteProvider::open_read_only(&snapshot).unwrap();
    let data: &dyn DataProvider = &readonly;
    assert_eq!(data.get_folder(-7).unwrap().path, "owned/Designed");
    assert_eq!(data.get_folder(0).unwrap().path, "owned/Empty");
    assert_eq!(data.get_folder(i64::MIN).unwrap().parent_id, Some(-7));
    let members = data
        .album_members_in_folder(album, -7, 64, Arc::new(AtomicBool::new(false)))
        .unwrap();
    assert_eq!(
        members.iter().map(|file| file.id).collect::<Vec<_>>(),
        (201..=264).collect::<Vec<_>>()
    );
    assert!(
        members.iter().all(|file| file.folder_id == Some(-7)
            && file.album_id == Some(album)
            && !file.missing)
    );
    assert_eq!(
        data.album_members_in_folder(album, i64::MIN, 64, Arc::new(AtomicBool::new(false)))
            .unwrap()
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>(),
        vec![190]
    );
    for empty_folder in [0, i64::MAX] {
        assert!(
            data.album_members_in_folder(album, empty_folder, 64, Arc::new(AtomicBool::new(false)))
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        data.album_members_in_unassigned_directory(album, "", 64, Arc::new(AtomicBool::new(false)))
            .unwrap()
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>(),
        vec![151]
    );
    drop(readonly);
    assert_eq!(fs::read(&snapshot).unwrap(), snapshot_bytes);
    assert_eq!(
        fs::metadata(&snapshot).unwrap().modified().unwrap(),
        snapshot_modified
    );
    assert_eq!(fs::read(&source).unwrap(), source_bytes);
    assert_eq!(
        fs::metadata(&source).unwrap().modified().unwrap(),
        source_modified
    );
}

#[test]
fn shared_album_folder_candidates_validate_limits_and_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&root.path().join("folder-validation.db")).unwrap();
    for (album, limit) in [(0, 64), (-1, 64), (1, 0), (1, 65)] {
        assert!(matches!(
            provider.album_members_in_folder(album, -7, limit, Arc::new(AtomicBool::new(false))),
            Err(Error::Validation(_))
        ));
    }
    assert!(matches!(
        provider.album_members_in_folder(1, i64::MIN, 64, Arc::new(AtomicBool::new(true))),
        Err(Error::Cancelled)
    ));
}

#[test]
fn shared_album_folder_candidate_work_does_not_grow_with_other_directories() {
    use rusqlite::StatementStatus;

    fn run(conn: &Connection, sql: &str, album: i64) -> (Vec<i64>, i32, i32) {
        let mut statement = conn.prepare(sql).unwrap();
        let ids = statement
            .query_map(params![-7, album, 64], audio_from_row)
            .unwrap()
            .map(|file| file.unwrap().id)
            .collect::<Vec<_>>();
        (
            ids,
            statement.get_status(StatementStatus::VmStep),
            statement.get_status(StatementStatus::FullscanStep),
        )
    }

    let root = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&root.path().join("folder-budget.db")).unwrap();
    let conn = Connection::open(provider.database_path()).unwrap();
    register_library_functions(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO folders(id,folder_path,folder_name) VALUES
        (11,'owned/CK','CK'),(-7,'owned/Designed','Designed');
        INSERT INTO folder_closure VALUES(11,11,0),(-7,-7,0);",
    )
    .unwrap();
    for id in 100_000..100_003 {
        insert_folder_member(&conn, id, -7, "Same shared album");
    }
    let album = provider.get_audio(100_000).unwrap().album_id.unwrap();
    let sql = albums::members_in_folder_sql(&conn).unwrap();
    let plan = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(params![-7, album, 64], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|detail| detail.contains("idx_audio_files_folder")
                && detail.contains("folder_id=?")),
        "{plan:?}"
    );
    let before = run(&conn, &sql, album);
    conn.execute_batch(
        "WITH RECURSIVE ids(id) AS (VALUES(1) UNION ALL SELECT id+1 FROM ids WHERE id<8192)
         INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,metadata_json)
         SELECT id,'CK/'||id||'.wav','CK/'||id||'.wav','ck.wav',11,
                '{\"wav_metadata\":{\"fields\":{\"library\":\"Same shared album\"}}}' FROM ids;",
    )
    .unwrap();
    let after = run(&conn, &sql, album);
    assert_eq!(before.0, vec![100_000, 100_001, 100_002]);
    assert_eq!(after.0, before.0);
    assert!(
        after.1 <= before.1 + 256,
        "VM steps grew with unrelated members: {before:?} -> {after:?}"
    );
    assert!(
        after.2 <= before.2 + 8,
        "Full scans grew with unrelated members: {before:?} -> {after:?}"
    );
    assert_eq!(
        provider
            .album_members_in_folder(album, -7, 64, Arc::new(AtomicBool::new(false)))
            .unwrap()
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>(),
        after.0
    );
}

#[test]
fn shared_metadata_album_has_sorted_present_members_and_query_scoped_counts() {
    let root = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&root.path().join("shared.db")).unwrap();
    let conn = Connection::open(provider.database_path()).unwrap();
    register_library_functions(&conn).unwrap();
    insert_current(&conn, 5, "other/water.wav", "Shared Library");
    insert_current(&conn, 2, "first/metal.wav", "Shared Library");
    insert_current(&conn, 9, "gone/missing.wav", "Shared Library");
    conn.execute("UPDATE audio_files SET missing=1 WHERE id=9", [])
        .unwrap();
    let first = provider.get_audio(2).unwrap();
    let entity = first.album_entity.as_ref().unwrap();
    assert_eq!(entity.kind, AlbumKind::Metadata);
    assert_eq!(entity.name, "Shared Library");
    assert_eq!(entity.metadata_key.as_deref(), Some("Shared Library"));
    assert_eq!(entity.representative_audio_id, Some(2));
    assert_eq!(first.album_id, Some(entity.id));
    assert_eq!(provider.get_audio(5).unwrap().album_id, first.album_id);
    assert_eq!(provider.get_audio(9).unwrap().album_id, first.album_id);
    assert_eq!(provider.get_album(entity.id).unwrap(), *entity);
    let members = provider
        .album_members(entity.id, 64, Arc::new(AtomicBool::new(false)))
        .unwrap();
    assert_eq!(
        members.iter().map(|file| file.id).collect::<Vec<_>>(),
        vec![2, 5]
    );
    let facets = provider
        .album_facets(&SearchRequest {
            query: "metal".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(facets.len(), 1);
    assert_eq!(facets[0].album.id, entity.id);
    assert_eq!(facets[0].matching_count, 1);
    let filtered = SearchRequest {
        filters: SearchFilters {
            album_id: Some(entity.id),
            ..Default::default()
        },
        ..Default::default()
    };
    assert_eq!(provider.search(&filtered).unwrap().total_count, 2);
    let invalid = SearchRequest {
        filters: SearchFilters {
            album_id: Some(entity.id),
            album: Some("Shared Library".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(matches!(
        provider.search(&invalid),
        Err(Error::Validation(_))
    ));
    assert!(matches!(
        provider.album_members(entity.id, 0, Arc::new(AtomicBool::new(false))),
        Err(Error::Validation(_))
    ));
    assert!(matches!(
        provider.album_members(entity.id, 65, Arc::new(AtomicBool::new(false))),
        Err(Error::Validation(_))
    ));
    assert!(matches!(
        provider.album_members(entity.id, 64, Arc::new(AtomicBool::new(true))),
        Err(Error::Cancelled)
    ));
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_xinfo('audio_files') WHERE name='album'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn legacy_override_member_identity_pages_include_missing_and_extreme_ids() {
    let root = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&root.path().join("members.db")).unwrap();
    let mut conn = Connection::open(provider.database_path()).unwrap();
    register_library_functions(&conn).unwrap();
    let mut expected = vec![i64::MIN, -7];
    expected.extend(1..=130);
    expected.push(i64::MAX);
    let tx = conn.transaction().unwrap();
    for id in &expected {
        insert_current(
            &tx,
            *id,
            &format!("members/{id}.wav"),
            "Library with legacy overrides",
        );
    }
    insert_current(&tx, 500, "other.wav", "Unrelated library");
    tx.execute(
        "UPDATE audio_files SET missing=1 WHERE id%3=0 OR id=?1",
        [i64::MIN],
    )
    .unwrap();
    tx.commit().unwrap();
    let album = provider.get_audio(i64::MIN).unwrap().album_id.unwrap();
    let mut after = None;
    let mut found = Vec::new();
    let mut pages = 0;
    loop {
        let page = provider
            .album_member_ids(album, after, 64, Arc::new(AtomicBool::new(false)))
            .unwrap();
        if page.is_empty() {
            break;
        }
        pages += 1;
        after = page.last().copied();
        found.extend(page);
    }
    assert_eq!(pages, 3);
    assert_eq!(found, expected);
    assert!(
        provider
            .album_member_ids(album, Some(i64::MAX), 64, Arc::new(AtomicBool::new(false)))
            .unwrap()
            .is_empty()
    );
    let present = provider
        .album_members(album, 64, Arc::new(AtomicBool::new(false)))
        .unwrap();
    assert!(present.iter().all(|file| !file.missing));
    assert!(!present.iter().any(|file| file.id == i64::MIN));
    assert!(matches!(
        provider.album_member_ids(album, None, 4097, Arc::new(AtomicBool::new(false))),
        Err(Error::Validation(_))
    ));
    assert!(matches!(
        provider.album_member_ids(album, None, 64, Arc::new(AtomicBool::new(true))),
        Err(Error::Cancelled)
    ));
}

#[test]
fn previous_local_album_ids_survive_an_earlier_group_in_the_next_snapshot_revision() {
    let (_old_root, old_path) = previous_fixture();
    let old = SqliteProvider::open_existing(&old_path).unwrap();
    let namespace = old.album_namespace();
    let previous_a = old.get_audio(i64::MIN).unwrap().album_id.unwrap();
    let previous_b = old.get_audio(2).unwrap().album_id.unwrap();
    drop(old);
    let (_new_root, new_path) = previous_fixture();
    let conn = Connection::open(&new_path).unwrap();
    register_library_functions(&conn).unwrap();
    conn.execute(
        "INSERT INTO audio_files(id,file_path,path_key,filename,metadata_json,album)
         VALUES(0,'alpha.wav','alpha.wav','alpha.wav',?1,'Alpha earlier group')",
        [
            serde_json::json!({"wav_metadata":{"fields":{"library":"Alpha earlier group"}}})
                .to_string(),
        ],
    )
    .unwrap();
    drop(conn);
    let new = SqliteProvider::open_existing(&new_path).unwrap();
    assert_ne!(new.get_audio(2).unwrap().album_id, Some(previous_b));
    drop(new);
    let original_source = fs::read(&old_path).unwrap();
    let before_cancel = fs::read(&new_path).unwrap();
    assert!(matches!(
        SqliteProvider::preserve_local_album_ids(&old_path, &new_path, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert_eq!(fs::read(&new_path).unwrap(), before_cancel);
    SqliteProvider::preserve_local_album_ids(&old_path, &new_path, &AtomicBool::new(false))
        .unwrap();
    assert_eq!(fs::read(&old_path).unwrap(), original_source);
    let new = SqliteProvider::open_read_only(&new_path).unwrap();
    assert_eq!(new.album_namespace(), namespace);
    assert_eq!(new.get_audio(i64::MIN).unwrap().album_id, Some(previous_a));
    assert_eq!(new.get_audio(2).unwrap().album_id, Some(previous_b));
    assert!(new.get_audio(0).unwrap().album_id.unwrap() > previous_a.max(previous_b));
    assert_eq!(new.get_audio(i64::MIN).unwrap().attributes["private"], 42);
    assert_eq!(new.get_audio(2).unwrap().notes, "second note");
    assert!(new.check_library().unwrap().is_ok());
}

#[test]
fn independently_created_libraries_have_distinct_album_namespaces() {
    let root = tempfile::tempdir().unwrap();
    let first = SqliteProvider::open(&root.path().join("first.db")).unwrap();
    let second = SqliteProvider::open(&root.path().join("second.db")).unwrap();
    for provider in [&first, &second] {
        let conn = Connection::open(provider.database_path()).unwrap();
        register_library_functions(&conn).unwrap();
        insert_current(&conn, 1, "one.wav", "Same label");
    }
    assert_eq!(
        first.get_audio(1).unwrap().album_id,
        second.get_audio(1).unwrap().album_id
    );
    assert_ne!(first.album_namespace(), second.album_namespace());
    assert!(!first.album_namespace().is_empty());
}

#[test]
fn reopening_repairs_missing_or_changed_album_objects_without_changing_identities() {
    let triggers = [
        ("audiolib_album_audio_insert", "AFTER INSERT ON audio_files"),
        (
            "audiolib_album_audio_update",
            "AFTER UPDATE OF metadata_json,folder_id ON audio_files",
        ),
        ("audiolib_album_folder_delete", "BEFORE DELETE ON folders"),
        ("audiolib_album_id_high_water", "AFTER INSERT ON albums"),
    ];
    let mut damage = Vec::new();
    for (name, header) in triggers {
        damage.push(format!("DROP TRIGGER {name}"));
        damage.push(format!(
            "DROP TRIGGER {name}; CREATE TRIGGER {name} {header} BEGIN SELECT 1; END;"
        ));
    }
    for name in ["idx_audio_visible_album", "idx_audio_album_members"] {
        damage.push(format!("DROP INDEX {name}"));
        damage.push(format!(
            "DROP INDEX {name}; CREATE INDEX {name} ON audio_files(id)"
        ));
    }
    for statement in damage {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("repair.db");
        let provider = SqliteProvider::open(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        register_library_functions(&conn).unwrap();
        insert_current(&conn, 1, "one.wav", "Original library");
        conn.execute(
            "UPDATE audio_files SET metadata_json=audiolib_metadata_store(?1,1) WHERE id=1",
            [serde_json::json!({"wav_metadata":{"fields":{"library":"Original library"}},"private":42,"padding":"preserve exact metadata ".repeat(100)}).to_string()],
        ).unwrap();
        conn.execute_batch(
            "ALTER TABLE audio_files ADD COLUMN vendor_field TEXT;
             UPDATE audio_files SET vendor_field='retained vendor value' WHERE id=1;
             CREATE TABLE vendor_extension(id INTEGER PRIMARY KEY,payload BLOB);
             INSERT INTO vendor_extension VALUES(5,x'00ff017f');
             CREATE UNIQUE INDEX vendor_extension_payload ON vendor_extension(payload);",
        )
        .unwrap();
        let original = provider.get_audio(1).unwrap();
        let namespace = provider.album_namespace();
        drop(provider);
        conn.execute_batch(&statement).unwrap();
        drop(conn);
        assert!(
            !SqliteProvider::has_current_album_state(&path).unwrap(),
            "{statement}"
        );
        let provider = SqliteProvider::open_existing(&path).unwrap();
        let restored = provider.get_audio(1).unwrap();
        assert_eq!(restored.album_id, original.album_id, "{statement}");
        assert_eq!(restored.attributes, original.attributes, "{statement}");
        assert_eq!(provider.album_namespace(), namespace, "{statement}");
        assert!(
            SqliteProvider::has_current_album_state(&path).unwrap(),
            "{statement}"
        );
        let conn = Connection::open(&path).unwrap();
        register_library_functions(&conn).unwrap();
        insert_current(&conn, 2, "two.wav", "New library");
        let added = provider.get_audio(2).unwrap();
        assert!(added.album_id.is_some(), "{statement}");
        assert_ne!(added.album_id, original.album_id, "{statement}");
        assert_eq!(added.album_entity.as_ref().unwrap().name, "New library");
        let extension: (String, Vec<u8>) = conn.query_row(
            "SELECT vendor_field,(SELECT payload FROM vendor_extension WHERE id=5) FROM audio_files WHERE id=1",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(
            extension,
            ("retained vendor value".into(), vec![0, 255, 1, 127])
        );
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE type='index' AND name='vendor_extension_payload'", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
        assert!(provider.check_library().unwrap().is_ok(), "{statement}");
    }
}

#[test]
fn old_backup_restore_preserves_retired_album_ids_and_the_restored_namespace() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("restore.db");
    let provider = SqliteProvider::open(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    register_library_functions(&conn).unwrap();
    insert_current(&conn, 1, "one.wav", "Before backup");
    let original = provider.get_audio(1).unwrap();
    let namespace = provider.album_namespace();
    let backup = root.path().join("backup.db");
    provider.backup_library(&backup).unwrap();
    let backup_bytes = fs::read(&backup).unwrap();
    insert_current(&conn, 2, "two.wav", "Later retired library");
    let retired = provider.get_audio(2).unwrap().album_id.unwrap();
    drop(conn);
    provider.remove_audio(&[2]).unwrap();
    provider.restore_library(&backup).unwrap();
    assert_eq!(provider.album_namespace(), namespace);
    assert_eq!(provider.get_audio(1).unwrap().album_id, original.album_id);
    assert_eq!(fs::read(&backup).unwrap(), backup_bytes);
    drop(provider);
    let provider = SqliteProvider::open_existing(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    register_library_functions(&conn).unwrap();
    insert_current(&conn, 3, "three.wav", "New library after restore");
    assert!(provider.get_audio(3).unwrap().album_id.unwrap() > retired);
    assert!(matches!(
        provider.get_album(retired),
        Err(Error::NotFound(_))
    ));
    assert_eq!(provider.album_namespace(), namespace);
}

#[test]
fn restoring_another_library_rebinds_the_cached_album_namespace() {
    let root = tempfile::tempdir().unwrap();
    let active = SqliteProvider::open(&root.path().join("active.db")).unwrap();
    let incoming = SqliteProvider::open(&root.path().join("incoming.db")).unwrap();
    for (provider, name) in [(&active, "Old library"), (&incoming, "Incoming library")] {
        let conn = Connection::open(provider.database_path()).unwrap();
        register_library_functions(&conn).unwrap();
        insert_current(&conn, 1, "one.wav", name);
    }
    let old_namespace = active.album_namespace();
    let incoming_namespace = incoming.album_namespace();
    assert_ne!(old_namespace, incoming_namespace);
    let backup = root.path().join("incoming-backup.db");
    incoming.backup_library(&backup).unwrap();
    active.restore_library(&backup).unwrap();
    assert_eq!(active.get_audio(1).unwrap().album, "Incoming library");
    assert_eq!(active.album_namespace(), incoming_namespace);
    assert_ne!(active.album_namespace(), old_namespace);
}

#[test]
fn schema7_migration_preserves_unknown_extensions_without_replaying_metadata_triggers() {
    let (_root, path) = previous_fixture();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "ALTER TABLE audio_files ADD COLUMN vendor_blob BLOB;
         UPDATE audio_files SET vendor_blob=x'00ff017f' WHERE id=2;
         CREATE TABLE vendor_events(payload TEXT);
         INSERT INTO vendor_events(rowid,payload) VALUES(-99,'retain extension identity');
         CREATE TRIGGER vendor_metadata_audit AFTER UPDATE OF metadata_json ON audio_files BEGIN
           INSERT INTO vendor_events(payload) VALUES('metadata touched'); END;",
    )
    .unwrap();
    drop(conn);
    let provider = SqliteProvider::open_existing(&path).unwrap();
    assert_eq!(
        provider.get_audio(i64::MIN).unwrap().attributes["private"],
        42
    );
    let conn = Connection::open(&path).unwrap();
    let preserved: (Vec<u8>, String) = conn
        .query_row(
            "SELECT vendor_blob,typeof(metadata_json) FROM audio_files WHERE id=2",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(preserved, (vec![0, 255, 1, 127], "text".into()));
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
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "retain extension identity"
    );
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE type='trigger' AND name='vendor_metadata_audit'", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
}

#[test]
fn schema7_new_album_object_name_collisions_are_rejected_without_source_changes() {
    for collision in [
        "CREATE TRIGGER audiolib_album_audio_update AFTER UPDATE OF user_notes ON audio_files BEGIN INSERT INTO vendor_rows(payload) VALUES('keep audit behavior'); END;",
        "CREATE TRIGGER audiolib_album_id_high_water AFTER INSERT ON vendor_rows BEGIN SELECT 1; END;",
        "CREATE UNIQUE INDEX idx_albums_folder ON vendor_rows(payload);",
        "CREATE UNIQUE INDEX idx_audio_album_members ON vendor_rows(payload);",
        "DROP INDEX idx_audio_visible_album; CREATE UNIQUE INDEX idx_audio_visible_album ON vendor_rows(payload);",
    ] {
        let (_root, path) = previous_fixture();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE vendor_rows(id INTEGER PRIMARY KEY,payload TEXT); INSERT INTO vendor_rows VALUES(1,'preserve extension');").unwrap();
        conn.execute_batch(collision).unwrap();
        drop(conn);
        let before = fs::read(&path).unwrap();
        assert!(
            matches!(
                SqliteProvider::open_existing(&path),
                Err(Error::Validation(_))
            ),
            "{collision}"
        );
        assert_eq!(fs::read(&path).unwrap(), before, "{collision}");
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            7
        );
        assert_eq!(
            conn.query_row("SELECT payload FROM vendor_rows WHERE id=1", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
            "preserve extension"
        );
    }
}

#[test]
fn album_upgrade_respects_scan_administration_and_reader_leases() {
    let (_root, path) = previous_fixture();
    let before = fs::read(&path).unwrap();
    let scan = management::LibraryLease::acquire(&path).unwrap();
    assert!(matches!(
        SqliteProvider::open_existing(&path),
        Err(Error::Busy(_))
    ));
    drop(scan);
    let admin = management::AdministrationLease::exclusive(&path).unwrap();
    assert!(matches!(
        SqliteProvider::open_existing(&path),
        Err(Error::Busy(_))
    ));
    drop(admin);
    let reader = management::SearchUpgradeLease::acquire(&path, false).unwrap();
    assert!(matches!(
        SqliteProvider::open_existing(&path),
        Err(Error::Busy(_))
    ));
    drop(reader);
    assert_eq!(fs::read(&path).unwrap(), before);
    let provider = SqliteProvider::open_existing(&path).unwrap();
    assert_eq!(provider.get_audio(i64::MIN).unwrap().album, "Library A");
    assert_eq!(provider.get_audio(i64::MIN).unwrap().notes, "keep note");
    assert_eq!(
        provider.get_audio(i64::MIN).unwrap().attributes["private"],
        42
    );
    assert_eq!(
        provider.library_info().unwrap().schema_version,
        SCHEMA_VERSION
    );
}

#[test]
fn cancelled_album_migration_keeps_source_business_and_resumes() {
    let (_root, path) = previous_fixture();
    let source = Connection::open(&path).unwrap();
    register_library_functions(&source).unwrap();
    let before = migration::digest_business(&source, &AtomicBool::new(false)).unwrap();
    drop(source);
    let cancel = AtomicBool::new(false);
    let result = SqliteProvider::open_existing_with_progress_cancellable(&path, &cancel, |event| {
        if matches!(event, SnapshotUpgradeProgress::Copying { completed, .. } if completed > 0) {
            cancel.store(true, Ordering::Release);
        }
    });
    assert!(matches!(result, Err(Error::Cancelled)));
    let source = Connection::open(&path).unwrap();
    register_library_functions(&source).unwrap();
    // Durable guards are expected, but no business row or new structure is published.
    assert_eq!(
        migration::digest_business(&source, &AtomicBool::new(false)).unwrap(),
        before
    );
    assert_eq!(
        source
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        7
    );
    assert!(!SqliteProvider::has_current_album_state(&path).unwrap());
    drop(source);
    let provider = SqliteProvider::open_existing(&path).unwrap();
    assert_eq!(provider.get_audio(i64::MIN).unwrap().notes, "keep note");
    assert!(provider.get_audio(i64::MIN).unwrap().album_id.is_some());
    assert!(provider.check_library().unwrap().is_ok());
}

#[test]
fn pre_cancelled_album_migration_leaves_source_bytes_unchanged() {
    let (_root, path) = previous_fixture();
    let before = fs::read(&path).unwrap();
    assert!(matches!(
        SqliteProvider::open_existing_with_progress_cancellable(
            &path,
            &AtomicBool::new(true),
            |_| {}
        ),
        Err(Error::Cancelled)
    ));
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn album_upgrade_does_not_publish_damaged_identity_or_relationships() {
    for damage in [
        "UPDATE audiolib_audio_id_sequence SET high_water=0",
        "PRAGMA foreign_keys=OFF; INSERT INTO playlist_items(playlist_id,audio_file_id) VALUES(123,2)",
    ] {
        let (_root, path) = previous_fixture();
        Connection::open(&path)
            .unwrap()
            .execute_batch(damage)
            .unwrap();
        let before = fs::read(&path).unwrap();
        assert!(SqliteProvider::open_existing(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}

#[test]
fn previous_read_only_snapshots_upgrade_owned_copies_and_reopen_after_source_patches() {
    let (_root, path) = previous_fixture();
    let before = fs::read(&path).unwrap();
    assert!(!SqliteProvider::has_current_album_state(&path).unwrap());
    let provider = SqliteProvider::open_read_only(&path).unwrap();
    assert_ne!(provider.database_path(), path);
    assert!(provider._owned_snapshot_directory.is_some());
    assert!(SqliteProvider::has_current_album_state(provider.database_path()).unwrap());
    let source = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        source
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        7
    );
    assert_eq!(
        facet_counts(&provider, &SearchRequest::default()),
        vec![("Library A".into(), 1), ("Library B".into(), 1)]
    );
    let request = SearchRequest {
        filters: SearchFilters {
            album: Some("Library A".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let page = provider.search(&request).unwrap();
    assert_eq!(page.total_count, 1);
    assert_eq!(page.results[0].id, i64::MIN);
    assert_eq!(page.results[0].album, "Library A");
    let entity = page.results[0].album_entity.as_ref().unwrap();
    assert_eq!(Some(entity.id), page.results[0].album_id);
    assert_eq!(entity.kind, AlbumKind::Metadata);
    let query = SearchRequest {
        query: "metal".into(),
        ..Default::default()
    };
    assert_eq!(
        facet_counts(&provider, &query),
        vec![("Library A".into(), 1)]
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(matches!(
        provider.album_facets_cancellable(&query, Arc::new(AtomicBool::new(true))),
        Err(Error::Cancelled)
    ));
    let mut patched = page.results[0].clone();
    patched.notes = "updatedunique".into();
    SqliteProvider::apply_snapshot_records(&path, &[patched]).unwrap();
    // Existing owned views are immutable snapshots; reopening observes the patched source.
    assert_eq!(provider.get_audio(i64::MIN).unwrap().notes, "keep note");
    drop(provider);
    let provider = SqliteProvider::open_read_only(&path).unwrap();
    assert_eq!(
        provider
            .search(&SearchRequest {
                query: "updatedunique".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        1
    );
    assert_eq!(provider.get_audio(i64::MIN).unwrap().album, "Library A");
    assert_eq!(
        provider.get_audio(i64::MIN).unwrap().attributes["private"],
        42
    );
    assert_eq!(
        source
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        7
    );
}

#[test]
fn album_backup_restore_and_annotation_exports_preserve_authoritative_metadata() {
    use audiolib_core::LibraryExportFormat;

    let (root, path) = previous_fixture();
    let provider = SqliteProvider::open_existing(&path).unwrap();
    let original = provider.get_audio(i64::MIN).unwrap();
    let mut source_metadata = original.attributes.clone();
    // modified_at is a live DTO projection of the annotation revision, not
    // embedded Metadata. Importing an annotation correctly advances that value.
    source_metadata
        .as_object_mut()
        .unwrap()
        .remove("modified_at");
    let backup = root.path().join("backup.db");
    provider.backup_library(&backup).unwrap();
    for (name, format) in [
        ("annotations.jsonl", LibraryExportFormat::JsonLines),
        ("annotations.csv", LibraryExportFormat::Csv),
    ] {
        let exported = root.path().join(name);
        provider.export_library(&exported, format).unwrap();
        let bytes = fs::read_to_string(&exported).unwrap();
        assert!(bytes.contains("Library A"));
        assert!(bytes.contains("private"));
        provider
            .update_metadata(
                original.id,
                &MetadataPatch {
                    notes: Some("temporary edit".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let report = provider.import_annotations(&exported).unwrap();
        assert_eq!(report.records, 2);
        assert_eq!(report.skipped, 0);
        let mut imported = provider.get_audio(original.id).unwrap();
        assert_eq!(imported.notes, original.notes);
        imported
            .attributes
            .as_object_mut()
            .unwrap()
            .remove("modified_at");
        assert_eq!(imported.attributes, source_metadata);
        assert_eq!(imported.album, original.album);
    }
    provider
        .update_metadata(
            original.id,
            &MetadataPatch {
                notes: Some("edit after backup".into()),
                ..Default::default()
            },
        )
        .unwrap();
    provider.restore_library(&backup).unwrap();
    let restored = provider.get_audio(original.id).unwrap();
    assert_eq!(restored.notes, original.notes);
    assert_eq!(restored.attributes, original.attributes);
    assert_eq!(restored.album, original.album);
    assert_eq!(
        provider
            .album_facets(&SearchRequest::default())
            .unwrap()
            .len(),
        2
    );
}

/// Explicit acceptance against an already downloaded v6 mirror; source stays read-only.
#[test]
#[ignore = "requires AUDIOLIB_TEST_V6_SNAPSHOT and AUDIOLIB_TEST_AUDIO_ID"]
fn existing_v6_mirror_upgrades_an_owned_copy_and_reads_audio() {
    let path = PathBuf::from(std::env::var_os("AUDIOLIB_TEST_V6_SNAPSHOT").unwrap());
    let id: i64 = std::env::var("AUDIOLIB_TEST_AUDIO_ID")
        .unwrap()
        .parse()
        .unwrap();
    let before = fs::metadata(&path).unwrap();
    let source = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert_eq!(
        source
            .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        6
    );
    let provider = SqliteProvider::open_read_only(&path).unwrap();
    assert_ne!(provider.database_path(), path);
    assert!(provider._owned_snapshot_directory.is_some());
    let audio = provider.get_audio(id).unwrap();
    assert_eq!(audio.id, id);
    assert!(!audio.file_path.is_empty());
    assert_eq!(audio.album, audiolib_core::album(&audio.attributes));
    assert!(audio.album_id.is_some());
    assert!(audio.album_entity.is_some());
    drop(provider);
    let after = fs::metadata(&path).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
}

#[test]
fn captured_v5_migration_publishes_complete_album_entities_before_open_returns() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("legacy.db");
    let conn = Connection::open(&path).unwrap();
    register_library_functions(&conn).unwrap();
    conn.execute_batch(include_str!("../tests/fixtures/schema-v5.sql"))
        .unwrap();
    conn.execute_batch(
        "INSERT INTO audio_files(id,file_path,filename,user_notes,metadata_json)
         VALUES(1,'one.wav','metal.wav','retain annotation','{\"wav_metadata\":{\"fields\":{\"library\":\"Captured Album\"}},\"private\":42}');"
    ).unwrap();
    drop(conn);
    let provider = SqliteProvider::open_existing(&path).unwrap();
    let file = provider.get_audio(1).unwrap();
    assert_eq!(file.album, "Captured Album");
    assert_eq!(file.notes, "retain annotation");
    assert_eq!(file.attributes["private"], 42);
    assert_eq!(file.album_entity.as_ref().unwrap().name, "Captured Album");
    assert!(file.album_id.is_some());
    assert!(schema::current_format_ready(&Connection::open(&path).unwrap()).unwrap());
    assert_eq!(
        facet_counts(&provider, &SearchRequest::default()),
        vec![("Captured Album".into(), 1)]
    );
}

#[cfg(feature = "scanner")]
fn write_owned_wav(path: &Path, library: Option<&str>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut chunks = b"fmt ".to_vec();
    chunks.extend(16_u32.to_le_bytes());
    chunks.extend(1_u16.to_le_bytes());
    chunks.extend(1_u16.to_le_bytes());
    chunks.extend(48_000_u32.to_le_bytes());
    chunks.extend(96_000_u32.to_le_bytes());
    chunks.extend(2_u16.to_le_bytes());
    chunks.extend(16_u16.to_le_bytes());
    if let Some(library) = library {
        let mut payload = b"INFOIPRD".to_vec();
        payload.extend(u32::try_from(library.len() + 1).unwrap().to_le_bytes());
        payload.extend(library.as_bytes());
        payload.push(0);
        if !(library.len() + 1).is_multiple_of(2) {
            payload.push(0);
        }
        chunks.extend(b"LIST");
        chunks.extend(u32::try_from(payload.len()).unwrap().to_le_bytes());
        chunks.extend(payload);
    }
    chunks.extend(b"data");
    chunks.extend(2_u32.to_le_bytes());
    chunks.extend(0_i16.to_le_bytes());
    let mut bytes = b"RIFF".to_vec();
    bytes.extend(u32::try_from(chunks.len() + 4).unwrap().to_le_bytes());
    bytes.extend(b"WAVE");
    bytes.extend(chunks);
    fs::write(path, bytes).unwrap();
}

#[cfg(feature = "scanner")]
fn scan_owned_wav(
    provider: &SqliteProvider,
    root: &Path,
    path: &Path,
    library: Option<&str>,
) -> AudioFile {
    write_owned_wav(path, library);
    let item = ScannedAudio {
        path: path.to_owned(),
        root: root.to_owned(),
        size: fs::metadata(path).unwrap().len(),
        mtime_ns: i64::try_from(
            fs::metadata(path)
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .unwrap(),
        metadata: audiolib_audio::probe(path).unwrap(),
    };
    let session = provider.start_scan(root, false).unwrap();
    provider.write_scan_batch(&[item], session).unwrap();
    provider.find_audio_by_path(path).unwrap().unwrap()
}

#[cfg(feature = "scanner")]
#[test]
fn folder_fallback_uses_the_first_root_child_and_metadata_takes_priority() {
    let temp = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&temp.path().join("folders.db")).unwrap();
    let first_root = temp.path().join("first-root");
    let pack = first_root.join("Pack");
    let first = scan_owned_wav(&provider, &first_root, &pack.join("nested/metal.wav"), None);
    let second = scan_owned_wav(&provider, &first_root, &pack.join("other/water.wav"), None);
    let folder = first.album_entity.as_ref().unwrap();
    assert_eq!(folder.kind, AlbumKind::Folder);
    assert_eq!(folder.name, "Pack");
    assert_eq!(
        folder.folder_id,
        Some(provider.find_folder_by_path(&pack).unwrap().unwrap().id)
    );
    assert_ne!(folder.folder_id, first.folder_id);
    assert_eq!(first.album_id, second.album_id);
    assert!(first.album.is_empty());
    let declared = scan_owned_wav(
        &provider,
        &first_root,
        &pack.join("nested/declared.wav"),
        Some("Pack"),
    );
    assert_eq!(
        declared.album_entity.as_ref().unwrap().kind,
        AlbumKind::Metadata
    );
    assert_ne!(declared.album_id, first.album_id);
    let second_root = temp.path().join("second-root");
    let same_label = scan_owned_wav(
        &provider,
        &second_root,
        &second_root.join("Pack/wood.wav"),
        None,
    );
    assert_eq!(same_label.album_entity.as_ref().unwrap().name, folder.name);
    assert_ne!(same_label.album_id, first.album_id);
    let declared_elsewhere = scan_owned_wav(
        &provider,
        &second_root,
        &second_root.join("Other/declared.wav"),
        Some("Pack"),
    );
    assert_eq!(declared_elsewhere.album_id, declared.album_id);
    let direct = scan_owned_wav(&provider, &first_root, &first_root.join("direct.wav"), None);
    let direct_again = scan_owned_wav(
        &provider,
        &first_root,
        &first_root.join("another.wav"),
        None,
    );
    assert_eq!(direct.album_id, direct_again.album_id);
    assert_eq!(
        direct.album_entity.as_ref().unwrap().folder_id,
        direct.folder_id
    );
    assert_ne!(direct.album_id, first.album_id);
    let by_declared_name = provider
        .search(&SearchRequest {
            filters: SearchFilters {
                album: Some("Pack".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
    assert_eq!(by_declared_name.total_count, 2);
    assert!(
        by_declared_name
            .results
            .iter()
            .all(|file| file.album_id == declared.album_id)
    );
}

#[cfg(feature = "scanner")]
#[test]
fn unchanged_rescan_keeps_folder_entities_after_legacy_probe_metadata_becomes_text() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("unchanged.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    let root = temp.path().join("audio");
    let path = root.join("Pack/nested/one.wav");
    let original = scan_owned_wav(&provider, &root, &path, None);
    let namespace = provider.album_namespace();
    provider
        .update_metadata(
            original.id,
            &MetadataPatch {
                notes: Some("retain note".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let conn = Connection::open(provider.database_path()).unwrap();
    register_library_functions(&conn).unwrap();
    let raw: String = conn
        .query_row(
            "SELECT audiolib_metadata_json(metadata_json) FROM audio_files WHERE id=?1",
            [original.id],
            |row| row.get(0),
        )
        .unwrap();
    let mut attributes: serde_json::Value = serde_json::from_str(&raw).unwrap();
    attributes["private_padding"] = "unknown unchanged text ".repeat(400).into();
    let stored = crate::metadata_storage::encode_legacy_fixture(&attributes.to_string()).unwrap();
    assert!(matches!(&stored, rusqlite::types::Value::Blob(_)));
    conn.execute(
        "UPDATE audio_files SET metadata_json=?2 WHERE id=?1",
        params![original.id, stored],
    )
    .unwrap();
    conn.execute_batch("INSERT INTO audiolib_schema_features VALUES('metadata_storage',1); UPDATE audiolib_album_id_sequence SET compression=1;").unwrap();
    drop(conn);
    drop(provider);
    let provider = Arc::new(SqliteProvider::open_existing(&database).unwrap());
    assert_eq!(
        Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT typeof(metadata_json) FROM audio_files WHERE id=?1",
                [original.id],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "text"
    );
    let before = fs::read(&path).unwrap();
    let progress = ScanService::new(provider.clone())
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(progress.unchanged, 1);
    assert_eq!(progress.added, 0);
    assert_eq!(progress.updated, 0);
    assert_eq!(progress.errors, 0);
    let current = provider.get_audio(original.id).unwrap();
    assert_eq!(current.album_id, original.album_id);
    assert_eq!(current.notes, "retain note");
    assert_eq!(
        current.attributes["private_padding"],
        attributes["private_padding"]
    );
    assert_eq!(provider.album_namespace(), namespace);
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[cfg(feature = "scanner")]
#[test]
fn new_unknown_json_trigger_causes_owned_text_repair_before_the_next_scan() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("extension-repair.db");
    let provider = SqliteProvider::open(&database).unwrap();
    let root = temp.path().join("audio");
    let path = root.join("Pack/one.wav");
    let original = scan_owned_wav(&provider, &root, &path, None);
    let namespace = provider.album_namespace();
    let conn = Connection::open(&database).unwrap();
    register_library_functions(&conn).unwrap();
    let raw: String = conn
        .query_row(
            "SELECT audiolib_metadata_json(metadata_json) FROM audio_files WHERE id=?1",
            [original.id],
            |row| row.get(0),
        )
        .unwrap();
    let mut attributes: serde_json::Value = serde_json::from_str(&raw).unwrap();
    attributes["private_extension"] = "retain unknown text 🦉 ".repeat(300).into();
    let stored = crate::metadata_storage::encode_legacy_fixture(&attributes.to_string()).unwrap();
    assert!(matches!(&stored, rusqlite::types::Value::Blob(_)));
    conn.execute(
        "UPDATE audio_files SET metadata_json=?2 WHERE id=?1",
        params![original.id, stored],
    )
    .unwrap();
    conn.execute_batch("INSERT INTO audiolib_schema_features VALUES('metadata_storage',1); UPDATE audiolib_album_id_sequence SET compression=1;").unwrap();
    conn.execute_batch(
        "CREATE TABLE vendor_json_events(audio_id INTEGER,library TEXT);
         CREATE TRIGGER vendor_json_consumer AFTER UPDATE OF metadata_json ON audio_files BEGIN
           INSERT INTO vendor_json_events VALUES(new.id,json_extract(new.metadata_json,'$.wav_metadata.fields.library'));
         END;",
    ).unwrap();
    drop(conn);
    drop(provider);
    assert!(!SqliteProvider::has_current_album_state(&database).unwrap());
    let provider = Arc::new(SqliteProvider::open_existing(&database).unwrap());
    assert_eq!(provider.album_namespace(), namespace);
    assert_eq!(
        provider.get_audio(original.id).unwrap().album_id,
        original.album_id
    );
    assert_eq!(
        provider.get_audio(original.id).unwrap().attributes["private_extension"],
        attributes["private_extension"]
    );
    let conn = Connection::open(&database).unwrap();
    let mode: (String, i64) = conn.query_row(
        "SELECT typeof(metadata_json),(SELECT compression FROM audiolib_album_id_sequence WHERE singleton=1) FROM audio_files WHERE id=?1",
        [original.id], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!(mode, ("text".into(), 0));
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM vendor_json_events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    drop(conn);
    write_owned_wav(&path, Some("New source library"));
    let before_scan = fs::read(&path).unwrap();
    let result = ScanService::new(provider.clone())
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(result.updated, 1);
    assert_eq!(result.errors, 0);
    assert_eq!(
        provider.get_audio(original.id).unwrap().attributes["private_extension"],
        attributes["private_extension"]
    );
    let conn = Connection::open(&database).unwrap();
    let event: (i64, String) = conn
        .query_row(
            "SELECT audio_id,library FROM vendor_json_events",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(event, (original.id, "New source library".into()));
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM vendor_json_events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(fs::read(&path).unwrap(), before_scan);
}

#[cfg(feature = "scanner")]
#[test]
fn removing_a_folder_and_reusing_its_id_does_not_reuse_the_album_identity() {
    let temp = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&temp.path().join("reuse.db")).unwrap();
    let root = temp.path().join("audio");
    let old_path = root.join("Old/one.wav");
    let original = scan_owned_wav(&provider, &root, &old_path, None);
    let bytes = fs::read(&old_path).unwrap();
    let namespace = provider.album_namespace();
    let old_album = original.album_id.unwrap();
    let old_folder = original.folder_id.unwrap();
    assert_eq!(provider.remove_folder(old_folder).unwrap(), 1);
    assert_eq!(fs::read(&old_path).unwrap(), bytes);
    assert_eq!(provider.get_album(old_album).unwrap().folder_id, None);
    let replacement = scan_owned_wav(&provider, &root, &root.join("New/two.wav"), None);
    assert_eq!(replacement.folder_id, Some(old_folder));
    assert!(replacement.album_id.unwrap() > old_album);
    assert_ne!(replacement.album_id, original.album_id);
    assert_eq!(provider.album_namespace(), namespace);
    assert_eq!(
        provider
            .album_members(old_album, 64, Arc::new(AtomicBool::new(false)))
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        provider
            .album_facets(&SearchRequest::default())
            .unwrap()
            .len(),
        1
    );
}

#[cfg(feature = "scanner")]
#[test]
fn relocating_a_root_keeps_folder_album_identity_and_source_wav_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&temp.path().join("relink.db")).unwrap();
    let root = temp.path().join("old-root");
    let source = root.join("Pack/nested/one.wav");
    let original = scan_owned_wav(&provider, &root, &source, None);
    provider
        .update_metadata(
            original.id,
            &MetadataPatch {
                notes: Some("keep relink note".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let bytes = fs::read(&source).unwrap();
    let namespace = provider.album_namespace();
    let moved = temp.path().join("moved-root");
    fs::rename(&root, &moved).unwrap();
    assert_eq!(provider.relink_folder(&root, &moved).unwrap(), 1);
    let current = provider.get_audio(original.id).unwrap();
    assert_eq!(current.album_id, original.album_id);
    assert_eq!(
        current.album_entity.as_ref().unwrap().folder_id,
        original.album_entity.as_ref().unwrap().folder_id
    );
    assert_eq!(current.notes, "keep relink note");
    assert_eq!(provider.album_namespace(), namespace);
    assert_eq!(fs::read(moved.join("Pack/nested/one.wav")).unwrap(), bytes);
    assert_eq!(
        provider
            .get_album(current.album_id.unwrap())
            .unwrap()
            .folder_path,
        Some(folder_path_text(&moved.join("Pack")))
    );
}

#[cfg(feature = "scanner")]
#[test]
fn relinking_a_member_with_the_same_declared_library_keeps_shared_identity() {
    let temp = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&temp.path().join("member-relink.db")).unwrap();
    let root = temp.path().join("audio");
    let original_path = root.join("first/one.wav");
    let original = scan_owned_wav(&provider, &root, &original_path, Some("Shared Library"));
    provider
        .update_metadata(
            original.id,
            &MetadataPatch {
                notes: Some("retain note".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let target = root.join("other/two.wav");
    write_owned_wav(&target, Some("Shared Library"));
    let before = fs::read(&target).unwrap();
    let current = provider.relink_audio(original.id, &target).unwrap();
    assert_eq!(current.id, original.id);
    assert_eq!(current.album_id, original.album_id);
    assert_eq!(current.notes, "retain note");
    assert_eq!(fs::read(&target).unwrap(), before);
    assert!(original_path.is_file());
}

#[cfg(feature = "scanner")]
#[test]
fn rescanning_updates_declared_album_and_preserves_user_annotations() {
    let root = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&root.path().join("scan.db")).unwrap();
    let mut item = ScannedAudio {
        path: root.path().join("one.wav"),
        root: root.path().to_owned(),
        size: 100,
        mtime_ns: 1,
        metadata: AudioMetadata {
            attributes: serde_json::json!({"wav_metadata":{"fields":{"library":"Library A"}}}),
            ..Default::default()
        },
    };
    let session = provider.start_scan(root.path(), false).unwrap();
    provider
        .write_scan_batch(std::slice::from_ref(&item), session)
        .unwrap();
    let file = provider
        .search(&SearchRequest::default())
        .unwrap()
        .results
        .remove(0);
    provider
        .update_metadata(
            file.id,
            &MetadataPatch {
                notes: Some("preserve notes".into()),
                tags: Some("my tag".into()),
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    item.metadata.attributes =
        serde_json::json!({"wav_metadata":{"fields":{"library":"Library B"}}});
    item.mtime_ns = 2;
    provider.write_scan_batch(&[item], session).unwrap();
    let updated = provider.get_audio(file.id).unwrap();
    assert_eq!(updated.album, "Library B");
    assert_ne!(updated.album_id, file.album_id);
    assert_eq!(updated.album_entity.as_ref().unwrap().name, "Library B");
    assert_eq!(updated.notes, "preserve notes");
    assert_eq!(updated.tags, "my tag");
    assert!(updated.favorite);
    assert_eq!(
        facet_counts(&provider, &SearchRequest::default()),
        vec![("Library B".into(), 1)]
    );
}
