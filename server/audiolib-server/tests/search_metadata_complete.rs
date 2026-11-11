// server/audiolib-server/tests/search_metadata_complete.rs
//! Real source containers and vendor workbooks across HTTP, schema 9 and offline caches.
use audiolib_core::{
    AudioFile, DataProvider, MetadataPatch, ScanState, SearchPage, SearchRequest, SearchScope,
    compatibility::{
        ACCEPT_FORMATS_HEADER, CLIENT_COMPATIBILITY_HEADER, PLAN_B_FORMAT, PLAN_B_V9_FORMAT,
        compatibility_manifest,
    },
    vendor_import::*,
};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use audiolib_remote::{
    CacheState, CacheUpdate, RemoteCache, RemoteClient, RemoteWriter, ScanMode, ScanRequest,
    sync_library,
};
use audiolib_server::{AppState, Security, ServeOptions, ServerConfig, router, serve};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

fn fixtures(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates")
        .join(relative)
}

fn silent_wav(path: &Path) {
    let samples = vec![0_u8; 256];
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&8_000_u32.to_le_bytes());
    bytes.extend_from_slice(&16_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&samples);
    fs::write(path, bytes).unwrap();
}

fn source_library(root: &Path, with_wav: bool) -> PathBuf {
    let sources = root.join("sources");
    fs::create_dir_all(&sources).unwrap();
    for name in ["comments.flac", "comments.ogg", "comments.oga"] {
        fs::copy(
            fixtures("audiolib-audio/src/fixtures/vorbis").join(name),
            sources.join(name),
        )
        .unwrap();
    }
    if with_wav {
        silent_wav(&sources.join("door.wav"));
        silent_wav(&sources.join("slam.wav"));
    }
    let database = root.join("library.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    let report = ScanService::new(provider.clone())
        .scan(
            &sources,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(report.errors, 0, "{report:?}");
    assert_eq!(report.added, if with_wav { 5 } else { 3 });
    for file in provider.search(&SearchRequest::default()).unwrap().results {
        provider
            .update_metadata(
                file.id,
                &MetadataPatch {
                    tags: Some("protectedtag".into()),
                    notes: Some("protectednote".into()),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    database
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vendor_server_directory_preview_is_root_bound_read_only_and_commits_real_tables() {
    let root = tempfile::tempdir().unwrap();
    let database = source_library(root.path(), true);
    let tables = root.path().join("sources/tables/nested");
    fs::create_dir_all(&tables).unwrap();
    for name in ["basic.csv", "basic.xls", "basic.xlsx"] {
        fs::copy(
            fixtures("audiolib-db/tests/fixtures/vendor_import").join(name),
            tables.join(name),
        )
        .unwrap();
    }
    with_server(root.path(), "all", move |url| {
        let client = RemoteClient::new(&url, "").unwrap();
        migrate(&client);
        let before = client.library().unwrap();
        for body in [
            serde_json::json!({"root":"Owned", "root_relative_dir":"../outside"}),
            serde_json::json!({"root":"Owned", "root_relative_dir":"C:\\private"}),
            serde_json::json!({"root":"Unknown", "root_relative_dir":"tables"}),
        ] {
            let (status, _) = http(
                &url,
                "POST",
                "/api/vendor-import/preview",
                Some(&before.library_id),
                &[("Content-Type", "application/json")],
                &serde_json::to_vec(&body).unwrap(),
            );
            assert_eq!(status, 400, "{body}");
        }
        let body =
            serde_json::to_vec(&serde_json::json!({"root":"Owned", "root_relative_dir":"tables"}))
                .unwrap();
        let (status, bytes) = http(
            &url,
            "POST",
            "/api/vendor-import/preview",
            Some(&before.library_id),
            &[("Content-Type", "application/json; charset=utf-8")],
            &body,
        );
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        let preview: VendorImportPreview = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(preview.files.len(), 3, "{preview:?}");
        assert!(preview.can_commit, "{preview:?}");
        let oversized_mapping = vec![b' '; 1024 * 1024 + 1];
        let (status, _) = http(
            &url,
            "PUT",
            &format!("/api/vendor-import/{}/mapping", preview.import_id),
            Some(&before.library_id),
            &[("Content-Type", "application/json")],
            &oversized_mapping,
        );
        assert_eq!(
            status, 413,
            "Mapping JSON must be rejected at the 1MiB extraction boundary"
        );
        assert_eq!(client.library().unwrap().revision, before.revision);
        assert!(
            SqliteProvider::open_read_only(&database)
                .unwrap()
                .search(&SearchRequest {
                    query: "vendoruniqueword".into(),
                    ..Default::default()
                })
                .unwrap()
                .results
                .is_empty()
        );
        let committed = client
            .vendor_import_commit(&preview.import_id, "server-directory-tables")
            .unwrap();
        assert!(committed.written > 0, "{committed:?}");
        assert!(client.library().unwrap().revision > before.revision);
        client.vendor_import_rollback(&preview.import_id).unwrap();
        assert_eq!(
            client.library().unwrap().audio_files(),
            before.audio_files()
        );
    })
    .await;
}

/// Keep server shutdown and owned background workers outside the assertion task.
async fn with_server<T: Send + 'static>(
    root: &Path,
    manage: &str,
    work: impl FnOnce(String) -> T + Send + 'static,
) -> T {
    let config = ServerConfig::from_toml_str(&format!(
        "bind='127.0.0.1:0'\nlibrary_db='library.db'\nsnapshot_dir='snapshots'\nauth_mode='off'\nmanage_mode='{manage}'\nlist_waveform_workers=1\n[[roots]]\nname='Owned'\nserver_path='sources'\n"),root).unwrap();
    let state =
        AppState::open_with_security(&config, Security::from_config(&config, None)).unwrap();
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
    let result = tokio::task::spawn_blocking(move || work(url)).await;
    let _ = stop.send(());
    serving.await.unwrap();
    state.shutdown(Duration::from_secs(5)).unwrap();
    result.unwrap()
}

fn migrate(client: &RemoteClient) {
    let mut status = client.migration_action("start", true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while status.state != "done" {
        assert!(status.error_code.is_none(), "{status:?}");
        assert!(Instant::now() < deadline, "{status:?}");
        std::thread::sleep(Duration::from_millis(20));
        status = client.migration_status().unwrap();
    }
    assert_eq!(client.library().unwrap().schema_version, 9);
}

/// Tiny bounded HTTP/1 client for negative admission and exact source bytes, using
/// a real socket instead of routing a request directly through tower::Service.
fn http(
    url: &str,
    method: &str,
    path: &str,
    library: Option<&str>,
    extra: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<u8>) {
    let address = url.strip_prefix("http://").unwrap();
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let manifest = serde_json::to_string(&compatibility_manifest()).unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Length: {}\r\n{CLIENT_COMPATIBILITY_HEADER}: {manifest}\r\n{ACCEPT_FORMATS_HEADER}: {PLAN_B_FORMAT},{PLAN_B_V9_FORMAT}\r\nx-audiolib-admin: 1\r\n",
        body.len()
    );
    if let Some(library) = library {
        request.push_str(&format!("x-audiolib-library-id: {library}\r\n"));
    }
    for (name, value) in extra {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    socket.write_all(request.as_bytes()).unwrap();
    socket.write_all(body).unwrap();
    let mut response = Vec::new();
    socket
        .take(8 * 1024 * 1024)
        .read_to_end(&mut response)
        .unwrap();
    let end = response
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap();
    let headers = String::from_utf8_lossy(&response[..end]);
    let status = headers
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let body = &response[end + 4..];
    if headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked"))
    {
        let mut remaining = body;
        let mut decoded = Vec::new();
        loop {
            let end = remaining
                .windows(2)
                .position(|part| part == b"\r\n")
                .unwrap();
            let length = usize::from_str_radix(
                std::str::from_utf8(&remaining[..end])
                    .unwrap()
                    .split(';')
                    .next()
                    .unwrap(),
                16,
            )
            .unwrap();
            if length == 0 {
                break;
            }
            remaining = &remaining[end + 2..];
            decoded.extend_from_slice(&remaining[..length]);
            remaining = &remaining[length + 2..];
        }
        (status, decoded)
    } else {
        (status, body.to_vec())
    }
}

fn server_search(url: &str, query: &str) -> SearchPage {
    let body = serde_json::to_vec(&SearchRequest {
        query: query.into(),
        scope: SearchScope::Main,
        ..Default::default()
    })
    .unwrap();
    let (status, body) = http(
        url,
        "POST",
        "/api/v1/search",
        None,
        &[("Content-Type", "application/json")],
        &body,
    );
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

fn cached(cache: &RemoteCache, state: &CacheState) -> SqliteProvider {
    let prepared = cache.prepare_cached_state(state).unwrap();
    SqliteProvider::open_read_only(&cache.database_path(&prepared)).unwrap()
}

fn assert_words(provider: &SqliteProvider, query: &str, expected: u64) {
    assert_eq!(
        provider
            .search(&SearchRequest {
                query: query.into(),
                scope: SearchScope::Main,
                ..Default::default()
            })
            .unwrap()
            .total_count,
        expected,
        "{query}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_vorbis_sources_survive_schema9_http_sync_metadata_and_offline_open() {
    let root = tempfile::tempdir().unwrap();
    source_library(root.path(), false);
    let data = root.path().join("client");
    let (cache, state, ids) = with_server(root.path(), "all", move |url| {
        let client = RemoteClient::new(&url, "").unwrap();
        migrate(&client);
        let cache = RemoteCache::new(&data);
        let cancel = AtomicBool::new(false);
        let synced = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
        assert_eq!(synced.state.schema_version, 9);
        let provider = cached(&cache, &synced.state);
        assert_words(&provider, "树林 微风", 3);
        let files = provider.search(&SearchRequest::default()).unwrap().results;
        let ids = files.iter().map(|file| file.id).collect::<Vec<_>>();
        let folder = Path::new(&files[0].file_path)
            .parent()
            .unwrap()
            .to_string_lossy();
        let listing = client.browse_directory(Some(&folder), false).unwrap();
        assert_eq!(listing.wav_files, 0);
        assert_eq!(listing.audio_files, Some(3));
        for file in &files {
            assert_eq!(
                file.attributes["wav_metadata"]["vorbis"]["DESCRIPTION"],
                "树林微风"
            );
            assert_eq!(
                audiolib_core::source_description("", &file.attributes),
                "树林微风"
            );
            let (status, body) = http(
                &url,
                "GET",
                &format!("/api/v1/audio/{}", file.id),
                None,
                &[],
                &[],
            );
            assert_eq!(status, 200);
            let remote: AudioFile = serde_json::from_slice(&body).unwrap();
            assert_eq!(remote.attributes, file.attributes);
            let original = fs::read(&file.file_path).unwrap();
            let (status, bytes) = http(
                &url,
                "GET",
                &format!("/api/v1/audio/{}/stream", file.id),
                None,
                &[("Range", "bytes=0-63")],
                &[],
            );
            assert_eq!(status, 206);
            assert_eq!(bytes, &original[..64]);
        }
        drop(provider);
        let writer = RemoteWriter::new(client, cache.clone(), &synced.state.library_id);
        let written = writer
            .update_metadata(
                ids[0],
                &MetadataPatch {
                    notes: Some("postmigrationannotation".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            matches!(written.cache, CacheUpdate::Applied { .. }),
            "{:?}",
            written.cache
        );
        let state = cache.active().unwrap();
        (cache, state, ids)
    })
    .await;
    // The listener and server workers are gone before any of these offline reads.
    let offline = cached(&cache, &state);
    assert_words(&offline, "树林 微风", 3);
    assert_words(&offline, "postmigrationannotation", 1);
    for id in ids {
        assert_eq!(
            offline.get_audio(id).unwrap().attributes["wav_metadata"]["vorbis"]["DESCRIPTION"],
            "树林微风"
        );
    }
}

fn import_file(root: &Path, name: &str, contents: &str) -> PathBuf {
    let path = root.join(name);
    fs::write(&path, contents).unwrap();
    path
}

fn rescan(client: &RemoteClient) {
    let mut scan = client
        .start_scan(&ScanRequest {
            root: "Owned".into(),
            mode: ScanMode::Full,
            workers: Some(1),
            ..Default::default()
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while scan.active {
        assert!(Instant::now() < deadline, "{scan:?}");
        std::thread::sleep(Duration::from_millis(20));
        scan = client.scan_status(scan.progress.session_id).unwrap();
    }
    assert_eq!(scan.progress.state, ScanState::Completed, "{scan:?}");
    assert_eq!(scan.progress.errors, 0, "{scan:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vendor_csv_biff8_xlsx_http_lifecycle_preserves_snapshot_annotations_and_fresh_sources() {
    let root = tempfile::tempdir().unwrap();
    source_library(root.path(), true);
    let owned = root.path().to_path_buf();
    let data = root.path().join("client");
    let (cache, state, flac_id) = with_server(root.path(), "all", move |url| {
        let client = RemoteClient::new(&url, "").unwrap();
        migrate(&client);
        let identity = client.library().unwrap().library_id;
        let client = client.with_library_id(&identity).unwrap();
        assert!(client.capabilities().unwrap().vendor_import.is_some());
        let (status, body) = http(
            &url,
            "GET",
            &format!("{VENDOR_IMPORT_API}/"),
            Some("wrong-library"),
            &[],
            &[],
        );
        assert_eq!(status, 409, "{}", String::from_utf8_lossy(&body));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error_code"],
            "LIBRARY_ID_REQUIRED"
        );
        let mut batches = Vec::new();
        for name in ["basic.csv", "basic.xls", "basic.xlsx"] {
            let source = fixtures("audiolib-db/tests/fixtures/vendor_import").join(name);
            let preview = client
                .vendor_import_preview_files(&[source], &VendorImportOptions::default())
                .unwrap();
            assert!(
                preview.files.iter().all(|file| file.error.is_none()),
                "{preview:?}"
            );
            assert!(preview.can_commit);
            assert_eq!(preview.rows, if name.ends_with(".csv") { 2 } else { 3 });
            let mut options = preview.options.clone();
            options.preferred_languages = vec!["zh".into(), "en".into()];
            options.sheet_mappings = preview
                .sheets
                .iter()
                .map(|sheet| VendorSheetMapping {
                    source_file: sheet.source_file.clone(),
                    sheet: sheet.sheet.clone(),
                    header_row: Some(sheet.header_row),
                    mapping: sheet.mapping.clone(),
                })
                .collect();
            if name.ends_with(".csv") {
                let cancelled = client.vendor_import_cancel(&preview.import_id).unwrap();
                assert_eq!(cancelled.preview.state.as_deref(), Some("cancelled"));
            } else {
                assert_eq!(
                    preview
                        .sheets
                        .iter()
                        .find(|sheet| sheet.sheet == "English")
                        .unwrap()
                        .header_row,
                    2
                );
            }
            let preview = client
                .vendor_import_mapping(&preview.import_id, &options)
                .unwrap();
            let template = client
                .vendor_import_save_template(&VendorMappingTemplate {
                    id: String::new(),
                    name: format!("HTTP mapping {name}"),
                    mapping: preview.sheets[0].mapping.clone(),
                    encoding: options.encoding.clone(),
                    delimiter: options.delimiter.clone(),
                })
                .unwrap();
            assert!(
                client
                    .vendor_import_templates()
                    .unwrap()
                    .contains(&template)
            );
            let rows = client.vendor_import_rows(&preview.import_id, 0, 1).unwrap();
            assert_eq!(rows.len(), 1);
            assert!(rows[0].matched_id.is_some());
            assert_eq!(
                client
                    .vendor_import_status(&preview.import_id)
                    .unwrap()
                    .preview
                    .import_id,
                preview.import_id
            );
            let path = format!("{VENDOR_IMPORT_API}/{}/commit", preview.import_id);
            let (status, body) = http(&url, "POST", &path, Some(&identity), &[], &[]);
            assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
            let key = format!("http-{name}");
            let receipt = client
                .vendor_import_commit(&preview.import_id, &key)
                .unwrap();
            assert_eq!(receipt.state.as_deref(), Some("committed"));
            assert!(receipt.written > 0);
            let revision = client.library().unwrap().revision;
            assert_eq!(
                client
                    .vendor_import_commit(&preview.import_id, &key)
                    .unwrap(),
                receipt
            );
            assert_eq!(
                client.library().unwrap().revision,
                revision,
                "idempotent replay must not publish another revision"
            );
            assert!(
                client
                    .vendor_import_commit(&preview.import_id, "different-key")
                    .is_err()
            );
            let searchable = server_search(&url, "vendoruniqueword");
            assert!(searchable.total_count > 0);
            for file in searchable.results {
                assert_eq!(file.tags, "protectedtag");
                assert_eq!(file.notes, "protectednote");
            }
            let complete = client.vendor_import_status(&preview.import_id).unwrap();
            assert_eq!(complete.idempotency_key.as_deref(), Some(key.as_str()));
            assert!(
                client
                    .vendor_import_history()
                    .unwrap()
                    .imports
                    .iter()
                    .any(|item| item.preview.import_id == preview.import_id)
            );
            let rollback = client.vendor_import_rollback(&preview.import_id).unwrap();
            assert_eq!(rollback.state.as_deref(), Some("rolled_back"));
            assert_eq!(
                client.vendor_import_rollback(&preview.import_id).unwrap(),
                rollback
            );
            assert_eq!(server_search(&url, "vendoruniqueword").total_count, 0);
            batches.push(preview.import_id);
        }
        assert_eq!(batches.len(), 3);
        let a = import_file(
            &owned,
            "first-vendor.csv",
            "Filename,Description\ncomments.flac,vendorfirstmarker\n",
        );
        let b = import_file(
            &owned,
            "second-vendor.csv",
            "Filename,Keywords\ncomments.flac,vendorsecondmarker\n",
        );
        let a = client
            .vendor_import_preview_files(&[a], &VendorImportOptions::default())
            .unwrap();
        client
            .vendor_import_commit(&a.import_id, "first-owned-batch")
            .unwrap();
        let b = client
            .vendor_import_preview_files(&[b], &VendorImportOptions::default())
            .unwrap();
        client
            .vendor_import_commit(&b.import_id, "second-owned-batch")
            .unwrap();
        assert_eq!(server_search(&url, "vendorfirstmarker").total_count, 1);
        assert_eq!(server_search(&url, "vendorsecondmarker").total_count, 1);
        let cache = RemoteCache::new(&data);
        let cancel = AtomicBool::new(false);
        let imported = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
        let provider = cached(&cache, &imported.state);
        assert_words(&provider, "vendorfirstmarker", 1);
        assert_words(&provider, "vendorsecondmarker", 1);
        let file = provider
            .search(&SearchRequest {
                query: "vendorfirstmarker".into(),
                scope: SearchScope::Main,
                ..Default::default()
            })
            .unwrap()
            .results
            .remove(0);
        let id = file.id;
        let mirror = file.attributes["vendor_metadata"].clone();
        drop(provider);
        let writer = RemoteWriter::new(client.clone(), cache.clone(), &identity);
        let written = writer
            .update_metadata(
                id,
                &MetadataPatch {
                    tags: Some("postimporttag".into()),
                    notes: Some("postimportnote".into()),
                    favorite: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            matches!(written.cache, CacheUpdate::Applied { .. }),
            "{:?}",
            written.cache
        );
        let provider = cached(&cache, &cache.active().unwrap());
        assert_words(&provider, "vendorfirstmarker", 1);
        assert_words(&provider, "vendorsecondmarker", 1);
        drop(provider);
        let path = owned.join("sources/comments.flac");
        let mut bytes = fs::read(&path).unwrap();
        let old = b"Forest signal";
        let replacement = b"newsourceword";
        assert_eq!(old.len(), replacement.len());
        let positions = bytes
            .windows(old.len())
            .enumerate()
            .filter_map(|(index, value)| (value == old).then_some(index))
            .collect::<Vec<_>>();
        assert!(!positions.is_empty());
        for index in positions {
            bytes[index..index + old.len()].copy_from_slice(replacement);
        }
        fs::write(&path, bytes).unwrap();
        rescan(&client);
        let refreshed = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
        let provider = cached(&cache, &refreshed.state);
        let rescanned = provider.get_audio(id).unwrap();
        assert_eq!(rescanned.attributes["vendor_metadata"], mirror);
        assert_eq!(rescanned.tags, "postimporttag");
        assert_eq!(rescanned.notes, "postimportnote");
        assert!(rescanned.favorite);
        assert_eq!(
            rescanned.attributes["wav_metadata"]["vorbis"]["TITLE"],
            "newsourceword"
        );
        assert_words(&provider, "newsourceword", 1);
        assert_words(&provider, "vendorfirstmarker", 1);
        assert_words(&provider, "vendorsecondmarker", 1);
        drop(provider);
        client.vendor_import_rollback(&a.import_id).unwrap();
        assert_eq!(server_search(&url, "vendorfirstmarker").total_count, 0);
        assert_eq!(server_search(&url, "vendorsecondmarker").total_count, 1);
        assert_eq!(server_search(&url, "newsourceword").total_count, 1);
        let one_rollback = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
        let provider = cached(&cache, &one_rollback.state);
        assert_words(&provider, "vendorfirstmarker", 0);
        assert_words(&provider, "vendorsecondmarker", 1);
        drop(provider);
        client.vendor_import_rollback(&b.import_id).unwrap();
        let final_sync = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
        (cache, final_sync.state, id)
    })
    .await;
    let offline = cached(&cache, &state);
    assert_words(&offline, "newsourceword", 1);
    assert_words(&offline, "vendorfirstmarker", 0);
    assert_words(&offline, "vendorsecondmarker", 0);
    let file = offline.get_audio(flac_id).unwrap();
    assert_eq!(file.notes, "postimportnote");
    assert_eq!(file.tags, "postimporttag");
    assert!(file.favorite);
    assert_eq!(
        file.attributes["wav_metadata"]["vorbis"]["DESCRIPTION"],
        "树林微风"
    );
}

const VENDOR_IMPORT_API: &str = audiolib_core::server_upgrade::VENDOR_IMPORT_API;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vendor_http_pages_large_ambiguous_candidate_sets_and_accepts_off_page_confirmation() {
    let root = tempfile::tempdir().unwrap();
    let database = source_library(root.path(), false);
    let sources = root.path().join("sources");
    for index in 0..65 {
        let directory = sources.join(format!("candidate-{index:03}"));
        fs::create_dir(&directory).unwrap();
        silent_wav(&directory.join("collision.wav"));
    }
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    let report = ScanService::new(provider.clone())
        .scan(
            &sources,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(report.added, 65);
    assert_eq!(report.errors, 0);
    drop(provider);
    let table = import_file(
        root.path(),
        "many-candidates.csv",
        "Filename,Description\ncollision.wav,candidatepagedmarker\n",
    );
    let data = root.path().join("client");
    let (cache, state, chosen) = with_server(root.path(), "all", move |url| {
        let client = RemoteClient::new(&url, "").unwrap();
        migrate(&client);
        let identity = client.library().unwrap().library_id;
        let client = client.with_library_id(&identity).unwrap();
        let preview = client
            .vendor_import_preview_files(&[table], &VendorImportOptions::default())
            .unwrap();
        assert_eq!(preview.rows, 1);
        assert_eq!(preview.ambiguous, 1);
        assert!(!preview.can_commit);
        let row = &preview.samples[0];
        assert_eq!(row.candidate_count, 65);
        assert!(row.candidates_truncated);
        assert_eq!(row.candidates.len(), 32);
        assert!(row.matched_id.is_none());
        let mut candidates = Vec::new();
        let mut offset = 0;
        loop {
            let page = client
                .vendor_import_candidates(&preview.import_id, &row.row_id, offset, 32)
                .unwrap();
            assert_eq!(page.offset, offset);
            assert_eq!(page.total, 65);
            assert!(!page.candidates.is_empty());
            offset += page.candidates.len() as u64;
            candidates.extend(page.candidates);
            if offset == page.total {
                break;
            }
        }
        let ids = candidates
            .iter()
            .map(|candidate| candidate.id)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), 65);
        let chosen = candidates
            .iter()
            .find(|candidate| {
                !row.candidates
                    .iter()
                    .any(|inline| inline.id == candidate.id)
            })
            .unwrap()
            .id;
        let mut options = preview.options.clone();
        options.confirmations.insert(row.row_id.clone(), chosen);
        let mapped = client
            .vendor_import_mapping(&preview.import_id, &options)
            .unwrap();
        assert!(mapped.can_commit);
        assert_eq!(mapped.matched, 1);
        let receipt = client
            .vendor_import_commit(&preview.import_id, "off-page-confirmation")
            .unwrap();
        assert_eq!(receipt.written, 1);
        let found = server_search(&url, "candidatepagedmarker");
        assert_eq!(found.total_count, 1);
        assert_eq!(found.results[0].id, chosen);
        let cache = RemoteCache::new(&data);
        let synced =
            sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
        (cache, synced.state, chosen)
    })
    .await;
    let offline = cached(&cache, &state);
    assert_words(&offline, "candidatepagedmarker", 1);
    assert_eq!(
        offline
            .search(&SearchRequest {
                query: "candidatepagedmarker".into(),
                scope: SearchScope::Main,
                ..Default::default()
            })
            .unwrap()
            .results[0]
            .id,
        chosen
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vendor_http_requires_manage_permission_even_for_existing_readable_library() {
    let root = tempfile::tempdir().unwrap();
    source_library(root.path(), false);
    with_server(root.path(), "off", |url| {
        let client = RemoteClient::new(&url, "").unwrap();
        let identity = client.library().unwrap().library_id;
        let (status, body) = http(
            &url,
            "GET",
            &format!("{VENDOR_IMPORT_API}/"),
            Some(&identity),
            &[],
            &[],
        );
        assert_eq!(status, 403, "{}", String::from_utf8_lossy(&body));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["error_code"],
            "MANAGE_FORBIDDEN"
        );
        assert_eq!(client.library().unwrap().schema_version, 8);
    })
    .await;
}
