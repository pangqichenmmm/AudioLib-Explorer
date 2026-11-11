// server/audiolib-server/tests/scan_vendor_metadata.rs
//! Same-directory table scanning through real HTTP, snapshot sync and offline readers.
use audiolib_core::{
    DataProvider, MetadataPatch, ScanState, SearchRequest, SearchScope,
    compatibility::{
        ACCEPT_FORMATS_HEADER, CLIENT_COMPATIBILITY_HEADER, PLAN_B_FORMAT, PLAN_B_V9_FORMAT,
        compatibility_manifest,
    },
};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use audiolib_remote::{
    CacheState, RemoteCache, RemoteClient, RemoteProvider, RemoteWriter, ScanMode, ScanRequest,
    ServerScan, sync_library,
};
use audiolib_server::{AppState, Security, ServeOptions, ServerConfig, router, serve};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant, SystemTime},
};

const EMBEDDED: &str = "embeddedscanretained";
const MANUAL: &str = "manualvendorretained";
const FIRST: &str = "autoscantableinitial";
const SECOND: &str = "autoscantablereplacementvalue";
const OUTSIDE: &str = "mustnotreachanotherdirectory";
const USER_NOTE: &str = "userscannotationretained";
const USER_TAG: &str = "userscantagretained";

/// Generate actual RIFF/BWF audio; every Metadata assertion starts with a real scan.
fn wav(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let samples = [0_u8; 256];
    let mut bext = [0_u8; 602];
    bext[..EMBEDDED.len()].copy_from_slice(EMBEDDED.as_bytes());
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(36_u32 + 8 + bext.len() as u32 + samples.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&8_000_u32.to_le_bytes());
    bytes.extend_from_slice(&16_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"bext");
    bytes.extend_from_slice(&(bext.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&bext);
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&samples);
    fs::write(path, bytes).unwrap();
}

#[derive(Clone)]
struct AudioBefore {
    path: PathBuf,
    bytes: Vec<u8>,
    modified: SystemTime,
}

fn audio_before(paths: &[PathBuf]) -> Vec<AudioBefore> {
    paths
        .iter()
        .map(|path| AudioBefore {
            path: path.clone(),
            bytes: fs::read(path).unwrap(),
            modified: fs::metadata(path).unwrap().modified().unwrap(),
        })
        .collect()
}

fn assert_audio_unchanged(before: &[AudioBefore]) {
    for original in before {
        assert_eq!(fs::read(&original.path).unwrap(), original.bytes);
        assert_eq!(
            fs::metadata(&original.path).unwrap().modified().unwrap(),
            original.modified,
            "table-only scans must not rewrite the WAV header"
        );
    }
}

fn database_files(path: &Path) -> [Option<Vec<u8>>; 3] {
    ["", "-wal", "-shm"].map(|suffix| {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        fs::read(PathBuf::from(name)).ok()
    })
}

fn working_library(root: &Path) -> PathBuf {
    let sounds = root.join("working/sounds");
    wav(&sounds.join("working.wav"));
    let database = root.join("working/library.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    let progress = ScanService::new(provider.clone())
        .scan(
            &sounds,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(progress.added, 1);
    let id = provider.search(&SearchRequest::default()).unwrap().results[0].id;
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                notes: Some("localworkinglibrarymuststay".into()),
                ..Default::default()
            },
        )
        .unwrap();
    drop(provider);
    database
}

fn query_ids(provider: &dyn DataProvider, query: &str) -> Vec<i64> {
    provider
        .search(&SearchRequest {
            query: query.into(),
            scope: SearchScope::Main,
            ..Default::default()
        })
        .unwrap()
        .results
        .iter()
        .map(|file| file.id)
        .collect()
}

fn assert_retained(provider: &dyn DataProvider, door: i64, automatic: &str) {
    for query in [MANUAL, automatic, USER_NOTE, USER_TAG] {
        assert_eq!(query_ids(provider, query), [door], "{query}");
    }
    assert!(query_ids(provider, OUTSIDE).is_empty());
    let file = provider.get_audio(door).unwrap();
    assert_eq!(file.bext_description, EMBEDDED);
    assert_eq!(file.tags, USER_TAG);
    assert_eq!(file.notes, USER_NOTE);
}

fn scan(client: &RemoteClient, mode: ScanMode) -> ServerScan {
    let mut report = client
        .start_scan(&ScanRequest {
            root: "Owned".into(),
            subpath: None,
            mode,
            workers: Some(1),
        })
        .unwrap();
    let id = report.progress.session_id;
    assert!(
        id > 0,
        "the real scan route must return its session: {report:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while report.is_active() {
        assert!(Instant::now() < deadline, "scan timed out: {report:?}");
        std::thread::sleep(Duration::from_millis(10));
        report = client.scan_status(id).unwrap();
    }
    assert_eq!(report.progress.state, ScanState::Completed, "{report:?}");
    assert_eq!(report.progress.errors, 0, "{report:?}");
    report
}

fn migrate(client: &RemoteClient) {
    let mut report = client.migration_action("start", true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while report.state != "done" {
        assert!(report.error_code.is_none(), "{report:?}");
        assert!(Instant::now() < deadline, "migration timed out: {report:?}");
        std::thread::sleep(Duration::from_millis(10));
        report = client.migration_status().unwrap();
    }
    assert!(report.error_code.is_none(), "{report:?}");
    assert_eq!(client.library().unwrap().schema_version, 9);
}

fn remote_provider(
    client: &RemoteClient,
    cache: &RemoteCache,
    state: &CacheState,
) -> RemoteProvider {
    let prepared = cache.prepare_cached_state(state).unwrap();
    let snapshot =
        Arc::new(SqliteProvider::open_read_only(&cache.database_path(&prepared)).unwrap());
    RemoteProvider::new(
        snapshot,
        RemoteWriter::new(client.clone(), cache.clone(), &state.library_id),
        false,
        None,
    )
}

/// A real listener is always stopped, even if the blocking client assertions panic.
async fn with_server<T: Send + 'static>(
    root: &Path,
    work: impl FnOnce(String) -> T + Send + 'static,
) -> T {
    let config = ServerConfig::from_toml_str(
        "bind='127.0.0.1:0'\nlibrary_db='library.db'\nsnapshot_dir='snapshots'\n\
         auth_mode='off'\nmanage_mode='all'\nlist_waveform_workers=1\n\
         [[roots]]\nname='Owned'\nserver_path='sources'\n",
        root,
    )
    .unwrap();
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

/// Negotiate exactly the schema-8 snapshot an older reader can consume over HTTP.
fn legacy_snapshot(url: &str, library: &str) -> Vec<u8> {
    let address = url.strip_prefix("http://127.0.0.1:").unwrap();
    let mut socket = TcpStream::connect(format!("127.0.0.1:{address}")).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut catalog = compatibility_manifest();
    catalog.application_version = "0.8.7".into();
    catalog.catalog_revision = 1;
    catalog
        .snapshot_read_formats
        .retain(|id| id != PLAN_B_V9_FORMAT);
    let manifest = serde_json::to_string(&catalog).unwrap();
    write!(socket, "GET /api/v1/library/snapshot HTTP/1.1\r\nHost: 127.0.0.1:{address}\r\nConnection: close\r\n{CLIENT_COMPATIBILITY_HEADER}: {manifest}\r\n{ACCEPT_FORMATS_HEADER}: {PLAN_B_FORMAT}\r\nx-audiolib-snapshot-schema-max: 8\r\nx-audiolib-search-storage-max: 1\r\nx-audiolib-tokenization-max: 3\r\nx-audiolib-library-id: {library}\r\n\r\n").unwrap();
    let mut response = Vec::new();
    socket
        .take(8 * 1024 * 1024)
        .read_to_end(&mut response)
        .unwrap();
    let end = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap();
    let headers = String::from_utf8_lossy(&response[..end]);
    assert!(
        headers.lines().next().unwrap().contains(" 200 "),
        "{headers}"
    );
    assert!(
        headers.lines().any(|line| line
            .eq_ignore_ascii_case(&format!("x-audiolib-snapshot-format: {PLAN_B_FORMAT}"))),
        "{headers}"
    );
    let body = &response[end + 4..];
    if !headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked"))
    {
        return body.to_vec();
    }
    let mut body = body;
    let mut decoded = Vec::new();
    loop {
        let end = body.windows(2).position(|bytes| bytes == b"\r\n").unwrap();
        let count = usize::from_str_radix(
            std::str::from_utf8(&body[..end])
                .unwrap()
                .split(';')
                .next()
                .unwrap(),
            16,
        )
        .unwrap();
        if count == 0 {
            return decoded;
        }
        body = &body[end + 2..];
        decoded.extend_from_slice(&body[..count]);
        body = &body[count + 2..];
    }
}

struct Offline {
    cache: RemoteCache,
    state: CacheState,
    door: i64,
    url: String,
}

async fn round_trip(upgrade: bool) {
    let root = tempfile::tempdir().unwrap();
    let door = root.path().join("sources/Target/door.wav");
    let decoy = root.path().join("sources/Other/decoy.wav");
    wav(&door);
    wav(&decoy);
    let audio = audio_before(&[door.clone(), decoy]);
    let live_audio = audio.clone();
    // The library is empty until the real POST /scans route processes these files.
    drop(SqliteProvider::open(&root.path().join("library.db")).unwrap());
    let working = working_library(root.path());
    let working_before = database_files(&working);
    let working_path = working.clone();
    let data = root.path().join("client/cache");
    let manual = root.path().join("manual-upload.csv");
    fs::write(
        &manual,
        format!("Filename,Description\ndoor.wav,{MANUAL}\n"),
    )
    .unwrap();
    let automatic = door.parent().unwrap().join("catalog.csv");
    let unmatched = door.parent().unwrap().join("unmatched.csv");
    let legacy_path = root.path().join("old-client-schema8.db");
    let expected_schema = if upgrade { 9 } else { 8 };
    let offline = with_server(root.path(), move |url| {
        let client = RemoteClient::new(&url, "").unwrap();
        let first = scan(&client, ScanMode::Full);
        assert_eq!(first.progress.added, 2);
        let cache = RemoteCache::new(&data);
        let cancel = AtomicBool::new(false);
        let initial = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
        assert_eq!(initial.state.schema_version, 8);
        assert_ne!(cache.database_path(&initial.state), working_path);
        let provider = remote_provider(&client, &cache, &initial.state);
        let door = provider
            .search(&SearchRequest::default())
            .unwrap()
            .results
            .into_iter()
            .find(|file| file.filename == "door.wav")
            .unwrap()
            .id;
        drop(provider);
        RemoteWriter::new(client.clone(), cache.clone(), &initial.state.library_id)
            .update_metadata(
                door,
                &MetadataPatch {
                    tags: Some(USER_TAG.into()),
                    notes: Some(USER_NOTE.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let preview = client.vendor_import_preview(&manual).unwrap();
        assert_eq!(preview.matched, 1, "{preview:?}");
        assert!(preview.can_commit, "{preview:?}");
        assert_eq!(
            client
                .vendor_import_commit(&preview.import_id, "manual-source-before-automatic-scan")
                .unwrap()
                .written,
            1
        );
        if upgrade {
            migrate(&client);
        }
        let before = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
        assert_eq!(before.state.schema_version, expected_schema);
        let before_revision = before.state.revision;
        fs::write(
            &unmatched,
            format!("Filename,Description\ndecoy.wav,{OUTSIDE}\n"),
        )
        .unwrap();
        fs::write(
            &automatic,
            format!("Filename,Description\ndoor.wav,{FIRST}\n"),
        )
        .unwrap();
        let added = scan(&client, ScanMode::Incremental);
        assert_eq!(added.progress.added, 0);
        assert_eq!(added.progress.updated, 0, "only a table changed: {added:?}");
        assert_eq!(added.progress.unchanged, 2);
        assert_eq!(added.progress.vendor_metadata.tables, 2, "{added:?}");
        assert_eq!(added.progress.vendor_metadata.rows, 2, "{added:?}");
        assert_eq!(added.progress.vendor_metadata.matched, 1, "{added:?}");
        assert_eq!(added.progress.vendor_metadata.updated, 1, "{added:?}");
        assert_eq!(added.progress.vendor_metadata.unmatched, 1, "{added:?}");
        assert_eq!(added.progress.vendor_metadata.ambiguous, 0, "{added:?}");
        assert!(added.needs_sync(before_revision), "{added:?}");
        let library = client.library().unwrap();
        assert_eq!(library.schema_version, expected_schema);
        assert!(library.revision > before_revision);
        assert_eq!(added.revision, Some(library.revision));
        let first_sync = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
        assert!(first_sync.downloaded);
        assert_eq!(first_sync.state.revision, library.revision);
        let first_reader = remote_provider(&client, &cache, &first_sync.state);
        assert_retained(&first_reader, door, FIRST);
        assert_audio_unchanged(&live_audio);

        // A second scan must observe a table-only edit without touching audio headers.
        fs::write(
            &automatic,
            format!("Filename,Description\ndoor.wav,{SECOND}\n"),
        )
        .unwrap();
        let changed = scan(&client, ScanMode::Incremental);
        assert_eq!(changed.progress.added, 0);
        assert_eq!(changed.progress.updated, 0);
        assert_eq!(changed.progress.unchanged, 2);
        assert_eq!(changed.progress.vendor_metadata.tables, 2, "{changed:?}");
        assert_eq!(changed.progress.vendor_metadata.rows, 2, "{changed:?}");
        assert_eq!(changed.progress.vendor_metadata.matched, 1, "{changed:?}");
        assert_eq!(changed.progress.vendor_metadata.updated, 1, "{changed:?}");
        assert_eq!(changed.progress.vendor_metadata.unmatched, 1, "{changed:?}");
        assert_eq!(changed.progress.vendor_metadata.ambiguous, 0, "{changed:?}");
        assert!(changed.needs_sync(first_sync.state.revision), "{changed:?}");
        let second_sync = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
        assert!(second_sync.downloaded);
        assert!(second_sync.state.revision > first_sync.state.revision);
        assert_eq!(changed.revision, Some(second_sync.state.revision));
        assert_eq!(second_sync.state.library_id, initial.state.library_id);
        assert_eq!(second_sync.state.schema_version, expected_schema);
        // Published readers keep their own generation until the UI installs a new provider.
        assert_retained(&first_reader, door, FIRST);
        assert!(query_ids(&first_reader, SECOND).is_empty());
        drop(first_reader);
        let current = remote_provider(&client, &cache, &second_sync.state);
        assert_retained(&current, door, SECOND);
        assert!(query_ids(&current, FIRST).is_empty());
        drop(current);

        let unchanged = scan(&client, ScanMode::Incremental);
        assert_eq!(
            unchanged.progress.vendor_metadata.updated, 0,
            "{unchanged:?}"
        );
        assert_eq!(unchanged.revision, Some(second_sync.state.revision));
        assert_eq!(
            client.library().unwrap().revision,
            second_sync.state.revision
        );
        let reused = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
        assert!(
            !reused.downloaded,
            "an unchanged table must not invalidate a snapshot"
        );

        let bytes = legacy_snapshot(&url, &second_sync.state.library_id);
        fs::write(&legacy_path, bytes).unwrap();
        let legacy = SqliteProvider::open_read_only(&legacy_path).unwrap();
        assert_eq!(legacy.library_info().unwrap().schema_version, 8);
        assert_retained(&legacy, door, SECOND);
        assert!(query_ids(&legacy, FIRST).is_empty());
        assert_audio_unchanged(&live_audio);
        Offline {
            cache,
            state: second_sync.state,
            door,
            url,
        }
    })
    .await;

    // The listener and AppState are now shut down: these are actual offline reads.
    let prepared = offline.cache.prepare_cached_state(&offline.state).unwrap();
    let cached_path = offline.cache.database_path(&prepared);
    let cached_before = fs::read(&cached_path).unwrap();
    let snapshot = Arc::new(SqliteProvider::open_read_only(&cached_path).unwrap());
    assert!(
        snapshot
            .update_metadata(
                offline.door,
                &MetadataPatch {
                    notes: Some("must-not-write-snapshot".into()),
                    ..Default::default()
                }
            )
            .is_err()
    );
    let provider = RemoteProvider::new(
        snapshot,
        RemoteWriter::new(
            RemoteClient::new(&offline.url, "").unwrap(),
            offline.cache,
            &offline.state.library_id,
        ),
        false,
        None,
    );
    assert_retained(&provider, offline.door, SECOND);
    assert!(query_ids(&provider, FIRST).is_empty());
    drop(provider);
    assert_eq!(fs::read(&cached_path).unwrap(), cached_before);
    assert_eq!(
        database_files(&working),
        working_before,
        "server mode must not modify the local working library or sidecars"
    );
    let local = SqliteProvider::open_read_only(&working).unwrap();
    assert_eq!(query_ids(&local, "localworkinglibrarymuststay").len(), 1);
    assert!(query_ids(&local, SECOND).is_empty());
    assert_audio_unchanged(&audio);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn schema8_table_only_scans_publish_new_metadata_to_http_snapshots_and_offline_clients() {
    round_trip(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrated_schema9_table_only_scans_preserve_manual_sources_and_legacy_snapshot_search() {
    round_trip(true).await;
}
