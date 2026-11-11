// server/audiolib-server/tests/api.rs
//! End-to-end router tests against temporary libraries built through the normal db APIs.
//! Everything lives in per-test temporary directories; no real library or audio is touched.
mod support;
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use audiolib_server::{
    AppState, AudioWriteResponse, AuthMode, BatchWriteResponse, BearerToken, ConnectionInfo,
    ErrorBody, LibraryResponse, Listener, RetryPolicy, RootMonitor, RootPolicy, RootProbe,
    RootResponse, RootState, ServeOptions, ServerConfig, Service, open_in_background, router,
    router_with_request_timeout, serve, spawn_revision_poller,
};
use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use rusqlite::{Connection, OpenFlags};
use std::{fs, path::Path, sync::Arc, time::Duration};
use tower::ServiceExt;

const TOKEN: &str = "test-token-0123456789abcdef0123456789abcdef";

/// Write a tiny silent 16-bit mono WAV; never opens an output device.
fn wav(path: &Path) {
    let data = [0_u8; 8];
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.extend_from_slice(&96_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&data);
    fs::write(path, bytes).unwrap();
}

struct Fixture {
    dir: tempfile::TempDir,
    db: std::path::PathBuf,
    writer: Arc<SqliteProvider>,
    config: ServerConfig,
    state: AppState,
}

/// Fixture with bearer authentication enabled (`auth = true`).
fn fixture() -> Fixture {
    fixture_with("auth = true\n")
}

/// Fixture whose configuration gets `extra` top-level TOML lines (e.g. `max_streams = 1`).
/// Canonical path without a Windows verbatim prefix, like the library stores paths, so
/// configured roots match library folders lexically (macOS temp dirs sit behind the
/// `/var` -> `/private/var` symlink).
fn real_path(path: &Path) -> std::path::PathBuf {
    let canonical = path.canonicalize().unwrap();
    let text = canonical.to_string_lossy().into_owned();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => rest.into(),
        _ => canonical,
    }
}

fn fixture_with(extra: &str) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = real_path(dir.path());
    let sounds = base.join("sounds");
    fs::create_dir_all(sounds.join("Aircraft")).unwrap();
    fs::create_dir_all(dir.path().join("unscanned")).unwrap();
    wav(&sounds.join("Aircraft").join("flyby_01.wav"));
    wav(&sounds.join("Aircraft").join("flyby_02.wav"));
    wav(&sounds.join("door.wav"));
    let db = dir.path().join("library").join("library.db");
    let writer = Arc::new(SqliteProvider::open(&db).unwrap());
    let progress = ScanService::new(writer.clone())
        .scan(
            &sounds,
            &ScanOptions {
                incremental: false,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(progress.added, 3, "{progress:?}");
    let config = ServerConfig::from_toml_str(
        &format!(
            "bind = \"127.0.0.1:0\"\nlibrary_db = 'library/library.db'\nsnapshot_dir = 'snapshots'\n{extra}\
             [[roots]]\nname = \"SFX_Main\"\nserver_path = '{}'\nwebdav_url = \"http://nas.example.lan:8080/SFX_Main\"\n\
             [[roots]]\nname = \"Aircraft\"\nserver_path = '{}'\n\
             [[roots]]\nname = \"Unscanned\"\nserver_path = '{}'\n",
            sounds.display(),
            sounds.join("Aircraft").display(),
            base.join("unscanned").display(),
        ),
        dir.path(),
    )
    .unwrap();
    let state = AppState::open(&config, BearerToken::new(TOKEN).unwrap()).unwrap();
    Fixture {
        dir,
        db,
        writer,
        config,
        state,
    }
}

async fn send(state: &(impl Into<Service> + Clone), path: &str, token: Option<&str>) -> Response {
    let mut request = support::modern(Request::get(path));
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn bytes(response: Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

async fn json<T: serde::de::DeserializeOwned>(response: Response) -> T {
    serde_json::from_slice(&bytes(response).await).unwrap()
}

#[tokio::test]
async fn health_is_public_and_reports_crate_version() {
    let f = fixture();
    let response = send(&f.state, "/api/v1/health", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = json(response).await;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    // Existing fields stay: status + version + the nullable `public_url`.
    assert!(body["public_url"].is_null(), "{body}");
    // Added fields: API, schema and build commit.
    assert_eq!(body["api_version"], "v1");
    assert_eq!(body["schema_version"], audiolib_db::SCHEMA_VERSION);
    let commit = body["commit"].as_str().expect("commit is a string");
    assert!(
        commit == "unknown" || (commit.len() >= 7 && commit.chars().all(|c| c.is_ascii_hexdigit())),
        "{body}"
    );
    assert_eq!(body["compatibility_catalog_revision"], 2);
    assert_eq!(body.as_object().unwrap().len(), 7, "{body}");
}

#[tokio::test]
async fn protected_endpoints_reject_missing_or_wrong_tokens() {
    let f = fixture();
    for path in [
        "/api/v1/library",
        "/api/v1/library/snapshot",
        "/api/v1/roots",
        "/api/v1/audio/1",
        "/api/v1/audio/1/stream",
    ] {
        for token in [None, Some("wrong-token-0123456789abcdef")] {
            let response = send(&f.state, path, token).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
            assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
            let body: ErrorBody = json(response).await;
            assert_eq!(body.error_code, "UNAUTHORIZED");
            assert!(!body.message.contains(TOKEN));
        }
    }
    // Basic auth with the right secret is still not a bearer token.
    let response = router(f.state.clone())
        .oneshot(
            Request::get("/api/v1/roots")
                .header(header::AUTHORIZATION, format!("Basic {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_routes_return_json_not_found() {
    let f = fixture();
    let response = send(&f.state, "/api/v1/nope", Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: ErrorBody = json(response).await;
    assert_eq!(body.error_code, "NOT_FOUND");
}

#[tokio::test]
async fn wrong_methods_get_json_405_with_allow_header() {
    let f = fixture();
    for (path, token) in [
        ("/api/v1/health", None),
        ("/api/v1/library", Some(TOKEN)),
        ("/api/v1/library", None),
        ("/api/v1/audio/1/stream", Some(TOKEN)),
    ] {
        let mut request = Request::post(path);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = router(f.state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "{path}");
        let allow = header_str(&response, header::ALLOW).to_string();
        assert!(allow.contains("GET"), "{path}: {allow}");
        assert!(header_str(&response, header::CONTENT_TYPE).starts_with("application/json"));
        let body: ErrorBody = json(response).await;
        assert_eq!(body.error_code, "METHOD_NOT_ALLOWED", "{path}");
    }
}

#[tokio::test]
async fn library_reports_identity_schema_and_monotonic_revision() {
    let f = fixture();
    let first: LibraryResponse = json(send(&f.state, "/api/v1/library", Some(TOKEN)).await).await;
    assert!(first.identity_bound_writes);
    assert!(!first.library_id.is_empty());
    assert_eq!(first.info.schema_version, audiolib_db::SCHEMA_VERSION);
    assert_eq!(first.info.stats.audio_files, 3);
    assert!(first.revision >= 1);
    assert_eq!(
        first.info.database,
        std::path::PathBuf::from(first.info.database.file_name().unwrap()),
        "GET /library must expose only the database file name"
    );
    assert!(!first.info.database.is_absolute());
    let raw: serde_json::Value = json(send(&f.state, "/api/v1/library", Some(TOKEN)).await).await;
    for field in [
        "library_id",
        "revision",
        "schema_version",
        "sqlite_version",
        "stats",
        "database_bytes",
    ] {
        assert!(raw.get(field).is_some(), "missing {field} in {raw}");
    }
    let database = raw["database"].as_str().expect("database is a string");
    assert!(!database.is_empty());
    assert!(
        !database.contains('/') && !database.contains('\\'),
        "{database}"
    );
    assert_eq!(
        raw["revision"].as_u64(),
        Some(first.revision),
        "no change, no bump"
    );

    // A commit from another connection is detected on the next /library call.
    let id: i64 = Connection::open_with_flags(&f.db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row(
            "SELECT id FROM audio_files ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    audiolib_core::DataProvider::update_metadata(
        f.writer.as_ref(),
        id,
        &audiolib_core::MetadataPatch {
            favorite: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    let second: LibraryResponse = json(send(&f.state, "/api/v1/library", Some(TOKEN)).await).await;
    assert_eq!(second.library_id, first.library_id);
    assert!(second.revision > first.revision);
    let bumped = f.state.bump_revision().unwrap();
    assert_eq!(bumped.revision, second.revision + 1);

    // Identity and revision persist in the sidecar across server restarts.
    drop(f.state);
    let reopened = AppState::open(&f.config, BearerToken::new(TOKEN).unwrap()).unwrap();
    let after = reopened.revision().unwrap();
    assert_eq!(after.library_id, first.library_id);
    assert!(after.revision >= bumped.revision);
    assert!(
        f.dir
            .path()
            .join("library")
            .join("library.db.server-state.json")
            .is_file()
    );
}

#[tokio::test]
async fn roots_report_config_counts_and_null_webdav() {
    let f = fixture();
    let raw: serde_json::Value = json(send(&f.state, "/api/v1/roots", Some(TOKEN)).await).await;
    let roots: Vec<RootResponse> = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(roots.len(), 3);
    assert_eq!(roots[0].name, "SFX_Main");
    assert_eq!(
        roots[0].webdav_url.as_deref(),
        Some("http://nas.example.lan:8080/SFX_Main")
    );
    assert_eq!(roots[0].file_count, Some(3));
    assert_eq!(roots[1].name, "Aircraft");
    assert_eq!(
        roots[1].file_count,
        Some(2),
        "nested folder resolved by walking folders()"
    );
    // Unconfigured webdav_url is serialized as an explicit JSON null.
    assert!(raw[1]["webdav_url"].is_null());
    assert!(raw[1].as_object().unwrap().contains_key("webdav_url"));
    assert_eq!(roots[2].file_count, None, "not a library folder");
    assert!(raw[2]["file_count"].is_null());
}

#[tokio::test]
async fn snapshot_streams_an_openable_sqlite_copy_and_cleans_up() {
    let f = fixture();
    let info: LibraryResponse = json(send(&f.state, "/api/v1/library", Some(TOKEN)).await).await;
    let response = send(&f.state, "/api/v1/library/snapshot", Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers[header::CONTENT_TYPE], "application/octet-stream");
    assert_eq!(headers["x-library-id"], info.library_id.as_str());
    let revision: u64 = headers["x-library-revision"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(revision >= info.revision);
    assert_eq!(
        headers["x-schema-version"],
        audiolib_db::SCHEMA_VERSION.to_string()
    );
    let length: usize = headers[header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let body = bytes(response).await;
    assert_eq!(body.len(), length);
    assert!(body.starts_with(b"SQLite format 3\0"));

    let out = tempfile::tempdir().unwrap();
    let copy = out.path().join("copy.db");
    fs::write(&copy, &body).unwrap();
    let conn = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let user_version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(user_version, info.info.schema_version);
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM audio_files", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 3);
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(check, "ok");

    // The snapshot of this revision stays cached (one directory) for the next request.
    let leftovers: Vec<_> = fs::read_dir(f.dir.path().join("snapshots"))
        .unwrap()
        .collect();
    assert_eq!(leftovers.len(), 1, "{leftovers:?}");
}

#[tokio::test]
async fn slow_snapshot_preparation_answers_503_busy_and_the_retry_gets_it() {
    let f = fixture_with("snapshot_wait_secs = 1\n");
    f.state
        .set_snapshot_prepare_delay(Duration::from_millis(2500));
    let busy = send(&f.state, "/api/v1/library/snapshot", Some(TOKEN)).await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(busy.headers()[header::RETRY_AFTER], "5");
    let error: serde_json::Value = json(busy).await;
    assert_eq!(error["error_code"], "BUSY", "{error}");
    // The preparation went on without the first request; a retry joins it (one copy only).
    f.state.set_snapshot_prepare_delay(Duration::ZERO);
    let started = std::time::Instant::now();
    let ok = loop {
        let response = send(&f.state, "/api/v1/library/snapshot", Some(TOKEN)).await;
        if response.status() == StatusCode::OK || started.elapsed() > Duration::from_secs(20) {
            break response;
        }
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    };
    assert_eq!(ok.status(), StatusCode::OK);
    let length: usize = ok.headers()[header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(bytes(ok).await.len(), length);
    assert_eq!(snapshot_dirs(&f).len(), 1, "a single copy was prepared");
}

fn snapshot_dirs(f: &Fixture) -> Vec<std::path::PathBuf> {
    let mut dirs: Vec<_> = fs::read_dir(f.dir.path().join("snapshots"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    dirs.sort();
    dirs
}

#[tokio::test]
async fn snapshots_are_cached_per_revision_and_offered_with_zstd() {
    let f = fixture();
    let plain = send_with(&f.state, Method::GET, "/api/v1/library/snapshot", &[]).await;
    assert_eq!(plain.status(), StatusCode::OK);
    assert_eq!(header_str(&plain, header::VARY), "accept-encoding");
    assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
    let revision = header_str(&plain, "x-library-revision".parse().unwrap()).to_string();
    let plain_etag = header_str(&plain, header::ETAG).to_string();
    assert!(plain_etag.starts_with("\"snapshot-"), "{plain_etag}");
    assert_eq!(
        header_str(&plain, "x-uncompressed-length".parse().unwrap()),
        header_str(&plain, header::CONTENT_LENGTH)
    );
    let plain_body = bytes(plain).await;
    let cached = snapshot_dirs(&f);
    assert_eq!(cached.len(), 1);

    // zstd: compressed length on the wire, the plain length in X-Uncompressed-Length,
    // another ETag, the same revision, and the same database once decompressed.
    let packed = send_with(
        &f.state,
        Method::GET,
        "/api/v1/library/snapshot",
        &[(header::ACCEPT_ENCODING, "gzip, zstd;q=0.9")],
    )
    .await;
    assert_eq!(packed.status(), StatusCode::OK);
    assert_eq!(header_str(&packed, header::CONTENT_ENCODING), "zstd");
    assert_eq!(header_str(&packed, header::VARY), "accept-encoding");
    assert_eq!(
        header_str(&packed, "x-library-revision".parse().unwrap()),
        revision
    );
    let packed_etag = header_str(&packed, header::ETAG).to_string();
    assert_ne!(packed_etag, plain_etag);
    assert_eq!(
        header_str(&packed, "x-uncompressed-length".parse().unwrap()),
        plain_body.len().to_string()
    );
    let length: usize = header_str(&packed, header::CONTENT_LENGTH).parse().unwrap();
    let packed_body = bytes(packed).await;
    assert_eq!(packed_body.len(), length);
    assert!(length < plain_body.len());
    assert_eq!(zstd::decode_all(&packed_body[..]).unwrap(), plain_body);
    // zstd;q=0 means "not acceptable".
    let refused = send_with(
        &f.state,
        Method::GET,
        "/api/v1/library/snapshot",
        &[(header::ACCEPT_ENCODING, "zstd;q=0")],
    )
    .await;
    assert!(refused.headers().get(header::CONTENT_ENCODING).is_none());
    assert_eq!(bytes(refused).await, plain_body, "same cached copy");
    assert_eq!(snapshot_dirs(&f).len(), 2, "plain + zstd of one revision");
    assert!(snapshot_dirs(&f).contains(&cached[0]), "plain copy reused");

    // If-None-Match with the current ETag: 304 without preparing anything.
    for (etag, encoding) in [(&plain_etag, "identity"), (&packed_etag, "zstd")] {
        let response = send_with(
            &f.state,
            Method::GET,
            "/api/v1/library/snapshot",
            &[
                (header::IF_NONE_MATCH, etag.as_str()),
                (header::ACCEPT_ENCODING, encoding),
            ],
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{encoding}");
        assert_eq!(header_str(&response, header::ETAG), etag.as_str());
        assert_eq!(header_str(&response, header::VARY), "accept-encoding");
        assert_eq!(
            header_str(&response, "x-library-revision".parse().unwrap()),
            revision
        );
    }
    // The plain ETag never matches the zstd representation.
    let response = send_with(
        &f.state,
        Method::GET,
        "/api/v1/library/snapshot",
        &[
            (header::IF_NONE_MATCH, plain_etag.as_str()),
            (header::ACCEPT_ENCODING, "zstd"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = bytes(response).await;

    // A change bumps the revision: the cached copy is never served for it.
    let response = send_json(
        &f.state,
        Method::PATCH,
        &format!("/api/v1/audio/{}/metadata", audio_id(&f, "door.wav")),
        Some(r#"{"favorite":true}"#),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let fresh = send_with(&f.state, Method::GET, "/api/v1/library/snapshot", &[]).await;
    let fresh_revision = header_str(&fresh, "x-library-revision".parse().unwrap()).to_string();
    assert_ne!(fresh_revision, revision);
    assert_ne!(header_str(&fresh, header::ETAG), plain_etag);
    let body = bytes(fresh).await;
    let out = tempfile::tempdir().unwrap();
    let copy = out.path().join("fresh.db");
    fs::write(&copy, &body).unwrap();
    let favorite: i64 = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row(
            "SELECT favorite FROM audio_files WHERE filename='door.wav'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(favorite, 1);
    // The transmitted old representations remain available for interrupted clients.
    let now = snapshot_dirs(&f);
    assert_eq!(now.len(), 3, "{now:?}");
    assert!(cached.iter().all(|path| now.contains(path)));
}

#[tokio::test]
async fn pinned_snapshots_resume_exact_bytes_after_writes_in_both_encodings() {
    for encoding in ["identity", "zstd"] {
        let f = fixture();
        let first = send_with(
            &f.state,
            Method::GET,
            "/api/v1/library/snapshot",
            &[(header::ACCEPT_ENCODING, encoding)],
        )
        .await;
        let first_headers = first.headers().clone();
        let id = first_headers["x-snapshot-id"].to_str().unwrap();
        assert_eq!(uuid::Uuid::parse_str(id).unwrap().to_string(), id);
        let etag = first_headers[header::ETAG].to_str().unwrap();
        assert!(!etag.starts_with("W/"));
        let expected = bytes(first).await;
        let path = format!("/api/v1/library/snapshot?snapshot_id={id}");
        let changed = send_json(
            &f.state,
            Method::PATCH,
            &format!("/api/v1/audio/{}/metadata", audio_id(&f, "door.wav")),
            Some(r#"{"favorite":true}"#),
            Some(TOKEN),
        )
        .await;
        assert_eq!(changed.status(), StatusCode::OK);
        drop(bytes(changed).await);
        // Force the latest cache to a different revision before resuming the old id.
        let latest = send_with(
            &f.state,
            Method::GET,
            "/api/v1/library/snapshot",
            &[(header::ACCEPT_ENCODING, encoding)],
        )
        .await;
        assert_ne!(latest.headers()["x-snapshot-id"], id);
        assert_ne!(
            latest.headers()["x-library-revision"],
            first_headers["x-library-revision"]
        );
        drop(bytes(latest).await);

        let split = expected.len() / 2;
        let mut joined = Vec::new();
        for range in [format!("bytes=0-{}", split - 1), format!("bytes={split}-")] {
            let part = send_with(
                &f.state,
                Method::GET,
                &path,
                &[
                    (header::ACCEPT_ENCODING, encoding),
                    (header::RANGE, &range),
                    (header::IF_RANGE, etag),
                ],
            )
            .await;
            assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
            for name in [
                "etag",
                "x-snapshot-id",
                "x-library-id",
                "x-library-revision",
                "x-schema-version",
                "x-uncompressed-length",
                "x-audiolib-snapshot-format",
                "x-audiolib-library-format",
                "x-audiolib-server-compatibility",
                "x-search-storage-revision",
                "x-tokenization-revision",
                "x-source-search-revision",
                "x-source-tags-revision",
            ] {
                assert_eq!(part.headers()[name], first_headers[name], "{name}");
            }
            assert_eq!(part.headers()[header::ACCEPT_RANGES], "bytes");
            assert!(
                part.headers()[header::CONTENT_RANGE]
                    .to_str()
                    .unwrap()
                    .ends_with(&format!("/{}", expected.len()))
            );
            let content_length: usize = part.headers()[header::CONTENT_LENGTH]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            let part_bytes = bytes(part).await;
            assert_eq!(part_bytes.len(), content_length);
            joined.extend(part_bytes);
        }
        assert_eq!(joined, expected);
        if encoding == "zstd" {
            assert!(
                zstd::decode_all(&joined[..])
                    .unwrap()
                    .starts_with(b"SQLite format 3\0")
            );
            let refused = send_with(&f.state, Method::GET, &path, &[]).await;
            assert_eq!(refused.status(), StatusCode::NOT_ACCEPTABLE);
        }
        let refused = send_with(
            &f.state,
            Method::GET,
            &path,
            &[(header::ACCEPT_ENCODING, "identity;q=0, zstd;q=0")],
        )
        .await;
        assert_eq!(refused.status(), StatusCode::NOT_ACCEPTABLE);
        for validator in ["\"other\"".to_owned(), format!("W/{etag}")] {
            let full = send_with(
                &f.state,
                Method::GET,
                &path,
                &[
                    (header::ACCEPT_ENCODING, encoding),
                    (header::RANGE, "bytes=1-"),
                    (header::IF_RANGE, &validator),
                ],
            )
            .await;
            assert_eq!(full.status(), StatusCode::OK);
            assert_eq!(bytes(full).await, expected);
        }
        let not_modified = send_with(
            &f.state,
            Method::GET,
            &path,
            &[
                (header::ACCEPT_ENCODING, encoding),
                (header::IF_NONE_MATCH, etag),
            ],
        )
        .await;
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(not_modified.headers()["x-snapshot-id"], id);
        for name in [
            "x-audiolib-snapshot-format",
            "x-audiolib-library-format",
            "x-audiolib-server-compatibility",
            "x-schema-version",
        ] {
            assert_eq!(not_modified.headers()[name], first_headers[name], "{name}");
        }
        let invalid_range = format!("bytes={}-", expected.len());
        let unsatisfiable = send_with(
            &f.state,
            Method::GET,
            &path,
            &[
                (header::ACCEPT_ENCODING, encoding),
                (header::RANGE, &invalid_range),
                (header::IF_RANGE, etag),
            ],
        )
        .await;
        assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            unsatisfiable.headers()[header::CONTENT_RANGE],
            format!("bytes */{}", expected.len())
        );
        assert!(
            unsatisfiable
                .headers()
                .get(header::CONTENT_ENCODING)
                .is_none()
        );
        let error: ErrorBody = json(unsatisfiable).await;
        assert_eq!(error.error_code, "RANGE_NOT_SATISFIABLE");
    }
}

#[tokio::test]
async fn pinned_snapshot_refuses_incompatible_clients_before_ranges_or_not_modified() {
    use audiolib_core::compatibility::{
        CLIENT_COMPATIBILITY_HEADER, PLAN_B_INITIAL_FORMAT, compatibility_manifest,
    };
    let f = fixture();
    let first = send(&f.state, "/api/v1/library/snapshot", Some(TOKEN)).await;
    assert_eq!(first.status(), StatusCode::OK);
    let first_headers = first.headers().clone();
    let id = first_headers["x-snapshot-id"].to_str().unwrap();
    let etag = first_headers[header::ETAG].to_str().unwrap();
    let expected = bytes(first).await;
    let path = format!("/api/v1/library/snapshot?snapshot_id={id}");
    let dirs = snapshot_dirs(&f);
    for mode in ["missing", "older_format", "incompatible_version"] {
        for conditional in [header::RANGE, header::IF_NONE_MATCH] {
            let mut request = Request::get(&path)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(
                    conditional.clone(),
                    if conditional == header::RANGE {
                        "bytes=1-"
                    } else {
                        etag
                    },
                );
            if mode != "missing" {
                request = support::modern(request);
                let mut catalog = compatibility_manifest();
                if mode == "older_format" {
                    catalog.snapshot_read_formats = vec![PLAN_B_INITIAL_FORMAT.into()];
                } else {
                    // Unmarked (pre-0.7.2) client catalog.
                    catalog.marker_revision = None;
                }
                request.headers_mut().unwrap().insert(
                    CLIENT_COMPATIBILITY_HEADER,
                    serde_json::to_string(&catalog).unwrap().parse().unwrap(),
                );
            }
            let response = router(f.state.clone())
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::CONFLICT,
                "{mode} {conditional}"
            );
            let error: ErrorBody = json(response).await;
            assert_eq!(
                error.error_code,
                if mode != "older_format" {
                    // Peer markers are checked before the format (no catalog = pre-marker).
                    "PEER_UPGRADE_REQUIRED"
                } else {
                    "SNAPSHOT_FORMAT_UNSUPPORTED"
                }
            );
        }
    }
    assert_eq!(
        snapshot_dirs(&f),
        dirs,
        "refused resumes create no new payload"
    );
    let resume = send_with(
        &f.state,
        Method::GET,
        &path,
        &[(header::RANGE, "bytes=1-"), (header::IF_RANGE, etag)],
    )
    .await;
    assert_eq!(resume.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(bytes(resume).await, expected[1..]);
}

#[tokio::test]
async fn snapshot_ids_require_authentication_validation_and_the_same_live_server_library() {
    let f = fixture();
    let response = send(&f.state, "/api/v1/library/snapshot", Some(TOKEN)).await;
    let id = response.headers()["x-snapshot-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let old_etag = response.headers()[header::ETAG].clone();
    drop(bytes(response).await);
    let path = format!("/api/v1/library/snapshot?snapshot_id={id}");
    assert_eq!(
        send(&f.state, &path, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let other_library = fixture();
    let missing = send(&other_library.state, &path, Some(TOKEN)).await;
    assert_eq!(missing.status(), StatusCode::GONE);
    let error: ErrorBody = json(missing).await;
    assert_eq!(error.error_code, "SNAPSHOT_EXPIRED");
    for value in [
        "../payload",
        "not-a-uuid",
        "",
        "00000000-0000-0000-0000-000000000000&snapshot_id=x",
    ] {
        let response = send(
            &f.state,
            &format!("/api/v1/library/snapshot?snapshot_id={value}"),
            Some(TOKEN),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    f.state.shutdown(Duration::from_secs(5)).unwrap();
    let restarted = AppState::open(&f.config, BearerToken::new(TOKEN).unwrap()).unwrap();
    let missing = send(&restarted, &path, Some(TOKEN)).await;
    assert_eq!(missing.status(), StatusCode::GONE);
    let fresh = send(&restarted, "/api/v1/library/snapshot", Some(TOKEN)).await;
    assert_eq!(fresh.status(), StatusCode::OK);
    assert_ne!(fresh.headers()["x-snapshot-id"], id);
    assert_ne!(fresh.headers()[header::ETAG], old_etag);
    drop(bytes(fresh).await);
    restarted.shutdown(Duration::from_secs(5)).unwrap();
}

#[tokio::test]
async fn dropping_snapshot_responses_releases_slots_and_compressed_streams_do_not_pin_plain() {
    let f = fixture();
    let prepared = f.state.prepare_snapshot().unwrap();
    let old_plain = prepared.plain.path.clone();
    drop(prepared);
    let first = send_with(
        &f.state,
        Method::GET,
        "/api/v1/library/snapshot",
        &[(header::ACCEPT_ENCODING, "zstd")],
    )
    .await;
    let id = first.headers()["x-snapshot-id"].to_str().unwrap();
    let path = format!("/api/v1/library/snapshot?snapshot_id={id}");
    let second = send_with(
        &f.state,
        Method::GET,
        &path,
        &[(header::ACCEPT_ENCODING, "zstd")],
    )
    .await;
    let busy = send_with(
        &f.state,
        Method::GET,
        &path,
        &[(header::ACCEPT_ENCODING, "zstd")],
    )
    .await;
    assert_eq!(busy.status(), StatusCode::CONFLICT);
    drop(second);
    let changed = send_json(
        &f.state,
        Method::PATCH,
        &format!("/api/v1/audio/{}/metadata", audio_id(&f, "door.wav")),
        Some(r#"{"favorite":true}"#),
        Some(TOKEN),
    )
    .await;
    assert_eq!(changed.status(), StatusCode::OK);
    drop(bytes(changed).await);
    let fresh = send_with(&f.state, Method::GET, "/api/v1/library/snapshot", &[]).await;
    assert_eq!(fresh.status(), StatusCode::OK);
    assert!(
        !old_plain.exists(),
        "zstd body must not retain the old source database"
    );
    drop(bytes(fresh).await);
    let resumed = send_with(
        &f.state,
        Method::GET,
        &path,
        &[
            (header::ACCEPT_ENCODING, "zstd"),
            (header::RANGE, "bytes=1-"),
        ],
    )
    .await;
    assert_eq!(resumed.status(), StatusCode::PARTIAL_CONTENT);
    drop(resumed);
    f.state.shutdown(Duration::from_secs(5)).unwrap();
    assert_eq!(
        snapshot_dirs(&f).len(),
        1,
        "the in-flight zstd response stays owned"
    );
    drop(first);
    assert!(snapshot_dirs(&f).is_empty());
}

/// Send an authenticated request with extra headers.
async fn send_with(
    state: &AppState,
    method: Method,
    path: &str,
    headers: &[(header::HeaderName, &str)],
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    request = support::modern(request);
    for (name, value) in headers {
        request = request.header(name, *value);
    }
    router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

/// Library id of the scanned file whose name is `filename`.
fn audio_id(f: &Fixture, filename: &str) -> i64 {
    Connection::open_with_flags(&f.db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row(
            "SELECT id FROM audio_files WHERE filename=?1",
            [filename],
            |row| row.get(0),
        )
        .unwrap()
}

/// Point a library row at another path directly in SQLite (simulates stale or hostile rows).
fn set_file_path(f: &Fixture, id: i64, path: &Path) {
    let conn = Connection::open(&f.db).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute(
        "UPDATE audio_files SET file_path=?1 WHERE id=?2",
        rusqlite::params![path.to_string_lossy(), id],
    )
    .unwrap();
}

fn header_str(response: &Response, name: header::HeaderName) -> &str {
    response
        .headers()
        .get(&name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .to_str()
        .unwrap()
}

#[tokio::test]
async fn audio_returns_the_library_row_or_json_errors() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    let response = send(&f.state, &format!("/api/v1/audio/{id}"), Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let audio: audiolib_core::AudioFile = json(response).await;
    assert_eq!(audio.id, id);
    assert_eq!(audio.filename, "door.wav");
    assert_eq!(audio.sample_rate, 48_000);
    assert!(!audio.missing);

    let response = send(&f.state, "/api/v1/audio/999999", Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(json::<ErrorBody>(response).await.error_code, "NOT_FOUND");
    for path in ["/api/v1/audio/abc", "/api/v1/audio/abc/stream"] {
        let response = send(&f.state, path, Some(TOKEN)).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(json::<ErrorBody>(response).await.error_code, "VALIDATION");
    }
    let response = send(&f.state, "/api/v1/audio/999999/stream", Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn stream_sends_full_file_with_validators_and_supports_head() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    let expected = fs::read(f.dir.path().join("sounds").join("door.wav")).unwrap();
    let path = format!("/api/v1/audio/{id}/stream");
    let response = send(&f.state, &path, Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, header::CONTENT_TYPE), "audio/wav");
    assert_eq!(header_str(&response, header::ACCEPT_RANGES), "bytes");
    assert_eq!(
        header_str(&response, header::CONTENT_LENGTH),
        expected.len().to_string()
    );
    let etag = header_str(&response, header::ETAG).to_string();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
    assert!(!response.headers().contains_key(header::CONTENT_RANGE));
    assert_eq!(bytes(response).await, expected);

    let head = send_with(&f.state, Method::HEAD, &path, &[]).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(header_str(&head, header::ETAG), etag);
    assert_eq!(
        header_str(&head, header::CONTENT_LENGTH),
        expected.len().to_string()
    );
    assert!(bytes(head).await.is_empty());
}

#[tokio::test]
async fn stream_answers_single_ranges_and_rejects_unsatisfiable_ones() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    let expected = fs::read(f.dir.path().join("sounds").join("door.wav")).unwrap();
    let len = expected.len();
    let path = format!("/api/v1/audio/{id}/stream");
    let cases = [
        ("bytes=0-3", 0, 3),
        ("bytes=40-", 40, len - 1),
        ("bytes=-8", len - 8, len - 1),
        ("bytes=10-100000", 10, len - 1),
        ("bytes=-100000", 0, len - 1),
    ];
    for (range, start, end) in cases {
        let response = send_with(&f.state, Method::GET, &path, &[(header::RANGE, range)]).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "{range}");
        assert_eq!(
            header_str(&response, header::CONTENT_RANGE),
            format!("bytes {start}-{end}/{len}"),
            "{range}"
        );
        assert_eq!(
            header_str(&response, header::CONTENT_LENGTH),
            (end - start + 1).to_string()
        );
        assert_eq!(header_str(&response, header::ACCEPT_RANGES), "bytes");
        assert_eq!(bytes(response).await, &expected[start..=end], "{range}");
    }

    // Multi-range, foreign units and malformed ranges are answered with the full file.
    for range in ["bytes=0-1,4-5", "items=0-1", "bytes=9-2"] {
        let response = send_with(&f.state, Method::GET, &path, &[(header::RANGE, range)]).await;
        assert_eq!(response.status(), StatusCode::OK, "{range}");
        assert_eq!(bytes(response).await, expected, "{range}");
    }

    for range in [format!("bytes={len}-"), "bytes=-0".to_string()] {
        let response = send_with(&f.state, Method::GET, &path, &[(header::RANGE, &range)]).await;
        assert_eq!(
            response.status(),
            StatusCode::RANGE_NOT_SATISFIABLE,
            "{range}"
        );
        assert_eq!(
            header_str(&response, header::CONTENT_RANGE),
            format!("bytes */{len}")
        );
        let body: ErrorBody = json(response).await;
        assert_eq!(body.error_code, "RANGE_NOT_SATISFIABLE");
    }
}

#[tokio::test]
async fn stream_honours_if_none_match_and_if_range() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    let file = f.dir.path().join("sounds").join("door.wav");
    let expected = fs::read(&file).unwrap();
    let path = format!("/api/v1/audio/{id}/stream");
    let etag = header_str(
        &send_with(&f.state, Method::HEAD, &path, &[]).await,
        header::ETAG,
    )
    .to_string();

    let response = send_with(
        &f.state,
        Method::GET,
        &path,
        &[(header::IF_NONE_MATCH, &format!("\"other\", W/{etag}"))],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(header_str(&response, header::ETAG), etag);
    assert!(bytes(response).await.is_empty());
    let response = send_with(
        &f.state,
        Method::GET,
        &path,
        &[(header::IF_NONE_MATCH, "\"other\"")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let ranged = |validator: String| {
        let state = f.state.clone();
        let path = path.clone();
        async move {
            send_with(
                &state,
                Method::GET,
                &path,
                &[(header::RANGE, "bytes=0-3"), (header::IF_RANGE, &validator)],
            )
            .await
        }
    };
    let response = ranged(etag.clone()).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(bytes(response).await, &expected[..4]);
    for stale in [
        "\"0-0\"".to_string(),
        format!("W/{etag}"),
        "Wed, 21 Oct 2015 07:28:00 GMT".to_string(),
    ] {
        let response = ranged(stale.clone()).await;
        assert_eq!(response.status(), StatusCode::OK, "{stale}");
        assert_eq!(bytes(response).await, expected, "{stale}");
    }

    // Replacing the file content changes the ETag, so the old validator no longer matches.
    let mut changed = expected.clone();
    changed.extend_from_slice(&[0, 0]);
    fs::write(&file, &changed).unwrap();
    let response = send_with(&f.state, Method::HEAD, &path, &[]).await;
    assert_ne!(header_str(&response, header::ETAG), etag);
    let response = ranged(etag.clone()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await, changed);
}

#[tokio::test]
async fn stream_refuses_paths_outside_roots_and_missing_files() {
    let f = fixture();
    let outside = f.dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    wav(&outside.join("secret.wav"));
    let id = audio_id(&f, "door.wav");
    let path = format!("/api/v1/audio/{id}/stream");
    let sounds = f.dir.path().join("sounds");

    // Existing files outside every configured root, directly or through `..`, get exactly
    // the missing-file reply so existence outside the roots is not revealed.
    let missing_message = format!("Not found: Audio {id} file is missing on the server");
    for target in [
        outside.join("secret.wav"),
        sounds.join("..").join("outside").join("secret.wav"),
        f.db.clone(),
        outside.join("does-not-exist.wav"),
    ] {
        set_file_path(&f, id, &target);
        let response = send(&f.state, &path, Some(TOKEN)).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{}",
            target.display()
        );
        let body: ErrorBody = json(response).await;
        assert_eq!(body.error_code, "NOT_FOUND");
        assert_eq!(body.message, missing_message, "{}", target.display());
    }

    // A symlink inside a root that escapes it is resolved before the root check.
    #[cfg(unix)]
    {
        let link = sounds.join("escape.wav");
        std::os::unix::fs::symlink(outside.join("secret.wav"), &link).unwrap();
        set_file_path(&f, id, &link);
        let response = send(&f.state, &path, Some(TOKEN)).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // A row whose file no longer exists, and a row pointing at a root directory.
    for target in [
        sounds.join("Aircraft").join("gone.wav"),
        sounds.join("Aircraft"),
    ] {
        set_file_path(&f, id, &target);
        let response = send(&f.state, &path, Some(TOKEN)).await;
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{}",
            target.display()
        );
        let body: ErrorBody = json(response).await;
        assert_eq!(body.error_code, "NOT_FOUND");
        assert!(body.message.contains("missing"), "{}", body.message);
    }

    // A file under a nested root is still served. It must not be another row's file:
    // `file_path` is UNIQUE, and on Windows (no `/var` -> `/private/var` symlink as on
    // macOS) this spelling equals the scanned `flyby_01.wav` row's stored path.
    wav(&sounds.join("Aircraft").join("unscanned_03.wav"));
    set_file_path(&f, id, &sounds.join("Aircraft").join("unscanned_03.wav"));
    let response = send(&f.state, &path, Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
}

async fn health_status(service: &Service) -> serde_json::Value {
    let response = send(service, "/api/v1/health", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    json(response).await
}

/// Poll `/health` until it reports `status`, failing after a generous timeout.
async fn wait_for_health(service: &Service, status: &str) -> serde_json::Value {
    for _ in 0..500 {
        let body = health_status(service).await;
        if body["status"] == status {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("/health never reported {status}");
}

#[tokio::test]
async fn starting_service_answers_503_until_the_library_is_ready() {
    let f = fixture();
    let service = Service::starting(BearerToken::new(TOKEN).unwrap());
    let body = health_status(&service).await;
    assert_eq!(body["status"], "starting");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert!(body.get("message").is_none());
    for path in ["/api/v1/library", "/api/v1/roots", "/api/v1/audio/1/stream"] {
        let response = send(&service, path, Some(TOKEN)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(response.headers()[header::RETRY_AFTER], "5");
        assert_eq!(json::<ErrorBody>(response).await.error_code, "UNAVAILABLE");
        // Authentication is still checked first.
        let response = send(&service, path, None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }

    service.set_ready(f.state.clone());
    assert_eq!(health_status(&service).await["status"], "ok");
    let response = send(&service, "/api/v1/library", Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    service.set_failed("late failure");
    assert_eq!(
        health_status(&service).await["status"],
        "ok",
        "ready is final"
    );
}

#[tokio::test]
async fn failed_open_keeps_serving_health_and_retries_until_the_library_appears() {
    let f = fixture();
    let pending = f.dir.path().join("pending-volume").join("library.db");
    let mut config = f.config.clone();
    config.library_db = pending.clone();
    let service = Service::starting(BearerToken::new(TOKEN).unwrap());
    open_in_background(
        service.clone(),
        config,
        RetryPolicy {
            initial: Duration::from_millis(20),
            max: Duration::from_millis(50),
        },
        tokio::runtime::Handle::current(),
    )
    .unwrap();

    let body = wait_for_health(&service, "error").await;
    assert_eq!(body["message"], audiolib_server::service::FAILED_MESSAGE);
    let response = send(&service, "/api/v1/library", Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let error: ErrorBody = json(response).await;
    assert_eq!(error.error_code, "UNAVAILABLE");
    // Fixed text only: the raw open error (with the path) stays in the server log.
    assert_eq!(error.message, audiolib_server::service::FAILED_MESSAGE);
    assert!(!error.message.contains("pending-volume"));

    // The library becomes reachable (e.g. the volume is mounted or access granted).
    // Stage the copy elsewhere and rename it into place so a retry never sees a partial file.
    let staging = f.dir.path().join("staging.db");
    audiolib_core::DataProvider::backup_library(f.writer.as_ref(), &staging).unwrap();
    fs::create_dir_all(pending.parent().unwrap()).unwrap();
    fs::rename(&staging, &pending).unwrap();
    wait_for_health(&service, "ok").await;
    let library: LibraryResponse = json(send(&service, "/api/v1/library", Some(TOKEN)).await).await;
    assert_eq!(library.info.stats.audio_files, 3);
}

#[tokio::test]
async fn stream_limit_answers_503_busy_until_a_body_is_released() {
    let f = fixture_with("auth = true\nmax_streams = 1\n");
    let id = audio_id(&f, "door.wav");
    let path = format!("/api/v1/audio/{id}/stream");
    // The first response body is not consumed, so it keeps its stream slot.
    let held = send(&f.state, &path, Some(TOKEN)).await;
    assert_eq!(held.status(), StatusCode::OK);
    let busy = send(&f.state, &path, Some(TOKEN)).await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header_str(&busy, header::RETRY_AFTER), "1");
    assert_eq!(json::<ErrorBody>(busy).await.error_code, "BUSY");
    // Other endpoints are unaffected.
    let response = send(&f.state, &format!("/api/v1/audio/{id}"), Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(held);
    let response = send(&f.state, &path, Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
}

/// Start [`serve`] on an ephemeral port; returns the address, a shutdown trigger and the task.
async fn spawn_server(
    app: axum::Router,
    options: ServeOptions,
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(serve(
        listener,
        app,
        async {
            let _ = stop_rx.await;
        },
        options,
    ));
    (address, stop_tx, task)
}

#[tokio::test]
async fn server_closes_connections_that_never_finish_their_headers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let f = fixture();
    let options = ServeOptions {
        header_read_timeout: Duration::from_millis(200),
        shutdown_grace: Duration::from_secs(1),
        ..ServeOptions::default()
    };
    let (address, stop, task) = spawn_server(router(f.state.clone()), options).await;

    // A complete request is served normally over the hyper connection loop.
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client
        .write_all(b"GET /api/v1/health HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut reply = Vec::new();
    client.read_to_end(&mut reply).await.unwrap();
    assert!(
        reply.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&reply)
    );

    // A client that stalls inside its headers is disconnected after the timeout.
    let mut slow = tokio::net::TcpStream::connect(address).await.unwrap();
    slow.write_all(b"GET /api/v1/health HTTP/1.1\r\n")
        .await
        .unwrap();
    // read_to_end only completes once the server closes the connection.
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), slow.read_to_end(&mut rest))
        .await
        .expect("connection should be closed by the header read timeout");
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn shutdown_waits_at_most_the_grace_period_for_stalled_responses() {
    use tokio::io::AsyncWriteExt;
    let f = fixture();
    // A large file in a root whose response the client never reads, so it stays in flight.
    let big = f.dir.path().join("sounds").join("big.wav");
    fs::write(&big, vec![0_u8; 16 * 1024 * 1024]).unwrap();
    let id = audio_id(&f, "door.wav");
    set_file_path(&f, id, &big);
    let options = ServeOptions {
        header_read_timeout: Duration::from_secs(5),
        shutdown_grace: Duration::from_millis(300),
        ..ServeOptions::default()
    };
    let (address, stop, task) = spawn_server(router(f.state.clone()), options).await;
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client
        .write_all(
            format!(
                "GET /api/v1/audio/{id}/stream HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = std::time::Instant::now();
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("serve must return after the grace period")
        .unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
    drop(client);
}

#[tokio::test]
async fn auth_disabled_serves_every_endpoint_without_a_token() {
    // Default configuration: no `auth` key, so the mode is `external` and LAN-direct
    // requests (in-process requests count as LAN) need no token.
    let f = fixture_with("");
    assert!(!f.config.auth);
    assert_eq!(f.state.security().auth_mode(), AuthMode::External);
    let id = audio_id(&f, "door.wav");
    let paths = [
        "/api/v1/library".to_string(),
        "/api/v1/roots".to_string(),
        format!("/api/v1/audio/{id}"),
        format!("/api/v1/audio/{id}/stream"),
        "/api/v1/library/snapshot".to_string(),
    ];
    for path in &paths {
        // No header, a wrong bearer token and a non-bearer scheme are all accepted.
        for authorization in [
            None,
            Some("Bearer wrong-token-0123456789abcdef"),
            Some("Basic x"),
        ] {
            let mut request = support::modern(Request::get(path.as_str()));
            if let Some(value) = authorization {
                request = request.header(header::AUTHORIZATION, value);
            }
            let response = router(f.state.clone())
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{path} {authorization:?}"
            );
            assert!(!response.headers().contains_key(header::WWW_AUTHENTICATE));
            let _ = bytes(response).await;
        }
    }
    let response = send_without_auth(
        &f.state,
        Method::GET,
        &paths[3],
        &[(header::RANGE, "bytes=0-43")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(bytes(response).await.len(), 44);

    // A service started without a token reports 503 (not 401) until the library opens.
    let service = Service::starting(None);
    let response = send(&service, "/api/v1/library", None).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    service.set_ready(f.state.clone());
    let response = send(&service, "/api/v1/library", None).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn auth_enabled_requires_a_token_at_open() {
    let f = fixture();
    assert!(f.config.auth);
    assert!(matches!(
        AppState::open(&f.config, None),
        Err(audiolib_core::Error::Validation(_))
    ));
}

async fn send_without_auth(
    state: &AppState,
    method: Method,
    path: &str,
    headers: &[(header::HeaderName, &str)],
) -> Response {
    let mut request = Request::builder().method(method).uri(path);
    request = support::modern(request);
    if !headers
        .iter()
        .any(|(name, _)| name.as_str().eq_ignore_ascii_case("host"))
    {
        request = request.header(header::HOST, "localhost");
    }
    for (name, value) in headers {
        request = request.header(name, *value);
    }
    router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

/// Send a request with an optional JSON body and optional bearer token.
async fn send_json(
    state: &AppState,
    method: Method,
    path: &str,
    body: Option<&str>,
    token: Option<&str>,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(
            "x-audiolib-library-id",
            state.revision().unwrap().library_id,
        )
        .header(header::CONTENT_TYPE, "application/json");
    request = support::modern(request);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(body) => Body::from(body.to_string()),
        None => Body::empty(),
    };
    router(state.clone())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}

async fn library_revision(state: &AppState) -> u64 {
    let response = send(state, "/api/v1/library", Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    json::<LibraryResponse>(response).await.revision
}

/// Main database file size and mtime (the WAL is excluded).
fn main_file_state(path: &Path) -> (u64, std::time::SystemTime) {
    let metadata = fs::metadata(path).unwrap();
    (metadata.len(), metadata.modified().unwrap())
}

/// Guard regression requests deliberately omit the normal helpers' identity/type defaults.
async fn guard_request(
    state: &AppState,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    connection: Option<ConnectionInfo>,
    body: &str,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    request = support::modern(request);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request.body(Body::from(body.to_owned())).unwrap();
    let connection = connection.unwrap_or(ConnectionInfo {
        peer: "192.168.1.2:1".parse().unwrap(),
        listener: Listener::Http,
        local: None,
    });
    request.extensions_mut().insert(connection);
    router(state.clone()).oneshot(request).await.unwrap()
}

#[tokio::test]
async fn library_mutations_require_the_identity_of_the_captured_library() {
    let f = fixture_with("");
    let other = fixture_with("");
    let id = audio_id(&f, "door.wav");
    let before = f.state.revision().unwrap();
    let other_id = other.state.revision().unwrap().library_id;
    for (method, path, body) in [
        (
            Method::PATCH,
            format!("/api/v1/audio/{id}/metadata"),
            r#"{"notes":"guarded"}"#,
        ),
        (
            Method::POST,
            "/api/v1/audio/metadata/batch".into(),
            r#"{"ids":[1],"patch":{"favorite":true}}"#,
        ),
        (Method::POST, format!("/api/v1/audio/{id}/plays"), ""),
        (
            Method::POST,
            "/api/v1/audio/remove".into(),
            r#"{"ids":[1]}"#,
        ),
        (Method::DELETE, "/api/v1/folders/1".into(), ""),
        (
            Method::POST,
            "/api/v1/scans".into(),
            r#"{"root":"SFX_Main"}"#,
        ),
        (Method::POST, "/api/v1/scans/1/pause".into(), ""),
        (Method::POST, "/api/v1/scans/1/resume".into(), ""),
        (Method::POST, "/api/v1/scans/1/cancel".into(), ""),
    ] {
        for (library_id, expected) in [
            (None, "LIBRARY_ID_REQUIRED"),
            (Some(other_id.as_str()), "LIBRARY_CHANGED"),
        ] {
            let mut headers = vec![("content-type", "application/json")];
            if let Some(library_id) = library_id {
                headers.push(("x-audiolib-library-id", library_id));
            }
            let response =
                guard_request(&f.state, method.clone(), &path, &headers, None, body).await;
            assert_eq!(response.status(), StatusCode::CONFLICT, "{path}");
            assert_eq!(
                json::<ErrorBody>(response).await.error_code,
                expected,
                "{path}"
            );
        }
    }
    assert_eq!(f.state.revision().unwrap(), before);
    assert!(f.state.audio(id).unwrap().notes.is_empty());
    assert!(
        serde_json::to_value(f.state.audio(id).unwrap())
            .unwrap()
            .get("play_count")
            .is_none()
    );
    assert!(f.state.scans().current().is_none());
    // Root rechecks do not target cached database ids and remain header-free.
    let response = guard_request(
        &f.state,
        Method::POST,
        "/api/v1/roots/SFX_Main/check",
        &[],
        None,
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn browser_mutations_verify_origin_scheme_authority_and_json_media_type() {
    let f = fixture_with("");
    let id = audio_id(&f, "door.wav");
    let library_id = f.state.revision().unwrap().library_id;
    let path = format!("/api/v1/audio/{id}/plays");
    for origin in [
        "http://other.lan:8790",
        "https://server.lan:8790",
        "null",
        "http://server.lan:8791",
        "http://server.lan:8790/path",
    ] {
        let response = guard_request(
            &f.state,
            Method::POST,
            &path,
            &[
                ("x-audiolib-library-id", &library_id),
                ("host", "server.lan:8790"),
                ("origin", origin),
                ("content-type", "text/plain"),
            ],
            Some(peer("10.0.0.20:50000", Listener::Http)),
            "ignored body",
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{origin}");
        assert_eq!(
            json::<ErrorBody>(response).await.error_code,
            "ORIGIN_FORBIDDEN"
        );
    }
    assert!(
        serde_json::to_value(f.state.audio(id).unwrap())
            .unwrap()
            .get("play_count")
            .is_none()
    );
    let metadata_path = format!("/api/v1/audio/{id}/metadata");
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
    ] {
        let mut headers = vec![("x-audiolib-library-id", library_id.as_str())];
        if let Some(content_type) = content_type {
            headers.push(("content-type", content_type));
        }
        let response = guard_request(
            &f.state,
            Method::PATCH,
            &metadata_path,
            &headers,
            None,
            r#"{"notes":"unsaved"}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(
            json::<ErrorBody>(response).await.error_code,
            "JSON_CONTENT_TYPE_REQUIRED"
        );
    }
    assert!(f.state.audio(id).unwrap().notes.is_empty());
    // The listener proves TLS, so the same HTTPS browser origin succeeds.
    let response = guard_request(
        &f.state,
        Method::PATCH,
        &metadata_path,
        &[
            ("x-audiolib-library-id", &library_id),
            ("host", "server.lan:8790"),
            ("origin", "https://SERVER.lan:8790"),
            ("content-type", "application/json; charset=utf-8"),
        ],
        Some(peer("10.0.0.20:50000", Listener::Tls)),
        r#"{"notes":"saved"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(f.state.audio(id).unwrap().notes, "saved");
    // Native clients omit Origin and can use the bodyless play route.
    let response = guard_request(
        &f.state,
        Method::POST,
        &path,
        &[("x-audiolib-library-id", &library_id)],
        None,
        "",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    // The deprecated play route is a no-op: nothing is counted.
    assert!(
        serde_json::to_value(f.state.audio(id).unwrap())
            .unwrap()
            .get("play_count")
            .is_none()
    );
}

#[tokio::test]
async fn patch_metadata_returns_the_record_and_bumps_the_revision_once() {
    let f = fixture_with("auth = true\nrevision_poll_secs = 1\n");
    let poller = spawn_revision_poller(f.state.clone(), Duration::from_millis(50));
    let id = audio_id(&f, "door.wav");
    let start = library_revision(&f.state).await;
    let response = send_json(
        &f.state,
        Method::PATCH,
        &format!("/api/v1/audio/{id}/metadata"),
        Some(r##"{"tags":"loud door","color":"#FF8800","notes":"slam","favorite":true}"##),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let raw: serde_json::Value = json(response).await;
    let mut keys: Vec<_> = raw.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["file", "revision", "revision_before"], "{raw}");
    let body: AudioWriteResponse = serde_json::from_value(raw).unwrap();
    assert_eq!(body.revision_before, start);
    assert_eq!(body.revision, start + 1);
    assert_eq!(body.file.id, id);
    assert_eq!(body.file.tags, "loud door");
    assert_eq!(body.file.color.as_deref(), Some("#FF8800"));
    assert_eq!(body.file.notes, "slam");
    assert!(body.file.favorite);

    // Several poll intervals later the watcher has not counted the write a second time.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(library_revision(&f.state).await, body.revision);
    let fetched: serde_json::Value =
        json(send(&f.state, &format!("/api/v1/audio/{id}"), Some(TOKEN)).await).await;
    assert_eq!(fetched, serde_json::to_value(&body.file).unwrap());

    // A partial patch only changes the given fields; the next write continues the chain.
    let response = send_json(
        &f.state,
        Method::PATCH,
        &format!("/api/v1/audio/{id}/metadata"),
        Some(r#"{"color":""}"#),
        Some(TOKEN),
    )
    .await;
    let next: AudioWriteResponse = json(response).await;
    assert_eq!(next.revision_before, body.revision);
    assert_eq!(next.revision, body.revision + 1);
    assert_eq!(next.file.color, None);
    assert!(next.file.favorite);
    poller.abort();
}

#[tokio::test]
async fn batch_metadata_returns_the_report_and_changed_files_in_request_order() {
    let f = fixture();
    let door = audio_id(&f, "door.wav");
    let flyby = audio_id(&f, "flyby_01.wav");
    let untouched = audio_id(&f, "flyby_02.wav");
    let start = library_revision(&f.state).await;
    let response = send_json(
        &f.state,
        Method::POST,
        "/api/v1/audio/metadata/batch",
        Some(&format!(
            r#"{{"ids":[{door},{flyby},{door}],"patch":{{"tags":"wood"}}}}"#
        )),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let raw: serde_json::Value = json(response).await;
    let mut keys: Vec<_> = raw.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        ["files", "report", "revision", "revision_before"],
        "{raw}"
    );
    let body: BatchWriteResponse = serde_json::from_value(raw).unwrap();
    assert_eq!(body.report.updated, 2);
    assert_eq!(body.revision_before, start);
    assert_eq!(body.revision, start + 1);
    let ids: Vec<i64> = body.files.iter().map(|file| file.id).collect();
    assert_eq!(ids, [door, flyby]);
    assert!(body.files.iter().all(|file| file.tags == "wood"));

    let response = send_json(
        &f.state,
        Method::POST,
        "/api/v1/audio/metadata/batch",
        Some(&format!(
            r#"{{"ids":[{flyby}],"patch":{{"tags":"metal","favorite":true}},"tag_mode":"append"}}"#
        )),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: BatchWriteResponse = json(response).await;
    assert_eq!(body.revision_before, start + 1);
    assert_eq!(body.files.len(), 1);
    assert!(body.files[0].tags.contains("wood") && body.files[0].tags.contains("metal"));
    assert!(body.files[0].favorite);
    assert_eq!(library_revision(&f.state).await, body.revision);
    assert_eq!(f.state.audio(untouched).unwrap().tags, "");
}

#[tokio::test]
async fn plays_return_the_unchanged_record_without_writes_or_revision_changes() {
    let f = fixture();
    let id = audio_id(&f, "flyby_02.wav");
    let start = library_revision(&f.state).await;
    let observer = Connection::open(&f.db).unwrap();
    let data_version: i64 = observer
        .query_row("PRAGMA data_version", [], |row| row.get(0))
        .unwrap();
    for _ in 0..2 {
        let response = send_json(
            &f.state,
            Method::POST,
            &format!("/api/v1/audio/{id}/plays"),
            None,
            Some(TOKEN),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let value: serde_json::Value = json(response).await;
        assert!(value["file"].get("play_count").is_none());
        assert!(value["file"]["attributes"].get("last_played_at").is_none());
        let body: AudioWriteResponse = serde_json::from_value(value).unwrap();
        assert_eq!(body.file.id, id);
        assert_eq!(body.revision_before, start);
        assert_eq!(body.revision, start);
    }
    assert_eq!(library_revision(&f.state).await, start);
    assert_eq!(
        observer
            .query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        data_version
    );
    let history: serde_json::Value =
        json(send(&f.state, "/api/v1/history/plays", Some(TOKEN)).await).await;
    assert_eq!(history, serde_json::json!([]));

    // The no-op stays a no-op after restart; the next content write still bumps once.
    drop(f.state);
    let reopened = AppState::open(&f.config, BearerToken::new(TOKEN).unwrap()).unwrap();
    assert_eq!(library_revision(&reopened).await, start);
    assert!(
        serde_json::to_value(reopened.audio(id).unwrap())
            .unwrap()
            .get("play_count")
            .is_none()
    );
    let history = send_json(
        &reopened,
        Method::GET,
        "/api/v1/history/plays",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(history.status(), StatusCode::OK);
    let history: Vec<serde_json::Value> = json(history).await;
    assert!(history.is_empty());
    let response = send_json(
        &reopened,
        Method::PATCH,
        &format!("/api/v1/audio/{id}/metadata"),
        Some(r#"{"favorite":true}"#),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: AudioWriteResponse = json(response).await;
    assert_eq!((body.revision_before, body.revision), (start, start + 1));
    assert_eq!(library_revision(&reopened).await, start + 1);
}

#[tokio::test]
async fn metadata_writes_validate_input_require_auth_and_report_unknown_ids() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    let start = library_revision(&f.state).await;
    let patch_path = format!("/api/v1/audio/{id}/metadata");
    let cases: Vec<(Method, String, Option<String>, StatusCode, &str)> = vec![
        (
            Method::PATCH,
            patch_path.clone(),
            Some(r##"{"color":"#12345"}"##.into()),
            StatusCode::BAD_REQUEST,
            "VALIDATION",
        ),
        (
            Method::PATCH,
            patch_path.clone(),
            Some(r#"{"color":"red"}"#.into()),
            StatusCode::BAD_REQUEST,
            "VALIDATION",
        ),
        (
            Method::PATCH,
            patch_path.clone(),
            Some("{not json".into()),
            StatusCode::BAD_REQUEST,
            "VALIDATION",
        ),
        (
            Method::PATCH,
            patch_path.clone(),
            Some(r#"{"favorite":"yes"}"#.into()),
            StatusCode::BAD_REQUEST,
            "VALIDATION",
        ),
        (
            Method::PATCH,
            patch_path.clone(),
            None,
            StatusCode::BAD_REQUEST,
            "VALIDATION",
        ),
        (
            Method::PATCH,
            "/api/v1/audio/abc/metadata".into(),
            Some("{}".into()),
            StatusCode::BAD_REQUEST,
            "VALIDATION",
        ),
        (
            Method::PATCH,
            "/api/v1/audio/999999/metadata".into(),
            Some(r#"{"favorite":true}"#.into()),
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
        ),
        (
            Method::POST,
            "/api/v1/audio/999999/plays".into(),
            None,
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
        ),
        (
            Method::POST,
            "/api/v1/audio/x/plays".into(),
            None,
            StatusCode::BAD_REQUEST,
            "VALIDATION",
        ),
        (
            Method::POST,
            "/api/v1/audio/metadata/batch".into(),
            Some(r#"{"ids":[],"patch":{"favorite":true}}"#.into()),
            StatusCode::BAD_REQUEST,
            "VALIDATION",
        ),
        (
            Method::POST,
            "/api/v1/audio/metadata/batch".into(),
            Some(format!(r#"{{"ids":[{id}],"patch":{{}}}}"#)),
            StatusCode::BAD_REQUEST,
            "VALIDATION",
        ),
        (
            Method::POST,
            "/api/v1/audio/metadata/batch".into(),
            Some(format!(
                r#"{{"ids":[{id}],"patch":{{"favorite":true}},"tag_mode":"merge"}}"#
            )),
            StatusCode::BAD_REQUEST,
            "VALIDATION",
        ),
        (
            Method::POST,
            "/api/v1/audio/metadata/batch".into(),
            Some(format!(
                r#"{{"ids":[{id},999999],"patch":{{"favorite":true}}}}"#
            )),
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
        ),
    ];
    for (method, path, body, status, code) in cases {
        let response = send_json(
            &f.state,
            method.clone(),
            &path,
            body.as_deref(),
            Some(TOKEN),
        )
        .await;
        assert_eq!(response.status(), status, "{method} {path} {body:?}");
        let error: ErrorBody = json(response).await;
        assert_eq!(error.error_code, code, "{method} {path} {body:?}");
    }
    // Nothing was written: the batch with an unknown id rolled back as a whole.
    assert!(!f.state.audio(id).unwrap().favorite);
    assert_eq!(library_revision(&f.state).await, start);

    for (method, path) in [
        (Method::PATCH, patch_path.clone()),
        (Method::POST, "/api/v1/audio/metadata/batch".into()),
        (Method::POST, format!("/api/v1/audio/{id}/plays")),
    ] {
        for token in [None, Some("wrong-token-0123456789abcdef")] {
            let response = send_json(
                &f.state,
                method.clone(),
                &path,
                Some(r#"{"favorite":true}"#),
                token,
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path}"
            );
        }
    }
    let response = send_json(&f.state, Method::GET, &patch_path, None, Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert!(header_str(&response, header::ALLOW).contains("PATCH"));
    assert_eq!(library_revision(&f.state).await, start);

    // With authentication disabled the write endpoints need no token.
    let open = fixture_with("");
    let id = audio_id(&open, "door.wav");
    let response = send_json(
        &open.state,
        Method::POST,
        &format!("/api/v1/audio/{id}/plays"),
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn external_wal_commits_bump_and_restarts_do_not() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    let start = library_revision(&f.state).await;

    // Another process commits in WAL mode without checkpointing: the main file is unchanged.
    let external = Connection::open(&f.db).unwrap();
    audiolib_db::register_library_functions(&external).unwrap();
    external
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .unwrap();
    let main_before = main_file_state(&f.db);
    external
        .execute("UPDATE audio_files SET user_rating=3 WHERE id=?1", [id])
        .unwrap();
    assert_eq!(
        main_file_state(&f.db),
        main_before,
        "commit stayed in the WAL"
    );
    assert_eq!(library_revision(&f.state).await, start + 1);
    assert_eq!(library_revision(&f.state).await, start + 1, "counted once");

    // The compatibility play route leaves the externally advanced revision unchanged.
    let response = send_json(
        &f.state,
        Method::POST,
        &format!("/api/v1/audio/{id}/plays"),
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let played: AudioWriteResponse = json(response).await;
    assert_eq!(played.revision_before, start + 1);
    assert_eq!(played.revision, start + 1);

    // A subsequent content write continues the revision chain exactly once.
    let response = send_json(
        &f.state,
        Method::PATCH,
        &format!("/api/v1/audio/{id}/metadata"),
        Some(r#"{"favorite":true}"#),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let written: AudioWriteResponse = json(response).await;
    assert_eq!(written.revision_before, start + 1);
    assert_eq!(written.revision, start + 2);

    // Restarting the server (twice) does not bump.
    for _ in 0..2 {
        let state = AppState::open(&f.config, BearerToken::new(TOKEN).unwrap()).unwrap();
        assert_eq!(library_revision(&state).await, written.revision);
    }
    drop(f.state);
    let reopened = AppState::open(&f.config, BearerToken::new(TOKEN).unwrap()).unwrap();
    assert_eq!(library_revision(&reopened).await, written.revision);

    // A WAL-only commit made while the server was stopped is found at the next start.
    drop(reopened);
    external
        .execute("UPDATE audio_files SET user_rating=1 WHERE id=?1", [id])
        .unwrap();
    let reopened = AppState::open(&f.config, BearerToken::new(TOKEN).unwrap()).unwrap();
    assert_eq!(library_revision(&reopened).await, written.revision + 1);
}

#[tokio::test]
async fn library_database_bytes_include_the_wal() {
    let f = fixture();
    let raw: serde_json::Value = json(send(&f.state, "/api/v1/library", Some(TOKEN)).await).await;
    let main = fs::metadata(&f.db).unwrap().len();
    let mut wal = f.db.as_os_str().to_os_string();
    wal.push("-wal");
    let wal = fs::metadata(std::path::PathBuf::from(wal)).unwrap().len();
    assert!(wal > 0, "the scan left frames in the WAL");
    assert_eq!(raw["database_bytes"].as_u64(), Some(main + wal), "{raw}");
    assert_eq!(raw["wal_bytes"].as_u64(), Some(wal), "{raw}");
}

#[tokio::test]
async fn startup_removes_only_stale_snapshot_directories() {
    let f = fixture();
    let snapshots = f.dir.path().join("snapshots");
    fs::create_dir_all(snapshots.join("audiolib-snapshot-stale").join("nested")).unwrap();
    fs::write(
        snapshots
            .join("audiolib-snapshot-stale")
            .join(".owner.lock"),
        b"",
    )
    .unwrap();
    fs::write(
        snapshots
            .join("audiolib-snapshot-stale")
            .join("library-snapshot.db"),
        b"x",
    )
    .unwrap();
    // Startup only removes leftovers at least 30 s old; backdate the stale one
    // after its contents are written (writing children bumps the dir mtime).
    let old = filetime::FileTime::from_system_time(
        std::time::SystemTime::now() - std::time::Duration::from_secs(120),
    );
    filetime::set_file_mtime(snapshots.join("audiolib-snapshot-stale"), old).unwrap();
    // A freshly created, owned snapshot directory may belong to another
    // instance that is still starting up, so it must be kept.
    fs::create_dir_all(snapshots.join("audiolib-snapshot-fresh")).unwrap();
    fs::write(
        snapshots
            .join("audiolib-snapshot-fresh")
            .join(".owner.lock"),
        b"",
    )
    .unwrap();
    fs::write(snapshots.join("audiolib-snapshot-file"), b"keep").unwrap();
    fs::create_dir_all(snapshots.join("keep-me")).unwrap();
    let _reopened = AppState::open(&f.config, BearerToken::new(TOKEN).unwrap()).unwrap();
    assert!(!snapshots.join("audiolib-snapshot-stale").exists());
    assert!(snapshots.join("audiolib-snapshot-fresh").is_dir());
    assert!(snapshots.join("audiolib-snapshot-file").is_file());
    assert!(snapshots.join("keep-me").is_dir());
    #[cfg(unix)]
    {
        // A symlink with our prefix is not followed or removed.
        let target = f.dir.path().join("precious");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("data"), b"keep").unwrap();
        std::os::unix::fs::symlink(&target, snapshots.join("audiolib-snapshot-link")).unwrap();
        let _reopened = AppState::open(&f.config, BearerToken::new(TOKEN).unwrap()).unwrap();
        assert!(target.join("data").is_file());
        assert!(fs::symlink_metadata(snapshots.join("audiolib-snapshot-link")).is_ok());
    }
}

/// Give audio `id` a Chinese file name on disk and in the library row.
fn rename_to_chinese(f: &Fixture, id: i64) -> std::path::PathBuf {
    let path = f.dir.path().join("sounds").join("雷声 近.wav");
    fs::rename(f.dir.path().join("sounds").join("door.wav"), &path).unwrap();
    let conn = Connection::open(&f.db).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute(
        "UPDATE audio_files SET file_path=?1, filename=?2 WHERE id=?3",
        rusqlite::params![path.to_string_lossy(), "雷声 近.wav", id],
    )
    .unwrap();
    path
}

const CHINESE_DISPOSITION: &str =
    "attachment; filename=\"audio-ID.wav\"; filename*=UTF-8''%E9%9B%B7%E5%A3%B0%20%E8%BF%91.wav";

#[tokio::test]
async fn stream_download_adds_an_attachment_disposition_with_the_same_etag() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    let path = rename_to_chinese(&f, id);
    let expected = CHINESE_DISPOSITION.replace("ID", &id.to_string());
    let plain = send_with(
        &f.state,
        Method::GET,
        &format!("/api/v1/audio/{id}/stream"),
        &[],
    )
    .await;
    assert_eq!(plain.status(), StatusCode::OK);
    assert!(!plain.headers().contains_key(header::CONTENT_DISPOSITION));
    let plain_etag = header_str(&plain, header::ETAG).to_string();
    let _ = bytes(plain).await;

    for query in [
        "download=1",
        "download=true",
        "download=TRUE",
        "foo=bar&download=1",
    ] {
        let response = send_with(
            &f.state,
            Method::GET,
            &format!("/api/v1/audio/{id}/stream?{query}"),
            &[],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{query}");
        assert_eq!(header_str(&response, header::CONTENT_DISPOSITION), expected);
        assert_eq!(
            header_str(&response, header::ETAG),
            plain_etag,
            "ETag is byte-identical"
        );
        assert_eq!(header_str(&response, header::CONTENT_LENGTH), "52");
        assert_eq!(header_str(&response, header::CONTENT_TYPE), "audio/wav");
        assert_eq!(bytes(response).await, fs::read(&path).unwrap());
    }

    // HEAD: same headers, no body.
    let response = send_with(
        &f.state,
        Method::HEAD,
        &format!("/api/v1/audio/{id}/stream?download=1"),
        &[],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, header::CONTENT_DISPOSITION), expected);
    assert_eq!(header_str(&response, header::ETAG), plain_etag);
    assert_eq!(header_str(&response, header::CONTENT_LENGTH), "52");
    assert_eq!(header_str(&response, header::ACCEPT_RANGES), "bytes");
    assert!(bytes(response).await.is_empty());

    // Other values are rejected before any file is opened.
    for query in [
        "download=0",
        "download=yes",
        "download=",
        "download=1&download=2",
    ] {
        let response = send_with(
            &f.state,
            Method::GET,
            &format!("/api/v1/audio/{id}/stream?{query}"),
            &[],
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        let error: ErrorBody = json(response).await;
        assert_eq!(error.error_code, "VALIDATION", "{query}");
    }
}

#[tokio::test]
async fn stream_download_honours_if_none_match_and_ranges() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    let path = rename_to_chinese(&f, id);
    let expected = CHINESE_DISPOSITION.replace("ID", &id.to_string());
    let uri = format!("/api/v1/audio/{id}/stream?download=1");
    let first = send_with(&f.state, Method::GET, &uri, &[]).await;
    let etag = header_str(&first, header::ETAG).to_string();
    let _ = bytes(first).await;

    let response = send_with(
        &f.state,
        Method::GET,
        &uri,
        &[(header::IF_NONE_MATCH, &etag)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(header_str(&response, header::ETAG), etag);
    assert!(bytes(response).await.is_empty());

    // Resume an interrupted download: Range (+ If-Range) still yields 206.
    let content = fs::read(&path).unwrap();
    for headers in [
        vec![(header::RANGE, "bytes=10-")],
        vec![
            (header::RANGE, "bytes=10-"),
            (header::IF_RANGE, etag.as_str()),
        ],
    ] {
        let response = send_with(&f.state, Method::GET, &uri, &headers).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            header_str(&response, header::CONTENT_RANGE),
            "bytes 10-51/52"
        );
        assert_eq!(header_str(&response, header::CONTENT_LENGTH), "42");
        assert_eq!(header_str(&response, header::CONTENT_DISPOSITION), expected);
        assert_eq!(header_str(&response, header::ETAG), etag);
        assert_eq!(bytes(response).await, content[10..]);
    }
    let response = send_with(
        &f.state,
        Method::GET,
        &uri,
        &[(header::RANGE, "bytes=100-")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
}

#[tokio::test]
async fn stream_download_head_keeps_headers_on_the_wire() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    rename_to_chinese(&f, id);
    let expected = CHINESE_DISPOSITION.replace("ID", &id.to_string());
    let (address, stop, task) =
        spawn_server(router(f.state.clone()), ServeOptions::default()).await;
    let mut replies = Vec::new();
    for method in ["HEAD", "GET"] {
        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        client
            .write_all(
                format!(
                    "{method} /api/v1/audio/{id}/stream?download=1 HTTP/1.1\r\nHost: x\r\n\
                     Authorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await.unwrap();
        let split = reply.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8(reply[..split].to_vec()).unwrap();
        let body = &reply[split + 4..];
        let headers: std::collections::BTreeMap<String, String> = head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(": "))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.to_string()))
            .filter(|(name, _)| name != "date")
            .collect();
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert_eq!(headers["content-length"], "52", "{method}");
        assert_eq!(headers["content-disposition"], expected, "{method}");
        assert!(headers.contains_key("etag"));
        assert_eq!(
            body.len(),
            if method == "HEAD" { 0 } else { 52 },
            "{method}"
        );
        replies.push(headers);
    }
    assert_eq!(replies[0], replies[1], "HEAD and GET send the same headers");
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

/// A root policy with short waits for tests.
fn fast_policy() -> RootPolicy {
    RootPolicy {
        interval: Duration::from_secs(3600),
        waking_after: Duration::from_millis(100),
        offline_after: Duration::from_secs(60),
    }
}

/// Open a second `AppState` on the fixture library with a custom root probe.
fn state_with_probe(f: &Fixture, probe: RootProbe) -> AppState {
    let monitor = RootMonitor::start_with_probe(&f.config.roots, fast_policy(), probe);
    AppState::open_with_monitor(&f.config, BearerToken::new(TOKEN).unwrap(), monitor).unwrap()
}

async fn roots_of(state: &AppState) -> Vec<RootResponse> {
    let response = send(state, "/api/v1/roots", Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    json(response).await
}

async fn wait_for_root(state: &AppState, index: usize, status: RootState) -> RootResponse {
    let start = std::time::Instant::now();
    loop {
        let roots = roots_of(state).await;
        if roots[index].status == status {
            return roots[index].clone();
        }
        assert!(start.elapsed() < Duration::from_secs(5), "{roots:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn roots_expose_cached_online_status() {
    let f = fixture();
    let raw: serde_json::Value = loop {
        let raw: serde_json::Value = json(send(&f.state, "/api/v1/roots", Some(TOKEN)).await).await;
        if raw[0]["status"] == "online" {
            break raw;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let mut keys: Vec<_> = raw[0].as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "checked_at",
            "file_count",
            "last_scan",
            "missing_count",
            "name",
            "online",
            "scanning",
            "server_path",
            "status",
            "status_message",
            "webdav_url"
        ]
    );
    // The fixture library was built by a scan through the db API: its session shows up.
    assert_eq!(raw[0]["missing_count"], 0);
    assert_eq!(raw[0]["scanning"], false);
    assert_eq!(raw[0]["last_scan"]["state"], "completed");
    assert_eq!(raw[0]["last_scan"]["mode"], "full");
    assert_eq!(raw[0]["last_scan"]["added"], 3);
    assert_eq!(raw[0]["last_scan"]["subpath"], "");
    // `Aircraft` lies below the scanned directory but was never scanned itself.
    assert!(raw[1]["last_scan"].is_null());
    assert_eq!(raw[0]["online"], true);
    assert!(raw[0]["status_message"].is_null());
    assert!(raw[0]["checked_at"].as_u64().unwrap() > 1_700_000_000);
}

#[tokio::test]
async fn hung_root_checks_never_block_requests_and_report_checking() {
    let f = fixture();
    // Every check hangs, like a NAS disk that never answers; requests must not care.
    let (_hold, release) = std::sync::mpsc::channel::<()>();
    let release = std::sync::Mutex::new(release);
    let state = state_with_probe(
        &f,
        Arc::new(move |_: &Path| {
            let _ = release.lock().unwrap().recv();
            Err("released".into())
        }),
    );
    let id = audio_id(&f, "door.wav");
    let started = std::time::Instant::now();
    for path in [
        "/api/v1/health".to_string(),
        "/api/v1/library".to_string(),
        "/api/v1/roots".to_string(),
        "/api/v1/library/snapshot".to_string(),
        format!("/api/v1/audio/{id}"),
    ] {
        let response = send(&state, &path, Some(TOKEN)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let _ = bytes(response).await;
    }
    let response = send_json(
        &state,
        Method::PATCH,
        &format!("/api/v1/audio/{id}/metadata"),
        Some(r#"{"favorite":true}"#),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(started.elapsed() < Duration::from_secs(5));
    // A first check that is still pending is "checking" and counts as online.
    for root in roots_of(&state).await {
        assert_eq!(root.status, RootState::Checking, "{root:?}");
        assert!(root.online && root.checked_at.is_none());
    }
}

#[tokio::test]
async fn offline_roots_keep_metadata_and_answer_streams_with_503() {
    let f = fixture();
    let online = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe: RootProbe = {
        let online = online.clone();
        Arc::new(move |path: &Path| {
            if online.load(std::sync::atomic::Ordering::SeqCst) {
                Ok(path.to_path_buf())
            } else {
                Err("Root directory not found (volume not mounted?)".into())
            }
        })
    };
    let state = state_with_probe(&f, probe);
    let root = wait_for_root(&state, 0, RootState::Offline).await;
    assert!(!root.online);
    assert_eq!(
        root.status_message.as_deref(),
        Some("Root directory not found (volume not mounted?)")
    );
    assert_eq!(root.file_count, Some(3), "counts come from the library");

    // Metadata, snapshot and writes keep working; nothing is marked missing.
    let id = audio_id(&f, "door.wav");
    for path in [
        "/api/v1/library".to_string(),
        "/api/v1/library/snapshot".to_string(),
        format!("/api/v1/audio/{id}"),
    ] {
        let response = send(&state, &path, Some(TOKEN)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let _ = bytes(response).await;
    }
    assert!(!state.audio(id).unwrap().missing);

    // Streams and waveforms get a clear 503 without touching the disk (download=1 too).
    for endpoint in ["stream", "stream?download=1", "waveform"] {
        let response = send_with(
            &state,
            Method::GET,
            &format!("/api/v1/audio/{id}/{endpoint}"),
            &[],
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(header_str(&response, header::RETRY_AFTER), "5");
        let error: ErrorBody = json(response).await;
        assert_eq!(error.error_code, "ROOT_OFFLINE");
        assert_eq!(
            error.message,
            "Library root \"SFX_Main\" is offline: Root directory not found (volume not mounted?)"
        );
    }

    // The volume comes back: the very next stream rechecks the offline root inline
    // and is served at once (no 503 first, no waiting for the periodic check).
    online.store(true, std::sync::atomic::Ordering::SeqCst);
    let response = send_with(
        &state,
        Method::GET,
        &format!("/api/v1/audio/{id}/stream"),
        &[],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(bytes(response).await.len(), 52);
    assert_eq!(roots_of(&state).await[0].status, RootState::Online);
    let response = send_with(
        &state,
        Method::GET,
        &format!("/api/v1/audio/{id}/waveform"),
        &[],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn root_check_endpoint_rechecks_inline_and_returns_the_root() {
    let f = fixture();
    let online = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hang = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe: RootProbe = {
        let (online, hang) = (online.clone(), hang.clone());
        Arc::new(move |path: &Path| {
            while hang.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            if online.load(std::sync::atomic::Ordering::SeqCst) {
                Ok(path.to_path_buf())
            } else {
                Err("Root directory not found (volume not mounted?)".into())
            }
        })
    };
    let state = state_with_probe(&f, probe);
    wait_for_root(&state, 1, RootState::Offline).await;
    online.store(true, std::sync::atomic::Ordering::SeqCst);
    let response = send_json(
        &state,
        Method::POST,
        "/api/v1/roots/Aircraft/check",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let root: RootResponse = json(response).await;
    assert_eq!(root.name, "Aircraft");
    assert_eq!(root.status, RootState::Online);
    assert!(root.online && root.file_count == Some(2));
    // A check that does not answer within the budget returns the state at that point;
    // the check goes on in the background.
    hang.store(true, std::sync::atomic::Ordering::SeqCst);
    let started = std::time::Instant::now();
    let response = send_json(
        &state,
        Method::POST,
        "/api/v1/roots/Aircraft/check",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(started.elapsed() < Duration::from_secs(4));
    let root: RootResponse = json(response).await;
    assert_eq!(root.status, RootState::Waking, "slow is not offline");
    assert!(root.online);
    hang.store(false, std::sync::atomic::Ordering::SeqCst);
    // Unknown names are 404.
    let response = send_json(
        &state,
        Method::POST,
        "/api/v1/roots/Nope/check",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let error: ErrorBody = json(response).await;
    assert_eq!(error.error_code, "NOT_FOUND");
}

#[tokio::test]
async fn stuck_stream_opens_are_bounded_per_root() {
    let f = fixture_with("auth = true\nmax_streams = 1\nmax_previews = 1\n");
    let id = audio_id(&f, "door.wav");
    wait_for_root(&f.state, 0, RootState::Online).await;
    f.state.set_stream_open_delay(Duration::from_millis(1500));
    // 4 x (max_streams + max_previews) opens whose clients gave up (their threads are
    // still blocked).
    let stuck: Vec<_> = (0..8)
        .map(|_| {
            let state = f.state.clone();
            std::thread::spawn(move || state.open_audio(id).is_ok())
        })
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    let progress = audiolib_server::OpenProgress::default();
    match f.state.open_audio_reporting(id, &progress) {
        Err(audiolib_server::OpenError::RootOffline { root, reason }) => {
            assert_eq!(root, "SFX_Main");
            assert!(reason.contains("have not finished"), "{reason}");
        }
        other => panic!("expected RootOffline, got {other:?}"),
    }
    let (root, path) = progress.target().unwrap();
    assert_eq!(root, "SFX_Main");
    assert!(path.ends_with("door.wav"), "{path}");
    for handle in stuck {
        assert!(handle.join().unwrap());
    }
    f.state.set_stream_open_delay(Duration::ZERO);
    assert!(
        f.state.open_audio(id).is_ok(),
        "allowed again once they finished"
    );
}

#[tokio::test]
async fn a_stream_open_that_finds_the_root_gone_marks_it_offline() {
    let f = fixture();
    let id = audio_id(&f, "flyby_01.wav");
    wait_for_root(&f.state, 1, RootState::Online).await;
    // The volume disappears between checks: the open fails, the root is probed on the
    // stream path and reported offline instead of the file being "missing".
    let aircraft = f.config.roots[1].server_path.clone();
    let moved = aircraft.with_file_name("Aircraft.unmounted");
    fs::rename(&aircraft, &moved).unwrap();
    let response = send_with(
        &f.state,
        Method::GET,
        &format!("/api/v1/audio/{id}/stream"),
        &[],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let error: ErrorBody = json(response).await;
    assert_eq!(error.error_code, "ROOT_OFFLINE");
    assert!(
        error
            .message
            .starts_with("Library root \"Aircraft\" is offline"),
        "{error:?}"
    );
    let root = &roots_of(&f.state).await[1];
    assert_eq!(root.status, RootState::Offline);
    assert!(!f.state.audio(id).unwrap().missing);
    fs::rename(&moved, &aircraft).unwrap();
}

#[tokio::test]
async fn slow_disk_opens_are_never_cut_off_by_the_request_timeout() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    // Timed routes get 100 ms here; the stream open takes 600 ms (a waking disk).
    let app = router_with_request_timeout(f.state.clone(), Duration::from_millis(100));
    f.state.set_stream_open_delay(Duration::from_millis(600));
    for (method, uri) in [
        (Method::GET, format!("/api/v1/audio/{id}/stream")),
        (
            Method::HEAD,
            format!("/api/v1/audio/{id}/stream?download=1"),
        ),
        (Method::GET, format!("/api/v1/audio/{id}/stream?download=1")),
    ] {
        let started = std::time::Instant::now();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri(&uri)
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{method} {uri}");
        assert!(started.elapsed() >= Duration::from_millis(600));
        assert_eq!(header_str(&response, header::CONTENT_LENGTH), "52");
        let body = bytes(response).await;
        assert_eq!(body.len(), if method == Method::HEAD { 0 } else { 52 });
    }
    // Timed routes keep answering normally under the short limit.
    let response = app
        .oneshot(Request::get("/api/v1/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn disconnecting_while_the_disk_wakes_frees_the_stream_slot() {
    use tokio::io::AsyncWriteExt;
    let f = fixture_with("auth = true\nmax_streams = 1\n");
    let id = audio_id(&f, "door.wav");
    let (address, stop, task) =
        spawn_server(router(f.state.clone()), ServeOptions::default()).await;
    f.state.set_stream_open_delay(Duration::from_millis(1500));
    let started = std::time::Instant::now();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client
        .write_all(
            format!(
                "GET /api/v1/audio/{id}/stream HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // While the slow open is pending the only slot is taken.
    f.state.set_stream_open_delay(Duration::ZERO);
    let busy = send_with(
        &f.state,
        Method::GET,
        &format!("/api/v1/audio/{id}/stream"),
        &[],
    )
    .await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(client);
    // The disconnect drops the waiting handler and its slot long before the open ends.
    loop {
        let response = send_with(
            &f.state,
            Method::GET,
            &format!("/api/v1/audio/{id}/stream"),
            &[],
        )
        .await;
        if response.status() == StatusCode::OK {
            assert!(
                started.elapsed() < Duration::from_millis(1400),
                "{:?}",
                started.elapsed()
            );
            break;
        }
        assert!(
            started.elapsed() < Duration::from_millis(1400),
            "slot still held"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn equal_length_and_mtime_sources_get_distinct_waveforms_and_cache_entries() {
    let f = fixture();
    let quiet_id = audio_id(&f, "flyby_01.wav");
    let loud_id = audio_id(&f, "flyby_02.wav");
    let quiet = real_path(f.dir.path()).join("sounds/Aircraft/flyby_01.wav");
    let loud = real_path(f.dir.path()).join("sounds/Aircraft/flyby_02.wav");
    let mut loud_bytes = fs::read(&loud).unwrap();
    for sample in loud_bytes[44..].chunks_exact_mut(2) {
        sample.copy_from_slice(&16_000_i16.to_le_bytes());
    }
    fs::write(&loud, loud_bytes).unwrap();
    let modified = fs::metadata(&quiet).unwrap().modified().unwrap();
    fs::File::options()
        .write(true)
        .open(&loud)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(
        fs::metadata(&quiet).unwrap().len(),
        fs::metadata(&loud).unwrap().len()
    );
    assert_eq!(
        fs::metadata(&quiet).unwrap().modified().unwrap(),
        fs::metadata(&loud).unwrap().modified().unwrap()
    );
    let quiet_response = send(
        &f.state,
        &format!("/api/v1/audio/{quiet_id}/waveform?buckets=64&colors=0"),
        Some(TOKEN),
    )
    .await;
    let quiet_etag = header_str(&quiet_response, header::ETAG).to_owned();
    let quiet_body: serde_json::Value = json(quiet_response).await;
    let loud_response = send(
        &f.state,
        &format!("/api/v1/audio/{loud_id}/waveform?buckets=64&colors=0"),
        Some(TOKEN),
    )
    .await;
    let loud_etag = header_str(&loud_response, header::ETAG).to_owned();
    let loud_body: serde_json::Value = json(loud_response).await;
    assert_ne!(quiet_etag, loud_etag);
    assert_ne!(quiet_body, loud_body);
    assert_eq!(quiet_body["peaks"][0][0]["max"], 0.0);
    assert!(
        (loud_body["peaks"][0][0]["max"].as_f64().unwrap() - 16_000.0 / 32768.0).abs() < 0.00001
    );
    assert_eq!(
        fs::read_dir(f.state.waveform_cache_dir()).unwrap().count(),
        2
    );
}

#[tokio::test]
async fn waveform_is_cached_validated_and_compressed() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    let path = format!("/api/v1/audio/{id}/waveform?buckets=64&colors=0");

    let response = send_with(&f.state, Method::GET, &path, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let etag = header_str(&response, header::ETAG).to_owned();
    assert!(etag.starts_with("W/\"wf4-"), "{etag}");
    assert_eq!(header_str(&response, header::VARY), "accept-encoding");
    let body: serde_json::Value = json(response).await;
    // 4 frames, 64 requested columns -> one sample per pixel, covering the whole file.
    assert_eq!(body["frames"], 4);
    assert_eq!(body["sample_rate"], 48_000);
    assert_eq!(body["channels"], 1);
    assert_eq!(body["samples_per_pixel"], 1);
    assert_eq!(body["start_frame"], 0);
    assert_eq!(body["end_frame"], 4);
    assert_eq!(body["total_frames"], 4);
    assert_eq!(body["peaks"][0].as_array().unwrap().len(), 4);
    assert!(body["peaks"][0][0]["color"].is_null());
    assert_eq!(
        f.state.waveform_cache_dir(),
        f.dir.path().join("library").join("waveforms"),
        "default cache next to the library"
    );
    let cached = fs::read_dir(f.state.waveform_cache_dir()).unwrap().count();
    assert_eq!(cached, 1, "one json cache entry");

    // A repeat is served from the cache (only the file's metadata is read): a marked
    // cache entry comes back verbatim.
    let entry = fs::read_dir(f.state.waveform_cache_dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let original = fs::read(&entry).unwrap();
    fs::write(&entry, br#"{"from_cache":true}"#).unwrap();
    let response = send_with(&f.state, Method::GET, &path, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, header::ETAG), etag);
    assert_eq!(bytes(response).await, br#"{"from_cache":true}"#);
    fs::write(&entry, original).unwrap();

    // A changed file is a new entity: new ETag, recomputed.
    let file = real_path(f.dir.path()).join("sounds").join("door.wav");
    let modified = fs::metadata(&file).unwrap().modified().unwrap();
    wav(&file);
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(modified + Duration::from_secs(5))
        .unwrap();
    let response = send_with(&f.state, Method::GET, &path, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let etag = header_str(&response, header::ETAG).to_owned();
    assert_eq!(json::<serde_json::Value>(response).await["frames"], 4);
    assert_eq!(
        fs::read_dir(f.state.waveform_cache_dir()).unwrap().count(),
        2
    );

    let response = send_with(
        &f.state,
        Method::GET,
        &path,
        &[(header::IF_NONE_MATCH, etag.as_str())],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(header_str(&response, header::ETAG), etag);
    assert!(bytes(response).await.is_empty());

    // Different parameters are a different entity and a different cache entry.
    let ranged = format!("{path}&start_frame=1&end_frame=3");
    let response = send_with(&f.state, Method::GET, &ranged, &[]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let ranged_etag = header_str(&response, header::ETAG).to_owned();
    assert_ne!(ranged_etag, etag);
    let body: serde_json::Value = json(response).await;
    assert_eq!(body["start_frame"], 1);
    assert_eq!(body["end_frame"], 3);
    assert_eq!(body["frames"], 2);
    assert_eq!(body["total_frames"], 4);

    // zstd is a third entity: compressed bytes, uncompressed length via decode, distinct ETag.
    let response = send_with(
        &f.state,
        Method::GET,
        &ranged,
        &[(header::ACCEPT_ENCODING, "gzip, zstd;q=1.0")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, header::CONTENT_ENCODING), "zstd");
    let zstd_etag = header_str(&response, header::ETAG).to_owned();
    assert!(zstd_etag.ends_with("-zstd\""), "{zstd_etag}");
    assert_ne!(zstd_etag, ranged_etag);
    let decoded = zstd::decode_all(&bytes(response).await[..]).unwrap();
    let decoded: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(decoded["frames"], 2);
    assert_eq!(
        fs::read_dir(f.state.waveform_cache_dir()).unwrap().count(),
        4
    );

    // Validation and lookups.
    let bad = send_with(
        &f.state,
        Method::GET,
        &format!("/api/v1/audio/{id}/waveform?buckets=63"),
        &[],
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json::<ErrorBody>(bad).await.error_code, "VALIDATION");
    let missing = send(&f.state, "/api/v1/audio/999999/waveform", Some(TOKEN)).await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let empty = send_with(
        &f.state,
        Method::GET,
        &format!("/api/v1/audio/{id}/waveform?start_frame=10"),
        &[],
    )
    .await;
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json::<ErrorBody>(empty).await.error_code, "VALIDATION");
}

#[test]
fn a_cancelled_waveform_stops_decoding_and_caches_nothing() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    let params = audiolib_server::waveform::WaveformParams::parse(None, None, None, None).unwrap();
    let cancelled = std::sync::atomic::AtomicBool::new(true);
    let result = f.state.waveform(
        id,
        &params,
        false,
        None,
        &cancelled,
        &audiolib_server::OpenProgress::default(),
    );
    assert!(
        matches!(
            result,
            Err(audiolib_server::OpenError::Library(
                audiolib_core::Error::Cancelled
            ))
        ),
        "{result:?}"
    );
    assert!(!f.state.waveform_cache_dir().exists());
}

// ---------------------------------------------------------------------------------------
// GET /audio/{id}/preview (doc/api/README.md §8.1)

/// Write a PCM WAV (`bits` 16 or 24) whose sample for (frame, channel) is `sample`.
fn pcm_wav(
    path: &Path,
    rate: u32,
    channels: u16,
    bits: u16,
    frames: u32,
    sample: impl Fn(u32, u16) -> i32,
) {
    let width = usize::from(bits / 8);
    let block_align = u32::from(channels) * u32::from(bits / 8);
    let mut data = Vec::with_capacity(frames as usize * block_align as usize);
    for frame in 0..frames {
        for channel in 0..channels {
            data.extend_from_slice(&sample(frame, channel).to_le_bytes()[..width]);
        }
    }
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&rate.wrapping_mul(block_align).to_le_bytes());
    bytes.extend_from_slice(&(block_align as u16).to_le_bytes());
    bytes.extend_from_slice(&bits.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&data);
    fs::write(path, bytes).unwrap();
}

/// Point the library row of `door.wav` at a new file `name` in the scanned root, written
/// by `write`; returns the row id.
fn preview_source(f: &Fixture, name: &str, write: impl FnOnce(&Path)) -> i64 {
    let path = real_path(f.dir.path()).join("sounds").join(name);
    write(&path);
    let id = audio_id(f, "door.wav");
    set_file_path(f, id, &path);
    id
}

/// 24-bit stereo ramp: left = frame index, right = -frame index.
fn ramp(rate: u32, seconds: u32) -> impl FnOnce(&Path) {
    move |path: &Path| {
        pcm_wav(path, rate, 2, 24, rate * seconds, |frame, channel| {
            let value = (frame % (1 << 23)) as i32;
            if channel == 0 { value } else { -value }
        })
    }
}

/// 16-bit mono noise that FLAC cannot compress much (fills pipes and sockets).
fn noise(rate: u32, seconds: u32) -> impl FnOnce(&Path) {
    move |path: &Path| {
        pcm_wav(path, rate, 1, 16, rate * seconds, |frame, _| {
            let mut x = u64::from(frame).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            x ^= x >> 29;
            (x.wrapping_mul(0xBF58_476D_1CE4_E5B9) >> 48) as i16 as i32
        })
    }
}

fn ffmpeg_path() -> Option<std::path::PathBuf> {
    let path = std::path::PathBuf::from(audiolib_server::config::DEFAULT_FFMPEG_PATH);
    if audiolib_server::preview::ffmpeg_available(&path) {
        Some(path)
    } else {
        eprintln!("ffmpeg not found at {}; skipping", path.display());
        None
    }
}

/// Decode FLAC bytes with ffmpeg into interleaved i32 samples (24-bit values).
fn decode_flac(ffmpeg: &Path, flac: &[u8]) -> Vec<i32> {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("preview.flac");
    fs::write(&input, flac).unwrap();
    let output = std::process::Command::new(ffmpeg)
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(&input)
        .args(["-f", "s32le", "-c:a", "pcm_s32le", "pipe:1"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
        .stdout
        .chunks_exact(4)
        .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 8)
        .collect()
}

fn i24(bytes: &[u8]) -> i32 {
    i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]) >> 8
}

#[tokio::test]
async fn preview_validates_its_query() {
    let f = fixture();
    let id = preview_source(&f, "ramp.wav", ramp(48_000, 1));
    for query in [
        "",
        "?codec=opus",
        "?codec=pcm&sr=4000",
        "?codec=pcm&bits=32",
        "?codec=pcm&ch=0",
        "?codec=pcm&t=1",
        "?codec=flac&t=-1",
        "?codec=flac&t=abc",
        "?codec=pcm&ch=1&ch=2",
    ] {
        let response = send(
            &f.state,
            &format!("/api/v1/audio/{id}/preview{query}"),
            Some(TOKEN),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(json::<ErrorBody>(response).await.error_code, "VALIDATION");
    }
    let response = send(
        &f.state,
        "/api/v1/audio/999999/preview?codec=pcm",
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = send(
        &f.state,
        &format!("/api/v1/audio/{id}/preview?codec=pcm"),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn pcm_preview_decimates_with_exact_sizes_and_headers() {
    let f = fixture();
    // 96 kHz/24-bit stereo, 1.5 s + 1 frame: 48 kHz output has floor(144001 / 2) frames.
    let id = preview_source(&f, "hires.wav", |path| {
        pcm_wav(path, 96_000, 2, 24, 144_001, |frame, channel| {
            let x = (2.0 * std::f64::consts::PI * 1000.0 * f64::from(frame) / 96_000.0).sin();
            ((if channel == 0 { x } else { -x }) * 4_000_000.0) as i32
        })
    });
    let path = format!("/api/v1/audio/{id}/preview?codec=pcm&sr=48000&bits=16");
    let response = send(&f.state, &path, Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, header::CONTENT_TYPE), "audio/wav");
    assert_eq!(header_str(&response, header::ACCEPT_RANGES), "bytes");
    assert_eq!(
        header_str(&response, header::CACHE_CONTROL),
        "private, no-cache"
    );
    assert_eq!(
        header_str(&response, "x-preview-sample-rate".parse().unwrap()),
        "48000"
    );
    assert_eq!(
        header_str(&response, "x-preview-bits".parse().unwrap()),
        "16"
    );
    assert_eq!(
        header_str(&response, "x-preview-channels".parse().unwrap()),
        "2"
    );
    assert_eq!(
        header_str(&response, "x-preview-frames".parse().unwrap()),
        "72000"
    );
    assert_eq!(
        header_str(&response, "x-total-frames".parse().unwrap()),
        "72000"
    );
    assert_eq!(
        header_str(&response, "x-duration".parse().unwrap()),
        "1.500000"
    );
    assert_eq!(
        header_str(&response, header::CONTENT_LENGTH),
        (44 + 72_000 * 4).to_string()
    );
    let etag = header_str(&response, header::ETAG).to_string();
    assert!(
        etag.starts_with('"') && etag.contains("-pcm2-48000-16-2"),
        "{etag}"
    );
    let body = bytes(response).await;
    assert_eq!(body.len(), 44 + 72_000 * 4);
    assert_eq!(&body[0..4], b"RIFF");
    assert_eq!(u32::from_le_bytes(body[24..28].try_into().unwrap()), 48_000);
    // The 1 kHz tone survives: peak close to 4e6 / 256 = 15625 in 16-bit units.
    let peak = body[44..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
        .max()
        .unwrap();
    assert!((15_550..=15_700).contains(&peak), "{peak}");

    // Requested rates above the source never upsample; mono and 24-bit headers follow.
    let response = send(
        &f.state,
        &format!("/api/v1/audio/{id}/preview?codec=pcm&sr=192000&ch=1"),
        Some(TOKEN),
    )
    .await;
    assert_eq!(
        header_str(&response, "x-preview-sample-rate".parse().unwrap()),
        "96000"
    );
    assert_eq!(
        header_str(&response, "x-preview-bits".parse().unwrap()),
        "24"
    );
    assert_eq!(
        header_str(&response, "x-preview-channels".parse().unwrap()),
        "1"
    );
    assert_eq!(
        header_str(&response, header::CONTENT_LENGTH),
        (44 + 144_001 * 3).to_string()
    );
    // L + R cancel exactly in the mono fold.
    let body = bytes(response).await;
    assert!(body[44..].chunks_exact(3).all(|b| i24(b) == 0));

    // HEAD: same headers, no body.
    let head = send_with(&f.state, Method::HEAD, &path, &[]).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(header_str(&head, header::ETAG), etag);
    assert_eq!(
        header_str(&head, header::CONTENT_LENGTH),
        (44 + 72_000 * 4).to_string()
    );
    assert_eq!(
        header_str(&head, "x-preview-sample-rate".parse().unwrap()),
        "48000"
    );
    assert_eq!(
        header_str(&head, "x-total-frames".parse().unwrap()),
        "72000"
    );
    assert_eq!(header_str(&head, "x-duration".parse().unwrap()), "1.500000");
    assert!(bytes(head).await.is_empty());
}

#[tokio::test]
async fn pcm_preview_ranges_match_the_full_download_and_honour_validators() {
    let f = fixture();
    let id = preview_source(&f, "ramp.wav", ramp(96_000, 2));
    let path = format!("/api/v1/audio/{id}/preview?codec=pcm&sr=48000&bits=16");
    let full_response = send(&f.state, &path, Some(TOKEN)).await;
    let etag = header_str(&full_response, header::ETAG).to_string();
    let full = bytes(full_response).await;
    let total = full.len();
    assert_eq!(total, 44 + 96_000 * 4);
    for (range, first, last) in [
        ("bytes=0-99", 0, 99),
        ("bytes=40-47", 40, 47),
        ("bytes=100001-250000", 100_001, 250_000),
        ("bytes=-1000", total - 1000, total - 1),
        ("bytes=300000-", 300_000, total - 1),
    ] {
        let response = send_with(&f.state, Method::GET, &path, &[(header::RANGE, range)]).await;
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT, "{range}");
        assert_eq!(
            header_str(&response, header::CONTENT_RANGE),
            format!("bytes {first}-{last}/{total}")
        );
        assert_eq!(
            header_str(&response, header::CONTENT_LENGTH),
            (last - first + 1).to_string()
        );
        assert_eq!(bytes(response).await, full[first..=last], "{range}");
    }
    let response = send_with(
        &f.state,
        Method::GET,
        &path,
        &[(header::RANGE, &format!("bytes={total}-"))],
    )
    .await;
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        header_str(&response, header::CONTENT_RANGE),
        format!("bytes */{total}")
    );
    let response = send_with(
        &f.state,
        Method::GET,
        &path,
        &[(header::IF_NONE_MATCH, &etag)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        header_str(&response, "x-preview-sample-rate".parse().unwrap()),
        "48000"
    );
    let response = send_with(
        &f.state,
        Method::GET,
        &path,
        &[
            (header::RANGE, "bytes=0-9"),
            (header::IF_RANGE, "\"other\""),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = send_with(
        &f.state,
        Method::GET,
        &path,
        &[(header::RANGE, "bytes=0-9"), (header::IF_RANGE, &etag)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    // Other parameters give another ETag.
    let other = send_with(&f.state, Method::HEAD, &format!("{path}&ch=1"), &[]).await;
    assert_ne!(header_str(&other, header::ETAG), etag);

    // Without decimation a 24-bit preview is the source sample data: frame k at 44 + 6k.
    let identity = format!("/api/v1/audio/{id}/preview?codec=pcm&bits=24");
    let k = 123_457_u64;
    let response = send_with(
        &f.state,
        Method::GET,
        &identity,
        &[(
            header::RANGE,
            &format!("bytes={}-{}", 44 + 6 * k, 44 + 6 * k + 5),
        )],
    )
    .await;
    let frame = bytes(response).await;
    assert_eq!(
        (i24(&frame[0..3]), i24(&frame[3..6])),
        (k as i32, -(k as i32))
    );
}

#[tokio::test]
async fn pcm_preview_rejects_compressed_and_foreign_files() {
    let f = fixture();
    let adpcm = preview_source(&f, "adpcm.wav", |path| {
        let mut bytes = b"RIFF\x28\0\0\0WAVEfmt \x10\0\0\0".to_vec();
        bytes.extend_from_slice(&[0x11, 0, 1, 0, 0x80, 0xBB, 0, 0, 0, 0x5D, 0, 0, 0, 1, 4, 0]);
        bytes.extend_from_slice(b"data\x04\0\0\0\0\0\0\0");
        fs::write(path, bytes).unwrap();
    });
    let response = send(
        &f.state,
        &format!("/api/v1/audio/{adpcm}/preview?codec=pcm"),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let error: ErrorBody = json(response).await;
    assert_eq!(error.error_code, "PREVIEW_UNSUPPORTED_FORMAT");
    assert!(
        error.message.contains("IMA ADPCM") && error.message.contains("codec=flac"),
        "{}",
        error.message
    );
    let mp3 = preview_source(&f, "song.mp3", |path| {
        fs::write(path, b"ID3\x04\0\0\0\0\0\0\xff\xfb\x90\x64").unwrap()
    });
    let response = send(
        &f.state,
        &format!("/api/v1/audio/{mp3}/preview?codec=pcm"),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        json::<ErrorBody>(response).await.error_code,
        "PREVIEW_UNSUPPORTED_FORMAT"
    );
}

#[tokio::test]
async fn pcm_preview_rejects_malformed_and_unreasonable_headers_with_415() {
    let f = fixture();
    let expect_415 = |response: Response, what: &'static str| async move {
        assert_eq!(
            response.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{what}"
        );
        let error: ErrorBody = json(response).await;
        assert_eq!(error.error_code, "PREVIEW_UNSUPPORTED_FORMAT", "{what}");
        error.message
    };
    // A fmt chunk cut off by the end of the file (truncated header): 415, not 500 IO.
    let id = preview_source(&f, "cut.wav", |path| {
        let mut bytes = b"RIFF\x24\0\0\0WAVEfmt \x10\0\0\0".to_vec();
        bytes.extend_from_slice(&[1, 0, 2, 0, 0x80, 0xBB, 0, 0]);
        fs::write(path, bytes).unwrap();
    });
    let uri = format!("/api/v1/audio/{id}/preview?codec=pcm");
    let message = expect_415(send(&f.state, &uri, Some(TOKEN)).await, "truncated").await;
    assert!(message.contains("truncated"), "{message}");
    // A 4 GHz header rate (corrupt) is refused instead of overflowing the WAV header.
    let id = preview_source(&f, "fast.wav", |path| {
        pcm_wav(path, 4_000_000_000, 1, 16, 16, |_, _| 0)
    });
    let uri = format!("/api/v1/audio/{id}/preview?codec=pcm");
    let head = send_with(&f.state, Method::HEAD, &uri, &[]).await;
    assert_eq!(head.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let message = expect_415(send(&f.state, &uri, Some(TOKEN)).await, "4 GHz").await;
    assert!(message.contains("sample rate"), "{message}");
    // A prime rate has no small integer factor reaching sr=48000: 415 (codec=flac can
    // resample it) instead of a 1 Hz preview built with a 4.6-million-tap filter.
    let id = preview_source(&f, "prime.wav", |path| {
        pcm_wav(path, 96_001, 1, 16, 960, |_, _| 0)
    });
    let uri = format!("/api/v1/audio/{id}/preview?codec=pcm&sr=48000");
    let head = send_with(&f.state, Method::HEAD, &uri, &[]).await;
    assert_eq!(head.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let message = expect_415(send(&f.state, &uri, Some(TOKEN)).await, "prime").await;
    assert!(
        message.contains("96001") && message.contains("codec=flac"),
        "{message}"
    );
    // Without sr the same file plays at its own rate.
    let uri = format!("/api/v1/audio/{id}/preview?codec=pcm");
    let response = send(&f.state, &uri, Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_str(&response, "x-preview-sample-rate".parse().unwrap()),
        "96001"
    );
}

#[tokio::test]
async fn previews_have_their_own_slots_and_free_them_on_disconnect() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Saturated /stream slots (e.g. drag-out downloads) never make a preview BUSY.
    let f = fixture_with("auth = true\nmax_streams = 1\nmax_previews = 1\n");
    let id = preview_source(&f, "noise.wav", noise(48_000, 120));
    let stream = format!("/api/v1/audio/{id}/stream");
    let preview = format!("/api/v1/audio/{id}/preview?codec=pcm");
    let held_stream = send(&f.state, &stream, Some(TOKEN)).await;
    assert_eq!(held_stream.status(), StatusCode::OK);
    let busy = send(&f.state, &stream, Some(TOKEN)).await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    let held_preview = send(&f.state, &preview, Some(TOKEN)).await;
    assert_eq!(held_preview.status(), StatusCode::OK);
    // The single preview slot is taken: the next preview is 503 BUSY + Retry-After 1.
    let busy = send(&f.state, &preview, Some(TOKEN)).await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header_str(&busy, header::RETRY_AFTER), "1");
    assert_eq!(json::<ErrorBody>(busy).await.error_code, "BUSY");
    if ffmpeg_path().is_some() {
        // flac counts against the same preview slots (besides max_transcodes).
        let flac = format!("/api/v1/audio/{id}/preview?codec=flac");
        let busy = send(&f.state, &flac, Some(TOKEN)).await;
        assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(header_str(&busy, header::RETRY_AFTER), "1");
        assert_eq!(json::<ErrorBody>(busy).await.error_code, "BUSY");
    }
    // Releasing the stream frees /stream only; releasing the preview frees previews.
    drop(held_stream);
    let response = send(&f.state, &stream, Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    drop(held_preview);
    let response = send(&f.state, &preview, Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);

    // A real client that reads a little and disconnects releases the preview slot.
    let (address, stop, task) =
        spawn_server(router(f.state.clone()), ServeOptions::default()).await;
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client
        .write_all(
            format!("GET {preview} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut buf = vec![0_u8; 64 * 1024];
    let n = client.read(&mut buf).await.unwrap();
    assert!(String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"));
    let busy = send_with(&f.state, Method::HEAD, &preview, &[]).await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(client);
    let started = std::time::Instant::now();
    loop {
        let response = send_with(&f.state, Method::HEAD, &preview, &[]).await;
        if response.status() == StatusCode::OK {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "slot still held"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn previews_follow_the_slow_and_offline_root_rules() {
    let f = fixture();
    let id = preview_source(&f, "ramp.wav", ramp(48_000, 1));
    // A waking disk (600 ms open) is not cut off by a 100 ms request timeout.
    let app = router_with_request_timeout(f.state.clone(), Duration::from_millis(100));
    f.state.set_stream_open_delay(Duration::from_millis(600));
    let mut uris = vec![format!("/api/v1/audio/{id}/preview?codec=pcm")];
    if ffmpeg_path().is_some() {
        uris.push(format!("/api/v1/audio/{id}/preview?codec=flac"));
    }
    for uri in &uris {
        let started = std::time::Instant::now();
        let response = app
            .clone()
            .oneshot(
                Request::get(uri)
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert!(started.elapsed() >= Duration::from_millis(600));
        let _ = bytes(response).await;
    }
    f.state.set_stream_open_delay(Duration::ZERO);

    // An offline root answers 503 ROOT_OFFLINE + Retry-After: 5 for both codecs.
    let probe: RootProbe =
        Arc::new(|_: &Path| Err("Root directory not found (volume not mounted?)".into()));
    let state = state_with_probe(&f, probe);
    wait_for_root(&state, 0, RootState::Offline).await;
    for uri in &uris {
        let response = send_with(&state, Method::GET, uri, &[]).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(header_str(&response, header::RETRY_AFTER), "5");
        assert_eq!(json::<ErrorBody>(response).await.error_code, "ROOT_OFFLINE");
    }
}

#[tokio::test]
async fn flac_preview_streams_flac_from_the_reported_start() {
    let Some(ffmpeg) = ffmpeg_path() else { return };
    let f = fixture();
    let id = preview_source(&f, "ramp.wav", ramp(48_000, 10));
    let path = format!("/api/v1/audio/{id}/preview?codec=flac&bits=24&t=3.21");
    let response = send(&f.state, &path, Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, header::CONTENT_TYPE), "audio/flac");
    assert_eq!(
        header_str(&response, header::CACHE_CONTROL),
        "private, no-cache"
    );
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    let etag = header_str(&response, header::ETAG).to_string();
    assert!(
        etag.starts_with("W/\"")
            && etag.contains("-flac1-")
            && etag.ends_with("-48000-24-2-154080\""),
        "{etag}"
    );
    // Output frames from the start at the output rate, and their duration.
    assert_eq!(
        header_str(&response, "x-total-frames".parse().unwrap()),
        "325920"
    );
    assert_eq!(
        header_str(&response, "x-duration".parse().unwrap()),
        "6.790000"
    );
    assert_eq!(
        header_str(&response, "x-start-time".parse().unwrap()),
        "3.210000"
    );
    assert_eq!(
        header_str(&response, "x-start-frame".parse().unwrap()),
        "154080"
    );
    assert_eq!(
        header_str(&response, "x-preview-sample-rate".parse().unwrap()),
        "48000"
    );
    assert_eq!(
        header_str(&response, "x-preview-bits".parse().unwrap()),
        "24"
    );
    assert_eq!(
        header_str(&response, "x-preview-channels".parse().unwrap()),
        "2"
    );
    let body = bytes(response).await;
    assert_eq!(&body[0..4], b"fLaC");
    // Sample-accurate: the first decoded frame is source frame 154080, to the end.
    let samples = decode_flac(&ffmpeg, &body);
    assert_eq!((samples[0], samples[1]), (154_080, -154_080));
    assert_eq!(samples.len(), (480_000 - 154_080) * 2);
    assert!(f.state.transcode_pids().is_empty());

    // If-None-Match: 304 with the same headers, without starting ffmpeg.
    let response = send_with(
        &f.state,
        Method::GET,
        &path,
        &[(header::IF_NONE_MATCH, &etag)],
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(header_str(&response, header::ETAG), etag);
    assert_eq!(
        header_str(&response, "x-total-frames".parse().unwrap()),
        "325920"
    );
    assert!(f.state.transcode_pids().is_empty());
    assert!(bytes(response).await.is_empty());
    // Another start (or any other parameter) is another ETag.
    let other = send_with(
        &f.state,
        Method::HEAD,
        &format!("/api/v1/audio/{id}/preview?codec=flac&bits=24&t=3.22"),
        &[(header::IF_NONE_MATCH, &etag)],
    )
    .await;
    assert_eq!(other.status(), StatusCode::OK);
    assert_ne!(header_str(&other, header::ETAG), etag);

    // Decimated, 16-bit, mono, with a Range header that is ignored (200, whole stream).
    let response = send_with(
        &f.state,
        Method::GET,
        &format!("/api/v1/audio/{id}/preview?codec=flac&sr=24000&bits=16&ch=1&t=1.5"),
        &[(header::RANGE, "bytes=100-200")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header_str(&response, "x-start-frame".parse().unwrap()),
        "36000"
    );
    assert_eq!(
        header_str(&response, "x-preview-sample-rate".parse().unwrap()),
        "24000"
    );
    assert_eq!(
        header_str(&response, "x-preview-bits".parse().unwrap()),
        "16"
    );
    assert_eq!(
        header_str(&response, "x-preview-channels".parse().unwrap()),
        "1"
    );
    assert_eq!(
        header_str(&response, "x-total-frames".parse().unwrap()),
        "204000"
    );
    assert_eq!(
        header_str(&response, "x-duration".parse().unwrap()),
        "8.500000"
    );
    let body = bytes(response).await;
    assert_eq!(&body[0..4], b"fLaC");
    assert_eq!(decode_flac(&ffmpeg, &body).len(), 24_000 * 10 - 36_000);

    // HEAD: the same headers without starting ffmpeg; t beyond the end is 416.
    let head = send_with(&f.state, Method::HEAD, &path, &[]).await;
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(
        header_str(&head, "x-start-frame".parse().unwrap()),
        "154080"
    );
    assert!(!head.headers().contains_key(header::CONTENT_LENGTH));
    assert_eq!(header_str(&head, header::ETAG), etag);
    assert_eq!(
        header_str(&head, "x-total-frames".parse().unwrap()),
        "325920"
    );
    assert_eq!(header_str(&head, "x-duration".parse().unwrap()), "6.790000");
    assert!(f.state.transcode_pids().is_empty());
    assert!(bytes(head).await.is_empty());
    let response = send(
        &f.state,
        &format!("/api/v1/audio/{id}/preview?codec=flac&t=10"),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        json::<ErrorBody>(response).await.error_code,
        "RANGE_NOT_SATISFIABLE"
    );
}

/// Whether process `pid` still exists (zombies included).
fn process_exists(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn flac_preview_kills_ffmpeg_when_the_client_disconnects() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if ffmpeg_path().is_none() {
        return;
    }
    let f = fixture_with("auth = true\nmax_transcodes = 1\n");
    let id = preview_source(&f, "noise.wav", noise(48_000, 120));
    let uri = format!("/api/v1/audio/{id}/preview?codec=flac");
    let (address, stop, task) =
        spawn_server(router(f.state.clone()), ServeOptions::default()).await;
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    client
        .write_all(
            format!("GET {uri} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut buf = vec![0_u8; 16 * 1024];
    let n = client.read(&mut buf).await.unwrap();
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("transfer-encoding: chunked"),
        "{head}"
    );
    let pids = f.state.transcode_pids();
    assert_eq!(pids.len(), 1);
    let pid = pids[0];
    assert!(process_exists(pid));
    // The only transcode slot is taken: another request waits 0.5 s, then 503 BUSY.
    let started = std::time::Instant::now();
    let busy = send(&f.state, &uri, Some(TOKEN)).await;
    assert!(started.elapsed() >= Duration::from_millis(450));
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header_str(&busy, header::RETRY_AFTER), "1");
    assert_eq!(json::<ErrorBody>(busy).await.error_code, "BUSY");

    drop(client);
    let started = std::time::Instant::now();
    while process_exists(pid) || !f.state.transcode_pids().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "ffmpeg {pid} still running"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    eprintln!(
        "ffmpeg {pid} gone {:?} after the disconnect",
        started.elapsed()
    );
    // The slot is free again.
    let response = send(&f.state, &format!("{uri}&t=119"), Some(TOKEN)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(&bytes(response).await[0..4], b"fLaC");
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn flac_preview_without_ffmpeg_is_503_and_pcm_still_works() {
    let f = fixture_with("auth = true\nffmpeg_path = \"/nonexistent/ffmpeg\"\n");
    let id = preview_source(&f, "ramp.wav", ramp(48_000, 1));
    for method in [Method::GET, Method::HEAD] {
        let response = send_with(
            &f.state,
            method.clone(),
            &format!("/api/v1/audio/{id}/preview?codec=flac"),
            &[],
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{method}"
        );
        assert!(!response.headers().contains_key(header::RETRY_AFTER));
        if method == Method::GET {
            let error: ErrorBody = json(response).await;
            assert_eq!(error.error_code, "TRANSCODER_UNAVAILABLE");
            assert!(!error.message.contains("/nonexistent"), "{}", error.message);
        }
    }
    let response = send(
        &f.state,
        &format!("/api/v1/audio/{id}/preview?codec=pcm"),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------------------
// Authentication modes, admin page and native TLS (doc/api/README.md §2.2, §2.3, §10.1).

/// Peer of a connection accepted by the plain or TLS listener.
fn peer(address: &str, listener: Listener) -> ConnectionInfo {
    ConnectionInfo {
        peer: address.parse().unwrap(),
        listener,
        local: None,
    }
}

/// Send `method path` through the router with headers, an optional connection (absent =
/// in-process, which counts as LAN-direct) and an optional JSON body.
async fn call(
    service: &(impl Into<Service> + Clone),
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    connection: Option<ConnectionInfo>,
    body: Option<&str>,
) -> Response {
    let mut request = Request::builder().method(method).uri(path);
    request = support::modern(request);
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        request = request.header(header::HOST, "localhost");
    }
    if let Ok(state) = service.clone().into().app_state() {
        request = request.header(
            "x-audiolib-library-id",
            state.revision().unwrap().library_id,
        );
    }
    if body.is_some()
        && !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
    {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let connection = connection.unwrap_or(ConnectionInfo {
        peer: "192.168.1.2:1".parse().unwrap(),
        listener: Listener::Http,
        local: None,
    });
    request.extensions_mut().insert(connection);
    router(service.clone()).oneshot(request).await.unwrap()
}

const ADMIN: (&str, &str) = ("x-audiolib-admin", "1");

/// Request headers plus the connection a request arrives on.
type RequestCase = (Vec<(&'static str, &'static str)>, Option<ConnectionInfo>);

/// Generate a token through the admin API; returns `(id, secret)`.
async fn admin_token(state: &AppState, name: &str) -> (String, String) {
    let response = call(
        state,
        Method::POST,
        "/admin/api/tokens",
        &[ADMIN],
        None,
        Some(&format!("{{\"name\":\"{name}\"}}")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: serde_json::Value = json(response).await;
    (
        body["token"]["id"].as_str().unwrap().to_string(),
        body["secret"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn external_mode_needs_tokens_only_for_proxied_or_public_requests() {
    // Production-like configuration: `auth` absent (false), so `external`. The fixture
    // passes TOKEN as the environment token, which stays valid for external clients.
    let f = fixture_with("");
    let id = audio_id(&f, "door.wav");
    let (revoked_id, revoked) = admin_token(&f.state, "old laptop").await;
    let (_, good) = admin_token(&f.state, "office laptop").await;
    let response = call(
        &f.state,
        Method::DELETE,
        &format!("/admin/api/tokens/{revoked_id}"),
        &[ADMIN],
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let lan = [
        None,
        Some(peer("192.168.1.2:50000", Listener::Http)),
        Some(peer("10.0.0.20:50000", Listener::Http)),
        Some(peer("[fe80::1]:50000", Listener::Http)),
        Some(peer("10.0.0.20:50000", Listener::Tls)),
    ];
    let paths = [
        "/api/v1/library".to_string(),
        "/api/v1/roots".to_string(),
        format!("/api/v1/audio/{id}/stream"),
        format!("/api/v1/audio/{id}/preview?codec=pcm&sr=48000&bits=16&ch=2"),
    ];
    // LAN-direct: Authorization is ignored entirely - no, garbage, revoked, valid.
    let garbage = "Bearer garbage".to_string();
    let revoked_header = format!("Bearer {revoked}");
    let good_header = format!("Bearer {good}");
    for connection in lan {
        for authorization in [
            None,
            Some(garbage.as_str()),
            Some(revoked_header.as_str()),
            Some(good_header.as_str()),
            Some("Basic eA=="),
        ] {
            for path in &paths {
                let headers: Vec<(&str, &str)> = authorization
                    .map(|value| ("authorization", value))
                    .into_iter()
                    .collect();
                let response = call(&f.state, Method::GET, path, &headers, connection, None).await;
                assert!(
                    response.status().is_success(),
                    "{path} {connection:?} {authorization:?}: {}",
                    response.status()
                );
                let _ = bytes(response).await;
            }
        }
    }

    // External: forwarded (any forwarding header) or a public peer, including the TLS
    // listener's public peers.
    let external: Vec<RequestCase> = vec![
        (vec![("x-forwarded-for", "203.0.113.9")], None),
        (
            vec![("x-forwarded-for", "203.0.113.9")],
            Some(peer("192.168.1.2:1", Listener::Http)),
        ),
        (vec![("forwarded", "for=203.0.113.9;proto=https")], None),
        (vec![("x-real-ip", "203.0.113.9")], None),
        (vec![("x-forwarded-proto", "https")], None),
        (vec![], Some(peer("203.0.113.9:1", Listener::Http))),
        (vec![], Some(peer("203.0.113.9:1", Listener::Tls))),
    ];
    for (base, connection) in &external {
        for (authorization, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("Bearer garbage".to_string()), StatusCode::UNAUTHORIZED),
            (Some(format!("Bearer {revoked}")), StatusCode::UNAUTHORIZED),
            (Some(format!("Basic {good}")), StatusCode::UNAUTHORIZED),
            (Some(format!("Bearer {good}")), StatusCode::OK),
            (Some(format!("bearer {TOKEN}")), StatusCode::OK),
        ] {
            let mut headers: Vec<(&str, &str)> = base.clone();
            if let Some(value) = &authorization {
                headers.push(("authorization", value.as_str()));
            }
            let response = call(
                &f.state,
                Method::GET,
                "/api/v1/library",
                &headers,
                *connection,
                None,
            )
            .await;
            assert_eq!(response.status(), expected, "{headers:?} {connection:?}");
            if expected == StatusCode::UNAUTHORIZED {
                assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
                let body: ErrorBody = json(response).await;
                assert_eq!(body.error_code, "UNAUTHORIZED", "{}", body.message);
                assert!(!body.message.contains(&good) && !body.message.contains(TOKEN));
            }
        }
    }
    // Missing and invalid tokens share the code; the message tells them apart.
    let missing: ErrorBody = json(
        call(
            &f.state,
            Method::GET,
            "/api/v1/roots",
            &[("x-forwarded-for", "1.2.3.4")],
            None,
            None,
        )
        .await,
    )
    .await;
    assert!(missing.message.contains("required"), "{}", missing.message);
    let invalid: ErrorBody = json(
        call(
            &f.state,
            Method::GET,
            "/api/v1/roots",
            &[
                ("x-forwarded-for", "1.2.3.4"),
                ("authorization", "Bearer nope"),
            ],
            None,
            None,
        )
        .await,
    )
    .await;
    assert!(invalid.message.contains("invalid"), "{}", invalid.message);

    // Streams and previews through the proxy work with the token (Range included).
    let forwarded_good = [
        ("x-forwarded-for", "203.0.113.9"),
        ("authorization", good_header.as_str()),
    ];
    let mut ranged = forwarded_good.to_vec();
    ranged.push(("range", "bytes=0-43"));
    let response = call(&f.state, Method::GET, &paths[2], &ranged, None, None).await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    let response = call(
        &f.state,
        Method::GET,
        &paths[3],
        &forwarded_good,
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = bytes(response).await;
    // /health never needs a token.
    let response = call(
        &f.state,
        Method::GET,
        "/api/v1/health",
        &[("x-forwarded-for", "1.2.3.4")],
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    // The token's last use is recorded (minute resolution) and listed.
    let list: serde_json::Value =
        json(call(&f.state, Method::GET, "/admin/api/tokens", &[], None, None).await).await;
    let tokens = list["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 1, "revoked token is gone: {list}");
    assert_eq!(tokens[0]["name"], "office laptop");
    assert!(tokens[0]["last_used_at"].is_i64(), "{list}");
    assert!(
        tokens[0].get("sha256").is_none(),
        "hash never leaves the server"
    );
}

#[tokio::test]
async fn auth_modes_off_and_all_from_the_admin_page() {
    let f = fixture_with("");
    let (_, good) = admin_token(&f.state, "t").await;
    let set_mode = |mode: &'static str| {
        let state = f.state.clone();
        async move {
            let response = call(
                &state,
                Method::PUT,
                "/admin/api/settings",
                &[ADMIN],
                None,
                Some(&format!("{{\"auth_mode\":{mode}}}")),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            json::<serde_json::Value>(response).await
        }
    };
    let status = set_mode("\"all\"").await;
    assert_eq!(status["auth_mode"], "all");
    assert_eq!(status["auth_mode_source"], "admin");
    let response = call(&f.state, Method::GET, "/api/v1/library", &[], None, None).await;
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "all: LAN needs a token too"
    );
    let response = call(
        &f.state,
        Method::GET,
        "/api/v1/library",
        &[("authorization", &format!("Bearer {good}"))],
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    // The admin page itself is not token-protected (LAN only), so it cannot lock itself out.
    let response = call(&f.state, Method::GET, "/admin/api/status", &[], None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    set_mode("\"off\"").await;
    let response = call(
        &f.state,
        Method::GET,
        "/api/v1/library",
        &[("x-forwarded-for", "1.2.3.4")],
        None,
        None,
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "off: nobody needs a token"
    );
    let status = set_mode("null").await;
    assert_eq!(status["auth_mode"], "external");
    assert_eq!(status["auth_mode_source"], "default");
    let response = call(
        &f.state,
        Method::PUT,
        "/admin/api/settings",
        &[ADMIN],
        None,
        Some("{\"auth_mode\":\"some\"}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn admin_is_lan_only_and_changes_need_the_admin_header() {
    let f = fixture_with("");
    // The page and its script from the LAN.
    let response = call(
        &f.state,
        Method::GET,
        "/admin",
        &[("host", "192.0.2.10:8790")],
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(header_str(&response, header::CONTENT_TYPE).starts_with("text/html"));
    assert_eq!(header_str(&response, header::CACHE_CONTROL), "no-store");
    assert!(
        header_str(&response, header::CONTENT_SECURITY_POLICY).contains("frame-ancestors 'none'")
    );
    let html = String::from_utf8(bytes(response).await).unwrap();
    assert!(html.contains("/admin/admin.js"), "{html}");
    assert!(!html.contains("https://cdn"), "no external resources");
    let response = call(
        &f.state,
        Method::GET,
        "/admin/admin.js",
        &[],
        Some(peer("192.168.1.2:2", Listener::Http)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        String::from_utf8(bytes(response).await)
            .unwrap()
            .contains("X-AudioLib-Admin")
    );
    let status: serde_json::Value =
        json(call(&f.state, Method::GET, "/admin/api/status", &[], None, None).await).await;
    assert_eq!(status["auth_mode"], "external");
    assert!(status["public_url"].is_null());
    assert_eq!(status["env_token"], true);
    assert_eq!(status["tls"]["state"], "off");

    // Proxied, public, public-over-TLS or rebinding host names: 403 ADMIN_LAN_ONLY.
    let refused: Vec<RequestCase> = vec![
        (vec![("x-forwarded-for", "10.0.0.20")], None),
        (
            vec![("x-forwarded-for", "203.0.113.9")],
            Some(peer("192.168.1.2:1", Listener::Http)),
        ),
        (vec![("forwarded", "for=10.0.0.20")], None),
        (vec![("x-real-ip", "127.0.0.1")], None),
        (vec![], Some(peer("203.0.113.9:1", Listener::Http))),
        (vec![], Some(peer("203.0.113.9:1", Listener::Tls))),
        (
            vec![("host", "rebind.attacker.example:8790")],
            Some(peer("10.0.0.20:1", Listener::Http)),
        ),
        (vec![("host", "audiolib.example.com")], None),
    ];
    for path in ["/admin", "/admin/api/status", "/admin/api/tokens"] {
        for (headers, connection) in &refused {
            let response = call(&f.state, Method::GET, path, headers, *connection, None).await;
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "{path} {headers:?} {connection:?}"
            );
            let body: ErrorBody = json(response).await;
            assert_eq!(body.error_code, "ADMIN_LAN_ONLY");
        }
    }
    // LAN peers of the TLS listener and .local names are fine.
    let response = call(
        &f.state,
        Method::GET,
        "/admin/api/status",
        &[("host", "myhost.local:8791")],
        Some(peer("10.0.0.20:1", Listener::Tls)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    // Changes need X-AudioLib-Admin: 1 and, when Origin is sent, the same origin.
    let create = Some("{\"name\":\"x\"}");
    for headers in [
        vec![],
        vec![("x-audiolib-admin", "yes")],
        vec![
            ADMIN,
            ("host", "192.0.2.10:8790"),
            ("origin", "http://evil.example.com"),
        ],
        vec![ADMIN, ("host", "192.0.2.10:8790"), ("origin", "null")],
    ] {
        let response = call(
            &f.state,
            Method::POST,
            "/admin/api/tokens",
            &headers,
            None,
            create,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{headers:?}");
        assert_eq!(
            json::<ErrorBody>(response).await.error_code,
            "ADMIN_HEADER_REQUIRED"
        );
    }
    let response = call(&f.state, Method::DELETE, "/admin/api/tls", &[], None, None).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = call(
        &f.state,
        Method::PUT,
        "/admin/api/settings",
        &[],
        None,
        Some("{\"public_url\":\"https://a.example.com\"}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(f.state.security().public_url().is_none(), "nothing changed");
    let response = call(
        &f.state,
        Method::POST,
        "/admin/api/tokens",
        &[
            ADMIN,
            ("host", "192.0.2.10:8790"),
            ("origin", "http://192.0.2.10:8790"),
        ],
        None,
        create,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(header_str(&response, header::CACHE_CONTROL), "no-store");
    let response = call(
        &f.state,
        Method::DELETE,
        "/admin/api/tokens/doesnotexist",
        &[ADMIN],
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = call(
        &f.state,
        Method::POST,
        "/admin/api/tokens",
        &[ADMIN],
        None,
        Some("{\"name\":\"\"}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Public URL: validated, normalized, persisted beside the library and reported by /health.
    for bad in [
        "audiolib.example.com",
        "ftp://x.example.com",
        "https://u:p@x.example.com",
        "https://x.example.com/?q",
    ] {
        let response = call(
            &f.state,
            Method::PUT,
            "/admin/api/settings",
            &[ADMIN],
            None,
            Some(&format!("{{\"public_url\":\"{bad}\"}}")),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(json::<ErrorBody>(response).await.error_code, "VALIDATION");
    }
    let response = call(
        &f.state,
        Method::PUT,
        "/admin/api/settings",
        &[ADMIN],
        None,
        Some("{\"public_url\":\"HTTPS://AudioLib.Example.com:8443/audiolib/\"}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let status: serde_json::Value = json(response).await;
    assert_eq!(
        status["public_url"],
        "https://audiolib.example.com:8443/audiolib"
    );
    let health: serde_json::Value = json(send(&f.state, "/api/v1/health", None).await).await;
    assert_eq!(
        health["public_url"],
        "https://audiolib.example.com:8443/audiolib"
    );
    let settings_path = f.db.parent().unwrap().join("admin-settings.json");
    let saved: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    assert_eq!(
        saved["public_url"],
        "https://audiolib.example.com:8443/audiolib"
    );
    #[cfg(unix)]
    for name in ["admin-settings.json", "tokens.json"] {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(f.db.parent().unwrap().join(name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{name}");
    }
    // A restart (new state from the same files) keeps the settings.
    let reopened = AppState::open(&f.config, None).unwrap();
    assert_eq!(
        reopened.security().public_url().as_deref(),
        Some("https://audiolib.example.com:8443/audiolib")
    );
    let response = call(
        &f.state,
        Method::PUT,
        "/admin/api/settings",
        &[ADMIN],
        None,
        Some("{\"public_url\":null}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let health: serde_json::Value = json(send(&f.state, "/api/v1/health", None).await).await;
    assert!(health["public_url"].is_null());
}

/// TLS client verifier that accepts any certificate (the test talks to its own
/// self-signed listener); signatures are still checked.
#[derive(Debug)]
struct AcceptAnyCertificate(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// One HTTP/1.1 request over TLS to `127.0.0.1:port`; returns the raw response text.
async fn tls_get(port: u16, path: &str, extra: &str) -> std::io::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider)))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
    let mut stream = connector
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").unwrap(),
            tcp,
        )
        .await?;
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{extra}\r\n")
                .as_bytes(),
        )
        .await?;
    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response)).await;
    Ok(String::from_utf8_lossy(&response).into_owned())
}

#[tokio::test]
async fn native_tls_listener_follows_the_admin_settings_without_a_restart() {
    let f = fixture_with("");
    // Self-signed certificate generated for this test only.
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let tls_dir = f.dir.path().join("tls");
    fs::create_dir_all(&tls_dir).unwrap();
    let cert_path = real_path(&tls_dir).join("cert.pem");
    let key_path = real_path(&tls_dir).join("key.pem");
    fs::write(&cert_path, certified.cert.pem()).unwrap();
    fs::write(&key_path, certified.signing_key.serialize_pem()).unwrap();
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let service = Service::from(f.state.clone());
    let app = router(service.clone());
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let manager = audiolib_server::tls::spawn_manager(
        service.security().clone(),
        app,
        "127.0.0.1".parse().unwrap(),
        ServeOptions::default(),
        stop_rx,
    );

    // Bad input is rejected before anything is saved.
    let body = |cert: &Path, key: &Path, port: u16| {
        serde_json::json!({"cert_path": cert, "key_path": key, "port": port}).to_string()
    };
    for (cert, key) in [
        (tls_dir.join("missing.pem"), key_path.clone()),
        (cert_path.clone(), cert_path.clone()),
        (key_path.clone(), key_path.clone()),
        (Path::new("relative.pem").to_path_buf(), key_path.clone()),
    ] {
        let response = call(
            &service,
            Method::PUT,
            "/admin/api/tls",
            &[ADMIN],
            None,
            Some(&body(&cert, &key, port)),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{cert:?} {key:?}"
        );
        assert_eq!(json::<ErrorBody>(response).await.error_code, "VALIDATION");
    }
    assert!(!f.db.parent().unwrap().join("admin-settings.json").exists());

    let response = call(
        &service,
        Method::PUT,
        "/admin/api/tls",
        &[ADMIN],
        None,
        Some(&body(&cert_path, &key_path, port)),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let status: serde_json::Value = json(response).await;
    assert_eq!(status["tls"]["state"], "listening", "{status}");
    assert_eq!(status["tls"]["port"], port);
    assert_eq!(status["tls"]["source"], "admin");

    // The API over TLS: 127.0.0.1 is this host (a same-host reverse proxy looks the
    // same), so it is external and needs a token; health stays public. A forwarded
    // request through the TLS listener needs one too.
    let health = tls_get(port, "/api/v1/health", "").await.unwrap();
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");
    assert!(health.contains("\"status\":\"ok\""), "{health}");
    let library = tls_get(port, "/api/v1/library", "").await.unwrap();
    assert!(library.starts_with("HTTP/1.1 401"), "{library}");
    let library = tls_get(
        port,
        "/api/v1/library",
        &format!("Authorization: Bearer {TOKEN}\r\n"),
    )
    .await
    .unwrap();
    assert!(library.starts_with("HTTP/1.1 200"), "{library}");
    let forwarded = tls_get(port, "/api/v1/library", "X-Forwarded-For: 203.0.113.9\r\n")
        .await
        .unwrap();
    assert!(forwarded.starts_with("HTTP/1.1 401"), "{forwarded}");
    let authorized = tls_get(
        port,
        "/api/v1/library",
        &format!("X-Forwarded-For: 203.0.113.9\r\nAuthorization: Bearer {TOKEN}\r\n"),
    )
    .await
    .unwrap();
    assert!(authorized.starts_with("HTTP/1.1 200"), "{authorized}");
    let admin = tls_get(port, "/admin/api/status", "").await.unwrap();
    assert!(
        admin.starts_with("HTTP/1.1 403") && admin.contains("ADMIN_LAN_ONLY"),
        "a same-host (loopback) peer is not LAN, even over TLS: {admin}"
    );

    // Disabling closes the listener.
    let response = call(
        &service,
        Method::DELETE,
        "/admin/api/tls",
        &[ADMIN],
        None,
        None,
    )
    .await;
    let status: serde_json::Value = json(response).await;
    assert_eq!(status["tls"]["state"], "off", "{status}");
    let mut closed = false;
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
        {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(closed, "TLS port still accepting connections");
    stop_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(15), manager)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn legacy_rating_in_a_patch_is_ignored_and_stored_ratings_stay() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    Connection::open(&f.db)
        .unwrap()
        .execute("UPDATE audio_files SET user_rating=3 WHERE id=?1", [id])
        .unwrap();
    let response = send_json(
        &f.state,
        Method::PATCH,
        &format!("/api/v1/audio/{id}/metadata"),
        Some(r#"{"rating":5,"tags":"legacy ok"}"#),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: AudioWriteResponse = json(response).await;
    assert_eq!(body.file.tags, "legacy ok");
    let raw = serde_json::to_value(&body.file).unwrap();
    assert!(raw.get("rating").is_none(), "{raw}");
    let stored: i64 = Connection::open(&f.db)
        .unwrap()
        .query_row(
            "SELECT user_rating FROM audio_files WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, 3, "stored rating data is neither used nor modified");
}

fn stored_row(f: &Fixture, id: i64) -> (i64, String, String, String, i64, String) {
    Connection::open(&f.db)
        .unwrap()
        .query_row(
            "SELECT COALESCE(user_rating,-1), COALESCE(user_tags,''), COALESCE(user_notes,''), \
             COALESCE(user_color,''), COALESCE(favorite,0), COALESCE(modified_at,'') \
             FROM audio_files WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .unwrap()
}

#[tokio::test]
async fn legacy_rating_only_patch_is_a_200_noop() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    Connection::open(&f.db)
        .unwrap()
        .execute("UPDATE audio_files SET user_rating=3 WHERE id=?1", [id])
        .unwrap();
    let before_row = stored_row(&f, id);
    let before_rev = library_revision(&f.state).await;
    for body in [
        r#"{"rating":5}"#,
        r#"{"rating":null}"#,
        r#"{"rating":4,"tags":null,"favorite":null}"#,
    ] {
        let response = send_json(
            &f.state,
            Method::PATCH,
            &format!("/api/v1/audio/{id}/metadata"),
            Some(body),
            Some(TOKEN),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{body}");
        let reply: AudioWriteResponse = json(response).await;
        assert_eq!(reply.file.id, id);
        assert_eq!(reply.revision_before, before_rev);
        assert_eq!(reply.revision, before_rev, "no write, no revision bump");
        assert!(
            serde_json::to_value(&reply.file)
                .unwrap()
                .get("rating")
                .is_none()
        );
    }
    assert_eq!(stored_row(&f, id), before_row, "no field changed");
    assert_eq!(library_revision(&f.state).await, before_rev);
    // Unknown ids and missing auth keep their errors; a bare `{}` is still not a legacy patch.
    let missing = send_json(
        &f.state,
        Method::PATCH,
        "/api/v1/audio/999999/metadata",
        Some(r#"{"rating":5}"#),
        Some(TOKEN),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let anonymous = send_json(
        &f.state,
        Method::PATCH,
        &format!("/api/v1/audio/{id}/metadata"),
        Some(r#"{"rating":5}"#),
        None,
    )
    .await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let wrong_type = send_json(
        &f.state,
        Method::PATCH,
        &format!("/api/v1/audio/{id}/metadata"),
        Some(r#"{"rating":5,"favorite":"yes"}"#),
        Some(TOKEN),
    )
    .await;
    assert_eq!(wrong_type.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn legacy_rating_only_batch_is_a_200_noop() {
    let f = fixture();
    let id = audio_id(&f, "door.wav");
    let before_row = stored_row(&f, id);
    let before_rev = library_revision(&f.state).await;
    let response = send_json(
        &f.state,
        Method::POST,
        "/api/v1/audio/metadata/batch",
        Some(&format!(r#"{{"ids":[{id},{id}],"patch":{{"rating":2}}}}"#)),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let reply: BatchWriteResponse = json(response).await;
    assert_eq!(reply.report.updated, 0);
    assert_eq!(reply.files.len(), 1);
    assert_eq!(reply.revision_before, before_rev);
    assert_eq!(reply.revision, before_rev);
    assert_eq!(stored_row(&f, id), before_row);
    // Mixed patch applies the real fields as usual.
    let mixed = send_json(
        &f.state,
        Method::POST,
        "/api/v1/audio/metadata/batch",
        Some(&format!(
            r#"{{"ids":[{id}],"patch":{{"rating":2,"favorite":true}}}}"#
        )),
        Some(TOKEN),
    )
    .await;
    assert_eq!(mixed.status(), StatusCode::OK);
    let reply: BatchWriteResponse = json(mixed).await;
    assert_eq!(reply.report.updated, 1);
    assert_eq!(reply.revision, before_rev + 1);
    // Id validation still applies.
    let empty = send_json(
        &f.state,
        Method::POST,
        "/api/v1/audio/metadata/batch",
        Some(r#"{"ids":[],"patch":{"rating":2}}"#),
        Some(TOKEN),
    )
    .await;
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn legacy_sort_rating_and_rating_min_query_params_are_ignored() {
    let f = fixture();
    for path in [
        "/api/v1/library?sort=rating&rating_min=3",
        "/api/v1/library?rating_min=5",
        "/api/v1/roots?sort=rating",
        "/api/v1/history/plays?sort=rating&rating_min=2&limit=5",
        "/api/v1/history/scans?sort=rating",
    ] {
        let response = send(&f.state, path, Some(TOKEN)).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
}

#[tokio::test]
async fn admin_runtime_reports_in_memory_state_and_is_lan_only() {
    let f = fixture_with("max_streams = 3\nmax_previews = 5\nmax_transcodes = 2\n");
    let response = call(&f.state, Method::GET, "/admin/api/runtime", &[], None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header_str(&response, header::CACHE_CONTROL), "no-store");
    let runtime: serde_json::Value = json(response).await;
    println!(
        "GET /admin/api/runtime sample:\n{}",
        serde_json::to_string_pretty(&runtime).unwrap()
    );
    assert_eq!(runtime["library"], "ok");
    let slots = &runtime["slots"];
    assert_eq!(slots["streams"], serde_json::json!({"active": 0, "max": 3}));
    assert_eq!(
        slots["previews"],
        serde_json::json!({"active": 0, "max": 5})
    );
    assert_eq!(
        slots["transcodes"],
        serde_json::json!({"active": 0, "max": 2})
    );
    assert!(slots["waveforms"]["max"].as_u64().unwrap() >= 1);
    let list = &runtime["list_waveforms"];
    assert_eq!(list["enabled"], false);
    assert_eq!(list["rate"], 10);
    assert!(list["cache_limit_bytes"].as_u64().unwrap() > 0);
    assert!(list["job"].is_null());
    assert!(runtime["waveform_cache"]["limit_bytes"].as_u64().unwrap() > 0);
    let revision = f.state.revision().unwrap();
    assert_eq!(runtime["snapshot"]["library_id"], revision.library_id);
    assert_eq!(runtime["snapshot"]["revision"], revision.revision);
    assert_eq!(runtime["snapshot"]["preparing"], false);
    assert!(runtime["snapshot"]["cached"].is_null());
    let names: Vec<&str> = runtime["roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|root| root["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["SFX_Main", "Aircraft", "Unscanned"]);
    assert_eq!(runtime["update"]["state"], "disabled");
    assert_eq!(runtime["update"]["skip_reason"], "disabled");
    assert!(runtime["update"]["selected"].is_null());
    let update: serde_json::Value =
        json(call(&f.state, Method::GET, "/admin/api/update", &[], None, None).await).await;
    assert_eq!(update["state"], "disabled");
    assert_eq!(update["skipped_newer"], serde_json::json!([]));

    // A prepared snapshot and a changed rate show up without any extra work.
    f.state.prepare_snapshot().unwrap();
    let response = call(
        &f.state,
        Method::PUT,
        "/admin/api/settings",
        &[ADMIN],
        None,
        Some("{\"list_waveform_rate\":40}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let runtime: serde_json::Value =
        json(call(&f.state, Method::GET, "/admin/api/runtime", &[], None, None).await).await;
    assert_eq!(runtime["list_waveforms"]["rate"], 40);
    let cached = &runtime["snapshot"]["cached"];
    assert_eq!(cached["revision"], revision.revision);
    assert!(cached["bytes"].as_u64().unwrap() > 0);

    // Same guard as the other admin endpoints.
    for (headers, connection) in [
        (vec![("x-forwarded-for", "203.0.113.9")], None),
        (vec![], Some(peer("203.0.113.9:1", Listener::Http))),
        (vec![("host", "audiolib.example.com")], None),
    ] {
        let response = call(
            &f.state,
            Method::GET,
            "/admin/api/runtime",
            &headers,
            connection,
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body: ErrorBody = json(response).await;
        assert_eq!(body.error_code, "ADMIN_LAN_ONLY");
    }
    let response = call(
        &f.state,
        Method::POST,
        "/admin/api/runtime",
        &[ADMIN],
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn search_scope_path_uses_directory_segments_and_keeps_main_order() {
    let f = fixture();
    let main = audiolib_core::SearchRequest {
        query: "flyby".into(),
        limit: 10,
        ..Default::default()
    };
    let body = serde_json::to_string(&main).unwrap();
    let response = send_json(
        &f.state,
        Method::POST,
        "/api/v1/search",
        Some(&body),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page: serde_json::Value = json(response).await;
    let main_ids: Vec<i64> = page["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["id"].as_i64().unwrap())
        .collect();
    assert_eq!(main_ids.len(), 2, "{page}");
    assert!(page.get("next_cursor").is_none());

    let mut path = main.clone();
    // A schema-8 library without the published directory index reports readiness
    // explicitly instead of silently returning an empty path page.
    path.query = "Aircraft".into();
    path.scope = audiolib_core::SearchScope::Path;
    let response = send_json(
        &f.state,
        Method::POST,
        "/api/v1/search",
        Some(&serde_json::to_string(&path).unwrap()),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["retry-after"], "30");
    assert_eq!(
        json::<serde_json::Value>(response).await["error_code"],
        "migration_in_progress"
    );

    let migrated = audiolib_db::migration9::execute(
        &f.db,
        audiolib_db::migration9::Action::Start,
        &Default::default(),
        &std::sync::atomic::AtomicBool::new(false),
    );
    assert_eq!(migrated.state, audiolib_db::migration9::State::Done);
    let response = send_json(
        &f.state,
        Method::POST,
        "/api/v1/search",
        Some(&serde_json::to_string(&path).unwrap()),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page: serde_json::Value = json(response).await;
    // Aircraft still covers 2 of 3 files, so the existing broad-directory rule
    // discards that set. Publication must not change path-search semantics.
    assert_eq!(page["results"].as_array().unwrap().len(), 0, "{page}");
    let after = send_json(
        &f.state,
        Method::POST,
        "/api/v1/search",
        Some(&body),
        Some(TOKEN),
    )
    .await;
    assert_eq!(after.status(), StatusCode::OK);
    let after: serde_json::Value = json(after).await;
    assert_eq!(
        after["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        main_ids
    );

    let mut root = path.clone();
    // The scan root's own name is not indexed.
    root.query = "sounds".into();
    let response = send_json(
        &f.state,
        Method::POST,
        "/api/v1/search",
        Some(&serde_json::to_string(&root).unwrap()),
        Some(TOKEN),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let page: serde_json::Value = json(response).await;
    assert_eq!(page["results"].as_array().unwrap().len(), 0, "{page}");
}
