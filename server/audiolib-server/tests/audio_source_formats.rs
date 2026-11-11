// server/audiolib-server/tests/audio_source_formats.rs
//! New source formats retain Metadata and exact bytes across HTTP and offline sync.
use audiolib_core::{
    AudioFile, DataProvider, SearchPage, SearchRequest,
    compatibility::{
        ACCEPT_FORMATS_HEADER, AUDIO_SOURCES_FEATURE, CLIENT_COMPATIBILITY_HEADER,
        PLAN_B_AUDIO_FORMAT, PLAN_B_V9_AUDIO_FORMAT, SEARCH_STORAGE_MAX, SEARCH_STORAGE_MAX_HEADER,
        SNAPSHOT_FORMAT_HEADER, SNAPSHOT_SCHEMA_MAX, SNAPSHOT_SCHEMA_MAX_HEADER, TOKENIZATION_MAX,
        TOKENIZATION_MAX_HEADER, compatibility_manifest,
    },
};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use audiolib_remote::{CacheState, RemoteCache, RemoteClient, sync_library};
use audiolib_server::{AppState, Security, ServeOptions, ServerConfig, router, serve};
use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::Value;
use std::{
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tower::ServiceExt;

fn chunk(id: &[u8; 4], body: &[u8], big_endian: bool, bytes: &mut Vec<u8>) {
    bytes.extend(id);
    let size = body.len() as u32;
    bytes.extend(if big_endian {
        size.to_be_bytes()
    } else {
        size.to_le_bytes()
    });
    bytes.extend(body);
    if !body.len().is_multiple_of(2) {
        bytes.push(0);
    }
}

fn tagged_mp3() -> Vec<u8> {
    let mut frames = Vec::new();
    for (id, value) in [
        (b"TIT2", b"\0Formatcoverage mp3title".as_slice()),
        (b"COMM", b"\0eng\0formatcoverage mp3description".as_slice()),
    ] {
        frames.extend(id);
        frames.extend((value.len() as u32).to_be_bytes());
        frames.extend([0, 0]);
        frames.extend(value);
    }
    let mut bytes = b"ID3\x03\0\0".to_vec();
    for shift in [21, 14, 7, 0] {
        bytes.push(((frames.len() >> shift) & 127) as u8);
    }
    bytes.extend(frames);
    bytes.extend(include_bytes!(
        "../../../crates/audiolib-audio/src/fixtures/mp3/cbr-info.mp3"
    ));
    bytes
}

fn tagged_aiff() -> Vec<u8> {
    let mut common = 1_u16.to_be_bytes().to_vec();
    common.extend(4_u32.to_be_bytes());
    common.extend(16_u16.to_be_bytes());
    // Exact 48 kHz IEEE extended precision value, with an explicit integer bit.
    common.extend([0x40, 0x0e, 0xbb, 0x80, 0, 0, 0, 0, 0, 0]);
    let mut sound = vec![0; 8];
    sound.extend(
        [-32_768_i16, -16_384, 0, 16_384]
            .into_iter()
            .flat_map(i16::to_be_bytes),
    );
    let mut bytes = b"FORM\0\0\0\0AIFF".to_vec();
    for (id, body) in [
        (b"COMM", common.as_slice()),
        (b"NAME", b"Formatcoverage aifftitle".as_slice()),
        (b"ANNO", b"formatcoverage aiffdescription".as_slice()),
        (b"SSND", sound.as_slice()),
    ] {
        chunk(id, body, true, &mut bytes);
    }
    let size = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&size.to_be_bytes());
    bytes
}

fn tagged_bwf() -> Vec<u8> {
    let mut format = 1_u16.to_le_bytes().to_vec();
    format.extend(1_u16.to_le_bytes());
    format.extend(48_000_u32.to_le_bytes());
    format.extend(96_000_u32.to_le_bytes());
    format.extend(2_u16.to_le_bytes());
    format.extend(16_u16.to_le_bytes());
    let mut broadcast = vec![0; 602];
    let description = b"formatcoverage bwfdescription";
    broadcast[..description.len()].copy_from_slice(description);
    let mut info = b"INFO".to_vec();
    chunk(b"INAM", b"Formatcoverage bwftitle\0", false, &mut info);
    let mut bytes = b"RIFF\0\0\0\0WAVE".to_vec();
    for (id, body) in [
        (b"fmt ", format.as_slice()),
        (b"bext", broadcast.as_slice()),
        (b"LIST", info.as_slice()),
        (b"data", [0_u8; 8].as_slice()),
    ] {
        chunk(id, body, false, &mut bytes);
    }
    let size = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
    bytes
}

fn source_library(root: &Path) -> Vec<AudioFile> {
    let sources = root.join("sources");
    fs::create_dir_all(&sources).unwrap();
    for (name, bytes) in [
        ("one.mp3", tagged_mp3()),
        ("two.aif", tagged_aiff()),
        ("three.bwf", tagged_bwf()),
        ("four.wav", tagged_mp3()),
    ] {
        fs::write(sources.join(name), bytes).unwrap();
    }
    let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
    let report = ScanService::new(provider.clone())
        .scan(
            &sources,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!((report.added, report.errors), (4, 0), "{report:?}");
    let files = provider.search(&SearchRequest::default()).unwrap().results;
    assert_eq!(files.len(), 4);
    assert_metadata(&provider, &files);
    files
}

fn headers(old: bool) -> HeaderMap {
    let mut manifest = compatibility_manifest();
    if old {
        manifest
            .snapshot_read_formats
            .retain(|id| id != PLAN_B_AUDIO_FORMAT && id != PLAN_B_V9_AUDIO_FORMAT);
        manifest.features.remove(AUDIO_SOURCES_FEATURE);
    }
    let mut headers = HeaderMap::new();
    for (name, value) in [
        (
            CLIENT_COMPATIBILITY_HEADER,
            serde_json::to_string(&manifest).unwrap(),
        ),
        (
            ACCEPT_FORMATS_HEADER,
            manifest.snapshot_read_formats.join(","),
        ),
        (SNAPSHOT_SCHEMA_MAX_HEADER, SNAPSHOT_SCHEMA_MAX.to_string()),
        (SEARCH_STORAGE_MAX_HEADER, SEARCH_STORAGE_MAX.to_string()),
        (TOKENIZATION_MAX_HEADER, TOKENIZATION_MAX.to_string()),
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
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("x-audiolib-library-id", state.library_id())
        .body(Body::from(body))
        .unwrap();
    request.headers_mut().extend(headers.clone());
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

fn assert_metadata(provider: &SqliteProvider, expected: &[AudioFile]) {
    let page = provider
        .search(&SearchRequest {
            query: "formatcoverage".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.total_count, expected.len() as u64);
    for original in expected {
        let file = provider.get_audio(original.id).unwrap();
        assert_eq!(
            file.attributes, original.attributes,
            "{}",
            original.filename
        );
        let kind = match file.filename.as_str() {
            "one.mp3" | "four.wav" => "mp3",
            "two.aif" => "aiff",
            "three.bwf" => "bwf",
            other => panic!("Unexpected test file {other}"),
        };
        assert_eq!(
            file.attributes["wav_metadata"]["fields"]["title"],
            format!("Formatcoverage {kind}title")
        );
        assert_eq!(
            audiolib_core::source_description(&file.bext_description, &file.attributes),
            format!("formatcoverage {kind}description")
        );
        for term in [format!("{kind}title"), format!("{kind}description")] {
            let matched = provider
                .search(&SearchRequest {
                    query: term,
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(matched.total_count, if kind == "mp3" { 2 } else { 1 });
            assert!(matched.results.iter().any(|row| row.id == file.id));
        }
        if kind == "mp3" {
            let layout = &file.attributes["source_audio_layout"];
            assert_eq!(layout["container"], "MP3");
            assert_eq!(layout["frames"], 6000);
            assert_eq!(layout["sample_rate"], 48_000);
            assert_eq!(layout["channels"], 2);
            assert!(layout["data_offset"].as_u64().unwrap() > 0);
            assert!(layout["data_bytes"].as_u64().unwrap() > 0);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_formats_scan_stream_negotiate_and_reopen_from_an_offline_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let original = source_library(root.path());
    let config = ServerConfig::from_toml_str(
        "bind='127.0.0.1:0'\nlibrary_db='library.db'\nsnapshot_dir='snapshots'\nauth_mode='off'\nmanage_mode='all'\nlist_waveform_workers=1\n[[roots]]\nname='Owned'\nserver_path='sources'\n",
        root.path(),
    ).unwrap();
    let state =
        AppState::open_with_security(&config, Security::from_config(&config, None)).unwrap();
    let modern = headers(false);
    let old = headers(true);
    let (status, _, body) = request(&state, "GET", "/api/v1/library", &modern, vec![]).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let library: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(library["snapshot_format"]["id"], PLAN_B_AUDIO_FORMAT);
    assert_eq!(library["schema_version"], 8);
    assert_eq!(library["stats"]["audio_files"], 4);
    for endpoint in ["/api/v1/library", "/api/v1/library/snapshot"] {
        let (status, _, body) = request(&state, "GET", endpoint, &old, vec![]).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{endpoint}: {}",
            String::from_utf8_lossy(&body)
        );
        assert!(String::from_utf8_lossy(&body).contains("SNAPSHOT_FORMAT_UNSUPPORTED"));
    }
    let mut json_headers = modern.clone();
    json_headers.insert("content-type", HeaderValue::from_static("application/json"));
    let search = serde_json::to_vec(&SearchRequest {
        query: "formatcoverage".into(),
        ..Default::default()
    })
    .unwrap();
    let (status, _, body) = request(&state, "POST", "/api/v1/search", &json_headers, search).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        serde_json::from_slice::<SearchPage>(&body)
            .unwrap()
            .total_count,
        4
    );
    for file in &original {
        let (status, _, body) = request(
            &state,
            "GET",
            &format!("/api/v1/audio/{}", file.id),
            &modern,
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let served: AudioFile = serde_json::from_slice(&body).unwrap();
        assert_eq!(served.attributes, file.attributes);
        let expected = fs::read(&file.file_path).unwrap();
        let mime = match file.filename.as_str() {
            "one.mp3" | "four.wav" => "audio/mpeg",
            "two.aif" => "audio/aiff",
            _ => "audio/wav",
        };
        let mut range = modern.clone();
        range.insert("range", HeaderValue::from_static("bytes=3-18"));
        let (status, response, body) = request(
            &state,
            "GET",
            &format!("/api/v1/audio/{}/stream", file.id),
            &range,
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(response["content-type"], mime);
        assert_eq!(
            response["content-range"],
            format!("bytes 3-18/{}", expected.len())
        );
        assert_eq!(body, expected[3..19]);
    }
    let (status, response, body) =
        request(&state, "GET", "/api/v1/library/snapshot", &modern, vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response[SNAPSHOT_FORMAT_HEADER], PLAN_B_AUDIO_FORMAT);
    let snapshot = root.path().join("accepted-snapshot.db");
    fs::write(&snapshot, body).unwrap();
    assert_metadata(
        &SqliteProvider::open_read_only(&snapshot).unwrap(),
        &original,
    );

    // Real RemoteClient sync checks all response-header/SQLite format claims.
    // Stop the listener and workers before preparing and reopening the cache.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(serve(
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
    let data_root = root.path().join("client-data");
    let source_folder = root.path().join("sources").to_string_lossy().into_owned();
    let cache = RemoteCache::new(&data_root);
    let syncing_cache = cache.clone();
    let synced = tokio::task::spawn_blocking(move || {
        let client = RemoteClient::new(&url, "").unwrap();
        let info = client.library().unwrap();
        assert_eq!(
            info.snapshot_format.as_ref().unwrap().id,
            PLAN_B_AUDIO_FORMAT
        );
        let listing = client
            .browse_directory(Some(&source_folder), false)
            .unwrap();
        assert_eq!(listing.audio_files, Some(4));
        sync_library(
            &client,
            &syncing_cache,
            &mut |_| {},
            &AtomicBool::new(false),
            false,
        )
        .unwrap()
        .state
    })
    .await;
    let _ = stop.send(());
    serving.await.unwrap();
    state.shutdown(Duration::from_secs(5)).unwrap();
    let synced: CacheState = synced.unwrap();
    let saved = cache.active().unwrap();
    assert_eq!(saved.database_file, synced.database_file);
    assert_eq!(saved.schema_version, 8);
    let observed = saved.format.as_ref().unwrap();
    assert_eq!(
        observed
            .source
            .as_ref()
            .unwrap()
            .snapshot_format_id()
            .as_deref(),
        Some(PLAN_B_AUDIO_FORMAT)
    );
    assert_eq!(
        observed.local.snapshot_format_id().as_deref(),
        Some(PLAN_B_AUDIO_FORMAT)
    );
    let prepared = cache.prepare_cached_state(&saved).unwrap();
    let offline = SqliteProvider::open_read_only(&cache.database_path(&prepared)).unwrap();
    assert_metadata(&offline, &original);
}
