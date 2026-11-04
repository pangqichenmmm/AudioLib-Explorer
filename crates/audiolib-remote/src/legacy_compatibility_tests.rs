// crates/audiolib-remote/src/legacy_compatibility_tests.rs
//! Published legacy protocol coverage using captured schema-5 storage and HTTP exchanges.
use crate::{
    CacheUpdate, CachedIdentity, ProbeStep, RemoteCache, RemoteClient, RemoteError, RemoteProvider,
    RemoteSecrets, RemoteWriter, RevisionCheck, check_revision, sync_library, test_connection,
    test_server::{TestRequest, TestResponse, TestServer},
};
use audiolib_core::{
    AudioFile, DataProvider, MetadataPatch, SearchRequest,
    compatibility::{CLIENT_COMPATIBILITY_HEADER, compatibility_manifest},
    config::DataSourceSettings,
};
use audiolib_db::SqliteProvider;
use rusqlite::{Connection, OpenFlags};
use std::{
    path::Path,
    sync::{Arc, Mutex, atomic::AtomicBool},
};

const LIBRARY: &str = "owned-legacy-protocol-library";
const AUDIO_ID: i64 = 7;
const INITIAL_REVISION: u64 = 41;
const HISTORICAL_PLAY_COUNT: i64 = 17;
const LAST_PLAYED: &str = "2026-01-02T03:04:05Z";

struct LegacyState {
    version: String,
    revision: u64,
    file: AudioFile,
    requests: Vec<(String, String)>,
    downloads: usize,
    writes: usize,
}

/// The fixture contains no working-library or real WAV data. Keep the original
/// legacy bytes available so sync/write tests also detect a hidden in-place upgrade.
fn legacy_snapshot(path: &Path) -> (Vec<u8>, AudioFile) {
    let conn = Connection::open(path).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute_batch(include_str!(
        "../../audiolib-db/tests/fixtures/schema-v5.sql"
    ))
    .unwrap();
    conn.execute(
        "INSERT INTO audio_files(id,file_path,path_key,filename,filename_tokenized,
         file_size,sample_rate,bit_depth,channels,duration,user_tags,user_tags_tokenized,
         user_notes,user_notes_tokenized,bwf_description,bwf_description_tokenized,
         ixml_scene,ixml_scene_tokenized,ixml_take,ixml_take_tokenized,play_count,last_played_at,modified_at)
         VALUES(?1,'/owned/legacy-metal.wav','/owned/legacy-metal.wav','legacy-metal.wav',
          audiolib_tokenize('legacy-metal.wav'),128,48000,16,2,1.0,
          'legacytag',audiolib_tokenize('legacytag'),'originalnote',
          audiolib_tokenize('originalnote'),'','','','','','',?2,?3,'2026-10-24 12:34:56.789')",
        rusqlite::params![AUDIO_ID, HISTORICAL_PLAY_COUNT, LAST_PLAYED],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO play_history(id,audio_file_id,played_at,filename,file_path)
         VALUES(1,?1,?2,'legacy-metal.wav','/owned/legacy-metal.wav')",
        rusqlite::params![AUDIO_ID, LAST_PLAYED],
    )
    .unwrap();
    conn.execute_batch("PRAGMA journal_mode=DELETE;").unwrap();
    drop(conn);
    assert!(SqliteProvider::has_current_search_state(path).unwrap());
    let reader = SqliteProvider::open_read_only(path).unwrap();
    assert_ne!(reader.database_path(), path);
    let mut file = reader.get_audio(AUDIO_ID).unwrap();
    file.album_id = None;
    file.album_entity = None;
    drop(reader);
    (std::fs::read(path).unwrap(), file)
}

fn json(value: &serde_json::Value) -> TestResponse {
    TestResponse::json(200, &value.to_string())
}

fn legacy_server(
    version: &str,
    bytes: Vec<u8>,
    file: AudioFile,
) -> (TestServer, Arc<Mutex<LegacyState>>) {
    let state = Arc::new(Mutex::new(LegacyState {
        version: version.into(),
        revision: INITIAL_REVISION,
        file,
        requests: Vec::new(),
        downloads: 0,
        writes: 0,
    }));
    let observed = state.clone();
    let server = TestServer::start(move |request: &TestRequest| {
        let mut state = observed.lock().unwrap();
        state
            .requests
            .push((request.method.clone(), request.path.clone()));
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/api/v1/health") => json(&serde_json::json!({
                "status":"ok", "version":state.version,
            })),
            // The tested published baselines expose management hints, without a modern
            // compatibility catalog or an actual-format descriptor.
            // A marker server of this release, or a published unmarked one.
            ("GET", "/api/v1/capabilities") if state.version == env!("CARGO_PKG_VERSION") => {
                crate::test_server::marker_capabilities(serde_json::json!({
                    "manage_mode":"all", "can_manage":true,
                }))
            }
            ("GET", "/api/v1/capabilities") => json(&serde_json::json!({
                "manage_mode":"all", "can_manage":true,
            })),
            ("GET", "/api/v1/library") => json(&serde_json::json!({
                "library_id":LIBRARY, "revision":state.revision,
                "schema_version":5, "identity_bound_writes":true,
                "stats":{"audio_files":1},
            })),
            ("GET", "/api/v1/roots") => json(&serde_json::json!([])),
            ("GET", "/api/v1/library/snapshot") => {
                state.downloads += 1;
                TestResponse::bytes(200, bytes.clone())
                    .with_header("x-library-id", LIBRARY)
                    .with_header("x-library-revision", &state.revision.to_string())
                    .with_header("x-schema-version", "5")
            }
            ("PATCH", "/api/v1/audio/7/metadata") => {
                state.writes += 1;
                assert_eq!(request.header(crate::LIBRARY_ID_HEADER), Some(LIBRARY));
                let catalog: audiolib_core::compatibility::CompatibilityManifest =
                    serde_json::from_str(request.header(CLIENT_COMPATIBILITY_HEADER).unwrap())
                        .unwrap();
                assert_eq!(catalog, compatibility_manifest());
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let note = body["notes"].as_str().unwrap();
                let before = state.revision;
                // Match the published server's no-op/changed Metadata response
                // contract, including revision equality for a no-op patch.
                if state.file.notes != note {
                    state.file.notes = note.into();
                    state.revision += 1;
                }
                let mut file = serde_json::to_value(&state.file).unwrap();
                file["play_count"] = HISTORICAL_PLAY_COUNT.into();
                json(&serde_json::json!({
                    "file":file, "revision_before":before, "revision":state.revision,
                }))
            }
            ("GET", "/api/v1/audio/7/stream") => {
                assert_eq!(request.header("range"), Some("bytes=0-0"));
                TestResponse::bytes(206, vec![0]).with_header("content-range", "bytes 0-0/128")
            }
            ("GET", path) if path.starts_with("/api/v1/audio/7/preview?") => {
                TestResponse::bytes(206, vec![0; 128])
                    .with_header("content-range", "bytes 0-127/128")
                    .with_header("x-preview-sample-rate", "48000")
                    .with_header("x-preview-bits", "16")
                    .with_header("x-preview-channels", "2")
            }
            _ => TestResponse::json(
                500,
                r#"{"error_code":"UNEXPECTED_TEST_REQUEST","message":"unexpected endpoint"}"#,
            ),
        }
    });
    (server, state)
}

fn cached_identity(cache: &RemoteCache) -> CachedIdentity {
    let state = cache.active().unwrap();
    CachedIdentity {
        library_id: state.library_id,
        revision: state.revision,
    }
}

#[test]
fn marker_server_with_published_schema5_snapshot_syncs_polls_probes_patches_and_ignores_retired_json_fields()
 {
    for version in [env!("CARGO_PKG_VERSION")] {
        let owned = tempfile::tempdir().unwrap();
        let original = owned.path().join("original-schema5.db");
        let (bytes, file) = legacy_snapshot(&original);
        let original_bytes = bytes.clone();
        let (server, observed) = legacy_server(version, bytes, file);
        let client = RemoteClient::new(&server.url, "").unwrap();
        let cache = RemoteCache::new(&owned.path().join("client"));
        let synced =
            sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
        assert!(synced.downloaded, "{version}");
        assert_eq!(
            synced.state.schema_version,
            audiolib_db::SCHEMA_VERSION,
            "{version}"
        );
        assert_eq!(synced.state.server_version, version);
        let format = synced.state.format.as_ref().unwrap();
        assert_eq!(format.source.as_ref().unwrap().schema_version, 5);
        assert_eq!(
            format.local,
            audiolib_core::compatibility::LibraryFormat::target()
        );
        assert_eq!(
            format.local.schema_version,
            audiolib_db::SCHEMA_VERSION as u32
        );
        assert_eq!(format.generation, 1);
        assert_eq!(
            check_revision(&client, Some(&cached_identity(&cache)), false),
            RevisionCheck::UpToDate {
                revision: INITIAL_REVISION,
            },
            "{version}"
        );
        let report = test_connection(
            &DataSourceSettings {
                server_url: server.url.clone(),
                ..Default::default()
            },
            &RemoteSecrets::default(),
            &[],
            Some(&cache),
            false,
        );
        assert!(report.ok(), "{version}: {}", report.summary(false));
        assert_eq!(report.server_version.as_deref(), Some(version));
        assert!(
            report
                .steps
                .iter()
                .any(|step| step.step == ProbeStep::Library && step.ok && !step.skipped)
        );
        assert!(
            report
                .steps
                .iter()
                .any(|step| step.step == ProbeStep::Stream && step.ok && !step.skipped)
        );
        let writer = RemoteWriter::new(client.clone(), cache.clone(), LIBRARY);
        for (note, expected_revision) in [
            ("originalnote", INITIAL_REVISION),
            ("changedlegacynote", INITIAL_REVISION + 1),
        ] {
            let outcome = writer
                .update_metadata(
                    AUDIO_ID,
                    &MetadataPatch {
                        notes: Some(note.into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(outcome.revision_before, INITIAL_REVISION, "{version}");
            assert_eq!(outcome.revision, expected_revision, "{version}");
            assert_eq!(
                outcome.cache,
                CacheUpdate::Applied {
                    revision: expected_revision,
                },
                "{version}"
            );
            let state = cache.active().unwrap();
            assert_eq!(state.revision, expected_revision);
            assert_eq!(state.format, synced.state.format);
            let reader = SqliteProvider::open_read_only(&cache.database_path(&state)).unwrap();
            assert_eq!(reader.database_path(), cache.database_path(&state));
            assert_eq!(reader.get_audio(AUDIO_ID).unwrap().notes, note);
            assert!(
                serde_json::to_value(&outcome.value)
                    .unwrap()
                    .get("play_count")
                    .is_none()
            );
            let value = serde_json::to_value(reader.get_audio(AUDIO_ID).unwrap()).unwrap();
            assert!(value.get("play_count").is_none());
            assert!(value["attributes"].get("last_played_at").is_none());
            assert_eq!(
                reader
                    .search(&SearchRequest {
                        query: note.into(),
                        ..Default::default()
                    })
                    .unwrap()
                    .total_count,
                1,
                "{version}: patch must update legacy FTS"
            );
        }
        assert_eq!(
            check_revision(&client, Some(&cached_identity(&cache)), false),
            RevisionCheck::UpToDate {
                revision: INITIAL_REVISION + 1,
            }
        );
        let again =
            sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
        assert!(
            !again.downloaded,
            "{version}: reuse the proven legacy source"
        );
        assert_eq!(again.state.format, synced.state.format);
        let path = cache.database_path(&again.state);
        let reader = Arc::new(SqliteProvider::open_read_only(&path).unwrap());
        let provider = RemoteProvider::new(reader, writer, false, None);
        let observer =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let data_version: i64 = observer
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .unwrap();
        let before_state = cache.active().unwrap();
        let before_bytes = std::fs::read(&path).unwrap();
        let before_requests = observed.lock().unwrap().requests.clone();
        // Ordinary cached reads have no playback API or history side effect.
        // Client-local playback/history workflows are tested by the desktop owner.
        let value = serde_json::to_value(provider.get_audio(AUDIO_ID).unwrap()).unwrap();
        assert!(value.get("play_count").is_none());
        assert!(value["attributes"].get("last_played_at").is_none());
        assert_eq!(cache.active().unwrap(), before_state);
        assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
        assert_eq!(observed.lock().unwrap().requests, before_requests);
        assert_eq!(
            observer
                .pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            data_version
        );
        let original_reader =
            Connection::open_with_flags(&original, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let retained: (i64, String, i64) = original_reader
            .query_row(
                "SELECT play_count,last_played_at,(SELECT COUNT(*) FROM play_history)
                 FROM audio_files WHERE id=?1",
                [AUDIO_ID],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(retained, (HISTORICAL_PLAY_COUNT, LAST_PLAYED.into(), 1));
        assert_eq!(observer.query_row("SELECT COUNT(*) FROM pragma_table_xinfo('audio_files') WHERE name IN ('play_count','last_played_at')", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(
            observer
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name='play_history'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(std::fs::read(&original).unwrap(), original_bytes);
        let state = observed.lock().unwrap();
        assert_eq!(state.downloads, 1, "{version}");
        assert_eq!(state.writes, 2, "{version}");
    }
}

#[test]
fn unmarked_servers_require_upgrade_before_download_or_write_and_retain_cache() {
    // Published catalog-less (0.4.11/0.5.x) and range-based (0.6.x–0.7.x) servers. (This
    // build still reports the workspace version, so the real 0.7.1 catalog is covered by
    // the core fixture tests.)
    for unsupported in ["0.4.11", "0.5.0", "0.5.1", "0.6.5", "0.7.0", "99.0.0"] {
        let owned = tempfile::tempdir().unwrap();
        let (bytes, file) = legacy_snapshot(&owned.path().join("source.db"));
        let (server, observed) = legacy_server(env!("CARGO_PKG_VERSION"), bytes, file);
        let client = RemoteClient::new(&server.url, "").unwrap();
        let cache = RemoteCache::new(&owned.path().join("client"));
        let initial =
            sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
        let files = [
            cache.database_path(&initial.state),
            cache.library_dir(LIBRARY).join("cache.json"),
            cache.root().join("active.json"),
        ];
        let retained: Vec<Vec<u8>> = files
            .iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect();
        observed.lock().unwrap().version = unsupported.into();
        let upgrade = |error: &RemoteError| {
            matches!(
                error,
                RemoteError::PeerIncompatible {
                    reason: crate::error::PeerIncompatibility::ServerUpgradeRequired,
                    ..
                }
            )
        };
        let error =
            sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), true).unwrap_err();
        assert!(upgrade(&error), "{unsupported}: {error:?}");
        assert!(error.localized(false).contains("Upgrade AudioLib Server"));
        let writer = RemoteWriter::new(client.clone(), cache.clone(), LIBRARY);
        assert!(matches!(
            writer.update_metadata(
                AUDIO_ID,
                &MetadataPatch {
                    notes: Some("mustnotcommit".into()),
                    ..Default::default()
                },
            ),
            Err(ref error) if upgrade(error)
        ));
        assert!(matches!(
            check_revision(&client, Some(&cached_identity(&cache)), false),
            RevisionCheck::Failed(_)
        ));
        let report = test_connection(
            &DataSourceSettings {
                server_url: server.url.clone(),
                ..Default::default()
            },
            &RemoteSecrets::default(),
            &[],
            Some(&cache),
            false,
        );
        assert!(!report.ok(), "{unsupported}");
        assert!(
            report
                .steps
                .iter()
                .any(|step| step.step == ProbeStep::Library && !step.ok && !step.skipped)
        );
        assert_eq!(cache.active().unwrap(), initial.state);
        for (path, expected) in files.iter().zip(retained) {
            assert_eq!(std::fs::read(path).unwrap(), expected, "{unsupported}");
        }
        let state = observed.lock().unwrap();
        assert_eq!(state.downloads, 1, "{unsupported}");
        assert_eq!(state.writes, 0, "{unsupported}");
        assert_eq!(state.revision, INITIAL_REVISION);
        assert_eq!(state.file.notes, "originalnote");
    }
}
