// server/audiolib-server/tests/artwork.rs
//! `GET /audio/{id}/artwork`: capability `album_artwork`, a JPEG/PNG body, and 404.
mod support;

use audiolib_core::DataProvider;
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use audiolib_remote::{ArtworkReply, RemoteClient};
use audiolib_server::{
    AppState, BearerToken, ErrorBody, ServeOptions, ServerConfig, router, serve,
};
use axum::{
    body::Body,
    http::{Method, Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use image::GenericImageView;
use rusqlite::{Connection, OpenFlags};
use std::{
    fs, io,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tower::ServiceExt;

const TOKEN: &str = "artwork-test-token-0123456789abcdef";
static HTTP_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn real_path(path: &Path) -> std::path::PathBuf {
    let canonical = path.canonicalize().unwrap();
    let text = canonical.to_string_lossy().into_owned();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => rest.into(),
        _ => canonical,
    }
}

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

fn synchsafe(size: usize) -> [u8; 4] {
    [
        ((size >> 21) & 127) as u8,
        ((size >> 14) & 127) as u8,
        ((size >> 7) & 127) as u8,
        (size & 127) as u8,
    ]
}

fn embed_png(path: &Path, image: &[u8]) {
    embed_picture(path, "image/png", image);
}

fn embed_picture(path: &Path, mime: &str, image: &[u8]) {
    let mut apic = format!("\0{mime}\0\x03\0").into_bytes();
    apic.extend_from_slice(image);
    let mut frame = b"APIC".to_vec();
    frame.extend_from_slice(&(apic.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&apic);
    let mut tag = b"ID3\x03\x00\x00".to_vec();
    tag.extend_from_slice(&synchsafe(frame.len()));
    tag.extend_from_slice(&frame);
    let mut bytes = fs::read(path).unwrap();
    let mut chunk = b"ID3 ".to_vec();
    chunk.extend_from_slice(&(tag.len() as u32).to_le_bytes());
    chunk.extend_from_slice(&tag);
    if !tag.len().is_multiple_of(2) {
        chunk.push(0);
    }
    bytes.extend(chunk);
    let size = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    fs::write(path, bytes).unwrap();
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([
            u8::try_from((x * 3 + y) % 200 + 20).unwrap_or(20),
            80,
            160,
            255,
        ])
    });
    let mut output = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut io::Cursor::new(&mut output), image::ImageFormat::Png)
        .unwrap();
    output
}

struct Fixture {
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
    state: AppState,
    cover_id: i64,
    plain_id: i64,
    image: Vec<u8>,
}

fn fixture() -> Fixture {
    fixture_with_second_cover(false)
}

fn fixture_with_second_cover(second_cover: bool) -> Fixture {
    let image = png(32, 16);
    fixture_with_cover("image/png", &image, image.clone(), second_cover)
}

/// `embedded` goes into the WAV; `image` is the body the server should return.
fn fixture_with_cover(mime: &str, embedded: &[u8], image: Vec<u8>, second_cover: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = real_path(dir.path());
    let sounds = base.join("sounds");
    fs::create_dir_all(&sounds).unwrap();
    let cover = sounds.join("cover.wav");
    let plain = sounds.join("plain.wav");
    wav(&cover);
    embed_picture(&cover, mime, embedded);
    wav(&plain);
    if second_cover {
        embed_png(&plain, &png(14, 10));
    }
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
    assert_eq!(progress.added, 2, "{progress:?}");
    drop(writer);
    let config = ServerConfig::from_toml_str(
        &format!(
            "bind = \"127.0.0.1:0\"\nauth = true\nlibrary_db = 'library/library.db'\n\
             [[roots]]\nname = \"SFX\"\nserver_path = '{}'\n",
            sounds.display()
        ),
        dir.path(),
    )
    .unwrap();
    let state = AppState::open(&config, BearerToken::new(TOKEN).unwrap()).unwrap();
    let (cover_id, plain_id) = {
        let audio_id = |name: &str| -> i64 {
            Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap()
                .query_row(
                    "SELECT id FROM audio_files WHERE filename=?1",
                    [name],
                    |row| row.get(0),
                )
                .unwrap()
        };
        (audio_id("cover.wav"), audio_id("plain.wav"))
    };
    Fixture {
        _dir: dir,
        db,
        state,
        cover_id,
        plain_id,
        image,
    }
}

async fn send(state: &AppState, path: &str) -> Response {
    send_method(state, Method::GET, path).await
}

async fn send_method(state: &AppState, method: Method, path: &str) -> Response {
    router(state.clone())
        .oneshot(
            support::modern(Request::builder().method(method).uri(path))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header("X-AudioLib-Library-Id", state.library_id())
                .header(
                    "X-AudioLib-Album-Namespace",
                    state.provider().album_namespace(),
                )
                .body(Body::empty())
                .unwrap(),
        )
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

fn contains(path: &Path, needle: &[u8]) -> bool {
    fs::read(path)
        .ok()
        .is_some_and(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
}

#[test]
fn cover_probe_keeps_the_authorized_handle_when_the_path_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("covered.wav");
    let image = png(8, 4);
    wav(&path);
    embed_png(&path, &image);
    let mut authorized = fs::File::open(&path).unwrap();
    fs::rename(&path, dir.path().join("original.wav")).unwrap();
    wav(&path);
    let (_, cover) = audiolib_audio::probe_file_with_artwork(&mut authorized).unwrap();
    assert_eq!(cover.unwrap().bytes, image);
    assert!(
        audiolib_audio::probe_with_artwork(&path)
            .unwrap()
            .1
            .is_none()
    );
}

#[tokio::test]
async fn capabilities_advertise_album_artwork_and_cover_round_trips() {
    let _serial = HTTP_SERIAL.lock().await;
    let f = fixture();
    let response = send(&f.state, "/api/v1/capabilities").await;
    assert_eq!(response.status(), StatusCode::OK);
    let caps: serde_json::Value = serde_json::from_slice(&body(response).await).unwrap();
    let features = caps["compatibility"]["features"]
        .as_object()
        .expect("features");
    assert!(
        features
            .get("album_artwork")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            >= 1,
        "{features:?}"
    );
    assert!(features.get("artwork").is_none(), "{features:?}");

    let path = format!("/api/v1/audio/{}/artwork", f.cover_id);
    let response = send(&f.state, &path).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/png"
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "private, no-cache"
    );
    let etag = response
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(etag.starts_with("\"aa2-") && etag.ends_with('"'), "{etag}");
    let bytes = body(response).await;
    assert_eq!(bytes, f.image);
    assert!(
        f.db.parent()
            .unwrap()
            .join("artwork")
            .read_dir()
            .unwrap()
            .next()
            .is_some(),
        "cover cache is stored beside the library"
    );
    assert!(
        !contains(&f.db, &f.image),
        "cover bytes must stay out of SQLite"
    );
    let wal = f.db.with_extension("db-wal");
    if wal.exists() {
        assert!(!contains(&wal, &f.image));
    }

    let not_modified = router(f.state.clone())
        .oneshot(
            support::modern(Request::get(&path))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header("X-AudioLib-Library-Id", f.state.library_id())
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        not_modified
            .headers()
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap(),
        etag
    );
    assert!(body(not_modified).await.is_empty());

    let head = router(f.state.clone())
        .oneshot(
            support::modern(Request::builder().method("HEAD").uri(&path))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header("X-AudioLib-Library-Id", f.state.library_id())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(
        head.headers().get(header::ETAG).unwrap().to_str().unwrap(),
        etag
    );
    assert!(body(head).await.is_empty());

    let thumb = send(&f.state, &format!("{path}?side=8")).await;
    assert_eq!(thumb.status(), StatusCode::OK);
    assert_eq!(
        thumb.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/png"
    );
    let thumb_etag = thumb
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_ne!(thumb_etag, etag);
    let thumb_bytes = body(thumb).await;
    let decoded = image::ImageReader::new(io::Cursor::new(thumb_bytes))
        .with_guessed_format()
        .unwrap()
        .decode()
        .unwrap();
    assert_eq!(decoded.dimensions(), (8, 4));

    let missing_cover = send(&f.state, &format!("/api/v1/audio/{}/artwork", f.plain_id)).await;
    assert_eq!(missing_cover.status(), StatusCode::NOT_FOUND);
    let error: ErrorBody = serde_json::from_slice(&body(missing_cover).await).unwrap();
    assert_eq!(error.error_code, "NOT_FOUND");
    let conditional_missing = router(f.state.clone())
        .oneshot(
            support::modern(Request::get(format!(
                "/api/v1/audio/{}/artwork",
                f.plain_id
            )))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header("X-AudioLib-Library-Id", f.state.library_id())
            .header(header::IF_NONE_MATCH, "*")
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(conditional_missing.status(), StatusCode::NOT_FOUND);

    let unknown = send(&f.state, "/api/v1/audio/999999/artwork").await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    let bad_side = send(&f.state, &format!("{path}?side=0")).await;
    assert_eq!(bad_side.status(), StatusCode::BAD_REQUEST);
    let error: ErrorBody = serde_json::from_slice(&body(bad_side).await).unwrap();
    assert_eq!(error.error_code, "VALIDATION");

    let cover_path = Connection::open_with_flags(&f.db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row(
            "SELECT file_path FROM audio_files WHERE id=?1",
            [f.cover_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    fs::write(&cover_path, b"not a wav").unwrap();
    let broken = send(&f.state, &path).await;
    assert_eq!(
        broken.status(),
        StatusCode::NOT_FOUND,
        "extract failure is 404"
    );

    fs::remove_file(&cover_path).unwrap();
    let gone = send(&f.state, &path).await;
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn remote_cover_client_round_trip_preserves_library_and_audio_stream() {
    let _serial = HTTP_SERIAL.lock().await;
    let f = fixture();
    let observer = Connection::open_with_flags(&f.db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let before: i64 = observer
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .unwrap();
    let schema: i64 = observer
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(serve(
        listener,
        router(f.state.clone()),
        async {
            let _ = stopped.await;
        },
        ServeOptions {
            shutdown_grace: Duration::from_secs(2),
            ..Default::default()
        },
    ));
    let (id, plain_id) = (f.cover_id, f.plain_id);
    let image = f.image.clone();
    let library_id = f.state.library_id().to_owned();
    let namespace = f.state.provider().album_namespace();
    let album_id = f.state.provider().get_audio(id).unwrap().album_id.unwrap();
    tokio::task::spawn_blocking(move || {
        let client = RemoteClient::new(&url, TOKEN).unwrap();
        let capabilities = client.capabilities().unwrap();
        assert!(capabilities.album_artwork_usable());
        assert!(capabilities.album_entity_artwork_usable());
        let cancel = AtomicBool::new(false);
        let ArtworkReply::Data { bytes, etag, .. } = client
            .artwork_for_library(&library_id, id, None, None, &cancel)
            .unwrap()
        else {
            panic!("cover must be served");
        };
        assert_eq!(bytes, image);
        assert_eq!(
            client
                .artwork_for_library(&library_id, id, None, etag.as_deref(), &cancel)
                .unwrap(),
            ArtworkReply::NotModified
        );
        assert_eq!(
            client
                .artwork_for_library(&library_id, plain_id, None, None, &cancel)
                .unwrap(),
            ArtworkReply::Missing
        );
        assert!(matches!(
            client.artwork_for_library("replaced-library", id, None, None, &cancel),
            Err(audiolib_remote::RemoteError::LibraryChanged(_))
        ));
        let ArtworkReply::Data { bytes, etag, .. } = client
            .album_artwork_for_library(&library_id, &namespace, album_id, None, None, &cancel)
            .unwrap()
        else {
            panic!("shared album cover must be served");
        };
        assert_eq!(bytes, image);
        assert_eq!(
            client
                .album_artwork_for_library(
                    &library_id,
                    &namespace,
                    album_id,
                    None,
                    etag.as_deref(),
                    &cancel
                )
                .unwrap(),
            ArtworkReply::NotModified
        );
        assert!(matches!(
            client.album_artwork_for_library(
                &library_id,
                "replaced-namespace",
                album_id,
                None,
                etag.as_deref(),
                &cancel
            ),
            Err(audiolib_remote::RemoteError::LibraryChanged(_))
        ));
    })
    .await
    .unwrap();
    let _ = stop.send(());
    task.await.unwrap();

    let stream = router(f.state.clone())
        .oneshot(
            support::modern(Request::get(format!("/api/v1/audio/{}/stream", f.cover_id)))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::RANGE, "bytes=0-43")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::PARTIAL_CONTENT);
    let header = body(stream).await;
    assert_eq!(header.len(), 44);
    assert_eq!(&header[..4], b"RIFF");
    assert_eq!(
        observer
            .pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        before
    );
    assert_eq!(
        observer
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        schema
    );
}

#[tokio::test]
async fn artwork_rejects_missing_duplicate_and_stale_snapshot_identity() {
    let _serial = HTTP_SERIAL.lock().await;
    let f = fixture();
    let path = format!("/api/v1/audio/{}/artwork", f.cover_id);
    for (identities, code) in [
        (Vec::new(), "LIBRARY_ID_REQUIRED"),
        (vec![""], "LIBRARY_ID_REQUIRED"),
        (vec!["different-library"], "LIBRARY_CHANGED"),
        (
            vec![f.state.library_id(), f.state.library_id()],
            "LIBRARY_ID_REQUIRED",
        ),
    ] {
        let mut request = support::modern(Request::get(&path))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
        for identity in identities {
            request = request.header("X-AudioLib-Library-Id", identity);
        }
        let response = router(f.state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let error: ErrorBody = serde_json::from_slice(&body(response).await).unwrap();
        assert_eq!(error.error_code, code);
    }
    assert!(
        !f.db.parent().unwrap().join("artwork").exists(),
        "rejected reads must not create a cache"
    );
}

#[tokio::test]
async fn shared_album_cover_uses_whole_album_and_preserves_legacy_wav_semantics() {
    let _serial = HTTP_SERIAL.lock().await;
    let f = fixture();
    let covered = f.state.provider().get_audio(f.cover_id).unwrap();
    let plain = f.state.provider().get_audio(f.plain_id).unwrap();
    let album_id = covered.album_id.expect("folder fallback album");
    assert_eq!(plain.album_id, Some(album_id));
    let path = format!("/api/v1/albums/{album_id}/artwork?side=512");
    let response = send(&f.state, &path).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-album-id"], album_id.to_string());
    assert_eq!(response.headers()["x-library-id"], f.state.library_id());
    assert_eq!(
        response.headers()["x-album-namespace"],
        f.state.provider().album_namespace()
    );
    let etag = response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(body(response).await, f.image);
    let again = send(&f.state, &path).await;
    assert_eq!(again.headers()[header::ETAG], etag);
    assert_eq!(body(again).await, f.image);
    let conditional = router(f.state.clone())
        .oneshot(
            support::modern(Request::get(&path))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header("X-AudioLib-Library-Id", f.state.library_id())
                .header(
                    "X-AudioLib-Album-Namespace",
                    f.state.provider().album_namespace(),
                )
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(conditional.headers()["x-album-id"], album_id.to_string());
    assert_eq!(
        conditional.headers()["x-album-namespace"],
        f.state.provider().album_namespace()
    );
    assert!(body(conditional).await.is_empty());
    assert_eq!(
        send(&f.state, &format!("/api/v1/audio/{}/artwork", f.plain_id))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let stale = router(f.state.clone())
        .oneshot(
            support::modern(Request::get(&path))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header("X-AudioLib-Library-Id", "different-library")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(
        send(&f.state, "/api/v1/albums/999999/artwork")
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn different_member_pictures_have_one_deterministic_album_representation() {
    let _serial = HTTP_SERIAL.lock().await;
    let f = fixture_with_second_cover(true);
    let file = f.state.provider().get_audio(f.cover_id).unwrap();
    let album_id = file.album_id.unwrap();
    let members = f
        .state
        .provider()
        .album_members(album_id, 64, Arc::new(AtomicBool::new(false)))
        .unwrap();
    assert_eq!(members.len(), 2);
    let first = body(
        send(
            &f.state,
            &format!("/api/v1/audio/{}/artwork", members[0].id),
        )
        .await,
    )
    .await;
    let second = body(
        send(
            &f.state,
            &format!("/api/v1/audio/{}/artwork", members[1].id),
        )
        .await,
    )
    .await;
    assert_ne!(first, second);
    let path = format!("/api/v1/albums/{album_id}/artwork");
    for _ in 0..3 {
        assert_eq!(body(send(&f.state, &path).await).await, first);
    }
}

#[tokio::test]
async fn album_artwork_rejects_missing_duplicate_and_stale_namespace_before_cache() {
    let _serial = HTTP_SERIAL.lock().await;
    let f = fixture();
    let album_id = f
        .state
        .provider()
        .get_audio(f.cover_id)
        .unwrap()
        .album_id
        .unwrap();
    let namespace = f.state.provider().album_namespace();
    for values in [
        Vec::new(),
        vec![""],
        vec!["another-library-namespace"],
        vec![namespace.as_str(), namespace.as_str()],
    ] {
        let mut request =
            support::modern(Request::get(format!("/api/v1/albums/{album_id}/artwork")))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header("X-AudioLib-Library-Id", f.state.library_id());
        for value in values {
            request = request.header("X-AudioLib-Album-Namespace", value);
        }
        let response = router(f.state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let error: ErrorBody = serde_json::from_slice(&body(response).await).unwrap();
        assert_eq!(error.error_code, "LIBRARY_CHANGED");
    }
    assert!(!f.db.parent().unwrap().join("artwork").exists());
}

async fn send_if_none_match(state: &AppState, path: &str, etag: &str) -> Response {
    router(state.clone())
        .oneshot(
            support::modern(Request::get(path))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header("X-AudioLib-Library-Id", state.library_id())
                .header(header::IF_NONE_MATCH, etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn cover_cache_hits_and_304_need_no_decode_slot_and_misses_wait_briefly() {
    let _serial = HTTP_SERIAL.lock().await;
    let f = fixture();
    let cached = format!("/api/v1/audio/{}/artwork", f.cover_id);
    let warm = send(&f.state, &cached).await;
    assert_eq!(warm.status(), StatusCode::OK);
    let etag = warm.headers()[header::ETAG].to_str().unwrap().to_owned();

    // Every decode slot is held (two slow decodes elsewhere).
    let slots = f.state.artwork_slots();
    let held = slots.clone().acquire_many_owned(2).await.unwrap();
    assert_eq!(slots.available_permits(), 0);

    // Slots stay held throughout, so a request that needed one would get 503;
    // 200 and 304 here prove cache hits and conditional hits never take a slot.
    let hit = send(&f.state, &cached).await;
    assert_eq!(hit.status(), StatusCode::OK);
    assert_eq!(body(hit).await, f.image);
    let not_modified = send_if_none_match(&f.state, &cached, &etag).await;
    assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);

    // A cache miss (new thumbnail size) waits about 0.5 s, then 503 BUSY + Retry-After.
    let miss = format!("/api/v1/audio/{}/artwork?side=8", f.cover_id);
    let started = std::time::Instant::now();
    let busy = send(&f.state, &miss).await;
    let waited = started.elapsed();
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(busy.headers()[header::RETRY_AFTER], "1");
    let error: ErrorBody = serde_json::from_slice(&body(busy).await).unwrap();
    assert_eq!(error.error_code, "BUSY");
    assert!(
        // Lower bound only: the timeout never fires early; slow machines may be late.
        waited >= Duration::from_millis(400),
        "{waited:?}"
    );

    // HEAD of the same busy miss: identical headers (incl. Retry-After and
    // Content-Length) to GET, and no body.
    let get_busy = send(&f.state, &miss).await;
    let head_busy = send_method(&f.state, Method::HEAD, &miss).await;
    assert_eq!(head_busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(head_busy.headers()[header::RETRY_AFTER], "1");
    let get_length = get_busy.headers()[header::CONTENT_LENGTH].clone();
    assert_eq!(
        head_busy.headers().get(header::CONTENT_LENGTH),
        Some(&get_length)
    );
    let mut get_names: Vec<_> = get_busy
        .headers()
        .keys()
        .map(|n| n.as_str().to_owned())
        .collect();
    let mut head_names: Vec<_> = head_busy
        .headers()
        .keys()
        .map(|n| n.as_str().to_owned())
        .collect();
    get_names.sort();
    head_names.sort();
    assert_eq!(get_names, head_names);
    let get_body = body(get_busy).await;
    assert_eq!(get_length.to_str().unwrap(), get_body.len().to_string());
    assert!(body(head_busy).await.is_empty());

    // A slot freed during the wait serves the queued miss instead of 503.
    let state = f.state.clone();
    let queued = tokio::spawn(async move { send(&state, &miss).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(held);
    let served = queued.await.unwrap();
    assert_eq!(served.status(), StatusCode::OK);
    let thumb = image::load_from_memory(&body(served).await).unwrap();
    assert!(thumb.width() <= 8 && thumb.height() <= 8);
    assert_eq!(slots.available_permits(), 2, "decode slots are returned");
}

#[tokio::test]
async fn truncated_cover_cache_is_a_miss_regenerated_through_a_slot() {
    let _serial = HTTP_SERIAL.lock().await;
    let f = fixture();
    let path = format!("/api/v1/audio/{}/artwork", f.cover_id);
    let warm = send(&f.state, &path).await;
    assert_eq!(warm.status(), StatusCode::OK);
    let cache = f.db.parent().unwrap().join("artwork");
    let entry = fs::read_dir(&cache)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_file())
        .unwrap();
    let full = fs::read(&entry).unwrap();
    fs::write(&entry, &full[..full.len() / 2]).unwrap();

    // With every slot held, the truncated entry must not be served as a hit.
    let slots = f.state.artwork_slots();
    let held = slots.clone().acquire_many_owned(2).await.unwrap();
    let busy = send(&f.state, &path).await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(held);

    let served = send(&f.state, &path).await;
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(body(served).await, f.image);
    assert_eq!(fs::read(&entry).unwrap(), full, "cache entry regenerated");

    // The repaired entry is a hit again without a slot.
    let held = slots.clone().acquire_many_owned(2).await.unwrap();
    let hit = send(&f.state, &path).await;
    assert_eq!(hit.status(), StatusCode::OK);
    assert_eq!(body(hit).await, f.image);
    drop(held);
    let leftovers: Vec<_> = fs::read_dir(&cache)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([u8::try_from((x + y) % 200).unwrap_or(1), 90, 30])
    });
    let mut output = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 85)
        .encode_image(&image::DynamicImage::ImageRgb8(image))
        .unwrap();
    output
}

/// Real embedded covers often carry bytes after JPEG FFD9 / PNG IEND. The cache
/// must store the trimmed picture so the next read is a slot-free hit, not an
/// endless delete-and-regenerate loop.
#[tokio::test]
async fn padded_embedded_covers_are_cached_as_hits_without_a_slot() {
    let _serial = HTTP_SERIAL.lock().await;
    let mut padded_jpeg = jpeg(24, 12);
    let clean_jpeg = padded_jpeg.clone();
    padded_jpeg.extend_from_slice(&[0; 64]);
    padded_jpeg.extend_from_slice(b"trailing garbage");
    let clean_png = png(20, 10);
    let mut padded_png = clean_png.clone();
    padded_png.extend_from_slice(&[0, 0, 0, 0, 7, 7]);
    for (mime, padded, clean) in [
        ("image/jpeg", padded_jpeg, clean_jpeg),
        ("image/png", padded_png, clean_png),
    ] {
        let f = fixture_with_cover(mime, &padded, clean.clone(), false);
        let path = format!("/api/v1/audio/{}/artwork", f.cover_id);
        let first = send(&f.state, &path).await;
        assert_eq!(first.status(), StatusCode::OK, "{mime}");
        assert_eq!(body(first).await, clean, "{mime}: trimmed original");
        let slots = f.state.artwork_slots();
        let held = slots.clone().acquire_many_owned(2).await.unwrap();
        for _ in 0..2 {
            let hit = send(&f.state, &path).await;
            assert_eq!(
                hit.status(),
                StatusCode::OK,
                "{mime}: cache hit needs no slot"
            );
            assert_eq!(body(hit).await, clean, "{mime}");
        }
        drop(held);
    }
}
