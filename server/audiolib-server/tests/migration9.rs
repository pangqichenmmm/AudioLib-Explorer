// server/audiolib-server/tests/migration9.rs
//! Migration wire contract and authorization on isolated libraries, without a listener.
use audiolib_db::{
    SqliteProvider,
    migration9::{Report, State},
};
use audiolib_server::{AppState, Security, ServerConfig, router};
use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    response::Response,
};
use http_body_util::BodyExt;
use std::{fs, path::PathBuf};
use tower::ServiceExt;

fn fixture(manage: &str) -> (tempfile::TempDir, PathBuf, AppState) {
    fixture_auth(manage, "auth_mode='off'\n")
}
fn fixture_auth(manage: &str, auth: &str) -> (tempfile::TempDir, PathBuf, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("library.db");
    drop(SqliteProvider::open(&db).unwrap());
    let config=ServerConfig::from_toml_str(&format!("bind='127.0.0.1:0'\nlibrary_db='library.db'\n{auth}manage_mode='{manage}'\nlist_waveform_workers=1\n"),dir.path()).unwrap();
    let security = Security::from_config(&config, None);
    let state = AppState::open_with_security(&config, security).unwrap();
    (dir, db, state)
}
async fn request(state: &AppState, method: Method, path: &str, marked: bool) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost");
    if marked {
        request = request.header("x-audiolib-admin", "1");
    }
    // Router-level LAN fixture; native loopback/proxy classification has separate
    // security.rs coverage. No network connection is made by this helper.
    let request = request.body(Body::empty()).unwrap();
    router(state.clone()).oneshot(request).await.unwrap()
}
async fn json(response: Response) -> serde_json::Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}
fn record(db: &std::path::Path, state: &AppState, report: Report) {
    let mut name = db.as_os_str().to_os_string();
    name.push(".migration9.json");
    fs::write(std::path::Path::new(&name),serde_json::to_vec(&serde_json::json!({"protocol":1,"library_id":state.provider().album_namespace(),"backup":null,"report":report})).unwrap()).unwrap();
}

#[tokio::test]
async fn admin_requires_manage_and_csrf_guard() {
    let (_dir, _db, state) = fixture("off");
    assert_eq!(
        request(&state, Method::GET, "/admin/api/migration/status", true)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(&state, Method::POST, "/admin/api/migration/start", true)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    state.shutdown(std::time::Duration::from_secs(5)).unwrap();
    let (_dir, _db, state) = fixture("all");
    assert_eq!(
        request(&state, Method::POST, "/admin/api/migration/start", false)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let status = request(&state, Method::GET, "/admin/api/migration/status", true).await;
    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(json(status).await["state"], "idle");
    state.shutdown(std::time::Duration::from_secs(5)).unwrap();
}

#[tokio::test]
async fn default_auth_does_not_expose_migration_admin_on_native_loopback() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (_dir, _db, state) = fixture_auth("all", "");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(audiolib_server::serve(
        listener,
        router(state.clone()),
        async {
            let _ = stopped.await;
        },
        audiolib_server::ServeOptions::default(),
    ));
    let mut replies = Vec::new();
    for (method, path) in [
        ("GET", "/api/v1/health"),
        ("GET", "/admin/api/migration/status"),
        ("POST", "/admin/api/migration/start"),
    ] {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        // Even a correctly marked native request must not bypass the LAN gate:
        // a local reverse proxy can produce exactly the same socket peer.
        let request = format!(
            "{method} {path} HTTP/1.0\r\nHost: {address}\r\nX-AudioLib-Admin: 1\r\nContent-Length: 0\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut reply = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            stream.read_to_end(&mut reply),
        )
        .await
        .unwrap()
        .unwrap();
        replies.push(String::from_utf8(reply).unwrap());
    }
    stop.send(()).unwrap();
    task.await.unwrap();
    state.shutdown(std::time::Duration::from_secs(5)).unwrap();
    assert!(replies[0].starts_with("HTTP/1.0 200"), "{}", replies[0]);
    for reply in &replies[1..] {
        assert!(reply.starts_with("HTTP/1.0 403"), "{reply}");
        let (_, body) = reply.split_once("\r\n\r\n").unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["error_code"], "ADMIN_LAN_ONLY");
    }
}

#[tokio::test]
async fn auth_off_native_loopback_is_limited_to_migration_and_manage_permission() {
    use audiolib_server::{ConnectionInfo, Listener};
    let (_dir, _db, state) = fixture("all");
    let cases = [
        (
            "/admin/api/migration/status",
            Method::GET,
            "",
            "",
            StatusCode::OK,
        ),
        (
            "/admin/api/migration/rollback",
            Method::POST,
            "content-type",
            "application/json",
            StatusCode::CONFLICT,
        ),
        (
            "/admin/api/status",
            Method::GET,
            "",
            "",
            StatusCode::FORBIDDEN,
        ),
        (
            "/admin/api/migration/status",
            Method::GET,
            "forwarded",
            "for=127.0.0.1",
            StatusCode::FORBIDDEN,
        ),
        (
            "/admin/api/migration/status",
            Method::GET,
            "x-forwarded-for",
            "127.0.0.1",
            StatusCode::FORBIDDEN,
        ),
        (
            "/admin/api/migration/status",
            Method::GET,
            "origin",
            "http://localhost",
            StatusCode::FORBIDDEN,
        ),
        (
            "/admin/api/migration/status",
            Method::GET,
            "sec-fetch-site",
            "same-origin",
            StatusCode::FORBIDDEN,
        ),
        (
            "/admin/api/migration/rollback",
            Method::POST,
            "content-type",
            "text/plain",
            StatusCode::FORBIDDEN,
        ),
        (
            "/admin/api/migration/status",
            Method::GET,
            "host",
            "public.example.com",
            StatusCode::FORBIDDEN,
        ),
    ];
    for (path, method, name, value, expected) in cases {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost");
        if !name.is_empty() {
            request = request.header(name, value);
        }
        let mut request = request.body(Body::empty()).unwrap();
        request.extensions_mut().insert(ConnectionInfo {
            peer: "127.0.0.1:4000".parse().unwrap(),
            listener: Listener::Http,
            local: Some("127.0.0.1:4001".parse().unwrap()),
        });
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), expected, "{path} {name}");
    }
    state.shutdown(std::time::Duration::from_secs(5)).unwrap();
    for (manage, auth, local) in [
        ("off", "auth_mode='off'\n", "127.0.0.1:4001"),
        ("lan", "auth_mode='off'\n", "127.0.0.1:4001"),
        ("all", "auth_mode='all'\n", "127.0.0.1:4001"),
        ("all", "auth_mode='off'\n", "192.0.2.1:4001"),
    ] {
        let (_dir, _db, state) = fixture_auth(manage, auth);
        let mut request = Request::get("/admin/api/migration/status")
            .header("host", "localhost")
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ConnectionInfo {
            peer: "127.0.0.1:4000".parse().unwrap(),
            listener: Listener::Http,
            local: Some(local.parse().unwrap()),
        });
        assert_eq!(
            router(state.clone())
                .oneshot(request)
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN,
            "{manage} {auth} {local}"
        );
        state.shutdown(std::time::Duration::from_secs(5)).unwrap();
    }
}

#[tokio::test]
async fn paused_reads_scopes_and_switching_writes_follow_http_schema() {
    let (_dir, db, state) = fixture("all");
    record(
        &db,
        &state,
        Report {
            state: State::Paused,
            ..Default::default()
        },
    );
    let main = request(&state, Method::GET, "/api/v1/search?q=wind", false).await;
    assert_eq!(main.status(), StatusCode::OK);
    assert_eq!(main.headers()["x-audiolib-migration"], "paused");
    assert!(!main.headers().contains_key("x-audiolib-schema-migration"));
    let all = request(
        &state,
        Method::GET,
        "/api/v1/search?q=wind&scope=all",
        false,
    )
    .await;
    assert_eq!(all.status(), StatusCode::OK);
    assert_eq!(all.headers()["x-audiolib-path-search"], "unavailable");
    assert_eq!(json(main).await, json(all).await);
    let path = request(
        &state,
        Method::GET,
        "/api/v1/search?q=wind&scope=path",
        false,
    )
    .await;
    assert_eq!(path.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(path.headers()["retry-after"], "30");
    assert_eq!(json(path).await["error_code"], "migration_in_progress");
    assert_eq!(
        request(&state, Method::POST, "/api/vendor-import", false)
            .await
            .status(),
        // The implemented history resource is GET-only. Its POST is not an import.
        StatusCode::METHOD_NOT_ALLOWED
    );
    let mut lock = db.as_os_str().to_os_string();
    lock.push(".migration9.lock");
    let owner = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(std::path::Path::new(&lock))
        .unwrap();
    owner.try_lock().unwrap();
    record(
        &db,
        &state,
        Report {
            state: State::Switching,
            ..Default::default()
        },
    );
    let write = request(&state, Method::PATCH, "/api/v1/audio/1/metadata", false).await;
    assert_eq!(write.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(write.headers()["retry-after"], "30");
    assert_eq!(write.headers()["x-audiolib-migration"], "switching");
    assert_eq!(json(write).await["error_code"], "migration_switching");
    let vendor = request(&state, Method::POST, "/api/vendor-import/batch", false).await;
    assert_eq!(vendor.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(vendor.headers()["retry-after"], "30");
    assert_eq!(vendor.headers()["x-audiolib-migration"], "switching");
    assert_eq!(json(vendor).await["error_code"], "migration_switching");
    assert_eq!(
        request(&state, Method::GET, "/api/v1/search?q=wind", false)
            .await
            .status(),
        StatusCode::OK
    );
    drop(owner);
    let report = audiolib_db::migration9::execute(
        &db,
        audiolib_db::migration9::Action::Start,
        &Default::default(),
        &std::sync::atomic::AtomicBool::new(false),
    );
    assert_eq!(report.state, State::Done, "{report:?}");
    let after = request(
        &state,
        Method::GET,
        "/api/v1/search?q=wind&scope=path",
        false,
    )
    .await;
    assert_eq!(after.status(), StatusCode::OK);
    assert!(!after.headers().contains_key("x-audiolib-migration"));
    let rollback = request(&state, Method::POST, "/admin/api/migration/rollback", true).await;
    assert_eq!(rollback.status(), StatusCode::OK);
    assert_eq!(json(rollback).await["state"], "rolled_back");
    state.shutdown(std::time::Duration::from_secs(5)).unwrap();
}

#[tokio::test]
async fn migration_search_response_cache_observes_own_and_external_writes() {
    use audiolib_core::{DataProvider, MetadataPatch};
    let (_dir, db, state) = fixture("all");
    let connection = rusqlite::Connection::open(&db).unwrap();
    audiolib_db::register_library_functions(&connection).unwrap();
    connection.execute("INSERT INTO audio_files(id,file_path,path_key,filename,user_notes,metadata_json) VALUES(1,'wind.wav','wind.wav','wind.wav','before','{}')", []).unwrap();
    record(
        &db,
        &state,
        Report {
            state: State::Paused,
            ..Default::default()
        },
    );
    let main = json(request(&state, Method::GET, "/api/v1/search?q=wind", false).await).await;
    let all = json(
        request(
            &state,
            Method::GET,
            "/api/v1/search?q=wind&scope=all",
            false,
        )
        .await,
    )
    .await;
    assert_eq!(main, all);
    assert_eq!(all["results"][0]["notes"], "before");
    state
        .provider()
        .update_metadata(
            1,
            &MetadataPatch {
                notes: Some("own write".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let own = json(
        request(
            &state,
            Method::GET,
            "/api/v1/search?q=wind&scope=all",
            false,
        )
        .await,
    )
    .await;
    assert_eq!(own["results"][0]["notes"], "own write");
    connection
        .execute(
            "UPDATE audio_files SET user_notes='external write' WHERE id=1",
            [],
        )
        .unwrap();
    let external = json(request(&state, Method::GET, "/api/v1/search?q=wind", false).await).await;
    assert_eq!(external["results"][0]["notes"], "external write");
    assert_eq!(
        external,
        json(
            request(
                &state,
                Method::GET,
                "/api/v1/search?q=wind&scope=all",
                false
            )
            .await
        )
        .await
    );
    state.shutdown(std::time::Duration::from_secs(5)).unwrap();
}
