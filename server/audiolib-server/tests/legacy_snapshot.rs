// server/audiolib-server/tests/legacy_snapshot.rs
//! Real router negotiation, legacy projection, generation isolation and write rejection.
use audiolib_core::{DataProvider, compatibility::*};
use audiolib_db::{SqliteProvider, migration9};
use audiolib_server::{AppState, Security, ServerConfig, router};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Request, StatusCode},
};
use http_body_util::BodyExt;
use rusqlite::Connection;
use std::{path::Path, sync::atomic::AtomicBool, time::Duration};
use tower::ServiceExt;

fn headers(modern: bool) -> HeaderMap {
    let mut catalog = compatibility_manifest();
    if !modern {
        catalog.application_version = "0.8.7".into();
        catalog.catalog_revision = 1;
        catalog.snapshot_read_formats.retain(|id| {
            !matches!(
                id.as_str(),
                PLAN_B_V9_FORMAT | PLAN_B_AUDIO_FORMAT | PLAN_B_V9_AUDIO_FORMAT
            )
        });
        catalog.features.remove(AUDIO_SOURCES_FEATURE);
    }
    let mut headers = HeaderMap::new();
    for (name, value) in [
        (
            CLIENT_COMPATIBILITY_HEADER,
            serde_json::to_string(&catalog).unwrap(),
        ),
        (
            ACCEPT_FORMATS_HEADER,
            if modern {
                format!("{PLAN_B_FORMAT},{PLAN_B_V9_FORMAT}")
            } else {
                PLAN_B_FORMAT.into()
            },
        ),
        (
            SNAPSHOT_SCHEMA_MAX_HEADER,
            if modern { "9" } else { "8" }.into(),
        ),
        (SEARCH_STORAGE_MAX_HEADER, "1".into()),
        (TOKENIZATION_MAX_HEADER, "3".into()),
    ] {
        headers.insert(name, HeaderValue::from_str(&value).unwrap());
    }
    headers
}

async fn request(
    state: &AppState,
    method: &str,
    path: &str,
    headers: &HeaderMap,
    body: &str,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .header("x-audiolib-library-id", state.library_id())
        .body(Body::from(body.to_owned()))
        .unwrap();
    request.headers_mut().extend(headers.clone());
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

fn fixture(path: &Path) {
    fixture_schema8(path);
    let report = migration9::execute(
        path,
        migration9::Action::Start,
        &Default::default(),
        &AtomicBool::new(false),
    );
    assert_eq!(report.state, migration9::State::Done, "{report:?}");
    assert_source_projection(path, "after schema-9 migration");
}

fn fixture_schema8(path: &Path) {
    drop(SqliteProvider::open(path).unwrap());
    let conn = Connection::open(path).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    let metadata: serde_json::Value = serde_json::from_str(
        r#"{
        "unknown_extension": {"retained": true},
        "wav_metadata": {
            "riff_info": {"ICMT": "embeddedtext"},
            "fields": {"description": "embeddedtext", "keywords": ["embeddedtag"]},
            "vorbis": {"DESCRIPTION": "vorbistext"}
        },
        "vendor_metadata": {"fields": {"description": ["vendortext"]}}
    }"#,
    )
    .expect("legacy snapshot fixture Metadata must be valid JSON before FTS insertion");
    conn.execute_batch(
        "INSERT INTO folders(id,folder_path,folder_name) VALUES(1,'owned','OwnedRoot');
        INSERT INTO folder_closure VALUES(1,1,0);",
    )
    .unwrap();
    // Real extracted WAV descriptions retain their raw INFO field as well as the
    // canonical display mirror. Binding validated JSON prevents a malformed fixture
    // from silently becoming empty source text in the production SQL function.
    conn.execute("INSERT INTO audio_files(id,file_path,filename,folder_id,user_notes,user_tags,favorite,metadata_json)
        VALUES(1,'owned/one.wav','one.wav',1,'userannotation','usertag',1,?1)",
        [metadata.to_string()]).unwrap();
    conn.execute_batch(r#"
        INSERT INTO audio_files(id,file_path,filename,folder_id,user_notes,user_tags,favorite,metadata_json) VALUES
        (2,'owned/two.WAV','two.WAV',1,'secondannotation','',0,'{}'),
        (3,'owned/three.flac','three.flac',1,'flacannotation','',0,'{}'),
        (4,'owned/four.ogg','four.ogg',1,'oggannotation','',0,'{}');
        INSERT INTO playlists(id,name) VALUES(1,'retained');
        INSERT INTO playlist_items(id,playlist_id,audio_file_id,position) VALUES(1,1,1,0),(2,1,3,1);
        INSERT INTO project_references(id,audio_file_id,project_path) VALUES(1,1,'kept-project'),(2,3,'excluded-project');
        INSERT INTO waveform_cache(id,audio_file_id) VALUES(1,1),(2,3);
    "#).unwrap();
    drop(conn);
    assert_source_projection(path, "before schema-9 migration");
}

fn ids(path: &Path, query: &str) -> Vec<i64> {
    let provider = SqliteProvider::open_read_only(path).unwrap();
    assert_eq!(
        provider.database_path(),
        path,
        "completed fixture must query its original FTS, not an owned repair"
    );
    provider
        .search(&audiolib_core::SearchRequest {
            query: query.into(),
            ..Default::default()
        })
        .unwrap()
        .results
        .into_iter()
        .map(|file| file.id)
        .collect()
}

fn assert_source_projection(path: &Path, stage: &str) {
    for term in ["vendortext", "vorbistext", "embeddedtext"] {
        assert_eq!(
            ids(path, term),
            [1],
            "{stage}: missing seeded source text {term}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_and_new_readers_get_separate_snapshots_without_downgrading_the_source() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("library.db");
    fixture(&database);
    let config = ServerConfig::from_toml_str(
        "bind='127.0.0.1:0'\nlibrary_db='library.db'\nsnapshot_dir='snapshots'\nauth_mode='off'\nmanage_mode='all'\nlist_waveform_workers=1\n", root.path()).unwrap();
    let state =
        AppState::open_with_security(&config, Security::from_config(&config, None)).unwrap();
    let original = std::fs::read(&database).unwrap();
    let original_wal = std::fs::read(database.with_extension("db-wal")).ok();
    let original_revision = state.revision().unwrap().revision;
    let old = headers(false);
    let new = headers(true);
    // Official 0.8.7 sends no negotiation markers on /library.
    let (status, _, body) = request(&state, "GET", "/api/v1/library", &HeaderMap::new(), "").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let info: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(info["schema_version"], 8);
    assert_eq!(info["library_schema"], 9);
    assert_eq!(info["stats"]["audio_files"], 2);
    assert_eq!(info["snapshot_format"]["id"], PLAN_B_FORMAT);
    assert_eq!(info["library_id"], state.library_id());
    assert_eq!(info["revision"], original_revision);
    let (status, _, body) = request(&state, "GET", "/api/v1/library", &new, "").await;
    assert_eq!(status, StatusCode::OK);
    let modern_info: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(modern_info["schema_version"], 9);
    assert_eq!(modern_info["stats"]["audio_files"], 4);
    let (status, old_headers, body) =
        request(&state, "GET", "/api/v1/library/snapshot", &old, "").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(old_headers[SNAPSHOT_FORMAT_HEADER], PLAN_B_FORMAT);
    assert!(old_headers.contains_key("x-audiolib-upgrade-available"));
    let legacy = root.path().join("legacy.db");
    std::fs::write(&legacy, &body).unwrap();
    let old_body = body;
    let (_, again_headers, again_body) =
        request(&state, "GET", "/api/v1/library/snapshot", &old, "").await;
    assert_eq!(old_headers["etag"], again_headers["etag"]);
    assert_eq!(old_body, again_body);
    assert_eq!(ids(&legacy, "vendortext"), [1]);
    assert_eq!(ids(&legacy, "vorbistext"), [1]);
    assert_eq!(ids(&legacy, "embeddedtext"), [1]);
    let conn = Connection::open(&legacy).unwrap();
    let actual_ids = conn
        .prepare("SELECT id FROM audio_files ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(actual_ids, [1, 2]);
    for name in [
        "audio_dir_search",
        "audio_dir_runs",
        "audiolib_dir_dirty",
        "vendor_metadata",
        "audiolib_vendor_jobs",
    ] {
        assert!(
            !conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
                    [name],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
        );
    }
    assert_eq!(
        conn.query_row(
            "SELECT value FROM library_meta WHERE key='source_search_rev'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "2"
    );
    assert_eq!(
        conn.query_row("SELECT total_file_count FROM folders WHERE id=1", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM playlist_items", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM project_references", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let legacy_provider = SqliteProvider::open_read_only(&legacy).unwrap();
    let file = legacy_provider.get_audio(1).unwrap();
    assert_eq!(file.notes, "userannotation");
    assert_eq!(file.attributes["unknown_extension"]["retained"], true);
    drop(legacy_provider);
    let (status, native_headers, body) =
        request(&state, "GET", "/api/v1/library/snapshot", &new, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(native_headers[SNAPSHOT_FORMAT_HEADER], PLAN_B_V9_FORMAT);
    assert!(!native_headers.contains_key(audiolib_core::server_upgrade::UPGRADE_AVAILABLE_HEADER));
    assert_ne!(native_headers["etag"], old_headers["etag"]);
    let native_pin = format!(
        "/api/v1/library/snapshot?snapshot_id={}",
        native_headers["x-snapshot-id"].to_str().unwrap()
    );
    assert_eq!(
        request(&state, "GET", &native_pin, &old, "").await.0,
        StatusCode::CONFLICT
    );
    let native = root.path().join("native.db");
    std::fs::write(&native, &body).unwrap();
    assert_eq!(
        SqliteProvider::open_read_only(&native)
            .unwrap()
            .library_info()
            .unwrap()
            .stats
            .audio_files,
        4
    );
    assert_eq!(std::fs::read(&database).unwrap(), original);
    assert_eq!(
        std::fs::read(database.with_extension("db-wal")).ok(),
        original_wal
    );
    let (status, _, _) = request(
        &state,
        "PATCH",
        "/api/v1/audio/1/metadata",
        &old,
        r#"{"notes":"forbidden"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(state.revision().unwrap().revision, original_revision);
    let (status, _, _) = request(
        &state,
        "PATCH",
        "/api/v1/audio/1/metadata",
        &new,
        r#"{"notes":"freshannotation"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, newer_headers, body) =
        request(&state, "GET", "/api/v1/library/snapshot", &old, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(old_headers["etag"], newer_headers["etag"]);
    std::fs::write(root.path().join("newer.db"), body).unwrap();
    assert_eq!(ids(&root.path().join("newer.db"), "freshannotation"), [1]);
    assert_eq!(ids(&legacy, "userannotation"), [1]);
    assert_eq!(std::fs::read(&legacy).unwrap(), old_body);
    let legacy_pin = format!(
        "/api/v1/library/snapshot?snapshot_id={}",
        old_headers["x-snapshot-id"].to_str().unwrap()
    );
    let (status, _, pinned_body) = request(&state, "GET", &legacy_pin, &old, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pinned_body, old_body);
    let mut compressed_headers = old.clone();
    compressed_headers.insert("accept-encoding", HeaderValue::from_static("zstd"));
    let (status, compressed_headers, compressed) = request(
        &state,
        "GET",
        "/api/v1/library/snapshot",
        &compressed_headers,
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(compressed_headers["content-encoding"], "zstd");
    assert_eq!(
        zstd::stream::decode_all(compressed.as_slice()).unwrap(),
        std::fs::read(root.path().join("newer.db")).unwrap()
    );
    state.set_legacy_snapshots_enabled(false);
    assert_eq!(
        request(&state, "GET", "/api/v1/library", &HeaderMap::new(), "")
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&state, "GET", "/api/v1/library/snapshot", &old, "")
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&state, "GET", "/api/v1/library/snapshot", &new, "")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(ids(&legacy, "vendortext"), [1]);
    drop(conn);
    state.shutdown(Duration::from_secs(5)).unwrap();
}

async fn capabilities(state: &AppState, peer: &HeaderMap) -> audiolib_server::CapabilitiesResponse {
    let (status, _, body) = request(state, "GET", "/api/v1/capabilities", peer, "").await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog2_reports_actual_server_role_and_peer_specific_upgrade_advice() {
    use audiolib_core::{server_upgrade::*, vendor_import::VendorLimits};
    let client_catalog = compatibility_manifest();
    assert_eq!(client_catalog.catalog_revision, 2);
    assert_eq!(
        client_catalog.database_write_formats,
        [PLAN_B_FORMAT, PLAN_B_AUDIO_FORMAT]
    );
    assert!(
        client_catalog
            .snapshot_read_formats
            .iter()
            .any(|id| id == PLAN_B_V9_FORMAT)
    );
    let old = headers(false);
    let modern = headers(true);
    for schema in [8, 9] {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("library.db");
        if schema == 8 {
            fixture_schema8(&database);
        } else {
            fixture(&database);
        }
        let config = ServerConfig::from_toml_str(
            "bind='127.0.0.1:0'\nlibrary_db='library.db'\nsnapshot_dir='snapshots'\nauth_mode='off'\nmanage_mode='all'\nlist_waveform_workers=1\n", root.path()).unwrap();
        let state =
            AppState::open_with_security(&config, Security::from_config(&config, None)).unwrap();
        let actual_format = if schema == 9 {
            PLAN_B_V9_FORMAT
        } else {
            PLAN_B_FORMAT
        };
        let expected_formats = if schema == 9 {
            vec![PLAN_B_V9_FORMAT.to_owned(), PLAN_B_FORMAT.to_owned()]
        } else {
            vec![PLAN_B_FORMAT.to_owned()]
        };
        let current = capabilities(&state, &modern).await;
        assert_eq!(current.compatibility.catalog_revision, 2);
        assert_eq!(
            current.compatibility.database_write_formats,
            [actual_format]
        );
        assert_eq!(current.search_scopes, ["main", "all", "path", "filename"]);
        let source = current.library_schema.as_ref().unwrap();
        assert_eq!(source.current, Some(schema));
        assert_eq!(
            source.source_search_rev,
            Some(if schema == 9 { 3 } else { 2 })
        );
        assert_eq!(source.snapshot_formats_served, expected_formats);
        assert!(
            current.client_upgrade.is_none(),
            "a capable peer must not receive a downgrade prompt"
        );
        let vendor = current.vendor_import.as_ref().unwrap();
        let actual_limits = VendorLimits::default();
        assert_eq!(vendor.revision, 1);
        assert_eq!(vendor.max_file_bytes, actual_limits.max_file_bytes);
        assert_eq!(vendor.max_rows, actual_limits.max_rows);
        assert_eq!(
            vendor.max_row_text_bytes,
            Some(actual_limits.max_row_text_bytes)
        );
        assert_eq!(vendor.endpoint.as_deref(), Some(VENDOR_IMPORT_API));
        // The advertised importer is routed and authorized, rather than a reserved placeholder.
        assert_eq!(
            request(&state, "GET", &format!("{VENDOR_IMPORT_API}/"), &modern, "")
                .await
                .0,
            StatusCode::OK
        );
        let previous = capabilities(&state, &old).await;
        assert_eq!(
            previous.compatibility.database_write_formats,
            [actual_format]
        );
        let (status, response_headers, body) =
            request(&state, "GET", "/api/v1/library", &old, "").await;
        assert_eq!(status, StatusCode::OK);
        let info: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(info["library_schema"], schema);
        if schema == 8 {
            assert!(previous.client_upgrade.is_none());
            assert!(!response_headers.contains_key(UPGRADE_AVAILABLE_HEADER));
            assert_eq!(info["snapshot_format"]["id"], PLAN_B_FORMAT);
        } else {
            let advice = previous.client_upgrade.unwrap();
            let header = parse_upgrade_available(
                response_headers[UPGRADE_AVAILABLE_HEADER].to_str().unwrap(),
            )
            .unwrap();
            assert_eq!(advice.level, ClientUpgradeLevel::Recommended);
            assert_eq!(advice.level, header.level);
            assert_eq!(advice.min_client_version, header.version);
            assert_eq!(advice.missing_features, header.features);
            assert_eq!(
                advice.served_snapshot_format.as_deref(),
                Some(PLAN_B_FORMAT)
            );
            assert_eq!(info["schema_version"], 8);
            let (status, response_headers, body) =
                request(&state, "GET", "/api/v1/library", &modern, "").await;
            assert_eq!(status, StatusCode::OK);
            assert!(!response_headers.contains_key(UPGRADE_AVAILABLE_HEADER));
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["schema_version"],
                9
            );
            state.set_legacy_snapshots_enabled(false);
            let closed = capabilities(&state, &old).await;
            let advice = closed.client_upgrade.unwrap();
            assert_eq!(advice.level, ClientUpgradeLevel::Required);
            assert!(advice.served_snapshot_format.is_none());
            assert_eq!(
                closed.library_schema.unwrap().snapshot_formats_served,
                [PLAN_B_V9_FORMAT]
            );
            assert_eq!(
                request(&state, "GET", "/api/v1/library", &old, "").await.0,
                StatusCode::CONFLICT
            );
            let current = capabilities(&state, &modern).await;
            assert!(current.client_upgrade.is_none());
            assert_eq!(
                current.compatibility.database_write_formats,
                [PLAN_B_V9_FORMAT]
            );
            assert_eq!(
                request(&state, "GET", "/api/v1/library", &modern, "")
                    .await
                    .0,
                StatusCode::OK
            );
        }
        state.shutdown(Duration::from_secs(5)).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_legacy_projection_failure_preserves_the_native_snapshot_and_source() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("library.db");
    fixture(&database);
    // An unknown extension retains a reference to a non-WAV business row. Refuse
    // the legacy projection rather than silently deleting extension business data.
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "CREATE TABLE retained_extension(audio_id INTEGER REFERENCES audio_files(id));
        INSERT INTO retained_extension VALUES(3);",
    )
    .unwrap();
    drop(conn);
    let config = ServerConfig::from_toml_str(
        "bind='127.0.0.1:0'\nlibrary_db='library.db'\nsnapshot_dir='snapshots'\nauth_mode='off'\nlist_waveform_workers=1\n", root.path()).unwrap();
    let state =
        AppState::open_with_security(&config, Security::from_config(&config, None)).unwrap();
    let before = std::fs::read(&database).unwrap();
    let (status, _, _) = request(
        &state,
        "GET",
        "/api/v1/library/snapshot",
        &headers(false),
        "",
    )
    .await;
    assert!(status.is_server_error());
    let (status, response_headers, body) = request(
        &state,
        "GET",
        "/api/v1/library/snapshot",
        &headers(true),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response_headers[SNAPSHOT_FORMAT_HEADER], PLAN_B_V9_FORMAT);
    let native = root.path().join("native.db");
    std::fs::write(&native, body).unwrap();
    assert_eq!(
        SqliteProvider::open_read_only(&native)
            .unwrap()
            .library_info()
            .unwrap()
            .stats
            .audio_files,
        4
    );
    assert_eq!(std::fs::read(&database).unwrap(), before);
    assert_eq!(
        state.provider().get_audio(3).unwrap().filename,
        "three.flac"
    );
    state.shutdown(Duration::from_secs(5)).unwrap();
}
