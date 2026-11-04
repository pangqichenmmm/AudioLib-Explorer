// crates/audiolib-remote/src/album_cache_tests.rs
//! Legacy server albums are upgraded only in owned files and retain their provenance.
use super::*;
use audiolib_core::{DataProvider, compatibility::SnapshotFormat};

pub(super) fn legacy_seven(path: &Path) {
    drop(SqliteProvider::open(path).unwrap());
    let conn = rusqlite::Connection::open(path).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
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
         VALUES(100,'one.wav','one.wav','one.wav','{\"wav_metadata\":{\"fields\":{\"library\":\"Shared\"}},\"extension\":{\"keep\":true}}','retained','2020-01-02 03:04:05','Shared'),
               (200,'two.wav','two.wav','two.wav','{\"wav_metadata\":{\"fields\":{\"library\":\"Shared\"}}}','second','2020-01-02 03:04:05','Shared');
         PRAGMA user_version=7;",
    ).unwrap();
}

#[test]
fn legacy_seven_album_projection_keeps_source_and_is_reused_with_annotation_replies() {
    let root = tempfile::tempdir().unwrap();
    let cache = RemoteCache::new(root.path());
    let mut state = CacheState {
        library_id: "legacy-albums".into(),
        revision: 12,
        schema_version: 7,
        database_file: "original.db".into(),
        server_url: "http://legacy.test/api/v1".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(cache.library_dir(&state.library_id)).unwrap();
    let source = cache.database_path(&state);
    legacy_seven(&source);
    let cancel = AtomicBool::new(false);
    let source_format = observed_format(&source, &cancel).unwrap();
    state.format = Some(Box::new(CacheFormat {
        source: Some(source_format.clone()),
        local: source_format.clone(),
        generation: 0,
    }));
    cache.save_state(&state).unwrap();
    let original = std::fs::read(&source).unwrap();
    let prepared = cache.prepare_cached_state(&state).unwrap();
    assert_ne!(prepared.database_file, state.database_file);
    assert_eq!(prepared.schema_version, audiolib_db::SCHEMA_VERSION);
    assert_eq!(std::fs::read(&source).unwrap(), original);
    assert_eq!(
        prepared.format.as_ref().unwrap().source.as_ref(),
        Some(&source_format)
    );
    assert_eq!(
        prepared.format.as_ref().unwrap().local,
        LibraryFormat::target()
    );
    assert_eq!(cache.prepare_cached_state(&prepared).unwrap(), prepared);
    let server = RemoteLibrary {
        library_id: state.library_id.clone(),
        revision: state.revision,
        schema_version: 7,
        snapshot_format: Some(SnapshotFormat {
            id: audiolib_core::compatibility::PLAN_B_V7_FORMAT.into(),
            format: source_format,
        }),
        ..Default::default()
    };
    assert!(reusable_source(&prepared, &server, &state.server_url));
    let provider = SqliteProvider::open_read_only(&cache.database_path(&prepared)).unwrap();
    let mut file = provider.get_audio(100).unwrap();
    let album_id = file.album_id.unwrap();
    assert_eq!(provider.get_audio(200).unwrap().album_id, Some(album_id));
    let entity = file.album_entity.clone();
    file.album_id = None;
    file.album_entity = None;
    file.notes = "legacy reply annotations".into();
    file.attributes["modified_at"] = serde_json::json!("2026-10-28 12:00:00");
    assert!(matches!(
        crate::apply_server_write(&cache, &state.library_id, &[file], 12, 13),
        crate::CacheUpdate::Applied { revision: 13 }
    ));
    let updated = provider.get_audio(100).unwrap();
    assert_eq!(updated.album_id, Some(album_id));
    assert_eq!(updated.album_entity, entity);
    assert_eq!(updated.notes, "legacy reply annotations");
    assert_eq!(updated.attributes["extension"]["keep"], true);
    assert_eq!(std::fs::read(source).unwrap(), original);
}

#[test]
fn experimental_compressed_cache_keeps_its_source_format_and_original_json() {
    use audiolib_core::compatibility::{METADATA_STORAGE_FEATURE, PLAN_B_V8_COMPRESSED_FORMAT};
    let root = tempfile::tempdir().unwrap();
    let cache = RemoteCache::new(root.path());
    let mut state = CacheState {
        library_id: "experimental-compressed".into(),
        revision: 12,
        schema_version: 8,
        database_file: "original.db".into(),
        server_url: "http://legacy.test/api/v1".into(),
        ..Default::default()
    };
    std::fs::create_dir_all(cache.library_dir(&state.library_id)).unwrap();
    let source = cache.database_path(&state);
    drop(SqliteProvider::open(&source).unwrap());
    let conn = rusqlite::Connection::open(&source).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    // Captured ALEMD-v1 zlib bytes, independent of the current write policy.
    let compressed: &[u8] = &[
        0x41, 0x4c, 0x45, 0x4d, 0x44, 0x01, 0x5a, 0x00, 0x13, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x78, 0x9c, 0xed, 0xca, 0xdb, 0x09, 0x84, 0x40, 0x10, 0x44, 0xd1, 0x5c, 0xea, 0xdb,
        0x4d, 0x60, 0xd2, 0x30, 0x00, 0xe9, 0xa5, 0x4b, 0x1d, 0xd6, 0x1d, 0x87, 0xb6, 0x7d, 0x21,
        0x93, 0xbb, 0x1a, 0x86, 0xe0, 0xd7, 0x29, 0xa8, 0x7b, 0x60, 0x95, 0xa5, 0xf9, 0xd3, 0x45,
        0xc5, 0x05, 0xe1, 0x40, 0x1b, 0x39, 0xe8, 0x74, 0xaf, 0x21, 0x7e, 0x4d, 0x6c, 0x47, 0x40,
        0xdd, 0x8b, 0x51, 0x51, 0x4a, 0x05, 0x6e, 0xce, 0x34, 0xc5, 0x31, 0xdd, 0xc5, 0x8f, 0xcc,
        0x08, 0x6e, 0x33, 0xaf, 0x27, 0x8b, 0x6a, 0x4c, 0xdd, 0x95, 0x1b, 0x33, 0xc5, 0x3f, 0x2f,
        0x2f, 0x8f, 0x03, 0xe5, 0x04, 0x6b, 0xbb, 0x2a, 0x86,
    ];
    conn.execute(
        "INSERT INTO audio_files(id,file_path,filename,metadata_json) VALUES(1,'shared.wav','shared.wav',?1)",
        [compressed],
    )
    .unwrap();
    conn.execute_batch(
        "INSERT INTO audiolib_schema_features(feature,version) VALUES('metadata_storage',1);
         UPDATE audiolib_album_id_sequence SET compression=1 WHERE singleton=1;
         PRAGMA journal_mode=DELETE;",
    )
    .unwrap();
    let raw = conn
        .query_row(
            "SELECT audiolib_metadata_json(metadata_json) FROM audio_files",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    assert_eq!(
        raw,
        format!(
            r#"{{"wav_metadata":{{"fields":{{"library":"Shared"}}}},"extension":{{"keep":true}},"padding":"{}"}}"#,
            "repeat-".repeat(100)
        )
    );
    drop(conn);
    let original = std::fs::read(&source).unwrap();
    let source_format = observed_format(&source, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        source_format.snapshot_format_id().as_deref(),
        Some(PLAN_B_V8_COMPRESSED_FORMAT)
    );
    assert_eq!(
        source_format.features.get(METADATA_STORAGE_FEATURE),
        Some(&1)
    );
    state.format = Some(Box::new(CacheFormat {
        source: Some(source_format.clone()),
        local: source_format.clone(),
        generation: 0,
    }));
    cache.save_state(&state).unwrap();
    let prepared = cache.prepare_cached_state(&state).unwrap();
    assert_eq!(
        prepared, state,
        "a readable legacy cache must not be repeatedly converted"
    );
    let provider = SqliteProvider::open_read_only(&source).unwrap();
    let file = provider.get_audio(1).unwrap();
    assert_eq!(file.album_entity.as_ref().unwrap().name, "Shared");
    assert_eq!(file.attributes["extension"]["keep"], true);
    assert_eq!(file.attributes["padding"], "repeat-".repeat(100));
    let server = RemoteLibrary {
        library_id: state.library_id.clone(),
        revision: state.revision,
        schema_version: 8,
        snapshot_format: Some(SnapshotFormat {
            id: PLAN_B_V8_COMPRESSED_FORMAT.into(),
            format: source_format,
        }),
        ..Default::default()
    };
    crate::compatibility::library(&server).unwrap();
    assert!(reusable_source(&prepared, &server, &state.server_url));
    assert_eq!(std::fs::read(&source).unwrap(), original);
}

#[test]
fn legacy_sync_inherits_album_ids_and_namespace_without_repeated_downloads() {
    use crate::test_server::{TestResponse, TestServer};
    use std::sync::{Arc, atomic::AtomicUsize};
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first.db");
    let second = root.path().join("second.db");
    legacy_seven(&first);
    legacy_seven(&second);
    let conn = rusqlite::Connection::open(&second).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute_batch("INSERT INTO audio_files(id,file_path,path_key,filename,metadata_json,album) VALUES(1,'earlier.wav','earlier.wav','earlier.wav','{\"wav_metadata\":{\"fields\":{\"library\":\"Before\"}}}','Before');").unwrap();
    drop(conn);
    let cancel = AtomicBool::new(false);
    let source_format = observed_format(&first, &cancel).unwrap();
    let payloads = [
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap(),
    ];
    let generation = Arc::new(AtomicUsize::new(0));
    let active = generation.clone();
    let downloads = Arc::new(AtomicUsize::new(0));
    let counted = downloads.clone();
    let server = TestServer::start(move |request| {
        let index = active.load(Ordering::Acquire);
        let revision = index + 1;
        match request.path.as_str() {
            "/api/v1/health" => crate::test_server::marker_health(),
            "/api/v1/capabilities" => crate::test_server::marker_capabilities(serde_json::json!({})),
            "/api/v1/roots" => TestResponse::json(200, "[]"),
            "/api/v1/library" => TestResponse::json(200, &serde_json::json!({
                "library_id":"stable-albums", "revision":revision, "schema_version":7,
                "snapshot_format":{"id":audiolib_core::compatibility::PLAN_B_V7_FORMAT,"format":source_format}
            }).to_string()),
            "/api/v1/library/snapshot" => {
                counted.fetch_add(1, Ordering::Relaxed);
                TestResponse::bytes(200, payloads[index].clone())
                    .with_header("x-library-id", "stable-albums")
                    .with_header("x-library-revision", &revision.to_string())
                    .with_header("x-schema-version", "7")
                    .with_header(audiolib_core::compatibility::SERVER_COMPATIBILITY_HEADER, &serde_json::to_string(&audiolib_core::compatibility::compatibility_manifest()).unwrap())
                    .with_header(audiolib_core::compatibility::SNAPSHOT_FORMAT_HEADER, audiolib_core::compatibility::PLAN_B_V7_FORMAT)
                    .with_header(audiolib_core::compatibility::LIBRARY_FORMAT_HEADER, &serde_json::to_string(&source_format).unwrap())
                    .with_header("x-search-storage-revision", "1")
                    .with_header("x-tokenization-revision", "3")
                    .with_header("x-source-search-revision", "2")
                    .with_header("x-source-tags-revision", "2")
            }
            _ => TestResponse::json(404, "{}"),
        }
    });
    let cache = RemoteCache::new(&root.path().join("cache"));
    let client = RemoteClient::new(&server.url, "token").unwrap();
    let before = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
    let old = SqliteProvider::open_read_only(&cache.database_path(&before.state)).unwrap();
    let original_id = old.get_audio(100).unwrap().album_id;
    let namespace = old.album_namespace();
    generation.store(1, Ordering::Release);
    let after = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
    let new = SqliteProvider::open_read_only(&cache.database_path(&after.state)).unwrap();
    assert_eq!(new.get_audio(100).unwrap().album_id, original_id);
    assert_eq!(new.album_namespace(), namespace);
    assert!(new.get_audio(1).unwrap().album_id > original_id);
    let reused = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
    assert!(!reused.downloaded);
    assert_eq!(reused.state.database_file, after.state.database_file);
    assert_eq!(downloads.load(Ordering::Acquire), 2);
}
