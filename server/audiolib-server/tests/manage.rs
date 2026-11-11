// server/audiolib-server/tests/manage.rs
//! Library management: `POST /audio/remove`, `DELETE /folders/{id}`, `GET /capabilities`
//! and the `manage_mode` gate (doc/api/README.md §6.8), against temporary libraries and
//! roots of tiny generated WAVs. Removal only ever touches the temporary library; the
//! tests check that every WAV is still on disk afterwards.
mod support;
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use audiolib_server::{
    AppState, BearerToken, CapabilitiesResponse, ConnectionInfo, ErrorBody, LibraryResponse,
    Listener, ManageMode, RemoveAudioResponse, RemoveFolderResponse, RootResponse, ScanResponse,
    Security, ServerConfig, Service, list_waveform::ListWaveformsCapability, router,
};
use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
    response::Response,
};
use http_body_util::BodyExt;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use tower::ServiceExt;

const TOKEN: &str = "test-token-0123456789abcdef0123456789abcdef";

/// A tiny silent 16-bit mono WAV whose length depends on `samples`.
fn wav(path: &Path, samples: usize) {
    let data = vec![0_u8; samples * 2];
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
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn real_path(path: &Path) -> PathBuf {
    let canonical = path.canonicalize().unwrap();
    let text = canonical.to_string_lossy().into_owned();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => rest.into(),
        _ => canonical,
    }
}

/// `base` joined with the `/`-separated `relative` path component by component, so the
/// result uses the platform separator (on Windows `join("a/b")` would keep the `/`, and
/// such a path never equals the `\\`-separated paths the scanner stores in the library).
fn under(base: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(base.to_path_buf(), |path, part| path.join(part))
}

const FILES: [&str; 5] = [
    "Doors/door_1.wav",
    "Doors/door_2.wav",
    "Doors/Inner/knock.wav",
    "wind.wav",
    "rain.wav",
];

struct Fixture {
    _dir: tempfile::TempDir,
    sounds: PathBuf,
    db: PathBuf,
    config: ServerConfig,
    state: AppState,
}

/// A library with root `SFX` (`FILES`, scanned) in external auth mode with the
/// environment token. `extra_roots` adds roots as `(name, "/"-separated path under the
/// SFX root)`.
fn fixture(extra_roots: &[(&str, &str)]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = real_path(dir.path());
    let sounds = base.join("sounds");
    for file in FILES {
        wav(&under(&sounds, file), 4);
    }
    let db = dir.path().join("library").join("library.db");
    let writer = Arc::new(SqliteProvider::open(&db).unwrap());
    let progress = ScanService::new(writer)
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
    assert_eq!(progress.added, FILES.len() as u64, "{progress:?}");
    let config = ServerConfig::from_toml_str(
        &format!(
            "bind = \"127.0.0.1:0\"\nlibrary_db = 'library/library.db'\nsnapshot_dir = 'snapshots'\n\
             [[roots]]\nname = \"SFX\"\nserver_path = '{}'\n{}",
            sounds.display(),
            extra_roots
                .iter()
                .map(|(name, relative)| format!(
                    "[[roots]]\nname = \"{name}\"\nserver_path = '{}'\n",
                    under(&sounds, relative).display()
                ))
                .collect::<String>(),
        ),
        dir.path(),
    )
    .unwrap();
    let state = AppState::open(&config, BearerToken::new(TOKEN).unwrap()).unwrap();
    Fixture {
        _dir: dir,
        sounds,
        db,
        config,
        state,
    }
}

impl Fixture {
    fn audio_id(&self, name: &str) -> i64 {
        let conn = rusqlite::Connection::open(&self.db).unwrap();
        conn.query_row(
            "SELECT id FROM audio_files WHERE filename = ?1",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn folder_id(&self, relative: &str) -> i64 {
        // The scanner stores folder paths with `/` separators on every platform
        // (`C:/…/sounds/Doors` on Windows); compute the same text.
        let path = audiolib_db::folder_path_text(&under(&self.sounds, relative));
        let conn = rusqlite::Connection::open(&self.db).unwrap();
        conn.query_row(
            "SELECT id FROM folders WHERE folder_path = ?1",
            [&path],
            |row| row.get(0),
        )
        .unwrap_or_else(|error| panic!("folder {path:?}: {error}"))
    }

    fn count(&self, sql: &str) -> i64 {
        let conn = rusqlite::Connection::open(&self.db).unwrap();
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    /// Every generated WAV is still on disk, byte for byte.
    fn assert_files_untouched(&self) {
        for file in FILES {
            let bytes = fs::read(under(&self.sounds, file)).unwrap();
            assert_eq!(bytes.len(), 44 + 8, "{file}");
        }
    }
}

const LAN: &str = "10.0.0.20:50000";
const PUBLIC: &str = "203.0.113.9:50000";

fn peer(address: &str) -> ConnectionInfo {
    ConnectionInfo {
        peer: address.parse().unwrap(),
        listener: Listener::Http,
        local: None,
    }
}

async fn call(
    state: &AppState,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    connection: Option<ConnectionInfo>,
    body: Option<&str>,
) -> Response {
    call_service(
        Service::from(state.clone()),
        method,
        path,
        headers,
        connection,
        body,
    )
    .await
}

async fn call_service(
    service: Service,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    connection: Option<ConnectionInfo>,
    body: Option<&str>,
) -> Response {
    let mut request = Request::builder().method(method).uri(path);
    request = support::modern(request);
    if let Ok(state) = service.app_state() {
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
        request = request.header("content-type", "application/json");
    }
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        request = request.header("host", "localhost");
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
    router(service).oneshot(request).await.unwrap()
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
    let body = bytes(response).await;
    serde_json::from_slice(&body)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&body)))
}

/// LAN-direct request (no token needed in external mode).
async fn lan(state: &AppState, method: Method, path: &str, body: Option<&str>) -> Response {
    call(
        state,
        method,
        &format!("/api/v1{path}"),
        &[],
        Some(peer(LAN)),
        body,
    )
    .await
}

async fn ok<T: serde::de::DeserializeOwned>(response: Response) -> T {
    let status = response.status();
    let body = bytes(response).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

async fn error(response: Response, status: StatusCode, code: &str) -> ErrorBody {
    let actual = response.status();
    let body: ErrorBody = json(response).await;
    assert_eq!(
        (actual, body.error_code.as_str()),
        (status, code),
        "{}",
        body.message
    );
    body
}

async fn revision(state: &AppState) -> u64 {
    ok::<LibraryResponse>(lan(state, Method::GET, "/library", None).await)
        .await
        .revision
}

async fn remove(state: &AppState, body: &str) -> Response {
    lan(state, Method::POST, "/audio/remove", Some(body)).await
}

#[tokio::test]
async fn removing_audio_forgets_records_reports_unknown_ids_and_bumps_only_on_change() {
    let f = fixture(&[]);
    let state = &f.state;
    let (door, wind) = (f.audio_id("door_1.wav"), f.audio_id("wind.wav"));
    // Warm the snapshot cache, then make sure the removal invalidates it.
    let snapshot = lan(state, Method::GET, "/library/snapshot", None).await;
    assert_eq!(snapshot.status(), StatusCode::OK);
    drop(bytes(snapshot).await);
    let before = revision(state).await;
    let removed: RemoveAudioResponse = ok(remove(
        state,
        &format!(r#"{{"ids":[{door},999999999,{door},{wind}]}}"#),
    )
    .await)
    .await;
    assert_eq!(
        removed,
        RemoveAudioResponse {
            removed: 2,
            not_found: vec![999_999_999],
            revision_before: before,
            revision: before + 1,
        }
    );
    assert_eq!(revision(state).await, before + 1, "counted exactly once");
    assert_eq!(revision(state).await, before + 1);
    f.assert_files_untouched();
    error(
        lan(state, Method::GET, &format!("/audio/{door}"), None).await,
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
    )
    .await;
    assert_eq!(f.count("SELECT COUNT(*) FROM audio_files"), 3);
    let library: LibraryResponse = ok(lan(state, Method::GET, "/library", None).await).await;
    assert_eq!(library.info.stats.audio_files, 3);
    let root: Vec<RootResponse> = ok(lan(state, Method::GET, "/roots", None).await).await;
    assert_eq!(root[0].file_count, Some(3), "folder statistics refreshed");
    let snapshot = lan(state, Method::GET, "/library/snapshot", None).await;
    assert_eq!(
        snapshot.headers()["x-library-revision"],
        (before + 1).to_string().as_str()
    );
    drop(bytes(snapshot).await);

    // Nothing to remove: 200, no bump, every id reported.
    for body in [
        r#"{"ids":[999999999]}"#.to_string(),
        format!(r#"{{"ids":[{door},{wind},{door}]}}"#),
    ] {
        let nothing: RemoveAudioResponse = ok(remove(state, &body).await).await;
        assert_eq!(nothing.removed, 0);
        assert_eq!(nothing.revision_before, before + 1);
        assert_eq!(nothing.revision, before + 1, "{body}");
        assert!(!nothing.not_found.is_empty());
    }
    let nothing: RemoveAudioResponse =
        ok(remove(state, &format!(r#"{{"ids":[{wind},{door}]}}"#)).await).await;
    assert_eq!(nothing.not_found, vec![wind, door], "request order");
    // 10000 ids are accepted (deduplicated), 10001 are not.
    let many: Vec<String> = (1_000_000..1_010_000).map(|id| id.to_string()).collect();
    let all: RemoveAudioResponse =
        ok(remove(state, &format!(r#"{{"ids":[{}]}}"#, many.join(","))).await).await;
    assert_eq!((all.removed, all.not_found.len()), (0, 10_000));
    assert_eq!(revision(state).await, before + 1);

    // Validation: 400 before anything else, nothing changes.
    let too_many = format!(r#"{{"ids":[{},1]}}"#, many.join(","));
    for body in [
        r#"{"ids":[]}"#,
        r#"{"ids":[0]}"#,
        r#"{"ids":[-3]}"#,
        r#"{"ids":"1"}"#,
        r#"{"ids":[1.5]}"#,
        r#"{}"#,
        "not json",
        too_many.as_str(),
    ] {
        error(
            remove(state, body).await,
            StatusCode::BAD_REQUEST,
            "VALIDATION",
        )
        .await;
    }
    error(
        lan(state, Method::GET, "/audio/remove", None).await,
        StatusCode::METHOD_NOT_ALLOWED,
        "METHOD_NOT_ALLOWED",
    )
    .await;
    assert_eq!(revision(state).await, before + 1);
    assert_eq!(f.count("SELECT COUNT(*) FROM audio_files"), 3);

    // Files still on disk come back with the next scan (new ids).
    let scan: ScanResponse =
        ok(lan(state, Method::POST, "/scans", Some(r#"{"root":"SFX"}"#)).await).await;
    let done = wait_done(state, scan.progress.session_id).await;
    assert_eq!(done.progress.added, 2);
    assert_eq!(f.count("SELECT COUNT(*) FROM audio_files"), 5);
    assert_ne!(
        f.audio_id("door_1.wav"),
        door,
        "a re-added file gets a new id"
    );
    f.assert_files_untouched();
}

async fn wait_done(state: &AppState, id: i64) -> ScanResponse {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let scan: ScanResponse =
            ok(lan(state, Method::GET, &format!("/scans/{id}"), None).await).await;
        if !scan.active {
            return scan;
        }
        assert!(Instant::now() < deadline, "scan {id} did not finish");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn deleting_a_folder_removes_its_subtree_but_never_a_root_or_unknown_folder() {
    let f = fixture(&[]);
    let state = &f.state;
    let before = revision(state).await;
    let root_folder = f.folder_id("");
    let doors = f.folder_id("Doors");
    error(
        lan(state, Method::DELETE, "/folders/abc", None).await,
        StatusCode::BAD_REQUEST,
        "VALIDATION",
    )
    .await;
    error(
        lan(state, Method::DELETE, "/folders/999999999", None).await,
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
    )
    .await;
    let refused = error(
        lan(
            state,
            Method::DELETE,
            &format!("/folders/{root_folder}"),
            None,
        )
        .await,
        StatusCode::BAD_REQUEST,
        "VALIDATION",
    )
    .await;
    assert!(
        refused.message.contains("\"SFX\"") && refused.message.contains("/admin"),
        "{}",
        refused.message
    );
    assert_eq!(revision(state).await, before, "refusals change nothing");
    assert_eq!(f.count("SELECT COUNT(*) FROM audio_files"), 5);

    let removed: RemoveFolderResponse =
        ok(lan(state, Method::DELETE, &format!("/folders/{doors}"), None).await).await;
    assert_eq!(
        removed,
        RemoveFolderResponse {
            removed: 3,
            folders_removed: 2,
            revision_before: before,
            revision: before + 1,
        }
    );
    assert_eq!(revision(state).await, before + 1);
    f.assert_files_untouched();
    assert_eq!(f.count("SELECT COUNT(*) FROM audio_files"), 2);
    assert_eq!(f.count("SELECT COUNT(*) FROM folders"), 1);
    assert_eq!(
        f.count("SELECT COUNT(*) FROM folder_closure WHERE ancestor_id <> descendant_id"),
        0
    );
    let roots: Vec<RootResponse> = ok(lan(state, Method::GET, "/roots", None).await).await;
    assert_eq!(roots[0].file_count, Some(2));
    error(
        lan(state, Method::DELETE, &format!("/folders/{doors}"), None).await,
        StatusCode::NOT_FOUND,
        "NOT_FOUND",
    )
    .await;
    assert_eq!(revision(state).await, before + 1);
}

#[tokio::test]
async fn folders_containing_a_configured_root_are_refused() {
    // A nested root at Doors/Inner: Doors contains it, Inner is its top-level folder.
    let f = fixture(&[("Knocks", "Doors/Inner")]);
    let state = &f.state;
    for folder in ["Doors", "Doors/Inner"] {
        let id = f.folder_id(folder);
        let refused = error(
            lan(state, Method::DELETE, &format!("/folders/{id}"), None).await,
            StatusCode::BAD_REQUEST,
            "VALIDATION",
        )
        .await;
        assert!(
            refused.message.contains("\"Knocks\""),
            "{}",
            refused.message
        );
    }
    assert_eq!(f.count("SELECT COUNT(*) FROM audio_files"), 5);
}

async fn wait_paused(state: &AppState, id: i64) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let scan: ScanResponse =
            ok(lan(state, Method::GET, &format!("/scans/{id}"), None).await).await;
        if scan.progress.paused {
            return;
        }
        assert!(scan.active, "scan finished before pausing");
        assert!(Instant::now() < deadline, "scan {id} did not pause");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn removals_are_busy_while_any_scan_holds_the_library() {
    let f = fixture(&[]);
    let state = &f.state;
    let door = f.audio_id("door_1.wav");
    let doors = f.folder_id("Doors");
    for index in 0..3000 {
        wav(
            &f.sounds
                .join("Bulk")
                .join(format!("{:02}", index % 30))
                .join(format!("f_{index:04}.wav")),
            4,
        );
    }
    let before = revision(state).await;
    let busy_requests = |state: AppState| async move {
        for body in [
            format!(r#"{{"ids":[{door}]}}"#),
            r#"{"ids":[999999999]}"#.into(),
        ] {
            error(remove(&state, &body).await, StatusCode::CONFLICT, "BUSY").await;
        }
        for id in [doors, 999_999_999] {
            error(
                lan(&state, Method::DELETE, &format!("/folders/{id}"), None).await,
                StatusCode::CONFLICT,
                "BUSY",
            )
            .await;
        }
    };

    // This server's scan, paused: busy, and the held revision does not move.
    let scan: ScanResponse = ok(lan(
        state,
        Method::POST,
        "/scans",
        Some(r#"{"root":"SFX","mode":"full","workers":1}"#),
    )
    .await)
    .await;
    let id = scan.progress.session_id;
    assert_eq!(
        lan(state, Method::POST, &format!("/scans/{id}/pause"), None)
            .await
            .status(),
        StatusCode::OK
    );
    wait_paused(state, id).await;
    busy_requests(state.clone()).await;
    assert_eq!(revision(state).await, before, "revision still held");
    assert_eq!(
        lan(state, Method::POST, &format!("/scans/{id}/cancel"), None)
            .await
            .status(),
        StatusCode::OK
    );
    wait_done(state, id).await;
    assert_eq!(
        f.count(&format!(
            "SELECT COUNT(*) FROM audio_files WHERE id = {door}"
        )),
        1
    );

    // Another process (CLI / local-mode desktop) scanning the same library: busy too.
    let other = Arc::new(SqliteProvider::open(&f.db).unwrap());
    let control = ScanCancellation::default();
    assert!(control.pause());
    let (started, session) = mpsc::channel();
    let thread_control = control.clone();
    let sounds = f.sounds.clone();
    let cli = std::thread::spawn(move || {
        ScanService::new(other).scan(
            &sounds,
            &ScanOptions {
                workers: 1,
                ..Default::default()
            },
            &thread_control,
            |progress| {
                let _ = started.send(progress.session_id);
            },
        )
    });
    session.recv_timeout(Duration::from_secs(30)).unwrap();
    busy_requests(state.clone()).await;
    control.cancel();
    control.resume();
    cli.join().unwrap().unwrap();
    // Idle again: the removal goes through.
    let removed: RemoveAudioResponse =
        ok(remove(state, &format!(r#"{{"ids":[{door}]}}"#)).await).await;
    assert_eq!(removed.removed, 1);
    f.assert_files_untouched();
}

async fn capabilities(
    state: &AppState,
    headers: &[(&str, &str)],
    address: &str,
) -> CapabilitiesResponse {
    ok(call(
        state,
        Method::GET,
        "/api/v1/capabilities",
        headers,
        Some(peer(address)),
        None,
    )
    .await)
    .await
}

fn plain_library_server_catalog() -> audiolib_core::compatibility::CompatibilityManifest {
    let mut catalog = audiolib_core::compatibility::compatibility_manifest();
    // A ready server reports its actual source-library writer role. The generic
    // client catalog also supports af1, but this fixture contains ordinary WAVs.
    catalog.database_write_formats = vec![audiolib_core::compatibility::PLAN_B_FORMAT.into()];
    catalog
}

#[tokio::test]
async fn manage_mode_gates_scans_and_removals_but_not_reads_or_metadata() {
    let f = fixture(&[]);
    let state = &f.state;
    state
        .security()
        .update_settings(|settings| settings.manage_mode = Some(ManageMode::All))
        .unwrap();
    let bearer = format!("Bearer {TOKEN}");
    let proxied: &[(&str, &str)] = &[
        ("x-forwarded-for", "198.51.100.7"),
        ("authorization", &bearer),
    ];
    let public: &[(&str, &str)] = &[("authorization", &bearer)];
    let wind = f.audio_id("wind.wav");

    // Explicit `all`: everyone authorized may manage; `external` tells the two apart.
    assert_eq!(
        capabilities(state, &[], LAN).await,
        CapabilitiesResponse {
            compatibility: plain_library_server_catalog(),
            library_schema: Some(audiolib_core::server_upgrade::LibrarySchemaInfo {
                current: Some(8),
                source_search_rev: Some(2),
                snapshot_formats_served: vec![audiolib_core::compatibility::PLAN_B_FORMAT.into()],
            }),
            schema_migration: Some(audiolib_core::server_upgrade::SchemaMigrationInfo {
                state: audiolib_core::server_upgrade::MigrationState::Idle,
                endpoint: Some("/admin/api/migration".into()),
            }),
            vendor_import: Some(audiolib_core::server_upgrade::VendorImportInfo {
                endpoint: Some(audiolib_core::server_upgrade::VENDOR_IMPORT_API.into()),
                revision: 1,
                max_file_bytes: 52_428_800,
                max_rows: 200_000,
                max_row_text_bytes: Some(262_144),
            }),
            client_upgrade: None,
            search_scopes: ["main", "all", "path", "filename"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            manage_mode: ManageMode::All,
            external: false,
            can_manage: true,
            root_management: true,
            list_waveforms: Some(ListWaveformsCapability::default()),
            pregenerate_list_waveforms: false,
            list_waveform_jobs: Some(
                audiolib_server::wf_job::ListWaveformJobsCapability::default()
            ),
        }
    );
    assert_eq!(
        capabilities(state, proxied, LAN).await,
        CapabilitiesResponse {
            compatibility: plain_library_server_catalog(),
            library_schema: Some(audiolib_core::server_upgrade::LibrarySchemaInfo {
                current: Some(8),
                source_search_rev: Some(2),
                snapshot_formats_served: vec![audiolib_core::compatibility::PLAN_B_FORMAT.into()],
            }),
            schema_migration: Some(audiolib_core::server_upgrade::SchemaMigrationInfo {
                state: audiolib_core::server_upgrade::MigrationState::Idle,
                endpoint: Some("/admin/api/migration".into()),
            }),
            vendor_import: Some(audiolib_core::server_upgrade::VendorImportInfo {
                endpoint: Some(audiolib_core::server_upgrade::VENDOR_IMPORT_API.into()),
                revision: 1,
                max_file_bytes: 52_428_800,
                max_rows: 200_000,
                max_row_text_bytes: Some(262_144),
            }),
            client_upgrade: None,
            search_scopes: ["main", "all", "path", "filename"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            manage_mode: ManageMode::All,
            external: true,
            can_manage: true,
            root_management: true,
            list_waveforms: Some(ListWaveformsCapability::default()),
            pregenerate_list_waveforms: false,
            list_waveform_jobs: Some(
                audiolib_server::wf_job::ListWaveformJobsCapability::default()
            ),
        }
    );
    // Authentication still comes first.
    for (headers, address) in [
        (&[("x-forwarded-for", "1.2.3.4")][..], LAN),
        (&[][..], PUBLIC),
    ] {
        error(
            call(
                state,
                Method::GET,
                "/api/v1/capabilities",
                headers,
                Some(peer(address)),
                None,
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
        )
        .await;
    }
    let ok_remove = call(
        state,
        Method::POST,
        "/api/v1/audio/remove",
        proxied,
        Some(peer(LAN)),
        Some(r#"{"ids":[999999999]}"#),
    )
    .await;
    assert_eq!(ok_remove.status(), StatusCode::OK);

    let managed: [(Method, String, Option<&str>); 6] = [
        (
            Method::POST,
            "/api/v1/audio/remove".into(),
            Some(r#"{"ids":[999999999]}"#),
        ),
        (Method::DELETE, "/api/v1/folders/999999999".into(), None),
        (
            Method::POST,
            "/api/v1/scans".into(),
            Some(r#"{"root":"SFX"}"#),
        ),
        (Method::POST, "/api/v1/scans/1/pause".into(), None),
        (Method::POST, "/api/v1/scans/1/resume".into(), None),
        (Method::POST, "/api/v1/scans/1/cancel".into(), None),
    ];
    let open: [(Method, String, Option<String>); 6] = [
        (Method::GET, "/api/v1/scans/current".into(), None),
        (Method::GET, "/api/v1/history/scans".into(), None),
        (Method::GET, "/api/v1/roots".into(), None),
        (Method::GET, format!("/api/v1/audio/{wind}"), None),
        (
            Method::PATCH,
            format!("/api/v1/audio/{wind}/metadata"),
            Some(r#"{"favorite":true}"#.into()),
        ),
        (Method::POST, format!("/api/v1/audio/{wind}/plays"), None),
    ];
    // `lan`: external requests (proxied or public peer, even with a valid token) get 403;
    // LAN-direct ones still work. Reads, metadata and plays are never gated.
    state
        .security()
        .update_settings(|settings| settings.manage_mode = Some(ManageMode::Lan))
        .unwrap();
    assert_eq!(
        capabilities(state, proxied, LAN).await,
        CapabilitiesResponse {
            compatibility: plain_library_server_catalog(),
            library_schema: Some(audiolib_core::server_upgrade::LibrarySchemaInfo {
                current: Some(8),
                source_search_rev: Some(2),
                snapshot_formats_served: vec![audiolib_core::compatibility::PLAN_B_FORMAT.into()],
            }),
            schema_migration: Some(audiolib_core::server_upgrade::SchemaMigrationInfo {
                state: audiolib_core::server_upgrade::MigrationState::Idle,
                endpoint: Some("/admin/api/migration".into()),
            }),
            vendor_import: Some(audiolib_core::server_upgrade::VendorImportInfo {
                endpoint: Some(audiolib_core::server_upgrade::VENDOR_IMPORT_API.into()),
                revision: 1,
                max_file_bytes: 52_428_800,
                max_rows: 200_000,
                max_row_text_bytes: Some(262_144),
            }),
            client_upgrade: None,
            search_scopes: ["main", "all", "path", "filename"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            manage_mode: ManageMode::Lan,
            external: true,
            can_manage: false,
            root_management: true,
            list_waveforms: Some(ListWaveformsCapability::default()),
            pregenerate_list_waveforms: false,
            list_waveform_jobs: Some(
                audiolib_server::wf_job::ListWaveformJobsCapability::default()
            ),
        }
    );
    assert_eq!(
        capabilities(state, public, PUBLIC).await,
        CapabilitiesResponse {
            compatibility: plain_library_server_catalog(),
            library_schema: Some(audiolib_core::server_upgrade::LibrarySchemaInfo {
                current: Some(8),
                source_search_rev: Some(2),
                snapshot_formats_served: vec![audiolib_core::compatibility::PLAN_B_FORMAT.into()],
            }),
            schema_migration: Some(audiolib_core::server_upgrade::SchemaMigrationInfo {
                state: audiolib_core::server_upgrade::MigrationState::Idle,
                endpoint: Some("/admin/api/migration".into()),
            }),
            vendor_import: Some(audiolib_core::server_upgrade::VendorImportInfo {
                endpoint: Some(audiolib_core::server_upgrade::VENDOR_IMPORT_API.into()),
                revision: 1,
                max_file_bytes: 52_428_800,
                max_rows: 200_000,
                max_row_text_bytes: Some(262_144),
            }),
            client_upgrade: None,
            search_scopes: ["main", "all", "path", "filename"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            manage_mode: ManageMode::Lan,
            external: true,
            can_manage: false,
            root_management: true,
            list_waveforms: Some(ListWaveformsCapability::default()),
            pregenerate_list_waveforms: false,
            list_waveform_jobs: Some(
                audiolib_server::wf_job::ListWaveformJobsCapability::default()
            ),
        }
    );
    assert_eq!(
        capabilities(state, &[], LAN).await,
        CapabilitiesResponse {
            compatibility: plain_library_server_catalog(),
            library_schema: Some(audiolib_core::server_upgrade::LibrarySchemaInfo {
                current: Some(8),
                source_search_rev: Some(2),
                snapshot_formats_served: vec![audiolib_core::compatibility::PLAN_B_FORMAT.into()],
            }),
            schema_migration: Some(audiolib_core::server_upgrade::SchemaMigrationInfo {
                state: audiolib_core::server_upgrade::MigrationState::Idle,
                endpoint: Some("/admin/api/migration".into()),
            }),
            vendor_import: Some(audiolib_core::server_upgrade::VendorImportInfo {
                endpoint: Some(audiolib_core::server_upgrade::VENDOR_IMPORT_API.into()),
                revision: 1,
                max_file_bytes: 52_428_800,
                max_rows: 200_000,
                max_row_text_bytes: Some(262_144),
            }),
            client_upgrade: None,
            search_scopes: ["main", "all", "path", "filename"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            manage_mode: ManageMode::Lan,
            external: false,
            can_manage: true,
            root_management: true,
            list_waveforms: Some(ListWaveformsCapability::default()),
            pregenerate_list_waveforms: false,
            list_waveform_jobs: Some(
                audiolib_server::wf_job::ListWaveformJobsCapability::default()
            ),
        }
    );
    for (method, path, body) in &managed {
        for (headers, address) in [(proxied, LAN), (public, PUBLIC)] {
            let response = call(
                state,
                method.clone(),
                path,
                headers,
                Some(peer(address)),
                *body,
            )
            .await;
            let denied = error(response, StatusCode::FORBIDDEN, "MANAGE_FORBIDDEN").await;
            assert!(denied.message.contains("LAN"), "{}", denied.message);
        }
        // Without a token the external request is 401, not 403.
        let response = call(
            state,
            method.clone(),
            path,
            &[("x-forwarded-for", "198.51.100.7")],
            Some(peer(LAN)),
            *body,
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
        let response = call(state, method.clone(), path, &[], Some(peer(LAN)), *body).await;
        assert_ne!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{method} {path} from the LAN"
        );
    }
    for (method, path, body) in &open {
        let response = call(
            state,
            method.clone(),
            path,
            proxied,
            Some(peer(LAN)),
            body.as_deref(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{method} {path}");
    }
    // A LAN removal goes through in `lan` mode.
    let removed: RemoveAudioResponse =
        ok(remove(state, &format!(r#"{{"ids":[{wind}]}}"#)).await).await;
    assert_eq!(removed.removed, 1);

    // `off`: nobody, not even the LAN; reads and metadata still work.
    state
        .security()
        .update_settings(|settings| settings.manage_mode = Some(ManageMode::Off))
        .unwrap();
    assert_eq!(
        capabilities(state, &[], LAN).await,
        CapabilitiesResponse {
            compatibility: plain_library_server_catalog(),
            library_schema: Some(audiolib_core::server_upgrade::LibrarySchemaInfo {
                current: Some(8),
                source_search_rev: Some(2),
                snapshot_formats_served: vec![audiolib_core::compatibility::PLAN_B_FORMAT.into()],
            }),
            schema_migration: Some(audiolib_core::server_upgrade::SchemaMigrationInfo {
                state: audiolib_core::server_upgrade::MigrationState::Idle,
                endpoint: Some("/admin/api/migration".into()),
            }),
            vendor_import: Some(audiolib_core::server_upgrade::VendorImportInfo {
                endpoint: Some(audiolib_core::server_upgrade::VENDOR_IMPORT_API.into()),
                revision: 1,
                max_file_bytes: 52_428_800,
                max_rows: 200_000,
                max_row_text_bytes: Some(262_144),
            }),
            client_upgrade: None,
            search_scopes: ["main", "all", "path", "filename"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            manage_mode: ManageMode::Off,
            external: false,
            can_manage: false,
            root_management: true,
            list_waveforms: Some(ListWaveformsCapability::default()),
            pregenerate_list_waveforms: false,
            list_waveform_jobs: Some(
                audiolib_server::wf_job::ListWaveformJobsCapability::default()
            ),
        }
    );
    for (method, path, body) in &managed {
        for (headers, address) in [(&[][..], LAN), (proxied, LAN)] {
            let response = call(
                state,
                method.clone(),
                path,
                headers,
                Some(peer(address)),
                *body,
            )
            .await;
            let denied = error(response, StatusCode::FORBIDDEN, "MANAGE_FORBIDDEN").await;
            assert!(denied.message.contains("disabled"), "{}", denied.message);
        }
    }
    let rain = f.audio_id("rain.wav");
    let response = lan(
        state,
        Method::PATCH,
        &format!("/audio/{rain}/metadata"),
        Some(r#"{"favorite":true}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(f.count("SELECT COUNT(*) FROM audio_files"), 4);
    f.assert_files_untouched();
}

#[tokio::test]
async fn capabilities_answer_while_the_library_is_still_opening() {
    let f = fixture(&[]);
    let service = Service::starting_with(Security::from_config(&f.config, None));
    let response = call_service(
        service.clone(),
        Method::GET,
        "/api/v1/capabilities",
        &[],
        Some(peer(LAN)),
        None,
    )
    .await;
    let caps: CapabilitiesResponse = ok(response).await;
    assert!(caps.can_manage && !caps.external);
    let response = call_service(
        service,
        Method::GET,
        "/api/v1/library",
        &[],
        Some(peer(LAN)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

async fn admin(state: &AppState, method: Method, path: &str, body: Option<&str>) -> Response {
    call(
        state,
        method,
        path,
        &[
            ("x-audiolib-admin", "1"),
            ("host", "192.0.2.10:8790"),
            ("content-type", "application/json"),
        ],
        Some(peer(LAN)),
        body,
    )
    .await
}

#[tokio::test]
async fn manage_mode_round_trips_through_the_admin_page() {
    let f = fixture(&[]);
    let state = &f.state;
    let status: serde_json::Value =
        ok(admin(state, Method::GET, "/admin/api/status", None).await).await;
    assert_eq!(status["manage_mode"], "lan");
    assert_eq!(status["manage_mode_source"], "default");
    let status: serde_json::Value = ok(admin(
        state,
        Method::PUT,
        "/admin/api/settings",
        Some(r#"{"manage_mode":"lan"}"#),
    )
    .await)
    .await;
    assert_eq!(
        (&status["manage_mode"], &status["manage_mode_source"]),
        (&"lan".into(), &"admin".into())
    );
    assert_eq!(status["auth_mode"], "external", "other settings untouched");
    // Hot: the very next API request sees it; persisted in admin-settings.json.
    let caps: CapabilitiesResponse = ok(call(
        state,
        Method::GET,
        "/api/v1/capabilities",
        &[
            ("x-forwarded-for", "1.2.3.4"),
            ("authorization", &format!("Bearer {TOKEN}")),
        ],
        Some(peer(LAN)),
        None,
    )
    .await)
    .await;
    assert_eq!(
        (caps.manage_mode, caps.can_manage),
        (ManageMode::Lan, false)
    );
    let settings = fs::read_to_string(f.config.admin_dir().join("admin-settings.json")).unwrap();
    let settings: serde_json::Value = serde_json::from_str(&settings).unwrap();
    assert_eq!(settings["manage_mode"], "lan");
    assert_eq!(
        Security::from_config(&f.config, None).manage_mode(),
        ManageMode::Lan,
        "survives a restart"
    );
    // Invalid values and missing admin header are refused; null resets to the default.
    error(
        admin(
            state,
            Method::PUT,
            "/admin/api/settings",
            Some(r#"{"manage_mode":"some"}"#),
        )
        .await,
        StatusCode::BAD_REQUEST,
        "VALIDATION",
    )
    .await;
    let unmarked = call(
        state,
        Method::PUT,
        "/admin/api/settings",
        &[("host", "192.0.2.10:8790")],
        Some(peer(LAN)),
        Some(r#"{"manage_mode":"off"}"#),
    )
    .await;
    error(unmarked, StatusCode::FORBIDDEN, "ADMIN_HEADER_REQUIRED").await;
    let status: serde_json::Value = ok(admin(
        state,
        Method::PUT,
        "/admin/api/settings",
        Some(r#"{"manage_mode":null}"#),
    )
    .await)
    .await;
    assert_eq!(
        (&status["manage_mode"], &status["manage_mode_source"]),
        (&"lan".into(), &"default".into())
    );
    let page = bytes(admin(state, Method::GET, "/admin", None).await).await;
    assert!(String::from_utf8_lossy(&page).contains("name=\"manage\""));
    let script = bytes(admin(state, Method::GET, "/admin/admin.js", None).await).await;
    assert!(String::from_utf8_lossy(&script).contains("manage_mode"));
}
