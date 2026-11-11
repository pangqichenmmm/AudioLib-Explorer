// server/audiolib-server/tests/folder_artwork.rs
//! Direct-folder covers stay within one shared Metadata album and HTTP context.
mod support;

use audiolib_core::DataProvider;
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use audiolib_remote::{ArtworkReply, RemoteClient};
use audiolib_server::{AppState, BearerToken, ServeOptions, ServerConfig, router, serve};
use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tower::ServiceExt;

const TOKEN: &str = "folder-artwork-test-token-0123456789";
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn real_path(path: &Path) -> PathBuf {
    let canonical = path.canonicalize().unwrap();
    let text = canonical.to_string_lossy().into_owned();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => rest.into(),
        _ => canonical,
    }
}

fn chunk(bytes: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
    bytes.extend(id);
    bytes.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
    bytes.extend(data);
    if !data.len().is_multiple_of(2) {
        bytes.push(0);
    }
}

fn wav(path: &Path, picture: Option<&[u8]>) {
    let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
    let mut format = Vec::new();
    format.extend(1_u16.to_le_bytes());
    format.extend(1_u16.to_le_bytes());
    format.extend(48_000_u32.to_le_bytes());
    format.extend(96_000_u32.to_le_bytes());
    format.extend(2_u16.to_le_bytes());
    format.extend(16_u16.to_le_bytes());
    chunk(&mut bytes, b"fmt ", &format);
    chunk(&mut bytes, b"data", &[0; 16]);
    let mut info = b"INFO".to_vec();
    chunk(&mut info, b"IPRD", b"Shared metadata album\0");
    chunk(&mut bytes, b"LIST", &info);
    if let Some(picture) = picture {
        let mut payload = b"\0image/png\0\x03\0".to_vec();
        payload.extend(picture);
        let mut frame = b"APIC".to_vec();
        frame.extend(u32::try_from(payload.len()).unwrap().to_be_bytes());
        frame.extend([0, 0]);
        frame.extend(payload);
        let mut tag = b"ID3\x03\0\0".to_vec();
        for shift in [21, 14, 7, 0] {
            tag.push(u8::try_from((frame.len() >> shift) & 127).unwrap());
        }
        tag.extend(frame);
        chunk(&mut bytes, b"ID3 ", &tag);
    }
    let size = u32::try_from(bytes.len() - 8).unwrap();
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    fs::write(path, bytes).unwrap();
}

fn png(color: [u8; 4]) -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(4, 3, image::Rgba(color))
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
}

struct Fixture {
    state: AppState,
    _directory: tempfile::TempDir,
    album: i64,
    early_folder: i64,
    late_folder: i64,
    empty_folder: i64,
    early_image: Vec<u8>,
    late_image: Vec<u8>,
    late_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = real_path(directory.path());
        let sounds = base.join("sounds");
        let early = sounds.join("early");
        let late = sounds.join("late %_[x]&+ 金属");
        let empty = sounds.join("empty");
        fs::create_dir_all(&early).unwrap();
        let early_image = png([200, 20, 40, 255]);
        let late_image = png([20, 40, 200, 255]);
        for index in 0..70 {
            wav(
                &early.join(format!("early-{index:03}.wav")),
                Some(&early_image),
            );
        }
        let provider = Arc::new(SqliteProvider::open(&base.join("library/library.db")).unwrap());
        let scanner = ScanService::new(provider.clone());
        let options = ScanOptions {
            incremental: false,
            workers: 1,
            ..Default::default()
        };
        let first = scanner
            .scan(&sounds, &options, &ScanCancellation::default(), |_| {})
            .unwrap();
        assert_eq!(first.added, 70);
        assert_eq!(first.errors, 0);
        fs::create_dir_all(&late).unwrap();
        fs::create_dir_all(empty.join("child")).unwrap();
        let late_path = late.join("covered.wav");
        wav(&late_path, Some(&late_image));
        let empty_path = empty.join("plain.wav");
        wav(&empty_path, None);
        wav(&empty.join("child/covered.wav"), Some(&early_image));
        let second = scanner
            .scan(&sounds, &options, &ScanCancellation::default(), |_| {})
            .unwrap();
        assert_eq!(second.added, 3);
        assert_eq!(second.errors, 0);
        let earlier = provider
            .find_audio_by_path(&early.join("early-000.wav"))
            .unwrap()
            .unwrap();
        let later = provider.find_audio_by_path(&late_path).unwrap().unwrap();
        let plain = provider.find_audio_by_path(&empty_path).unwrap().unwrap();
        let album = earlier.album_id.unwrap();
        assert_eq!(later.album_id, Some(album));
        assert_eq!(plain.album_id, Some(album));
        let global = provider
            .album_members(album, 64, Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert_eq!(global.len(), 64);
        assert!(
            global
                .iter()
                .all(|member| member.id < later.id && member.folder_id == earlier.folder_id)
        );
        assert!(provider.check_library().unwrap().is_ok());
        drop(scanner);
        drop(provider);
        let config = ServerConfig::from_toml_str(&format!(
            "bind = \"127.0.0.1:0\"\nauth = true\nlibrary_db = 'library/library.db'\n[[roots]]\nname = \"SFX\"\nserver_path = '{}'\n", sounds.display()), &base).unwrap();
        let state = AppState::open(&config, BearerToken::new(TOKEN).unwrap()).unwrap();
        Self {
            state,
            _directory: directory,
            album,
            early_folder: earlier.folder_id.unwrap(),
            late_folder: later.folder_id.unwrap(),
            empty_folder: plain.folder_id.unwrap(),
            early_image,
            late_image,
            late_path,
        }
    }

    fn path(&self, folder: Option<i64>) -> String {
        match folder {
            Some(folder) => format!("/api/v1/albums/{}/artwork?folder_id={folder}", self.album),
            None => format!("/api/v1/albums/{}/artwork", self.album),
        }
    }

    fn unassigned_path(&self, directory: &str) -> String {
        let mut encoded = String::new();
        use std::fmt::Write as _;
        for byte in directory.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                encoded.push(char::from(byte));
            } else {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
        format!(
            "/api/v1/albums/{}/artwork?unassigned_directory={encoded}",
            self.album
        )
    }
}

async fn send(fixture: &Fixture, method: Method, path: &str, etag: Option<&str>) -> Response {
    let mut request = support::modern(Request::builder().method(method).uri(path))
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header("X-AudioLib-Library-Id", fixture.state.library_id())
        .header(
            "X-AudioLib-Album-Namespace",
            fixture.state.provider().album_namespace(),
        );
    if let Some(etag) = etag {
        request = request.header(header::IF_NONE_MATCH, etag);
    }
    router(fixture.state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn body(response: Response) -> Vec<u8> {
    response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec()
}

#[tokio::test]
async fn directory_filter_precedes_member_limit_and_never_falls_back_to_siblings_or_children() {
    let _serial = SERIAL.lock().await;
    let fixture = Fixture::new();
    let before = fs::read(&fixture.late_path).unwrap();
    let stamp = fs::metadata(&fixture.late_path)
        .unwrap()
        .modified()
        .unwrap();
    let global = send(&fixture, Method::GET, &fixture.path(None), None).await;
    assert_eq!(global.status(), StatusCode::OK);
    assert!(global.headers().get("x-album-folder-id").is_none());
    let global_etag = global.headers()[header::ETAG].to_str().unwrap().to_owned();
    assert_eq!(body(global).await, fixture.early_image);
    let early = send(
        &fixture,
        Method::GET,
        &fixture.path(Some(fixture.early_folder)),
        None,
    )
    .await;
    let early_etag = early.headers()[header::ETAG].to_str().unwrap().to_owned();
    assert_ne!(
        early_etag, global_etag,
        "identical pixels in global/folder contexts must have distinct ETags"
    );
    assert_eq!(body(early).await, fixture.early_image);
    let path = fixture.path(Some(fixture.late_folder));
    let late = send(&fixture, Method::GET, &path, Some(&global_etag)).await;
    assert_eq!(late.status(), StatusCode::OK);
    assert_eq!(late.headers()["x-library-id"], fixture.state.library_id());
    assert_eq!(
        late.headers()["x-album-namespace"],
        fixture.state.provider().album_namespace()
    );
    assert_eq!(late.headers()["x-album-id"], fixture.album.to_string());
    assert_eq!(
        late.headers()["x-album-folder-id"],
        fixture.late_folder.to_string()
    );
    let directory = audiolib_db::normalize_source_path_identity(
        &fixture
            .state
            .provider()
            .get_folder(fixture.late_folder)
            .unwrap()
            .path,
    );
    let directory_hex = directory
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(late.headers()["x-album-directory-identity"], directory_hex);
    let late_etag = late.headers()[header::ETAG].to_str().unwrap().to_owned();
    assert_ne!(late_etag, global_etag);
    assert_ne!(late_etag, early_etag);
    assert_eq!(body(late).await, fixture.late_image);
    for method in [Method::GET, Method::HEAD] {
        let cached = send(&fixture, method, &path, Some(&late_etag)).await;
        assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            cached.headers()["x-album-folder-id"],
            fixture.late_folder.to_string()
        );
        assert_eq!(
            cached.headers()["x-album-directory-identity"],
            directory_hex
        );
        assert_eq!(cached.headers()["x-album-id"], fixture.album.to_string());
        assert_eq!(
            cached.headers()["x-album-namespace"],
            fixture.state.provider().album_namespace()
        );
        assert_eq!(cached.headers()["x-library-id"], fixture.state.library_id());
        assert!(body(cached).await.is_empty());
    }
    let other = send(
        &fixture,
        Method::GET,
        &fixture.path(Some(fixture.early_folder)),
        Some(&late_etag),
    )
    .await;
    assert_eq!(
        other.status(),
        StatusCode::OK,
        "another directory's ETag cannot return 304"
    );
    for etag in [None, Some("*"), Some(late_etag.as_str())] {
        let uncovered = send(
            &fixture,
            Method::GET,
            &fixture.path(Some(fixture.empty_folder)),
            etag,
        )
        .await;
        assert_eq!(
            uncovered.status(),
            StatusCode::NOT_FOUND,
            "no fallback to covered child or sibling"
        );
    }
    assert_eq!(fs::read(&fixture.late_path).unwrap(), before);
    assert_eq!(
        fs::metadata(&fixture.late_path)
            .unwrap()
            .modified()
            .unwrap(),
        stamp
    );
    assert!(fixture.state.provider().check_library().unwrap().is_ok());
}

#[tokio::test]
async fn directory_query_rejects_malformed_duplicate_and_unknown_signed_ids() {
    let _serial = SERIAL.lock().await;
    let fixture = Fixture::new();
    let base = fixture.path(None);
    for query in [
        "folder_id=",
        "folder_id=abc",
        "folder_id=9223372036854775808",
        "folder_id=-9223372036854775809",
        "folder_id=1&folder_id=2",
    ] {
        assert_eq!(
            send(&fixture, Method::GET, &format!("{base}?{query}"), None)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    for folder in [i64::MIN, i64::MAX, 0] {
        assert_eq!(
            send(&fixture, Method::GET, &fixture.path(Some(folder)), None)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn remote_directory_artwork_round_trip_verifies_context_including_conditional_reads() {
    let _serial = SERIAL.lock().await;
    let fixture = Fixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(
        listener,
        router(fixture.state.clone()),
        async {
            let _ = stopped.await;
        },
        ServeOptions {
            shutdown_grace: Duration::from_secs(2),
            ..Default::default()
        },
    ));
    let library = fixture.state.library_id().to_owned();
    let namespace = fixture.state.provider().album_namespace();
    let directory = audiolib_db::normalize_source_path_identity(
        &fixture
            .state
            .provider()
            .get_folder(fixture.late_folder)
            .unwrap()
            .path,
    );
    let (album, early, late, empty) = (
        fixture.album,
        fixture.early_folder,
        fixture.late_folder,
        fixture.empty_folder,
    );
    let expected = fixture.late_image.clone();
    let checked = tokio::task::spawn_blocking(move || {
        let client = RemoteClient::new(&url, TOKEN).unwrap();
        assert!(client.capabilities().unwrap().album_folder_artwork_usable());
        let cancel = AtomicBool::new(false);
        let ArtworkReply::Data { bytes, etag, .. } = client
            .album_artwork_in_folder_for_library(
                &library, &namespace, album, late, None, None, &cancel,
            )
            .unwrap()
        else {
            panic!("directory cover missing");
        };
        assert_eq!(bytes, expected);
        assert_eq!(
            client
                .album_artwork_in_folder_for_library(
                    &library,
                    &namespace,
                    album,
                    late,
                    None,
                    etag.as_deref(),
                    &cancel
                )
                .unwrap(),
            ArtworkReply::NotModified
        );
        let format = audiolib_core::compatibility::LibraryFormat::target();
        let snapshot = audiolib_remote::CacheState {
            library_id: library.clone(),
            schema_version: 8,
            format: Some(Box::new(audiolib_remote::CacheFormat {
                source: Some(format.clone()),
                local: format,
                generation: 0,
            })),
            ..Default::default()
        };
        assert_eq!(
            client
                .album_artwork_in_directory_for_snapshot(
                    &snapshot,
                    &client.capabilities().unwrap(),
                    &namespace,
                    album,
                    Some(late),
                    &directory,
                    &[],
                    None,
                    etag.as_deref(),
                    &cancel
                )
                .unwrap(),
            ArtworkReply::NotModified
        );
        assert!(
            matches!(
                client.album_artwork_in_directory_for_snapshot(
                    &snapshot,
                    &client.capabilities().unwrap(),
                    &namespace,
                    album,
                    Some(late),
                    &format!("{directory}/old-client-path"),
                    &[],
                    None,
                    etag.as_deref(),
                    &cancel
                ),
                Err(audiolib_remote::RemoteError::LibraryChanged(_))
            ),
            "same folder ID cannot validate a stale directory cache on 304"
        );
        assert!(matches!(
            client
                .album_artwork_in_folder_for_library(
                    &library,
                    &namespace,
                    album,
                    early,
                    None,
                    etag.as_deref(),
                    &cancel
                )
                .unwrap(),
            ArtworkReply::Data { .. }
        ));
        assert_eq!(
            client
                .album_artwork_in_folder_for_library(
                    &library,
                    &namespace,
                    album,
                    empty,
                    None,
                    etag.as_deref(),
                    &cancel
                )
                .unwrap(),
            ArtworkReply::Missing
        );
    })
    .await;
    let _ = stop.send(());
    server.await.unwrap();
    checked.unwrap();
}

#[tokio::test]
async fn null_folder_members_use_exact_source_parent_before_limit_with_safe_literal_echo() {
    let _serial = SERIAL.lock().await;
    let fixture = Fixture::new();
    // This exclusively owned fixture deliberately represents old NULL folder rows.
    // No live library or original WAV is changed, and library functions retain
    // the same FTS and album triggers as ordinary annotations/scans.
    let connection = rusqlite::Connection::open(fixture.state.provider().database_path()).unwrap();
    audiolib_db::register_library_functions(&connection).unwrap();
    connection
        .execute("UPDATE audio_files SET folder_id=NULL", [])
        .unwrap();
    drop(connection);
    let late_file = fixture
        .state
        .provider()
        .find_audio_by_path(&fixture.late_path)
        .unwrap()
        .unwrap();
    assert!(late_file.folder_id.is_none());
    let late_directory = audiolib_db::source_parent_path_identity(&late_file.file_path);
    assert!(late_directory.contains("%_[x]&+ 金属"));
    let path = fixture.unassigned_path(&late_directory);
    let expected_hex = late_directory
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let before = fs::read(&fixture.late_path).unwrap();
    let first = send(&fixture, Method::GET, &path, None).await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers()["x-album-directory-identity"], expected_hex);
    assert!(first.headers().get("x-album-folder-id").is_none());
    let etag = first.headers()[header::ETAG].to_str().unwrap().to_owned();
    assert_eq!(body(first).await, fixture.late_image);
    let cached = send(&fixture, Method::GET, &path, Some(&etag)).await;
    assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(cached.headers()["x-album-directory-identity"], expected_hex);
    assert_eq!(cached.headers()["x-library-id"], fixture.state.library_id());
    assert_eq!(
        cached.headers()["x-album-namespace"],
        fixture.state.provider().album_namespace()
    );
    assert!(body(cached).await.is_empty());
    assert_eq!(
        send(
            &fixture,
            Method::GET,
            &fixture.path(Some(fixture.late_folder)),
            None
        )
        .await
        .status(),
        StatusCode::NOT_FOUND,
        "registered-folder queries cannot include NULL rows"
    );
    let missing = fixture.unassigned_path("");
    assert_eq!(
        send(&fixture, Method::GET, &missing, Some("*"))
            .await
            .status(),
        StatusCode::NOT_FOUND,
        "empty unassigned parent is not the global album scope"
    );
    let mut empty_directory = late_directory.rsplit_once('/').unwrap().0.to_owned();
    empty_directory.push_str("/empty");
    assert_eq!(
        send(
            &fixture,
            Method::GET,
            &fixture.unassigned_path(&empty_directory),
            Some(&etag)
        )
        .await
        .status(),
        StatusCode::NOT_FOUND,
        "NULL child/sibling images must not become a parent cover"
    );
    let mut early_directory = late_directory.rsplit_once('/').unwrap().0.to_owned();
    early_directory.push_str("/early");
    let other = send(
        &fixture,
        Method::GET,
        &fixture.unassigned_path(&early_directory),
        Some(&etag),
    )
    .await;
    assert_eq!(other.status(), StatusCode::OK);
    assert_ne!(other.headers()[header::ETAG], etag);
    assert_eq!(body(other).await, fixture.early_image);
    let conflict = format!("{}&folder_id={}", path, fixture.late_folder);
    assert_eq!(
        send(&fixture, Method::GET, &conflict, None).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &fixture,
            Method::GET,
            &fixture.unassigned_path(&"x".repeat(4097)),
            None
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(fs::read(&fixture.late_path).unwrap(), before);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(
        listener,
        router(fixture.state.clone()),
        async {
            let _ = stopped.await;
        },
        ServeOptions {
            shutdown_grace: Duration::from_secs(2),
            ..Default::default()
        },
    ));
    let library = fixture.state.library_id().to_owned();
    let namespace = fixture.state.provider().album_namespace();
    let album = fixture.album;
    let expected = fixture.late_image.clone();
    let checked = tokio::task::spawn_blocking(move || {
        let client = RemoteClient::new(&url, TOKEN).unwrap();
        let cancel = AtomicBool::new(false);
        let ArtworkReply::Data { bytes, etag, .. } = client
            .album_artwork_in_unassigned_directory_for_library(
                &library,
                &namespace,
                album,
                &late_directory,
                None,
                None,
                &cancel,
            )
            .unwrap()
        else {
            panic!("NULL directory cover missing");
        };
        assert_eq!(bytes, expected);
        assert_eq!(
            client
                .album_artwork_in_unassigned_directory_for_library(
                    &library,
                    &namespace,
                    album,
                    &late_directory,
                    None,
                    etag.as_deref(),
                    &cancel
                )
                .unwrap(),
            ArtworkReply::NotModified
        );
    })
    .await;
    let _ = stop.send(());
    server.await.unwrap();
    checked.unwrap();
}
