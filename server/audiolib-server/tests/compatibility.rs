// server/audiolib-server/tests/compatibility.rs
//! Real router and remote-client negotiation against an owned temporary library.
use audiolib_core::{DataProvider, MetadataPatch, SearchRequest, compatibility::PLAN_B_FORMAT};
use audiolib_db::SqliteProvider;
use audiolib_remote::{CacheUpdate, RemoteCache, RemoteClient, RemoteWriter, sync_library};
use audiolib_server::{AppState, BearerToken, ServeOptions, ServerConfig, router, serve};
use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use std::{sync::atomic::AtomicBool, time::Duration};
use tower::ServiceExt;

#[tokio::test]
async fn mutation_admission_rejects_invalid_peer_catalogs_without_writing() {
    use audiolib_core::compatibility::{
        CLIENT_COMPATIBILITY_HEADER, PLAN_B_INITIAL_FORMAT, compatibility_manifest,
    };
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("library.db");
    drop(SqliteProvider::open(&database).unwrap());
    let conn = rusqlite::Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute("INSERT INTO audio_files(id,file_path,filename,user_notes) VALUES(1,'owned.wav','owned.wav','preserved')", []).unwrap();
    drop(conn);
    let config = ServerConfig::from_toml_str(
        "bind='127.0.0.1:0'\nlibrary_db='library.db'\nsnapshot_dir='snapshots'\nauth=true\n",
        root.path(),
    )
    .unwrap();
    let token = "compatibility-test-token-0123456789";
    let state = AppState::open(&config, BearerToken::new(token).unwrap()).unwrap();
    let revision = state.revision().unwrap().revision;
    let settings_path = config.admin_dir().join("admin-settings.json");
    let original_settings = std::fs::read(&settings_path).ok();
    let conn = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let data_version: i64 = conn
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .unwrap();
    for mode in [
        "unmarked",
        "protocol_newer",
        "protocol",
        "malformed",
        "duplicate",
        "oversized",
    ] {
        let mut catalog = compatibility_manifest();
        match mode {
            // Marker peers of any version are admitted; unmarked or protocol-disjoint
            // peers are not.
            "unmarked" => catalog.marker_revision = None,
            "protocol_newer" => {
                catalog.protocol =
                    Some(audiolib_core::compatibility::ProtocolRange { min: 2, max: 2 });
                catalog.api_protocols = vec!["api/v2".into()];
            }
            "protocol" => catalog.api_protocols.clear(),
            _ => {}
        }
        let value = match mode {
            "malformed" => "{}".into(),
            "oversized" => "x".repeat(4097),
            _ => serde_json::to_string(&catalog).unwrap(),
        };
        for (path, body) in [
            ("/api/v1/audio/1/metadata", r#"{"notes":"must not apply"}"#),
            ("/api/v1/settings", r#"{"list_waveform_rate":20}"#),
        ] {
            let mut request = Request::patch(path)
                .header("Authorization", format!("Bearer {token}"))
                .header("x-audiolib-library-id", state.library_id())
                .header("Content-Type", "application/json")
                .header(CLIENT_COMPATIBILITY_HEADER, &value);
            if mode == "duplicate" {
                request = request.header(CLIENT_COMPATIBILITY_HEADER, &value);
            }
            let response = router(state.clone())
                .oneshot(request.body(Body::from(body)).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT, "{mode} {path}");
            let body: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            let expected = match mode {
                "unmarked" => "PEER_UPGRADE_REQUIRED",
                "protocol_newer" => "PROTOCOL_UNSUPPORTED",
                _ => "PEER_CATALOG_INVALID",
            };
            assert_eq!(body["error_code"], expected, "{mode} {path}");
            if mode == "protocol_newer" {
                assert_eq!(body["sub_code"], "SERVER_UPGRADE_REQUIRED", "{path}");
            }
            assert_eq!(std::fs::read(&settings_path).ok(), original_settings);
            assert_eq!(state.revision().unwrap().revision, revision);
            assert_eq!(
                conn.pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
                    .unwrap(),
                data_version
            );
            assert_eq!(
                conn.query_row("SELECT user_notes FROM audio_files WHERE id=1", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
                "preserved"
            );
        }
    }
    // A valid protocol/version handshake is insufficient when the caller cannot
    // read the library's complete current format. Exercise every business-write
    // route through the real middleware, including the untimed POST /scans route.
    let root_body = serde_json::json!({"name":"Extra", "path":root.path()}).to_string();
    let path_body = serde_json::json!({"path":root.path()}).to_string();
    let mutations = [
        (
            Method::PATCH,
            "/api/v1/audio/1/metadata",
            r#"{"notes":"must not apply"}"#,
        ),
        (
            Method::POST,
            "/api/v1/audio/metadata/batch",
            r#"{"ids":[1],"patch":{"favorite":true}}"#,
        ),
        (Method::POST, "/api/v1/audio/remove", r#"{"ids":[1]}"#),
        (Method::DELETE, "/api/v1/folders/1", ""),
        (Method::POST, "/api/v1/roots", root_body.as_str()),
        (Method::PATCH, "/api/v1/roots/Existing", path_body.as_str()),
        (Method::DELETE, "/api/v1/roots/Existing", ""),
        (Method::POST, "/api/v1/scans", r#"{"root":"Existing"}"#),
        (Method::POST, "/api/v1/scans/1/pause", ""),
        (Method::POST, "/api/v1/scans/1/resume", ""),
        (Method::POST, "/api/v1/scans/1/cancel", ""),
    ];
    for (mode, code) in [
        ("missing", "PEER_UPGRADE_REQUIRED"),
        ("p0_only", "SNAPSHOT_FORMAT_UNSUPPORTED"),
        ("future_format", "SNAPSHOT_FORMAT_UNSUPPORTED"),
        ("unmarked_program", "PEER_UPGRADE_REQUIRED"),
    ] {
        let mut catalog = compatibility_manifest();
        match mode {
            "p0_only" => catalog.snapshot_read_formats = vec![PLAN_B_INITIAL_FORMAT.into()],
            "future_format" => catalog.snapshot_read_formats = vec!["ale-s99-future".into()],
            "unmarked_program" => catalog.marker_revision = None,
            _ => {}
        }
        if matches!(mode, "p0_only" | "future_format") {
            assert!(audiolib_core::compatibility::peer_versions_compatible(
                &compatibility_manifest(),
                &catalog.application_version,
                Some(&catalog),
            ));
        }
        let value = serde_json::to_string(&catalog).unwrap();
        for (method, path, body) in &mutations {
            let mut request = Request::builder()
                .method(method.clone())
                .uri(*path)
                .header("Authorization", format!("Bearer {token}"))
                .header("x-audiolib-library-id", state.library_id())
                .header("Content-Type", "application/json");
            if mode != "missing" {
                request = request.header(CLIENT_COMPATIBILITY_HEADER, &value);
            }
            let response = router(state.clone())
                .oneshot(request.body(Body::from((*body).to_owned())).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::CONFLICT,
                "{mode} {method} {path}"
            );
            let response: serde_json::Value =
                serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(response["error_code"], code, "{mode} {method} {path}");
            assert!(
                response["message"].as_str().unwrap().contains("update")
                    || response["message"].as_str().unwrap().contains("upgrade")
            );
            assert_eq!(std::fs::read(&settings_path).ok(), original_settings);
            assert_eq!(state.revision().unwrap().revision, revision);
            assert_eq!(
                conn.pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
                    .unwrap(),
                data_version
            );
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM audio_files", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert_eq!(
                conn.query_row("SELECT user_notes FROM audio_files WHERE id=1", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
                "preserved"
            );
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM scan_sessions", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
            assert!(state.roots_response().unwrap().is_empty());
        }
    }
    // Existing headerless JSON clients retain their compatibility endpoint.
    let response = router(state.clone())
        .oneshot(
            Request::post("/api/v1/audio/1/plays")
                .header("Authorization", format!("Bearer {token}"))
                .header("x-audiolib-library-id", state.library_id())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["file"]["id"], 1);
    assert!(body["file"].get("play_count").is_none());
    for (id, status, code) in [
        ("x", StatusCode::BAD_REQUEST, "VALIDATION"),
        ("999999", StatusCode::NOT_FOUND, "NOT_FOUND"),
    ] {
        let response = router(state.clone())
            .oneshot(
                Request::post(format!("/api/v1/audio/{id}/plays"))
                    .header("Authorization", format!("Bearer {token}"))
                    .header("x-audiolib-library-id", state.library_id())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["error_code"], code);
    }
    let response = router(state.clone())
        .oneshot(
            Request::get("/api/v1/history/plays")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body, serde_json::json!([]));
    assert_eq!(state.revision().unwrap().revision, revision);
    assert_eq!(
        conn.pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        data_version
    );

    // Authentication and management policy still take precedence over a bad peer
    // declaration. No settings or database write occurs on either refusal.
    for (authenticated, manage_mode, status, code) in [
        (
            false,
            audiolib_server::ManageMode::All,
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
        ),
        (
            true,
            audiolib_server::ManageMode::Off,
            StatusCode::FORBIDDEN,
            "MANAGE_FORBIDDEN",
        ),
    ] {
        state
            .security()
            .update_settings(|settings| settings.manage_mode = Some(manage_mode))
            .unwrap();
        let before_settings = std::fs::read(&settings_path).unwrap();
        for (method, path, body) in [
            (
                Method::PATCH,
                "/api/v1/settings",
                r#"{"list_waveform_rate":20}"#,
            ),
            (Method::POST, "/api/v1/audio/remove", r#"{"ids":[1]}"#),
            (Method::POST, "/api/v1/scans", r#"{"root":"Existing"}"#),
        ] {
            for catalog in [Some("{}"), None] {
                let mut request = Request::builder()
                    .method(method.clone())
                    .uri(path)
                    .header("Content-Type", "application/json")
                    .header("x-audiolib-library-id", state.library_id());
                if let Some(catalog) = catalog {
                    request = request.header(CLIENT_COMPATIBILITY_HEADER, catalog);
                }
                if authenticated {
                    request = request.header("Authorization", format!("Bearer {token}"));
                }
                let response = router(state.clone())
                    .oneshot(request.body(Body::from(body)).unwrap())
                    .await
                    .unwrap();
                assert_eq!(response.status(), status);
                let body: serde_json::Value = serde_json::from_slice(
                    &response.into_body().collect().await.unwrap().to_bytes(),
                )
                .unwrap();
                assert_eq!(body["error_code"], code);
                assert_eq!(std::fs::read(&settings_path).unwrap(), before_settings);
                assert_eq!(state.revision().unwrap().revision, revision);
                assert_eq!(
                    conn.pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
                        .unwrap(),
                    data_version
                );
            }
        }
    }
    state
        .security()
        .update_settings(|settings| settings.manage_mode = Some(audiolib_server::ManageMode::All))
        .unwrap();
    // Both the explicit legacy exception and a supported modern peer may change
    // settings. These configuration writes must not bump the library revision.
    for (modern, rate) in [(false, 20), (true, 21)] {
        let mut request = Request::patch("/api/v1/settings")
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json");
        if modern {
            request = request.header(
                CLIENT_COMPATIBILITY_HEADER,
                serde_json::to_string(&compatibility_manifest()).unwrap(),
            );
        }
        let response = router(state.clone())
            .oneshot(
                request
                    .body(Body::from(
                        serde_json::json!({"list_waveform_rate": rate}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "modern={modern}");
        let body: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["list_waveform_rate"], rate);
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
        assert_eq!(persisted["list_waveform_rate"], rate);
        assert_eq!(state.revision().unwrap().revision, revision);
        assert_eq!(
            conn.pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            data_version
        );
    }
    // A modern peer that explicitly reads the actual p1 format may write and can
    // deserialize the response after the schema-v6 playback fields were retired.
    let response = router(state.clone())
        .oneshot(
            Request::patch("/api/v1/audio/1/metadata")
                .header("Authorization", format!("Bearer {token}"))
                .header("x-audiolib-library-id", state.library_id())
                .header("Content-Type", "application/json")
                .header(
                    CLIENT_COMPATIBILITY_HEADER,
                    serde_json::to_string(&compatibility_manifest()).unwrap(),
                )
                .body(Body::from(r#"{"notes":"modern write applied"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["file"]["notes"], "modern write applied");
    assert!(body["file"].get("play_count").is_none());
    let decoded: audiolib_core::AudioFile = serde_json::from_value(body["file"].clone()).unwrap();
    assert_eq!(decoded.notes, "modern write applied");
    assert_eq!(state.revision().unwrap().revision, revision + 1);
    assert!(
        conn.pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
            .unwrap()
            > data_version
    );
    assert_eq!(
        conn.query_row("SELECT user_notes FROM audio_files WHERE id=1", [], |row| {
            row.get::<_, String>(0)
        })
        .unwrap(),
        "modern write applied"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_client_is_refused_before_preparation_and_modern_client_syncs_actual_format() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("library.db");
    let provider = SqliteProvider::open(&database).unwrap();
    let conn = rusqlite::Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute(
        "INSERT INTO audio_files(id,file_path,filename,user_tags) VALUES(1,'owned.wav','metal.wav','privateannotation')",
        [],
    ).unwrap();
    drop(conn);
    drop(provider);
    let config = ServerConfig::from_toml_str(
        "bind='127.0.0.1:0'\nlibrary_db='library.db'\nsnapshot_dir='snapshots'\nauth=true\n",
        root.path(),
    )
    .unwrap();
    let token = "compatibility-test-token-0123456789";
    let state = AppState::open(&config, BearerToken::new(token).unwrap()).unwrap();
    let response = router(state.clone())
        .oneshot(
            Request::get("/api/v1/library/snapshot")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    // No catalog: the peer check runs first and names the client as too old.
    assert_eq!(body["error_code"], "PEER_UPGRADE_REQUIRED");
    assert_eq!(
        std::fs::read_dir(root.path().join("snapshots")).map_or(0, |entries| entries.count()),
        0
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(serve(
        listener,
        router(state.clone()),
        async {
            let _ = stopped.await;
        },
        ServeOptions {
            shutdown_grace: Duration::from_secs(2),
            ..Default::default()
        },
    ));
    let data = root.path().join("client");
    tokio::task::spawn_blocking(move || {
        let client = RemoteClient::new(&url, token).unwrap();
        let remote = client.library().unwrap();
        assert_eq!(remote.snapshot_format.as_ref().unwrap().id, PLAN_B_FORMAT);
        let capabilities = client.capabilities().unwrap();
        assert_eq!(
            capabilities.compatibility.unwrap().application_version,
            env!("CARGO_PKG_VERSION")
        );
        let observer = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let before: i64 = observer
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .unwrap();
        let cache = RemoteCache::new(&data);
        let outcome =
            sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
        assert!(outcome.downloaded);
        assert_eq!(outcome.state.schema_version, audiolib_db::SCHEMA_VERSION);
        let format = outcome.state.format.as_ref().unwrap();
        assert_eq!(format.source.as_ref(), Some(&format.local));
        assert_eq!(format.generation, 0);
        let cached = SqliteProvider::open_read_only(&cache.database_path(&outcome.state)).unwrap();
        assert_eq!(
            cached
                .search(&SearchRequest {
                    query: "privateannotation".into(),
                    ..Default::default()
                })
                .unwrap()
                .total_count,
            1
        );
        let value = serde_json::to_value(cached.get_audio(1).unwrap()).unwrap();
        assert!(value.get("play_count").is_none());
        assert!(value["attributes"].get("last_played_at").is_none());
        assert_eq!(client.library().unwrap().revision, remote.revision);
        assert_eq!(
            observer
                .pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            before
        );
        drop(cached);
        let second =
            sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
        assert!(!second.downloaded);
        assert_eq!(outcome.state.revision, second.state.revision);
        // Equal server revisions do not authorize reuse of an unproven source format.
        let mut obsolete = second.state.clone();
        obsolete
            .format
            .as_mut()
            .unwrap()
            .source
            .as_mut()
            .unwrap()
            .features
            .insert("tokenization".into(), 2);
        cache.save_state(&obsolete).unwrap();
        let refreshed =
            sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
        assert!(refreshed.downloaded);
        assert_eq!(refreshed.state.revision, second.state.revision);
        assert_eq!(refreshed.state.format, second.state.format);
        let writer = RemoteWriter::new(client.clone(), cache.clone(), &remote.library_id);
        let changed = writer
            .update_metadata(
                1,
                &MetadataPatch {
                    notes: Some("retainedmigrationannotation".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            changed.cache,
            CacheUpdate::Applied {
                revision: changed.revision
            }
        );
        assert_eq!(changed.revision, refreshed.state.revision + 1);
        let patched = cache.active().unwrap();
        assert_eq!(patched.format, refreshed.state.format);
        let reader = SqliteProvider::open_read_only(&cache.database_path(&patched)).unwrap();
        assert_eq!(
            reader
                .search(&SearchRequest {
                    query: "retainedmigrationannotation".into(),
                    ..Default::default()
                })
                .unwrap()
                .total_count,
            1
        );
        let third =
            sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
        assert!(!third.downloaded);
        assert_eq!(third.state.revision, changed.revision);
    })
    .await
    .unwrap();
    let _ = stop.send(());
    task.await.unwrap();
}
