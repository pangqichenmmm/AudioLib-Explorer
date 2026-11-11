// server/audiolib-server/tests/smed_artwork.rs
//! SMED-only pictures through scanning, shared albums, HTTP and the remote client.
mod support;

use audiolib_core::{AlbumKind, DataProvider};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use audiolib_remote::{ArtworkReply, RemoteClient};
use audiolib_server::{AppState, BearerToken, ServeOptions, ServerConfig, router, serve};
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
    response::Response,
};
use http_body_util::BodyExt;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tower::ServiceExt;

const TOKEN: &str = "synthetic-smed-test-token-0123456789";
const SYNTHETIC_WAV: &[u8] = include_bytes!("fixtures/smed-artwork-synthetic.wav");
const SYNTHETIC_PNG: &[u8] = include_bytes!("fixtures/smed-artwork-synthetic.png");
const MAX_PRIVATE_FIXTURE_BYTES: u64 = 32 * 1024 * 1024;

fn real_path(path: &Path) -> PathBuf {
    let canonical = path.canonicalize().unwrap();
    let text = canonical.to_string_lossy().into_owned();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => rest.into(),
        _ => canonical,
    }
}

fn chunk(bytes: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]) {
    bytes.extend(id);
    bytes.extend(u32::try_from(payload.len()).unwrap().to_le_bytes());
    bytes.extend(payload);
    if !payload.len().is_multiple_of(2) {
        bytes.push(0);
    }
}

fn plain_wav_with_library(path: &Path, library: &str) {
    let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
    let mut format = Vec::new();
    format.extend(1_u16.to_le_bytes());
    format.extend(1_u16.to_le_bytes());
    format.extend(48_000_u32.to_le_bytes());
    format.extend(96_000_u32.to_le_bytes());
    format.extend(2_u16.to_le_bytes());
    format.extend(16_u16.to_le_bytes());
    chunk(&mut bytes, b"fmt ", &format);
    chunk(&mut bytes, b"data", &[0; 64]);
    // ID3v2.3 UTF-16 TALB gives the earlier member the same declared Library,
    // including a real fixture's non-ASCII name, without adding any picture.
    let mut text = vec![1, 0xff, 0xfe];
    text.extend(library.encode_utf16().flat_map(u16::to_le_bytes));
    let mut frame = b"TALB".to_vec();
    frame.extend(u32::try_from(text.len()).unwrap().to_be_bytes());
    frame.extend([0, 0]);
    frame.extend(text);
    let mut tag = b"ID3\x03\0\0".to_vec();
    for shift in [21, 14, 7, 0] {
        tag.push(u8::try_from((frame.len() >> shift) & 127).unwrap());
    }
    tag.extend(frame);
    chunk(&mut bytes, b"ID3 ", &tag);
    let size = u32::try_from(bytes.len() - 8).unwrap();
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    fs::write(path, bytes).unwrap();
}

fn assert_smed_without_id3(bytes: &[u8]) {
    assert!(bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE"));
    let mut offset = 12_usize;
    let mut smed = false;
    while let Some(header) = bytes.get(offset..offset + 8) {
        let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        assert!(
            !matches!(&header[..4], b"ID3 " | b"id3 "),
            "this fixture must exercise SMED only, without an ID3 fallback"
        );
        smed |= &header[..4] == b"SMED";
        offset = offset.checked_add(8 + size + size % 2).unwrap();
        assert!(offset <= bytes.len(), "invalid owned WAV chunk bounds");
    }
    assert!(smed, "a SMED chunk is required");
}

struct Fixture {
    state: AppState,
    _directory: tempfile::TempDir,
    plain: PathBuf,
    covered: PathBuf,
    plain_id: i64,
    covered_id: i64,
    album_id: i64,
    image: Vec<u8>,
}

impl Fixture {
    fn new(covered_bytes: &[u8]) -> Self {
        assert_smed_without_id3(covered_bytes);
        let directory = tempfile::tempdir().unwrap();
        let base = real_path(directory.path());
        let sounds = base.join("sounds");
        fs::create_dir_all(&sounds).unwrap();
        // Keep the source outside the first scan so the no-cover member receives
        // the smaller ID regardless of scanner enumeration or worker order.
        let covered = sounds.join("later-smed.wav");
        let staging = base.join("smed-source.wav");
        fs::write(&staging, covered_bytes).unwrap();
        let ordinary = audiolib_audio::probe(&staging).unwrap();
        let (metadata, art) = audiolib_audio::probe_with_artwork(&staging).unwrap();
        assert_eq!(metadata.attributes, ordinary.attributes);
        let art = art.expect("SMED-only fixture must yield embedded pixels");
        let decoded = image::load_from_memory(&art.bytes).unwrap();
        assert!(decoded.width() > 0 && decoded.height() > 0);
        let library = audiolib_core::album(&ordinary.attributes);
        assert!(!library.is_empty());
        assert!(ordinary.attributes["wav_metadata"]["smed"]["MAGIC/Picture"].is_null());
        let plain = sounds.join("first-no-picture.wav");
        plain_wav_with_library(&plain, &library);
        let database = base.join("library/library.db");
        let provider = Arc::new(SqliteProvider::open(&database).unwrap());
        let scanner = ScanService::new(provider.clone());
        let options = ScanOptions {
            incremental: false,
            workers: 1,
            ..Default::default()
        };
        let first = scanner
            .scan(&sounds, &options, &ScanCancellation::default(), |_| {})
            .unwrap();
        assert_eq!(first.added, 1, "{first:?}");
        assert_eq!(first.errors, 0, "{first:?}");
        fs::rename(staging, &covered).unwrap();
        let second = scanner
            .scan(&sounds, &options, &ScanCancellation::default(), |_| {})
            .unwrap();
        assert_eq!(second.added, 1, "{second:?}");
        assert_eq!(second.errors, 0, "{second:?}");
        let plain_file = provider.find_audio_by_path(&plain).unwrap().unwrap();
        let covered_file = provider.find_audio_by_path(&covered).unwrap().unwrap();
        assert!(plain_file.id < covered_file.id);
        assert_eq!(plain_file.album, library);
        assert_eq!(covered_file.album, library);
        assert!(covered_file.attributes["wav_metadata"]["smed"]["MAGIC/Picture"].is_null());
        let album_id = covered_file.album_id.unwrap();
        assert_eq!(plain_file.album_id, Some(album_id));
        let album = provider.get_album(album_id).unwrap();
        assert_eq!(album.kind, AlbumKind::Metadata);
        assert_eq!(album.representative_audio_id, Some(plain_file.id));
        assert!(provider.check_library().unwrap().is_ok());
        let config = ServerConfig::from_toml_str(
            &format!(
                "bind = \"127.0.0.1:0\"\nauth = true\nlibrary_db = 'library/library.db'\n\
                 [[roots]]\nname = \"Synthetic sounds\"\nserver_path = '{}'\n",
                sounds.display()
            ),
            &base,
        )
        .unwrap();
        drop(scanner);
        drop(provider);
        Self {
            state: AppState::open(&config, BearerToken::new(TOKEN).unwrap()).unwrap(),
            _directory: directory,
            plain,
            covered,
            plain_id: plain_file.id,
            covered_id: covered_file.id,
            album_id,
            image: art.bytes,
        }
    }
}

async fn send(fixture: &Fixture, path: &str, etag: Option<&str>) -> Response {
    let mut request = support::modern(Request::get(path))
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

async fn check_routes(fixture: &Fixture) {
    let wav_path = format!("/api/v1/audio/{}/artwork", fixture.covered_id);
    let original = send(fixture, &wav_path, None).await;
    assert_eq!(original.status(), StatusCode::OK);
    assert_eq!(
        original.headers()["x-library-id"],
        fixture.state.library_id()
    );
    let etag = original.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(body(original).await, fixture.image);
    let conditional = send(fixture, &wav_path, Some(&etag)).await;
    assert_eq!(conditional.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(
        conditional.headers()["x-library-id"],
        fixture.state.library_id()
    );
    assert!(body(conditional).await.is_empty());
    assert_eq!(
        send(
            fixture,
            &format!("/api/v1/audio/{}/artwork", fixture.plain_id),
            None
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    let thumb = send(fixture, &format!("{wav_path}?side=512"), None).await;
    assert_eq!(thumb.status(), StatusCode::OK);
    let thumbnail = body(thumb).await;
    let decoded = image::load_from_memory(&thumbnail).unwrap();
    assert!(decoded.width() <= 512 && decoded.height() <= 512);
    let album_path = format!("/api/v1/albums/{}/artwork?side=512", fixture.album_id);
    let shared = send(fixture, &album_path, None).await;
    assert_eq!(shared.status(), StatusCode::OK);
    assert_eq!(shared.headers()["x-library-id"], fixture.state.library_id());
    assert_eq!(shared.headers()["x-album-id"], fixture.album_id.to_string());
    assert_eq!(
        shared.headers()["x-album-namespace"],
        fixture.state.provider().album_namespace()
    );
    let shared_etag = shared.headers()[header::ETAG].to_str().unwrap().to_owned();
    assert_eq!(body(shared).await, thumbnail);
    let cached = send(fixture, &album_path, Some(&shared_etag)).await;
    assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(cached.headers()["x-album-id"], fixture.album_id.to_string());
    assert_eq!(
        cached.headers()["x-album-namespace"],
        fixture.state.provider().album_namespace()
    );
    assert!(body(cached).await.is_empty());
}

async fn check_remote_client(fixture: &Fixture) {
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
    let (file_id, album_id) = (fixture.covered_id, fixture.album_id);
    let expected = fixture.image.clone();
    let checked = tokio::task::spawn_blocking(move || {
        let client = RemoteClient::new(&url, TOKEN).unwrap();
        let capabilities = client.capabilities().unwrap();
        assert!(capabilities.album_artwork_usable());
        assert!(capabilities.album_entity_artwork_usable());
        let cancel = AtomicBool::new(false);
        for album in [false, true] {
            let request = |etag| {
                if album {
                    client.album_artwork_for_library(
                        &library, &namespace, album_id, None, etag, &cancel,
                    )
                } else {
                    client.artwork_for_library(&library, file_id, None, etag, &cancel)
                }
            };
            let ArtworkReply::Data { bytes, etag, .. } = request(None).unwrap() else {
                panic!("SMED-only image must reach the remote consumer");
            };
            assert_eq!(bytes, expected);
            assert_eq!(request(etag.as_deref()).unwrap(), ArtworkReply::NotModified);
        }
    })
    .await;
    // Stop only this test's listener even if the assertions in its blocking task failed.
    let _ = stop.send(());
    server.await.unwrap();
    checked.unwrap();
}

async fn check_fixture(bytes: &[u8], synthetic: bool) {
    let fixture = Fixture::new(bytes);
    if synthetic {
        assert_eq!(fixture.image, SYNTHETIC_PNG);
        let decoded = image::load_from_memory(&fixture.image).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (8, 4));
    }
    let plain_before = fs::read(&fixture.plain).unwrap();
    let covered_before = fs::read(&fixture.covered).unwrap();
    let covered_mtime = fs::metadata(&fixture.covered).unwrap().modified().unwrap();
    check_routes(&fixture).await;
    check_remote_client(&fixture).await;
    assert_eq!(fs::read(&fixture.plain).unwrap(), plain_before);
    assert_eq!(fs::read(&fixture.covered).unwrap(), covered_before);
    assert_eq!(
        fs::metadata(&fixture.covered).unwrap().modified().unwrap(),
        covered_mtime
    );
    assert!(fixture.state.provider().check_library().unwrap().is_ok());
}

#[tokio::test]
async fn smed_only_member_after_the_representative_reaches_server_and_remote_client() {
    check_fixture(SYNTHETIC_WAV, true).await;
}

#[tokio::test]
#[ignore = "requires explicit AUDIOLIB_SMED_SERVER_FIXTURE with private SMED-only blank-audio WAV"]
async fn real_smed_tag_supplies_audio_and_shared_album_artwork() {
    let path = std::env::var_os("AUDIOLIB_SMED_SERVER_FIXTURE")
        .map(PathBuf::from)
        .expect("explicit private SMED-only blank-audio fixture path is required");
    let before = fs::metadata(&path).unwrap();
    assert!(before.is_file() && before.len() <= MAX_PRIVATE_FIXTURE_BYTES);
    let bytes = fs::read(&path).unwrap();
    check_fixture(&bytes, false).await;
    let after = fs::metadata(&path).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    assert_eq!(fs::read(&path).unwrap(), bytes);
}
