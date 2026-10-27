// app/audiolib-desktop/src/metadata_recovery_tests.rs
//! Real rejected server writes, owned snapshots, durable recovery and close retries.
use super::*;
use audiolib_core::config::DataSourceMode;
use audiolib_remote::{CacheState, RemoteCache, RemoteSecrets, save_secrets};
use std::io::{BufRead, BufReader};

const ROOT_ENV: &str = "AUDIOLIB_METADATA_RECOVERY_TEST_ROOT";

fn isolated(child: &str) {
    let root = std::env::temp_dir().join(format!(
        "audiolib-metadata-recovery-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("controller::metadata_recovery::tests::{child}"),
            "--ignored",
            "--test-threads=1",
        ])
        .env(ROOT_ENV, &root)
        .output()
        .unwrap();
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap();
    }
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{child}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Fixture {
    root: PathBuf,
    provider: Arc<SqliteProvider>,
    engine: audiolib_audio::AudioEngine,
    ui: AppWindow,
    controller: Arc<Controller>,
    file: AudioFile,
    replacement: CacheState,
    requests: Arc<Mutex<Vec<String>>>,
}

fn wav() -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend(b"RIFF");
    bytes.extend(38u32.to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(48_000u32.to_le_bytes());
    bytes.extend(96_000u32.to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(2u32.to_le_bytes());
    bytes.extend(0i16.to_le_bytes());
    bytes
}

fn fixture(replaced: bool) -> Fixture {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = PathBuf::from(std::env::var_os(ROOT_ENV).unwrap());
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    let preferences = root.join("preferences");
    let cache = RemoteCache::new(&preferences);
    let old = CacheState {
        library_id: "original-library".into(),
        database_file: "library-r1-1.db".into(),
        revision: 1,
        ..Default::default()
    };
    let replacement = CacheState {
        library_id: "replacement-library".into(),
        database_file: "library-r1-2.db".into(),
        revision: 1,
        ..Default::default()
    };
    for (name, state) in [("original", &old), ("replacement", &replacement)] {
        let sounds = root.join(name);
        std::fs::create_dir_all(&sounds).unwrap();
        for file in ["draft.wav", "favorite.wav"] {
            std::fs::write(sounds.join(file), wav()).unwrap();
        }
        let server = Arc::new(SqliteProvider::open(&root.join(format!("{name}.db"))).unwrap());
        ScanService::new(server.clone())
            .scan(
                &sounds,
                &ScanOptions::default(),
                &ScanCancellation::default(),
                |_| {},
            )
            .unwrap();
        for file in server.search(&SearchRequest::default()).unwrap().results {
            server
                .update_metadata(
                    file.id,
                    &MetadataPatch {
                        notes: Some(format!("{name} baseline")),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        let path = cache.database_path(state);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        server.backup_library(&path).unwrap();
    }
    cache.save_state(&old).unwrap();
    let provider = Arc::new(SqliteProvider::open_read_only(&cache.database_path(&old)).unwrap());
    let file = provider
        .search(&SearchRequest::default())
        .unwrap()
        .results
        .into_iter()
        .find(|file| file.filename == "draft.wav")
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let received = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                head.push_str(&line);
            }
            received.lock().unwrap().push(head);
            let (status, body) = if replaced {
                (
                    "200 OK",
                    r#"{"library_id":"replacement-library","revision":1,"schema_version":5,"identity_bound_writes":true}"#,
                )
            } else {
                (
                    "503 Service Unavailable",
                    r#"{"error":{"code":"UNAVAILABLE","message":"temporarily unavailable"}}"#,
                )
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let mut config = AppConfig::load_in(preferences.clone()).unwrap();
    config.language = "en".into();
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.server_url = url;
    config.data_source.poll_interval_secs = 0;
    config.data_source.auto_sync = false;
    save_secrets(
        &preferences,
        &RemoteSecrets {
            token: "synthetic-private-recovery-token".into(),
            webdav_password: "synthetic-private-recovery-password".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider.clone(), engine.handle(), config, playback);
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(old);
    *controller.selected.lock().unwrap() = Some(file.clone());
    present_file(&ui, &file);
    ui.set_edit_notes("keep my original-library annotations".into());
    ui.set_metadata_dirty(true);
    controller.save_metadata(&ui);
    let sequence = controller.metadata.lock().unwrap().sequence;
    assert!(controller.wait_metadata(sequence, true).is_err());
    assert_eq!(controller.remote_recovery_snapshot().is_some(), replaced);
    Fixture {
        root,
        provider,
        engine,
        ui,
        controller,
        file,
        replacement,
        requests,
    }
}

fn run_until(ui: &AppWindow, ready: impl Fn() -> bool + 'static) {
    let started = Instant::now();
    let quit_requested = std::cell::Cell::new(false);
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(10),
        move || {
            if quit_requested.get() {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "Metadata recovery timed out"
            );
            if ready() {
                // The test backend updates timers before consuming its queued Quit.
                // Request only once so a second Quit cannot escape into the next loop.
                quit_requested.set(true);
                slint::quit_event_loop().unwrap();
            }
        },
    );
    ui.show().unwrap();
    slint::run_event_loop().unwrap();
    timer.stop();
}

fn drain_operation(fixture: &Fixture) {
    let c = fixture.controller.clone();
    run_until(&fixture.ui, move || {
        !c.library_busy.load(Ordering::Acquire) && !c.source_changing.load(Ordering::Acquire)
    });
}

fn artifact(fixture: &Fixture) -> (PathBuf, serde_json::Value) {
    let path = fixture
        .controller
        .metadata_recovery_notice
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    assert!(path.starts_with(recovery_directory(&fixture.root.join("preferences"))));
    let bytes = std::fs::read(&path).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(!text.contains("synthetic-private-recovery-token"));
    assert!(!text.contains("synthetic-private-recovery-password"));
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["library_id"], "original-library");
    assert_eq!(value["snapshot_file"], "library-r1-1.db");
    assert_eq!(value["format"], FORMAT);
    let draft = value["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["record_id"] == fixture.file.id)
        .unwrap();
    assert_eq!(draft["file_path"], fixture.file.file_path);
    assert_eq!(draft["filename"], fixture.file.filename);
    assert_eq!(
        draft["patch"]["notes"],
        "keep my original-library annotations"
    );
    (path, value)
}

fn assert_no_mutations(fixture: &Fixture) {
    assert!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|head| head.starts_with("GET "))
    );
    assert_eq!(
        fixture.provider.get_audio(fixture.file.id).unwrap().notes,
        "original baseline"
    );
    assert_eq!(std::fs::read(&fixture.file.file_path).unwrap(), wav());
}

#[test]
fn replaced_library_drafts_survive_failed_open_then_snapshot_handoff() {
    isolated("child_snapshot_handoff");
}

#[test]
#[ignore = "isolated identity-rejected save and snapshot handoff helper"]
fn child_snapshot_handoff() {
    let fixture = fixture(true);
    let favorite = fixture
        .provider
        .search(&SearchRequest::default())
        .unwrap()
        .results
        .into_iter()
        .find(|file| file.filename == "favorite.wav")
        .unwrap();
    // Model a quick action accepted before the replacement was known. New UI
    // actions are now refused once the original IDs are known to be invalid.
    fixture
        .controller
        .metadata
        .lock()
        .unwrap()
        .enqueue(favorite.id, MetadataWrite::QuickAction(5));
    fixture.controller.metadata_wake.notify_all();
    let sequence = fixture.controller.metadata.lock().unwrap().sequence;
    fixture.controller.wait_metadata(sequence, false).unwrap();
    fixture
        .controller
        .run_library_operation(LibraryOperation::DataSource(
            remote_mode::DataSourceSwitch::Server {
                database: fixture.root.join("missing-snapshot.db"),
                state: Some(Box::new(fixture.replacement.clone())),
            },
        ));
    drain_operation(&fixture);
    assert!(fixture.controller.remote_read_only());
    assert!(
        fixture
            .controller
            .metadata
            .lock()
            .unwrap()
            .drafts
            .contains_key(&fixture.file.id)
    );
    assert_eq!(
        fixture.ui.get_edit_notes(),
        "keep my original-library annotations"
    );
    assert!(fixture.ui.get_metadata_dirty());
    let (path, value) = artifact(&fixture);
    // The identity rejection also requests a background sync. Its own error can
    // replace the live status, but the handoff report must retain the open error.
    fixture.ui.invoke_data_source_sync_requested();
    let c = fixture.controller.clone();
    let weak = fixture.ui.as_weak();
    run_until(&fixture.ui, move || {
        !c.remote_busy.load(Ordering::Acquire) && !weak.upgrade().unwrap().get_ds_syncing()
    });
    let report = fixture.ui.get_management_report();
    assert!(
        report.contains("unable to open database file"),
        "handoff report={report}; feedback={}; live sync message={}",
        fixture.ui.get_library_feedback(),
        fixture.ui.get_ds_message(),
    );
    assert!(
        report.contains(path.to_string_lossy().as_ref()),
        "handoff report lost recovery location: {report}",
    );
    assert!(
        fixture
            .ui
            .get_ds_message()
            .contains(path.to_string_lossy().as_ref()),
        "sync completion lost recovery location: {}",
        fixture.ui.get_ds_message(),
    );
    let favorite = value["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["record_id"] == favorite.id)
        .unwrap();
    assert_eq!(favorite["quick_action"], 5);
    assert_eq!(favorite["patch"]["favorite"], true);
    let cache = RemoteCache::new(&fixture.root.join("preferences"));
    cache.save_state(&fixture.replacement).unwrap();
    let target = cache.database_path(&fixture.replacement);
    fixture
        .controller
        .run_library_operation(LibraryOperation::DataSource(
            remote_mode::DataSourceSwitch::Server {
                database: target.clone(),
                state: Some(Box::new(fixture.replacement.clone())),
            },
        ));
    drain_operation(&fixture);
    assert_eq!(fixture.controller.provider().database_path(), target);
    assert!(
        fixture
            .controller
            .metadata
            .lock()
            .unwrap()
            .drafts
            .is_empty()
    );
    assert!(fixture.controller.require_remote_write_identity().is_ok());
    assert_eq!(
        fixture
            .controller
            .provider()
            .get_audio(fixture.file.id)
            .unwrap()
            .notes,
        "replacement baseline"
    );
    assert_eq!(
        artifact(&fixture).0,
        path,
        "same drained drafts reuse their durable file"
    );
    assert!(
        path.is_file(),
        "replacement cache pruning must preserve recovery"
    );
    assert_no_mutations(&fixture);
    fixture.controller.shutdown();
    fixture.ui.hide().unwrap();
}

#[test]
fn replaced_library_drafts_allow_the_real_local_source_switch() {
    isolated("child_local_switch");
}

#[test]
#[ignore = "isolated source connection and provider handoff recovery helper"]
fn child_local_switch() {
    let fixture = fixture(true);
    let local_path = fixture
        .controller
        .config
        .lock()
        .unwrap()
        .paths
        .database
        .clone();
    drop(SqliteProvider::open(&local_path).unwrap());
    fixture.ui.invoke_sm_source_selected(0);
    fixture.ui.invoke_sm_switch_requested();
    drain_operation(&fixture);
    assert!(!fixture.controller.remote_read_only());
    // Writable opens freeze the leased canonical destination. Windows may expose
    // its verbatim prefix even when the configured path uses ordinary spelling.
    assert_eq!(
        fixture
            .controller
            .provider()
            .database_path()
            .canonicalize()
            .unwrap(),
        local_path.canonicalize().unwrap(),
        "the real local-source switch must open the configured local library"
    );
    assert!(
        fixture
            .controller
            .metadata
            .lock()
            .unwrap()
            .drafts
            .is_empty()
    );
    assert!(artifact(&fixture).0.is_file());
    assert_no_mutations(&fixture);
    fixture.controller.shutdown();
    fixture.ui.hide().unwrap();
}

#[test]
fn recovery_write_failure_retains_drafts_and_close_retry_is_discoverable() {
    isolated("child_close_retry");
}

#[test]
#[ignore = "isolated durable-write failure, close retry and restart discovery helper"]
fn child_close_retry() {
    let fixture = fixture(true);
    let blocked = fixture.root.join("preferences/recovery");
    std::fs::write(&blocked, "owned test obstacle").unwrap();
    fixture.controller.request_close(&fixture.ui);
    let c = fixture.controller.clone();
    run_until(&fixture.ui, move || !c.closing.load(Ordering::Acquire));
    assert!(fixture.ui.window().is_visible());
    assert!(fixture.ui.get_metadata_dirty());
    let status = fixture.ui.get_status().to_string();
    assert!(
        status.contains("original drafts were retained"),
        "unexpected status after rejected close: {status:?}"
    );
    assert!(
        fixture
            .controller
            .metadata
            .lock()
            .unwrap()
            .drafts
            .contains_key(&fixture.file.id)
    );
    assert!(fixture.controller.remote_provider().is_err());
    assert!(
        fixture
            .controller
            .metadata_recovery_notice
            .lock()
            .unwrap()
            .is_none()
    );
    std::fs::remove_file(&blocked).unwrap();
    let timer = slint::Timer::default();
    let started = Instant::now();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(20),
        move || {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "Recovered close did not finish"
            );
        },
    );
    fixture.controller.request_close(&fixture.ui);
    slint::run_event_loop().unwrap();
    timer.stop();
    assert!(!fixture.ui.window().is_visible());
    let (path, _) = artifact(&fixture);
    fixture.controller.shutdown();
    let config = fixture.controller.config.lock().unwrap().clone();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(
        &ui,
        fixture.provider.clone(),
        engine.handle(),
        config,
        playback,
    );
    let weak = ui.as_weak();
    let noticed = path.clone();
    run_until(&ui, move || {
        weak.upgrade()
            .unwrap()
            .get_ds_message()
            .contains(noticed.to_string_lossy().as_ref())
    });
    assert!(
        controller.metadata.lock().unwrap().drafts.is_empty(),
        "restart never replays old IDs"
    );
    assert_no_mutations(&fixture);
    controller.shutdown();
    ui.hide().unwrap();
    drop(engine);
    drop(fixture.engine);
}

#[test]
fn ordinary_server_save_failures_still_block_handoff_and_close() {
    isolated("child_ordinary_failure");
}

#[test]
#[ignore = "isolated same-identity save failure must not archive helper"]
fn child_ordinary_failure() {
    let fixture = fixture(false);
    let cache = RemoteCache::new(&fixture.root.join("preferences"));
    fixture
        .controller
        .run_library_operation(LibraryOperation::DataSource(
            remote_mode::DataSourceSwitch::Server {
                database: cache.database_path(&fixture.replacement),
                state: Some(Box::new(fixture.replacement.clone())),
            },
        ));
    drain_operation(&fixture);
    assert!(Arc::ptr_eq(
        &fixture.controller.provider(),
        &fixture.provider
    ));
    fixture.controller.request_close(&fixture.ui);
    let c = fixture.controller.clone();
    run_until(&fixture.ui, move || !c.closing.load(Ordering::Acquire));
    assert!(fixture.ui.window().is_visible());
    assert!(
        fixture
            .controller
            .metadata
            .lock()
            .unwrap()
            .drafts
            .contains_key(&fixture.file.id)
    );
    assert!(
        fixture
            .controller
            .metadata_recovery_notice
            .lock()
            .unwrap()
            .is_none()
    );
    assert!(!recovery_directory(&fixture.root.join("preferences")).exists());
    assert_no_mutations(&fixture);
    fixture.controller.shutdown();
    fixture.ui.hide().unwrap();
}

#[test]
fn explicit_recovery_exit_preserves_ordinary_failed_drafts_without_retrying() {
    isolated("child_explicit_recovery_exit");
}

#[test]
#[ignore = "isolated explicit recovery exit after a same-library save failure"]
fn child_explicit_recovery_exit() {
    let fixture = fixture(false);
    fixture.controller.request_close(&fixture.ui);
    let c = fixture.controller.clone();
    run_until(&fixture.ui, move || !c.closing.load(Ordering::Acquire));
    assert!(fixture.ui.get_close_recovery_available());
    let request_count = fixture.requests.lock().unwrap().len();
    let sequence = fixture.controller.metadata.lock().unwrap().sequence;
    // Recovery must preserve even an invalid editor value without resubmitting it.
    fixture.ui.set_edit_tags("x".repeat(16_385).into());
    fixture.ui.set_metadata_dirty(true);
    let obstacle = fixture.root.join("preferences/recovery");
    std::fs::write(&obstacle, "owned recovery failure obstacle").unwrap();
    fixture.ui.invoke_recover_metadata_and_close();
    let c = fixture.controller.clone();
    run_until(&fixture.ui, move || !c.closing.load(Ordering::Acquire));
    assert!(fixture.ui.window().is_visible());
    assert!(fixture.ui.get_close_recovery_available());
    assert_eq!(fixture.ui.get_edit_tags().len(), 16_385);
    assert!(
        fixture
            .controller
            .metadata_recovery_notice
            .lock()
            .unwrap()
            .is_none()
    );
    std::fs::remove_file(&obstacle).unwrap();
    fixture.ui.invoke_recover_metadata_and_close();
    let weak = fixture.ui.as_weak();
    run_until(&fixture.ui, move || {
        !weak.upgrade().unwrap().window().is_visible()
    });
    assert!(!fixture.ui.window().is_visible());
    assert_eq!(
        fixture.controller.metadata.lock().unwrap().sequence,
        sequence
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), request_count);
    let (path, payload) = artifact(&fixture);
    assert!(path.is_file());
    assert_eq!(payload["reason"], "user_requested_recovery_exit");
    assert_eq!(
        payload["records"][0]["patch"]["tags"]
            .as_str()
            .unwrap()
            .len(),
        16_385
    );
    assert_no_mutations(&fixture);
    fixture.controller.shutdown();
}

#[test]
fn restore_discards_only_current_failed_draft_and_menu_favorite_stays_a_draft() {
    isolated("child_restore_current_draft");
}

#[test]
#[ignore = "isolated explicit restore and consistent inspector/menu favorite semantics"]
fn child_restore_current_draft() {
    let fixture = fixture(false);
    let before = fixture.controller.metadata.lock().unwrap().sequence;
    fixture.controller.file_action(fixture.file.id, 5);
    assert!(fixture.ui.get_edit_favorite());
    assert!(fixture.ui.get_metadata_dirty());
    assert_eq!(fixture.controller.metadata.lock().unwrap().sequence, before);
    fixture.controller.metadata.lock().unwrap().edit(
        -9,
        MetadataPatch {
            notes: Some("another retained draft".into()),
            ..Default::default()
        },
    );
    fixture.ui.invoke_restore_metadata();
    let weak = fixture.ui.as_weak();
    run_until(&fixture.ui, move || {
        !weak.upgrade().unwrap().get_metadata_dirty()
    });
    assert_eq!(fixture.ui.get_edit_notes(), "original baseline");
    assert!(!fixture.ui.get_edit_favorite());
    let state = fixture.controller.metadata.lock().unwrap();
    assert!(!state.drafts.contains_key(&fixture.file.id));
    assert!(!state.failures.contains_key(&fixture.file.id));
    assert!(state.drafts.contains_key(&-9));
    drop(state);
    assert_no_mutations(&fixture);
    fixture.controller.shutdown();
    fixture.ui.hide().unwrap();
}

#[test]
fn recovery_preserves_owned_patches_even_when_the_record_is_already_gone() {
    isolated("child_missing_record_recovery");
}

#[test]
#[ignore = "isolated missing-record recovery without a readable Metadata baseline"]
fn child_missing_record_recovery() {
    let fixture = fixture(false);
    fixture.controller.metadata.lock().unwrap().edit(
        -99,
        MetadataPatch {
            notes: Some("deleted record draft".into()),
            ..Default::default()
        },
    );
    fixture
        .controller
        .preserve_metadata_recovery(false)
        .unwrap();
    let (_, payload) = artifact(&fixture);
    let record = payload["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["record_id"] == -99)
        .unwrap();
    assert_eq!(record["patch"]["notes"], "deleted record draft");
    assert!(record["read_error"].is_string());
    assert!(record["snapshot_metadata"].is_null());
    assert_no_mutations(&fixture);
    fixture.controller.shutdown();
    fixture.ui.hide().unwrap();
}

#[test]
fn restore_keeps_confirmed_server_metadata_while_the_snapshot_is_stale() {
    isolated("child_restore_confirmed_server_metadata");
}

#[test]
#[ignore = "isolated confirmed-server baseline, stale snapshot and saved-cache eviction"]
fn child_restore_confirmed_server_metadata() {
    let fixture = fixture(false);
    let mut confirmed = fixture.file.clone();
    confirmed.notes = "last confirmed server value".into();
    confirmed.favorite = true;
    {
        // Model the Metadata worker's successful response while RemoteWriter
        // has requested resync instead of applying it to the older snapshot.
        let mut state = fixture.controller.metadata.lock().unwrap();
        state.enqueue_draft(confirmed.id).unwrap();
        let job = state.next().unwrap();
        state.complete(&job, &Ok(confirmed.clone()));
    }
    *fixture.controller.selected.lock().unwrap() = Some(confirmed.clone());
    present_file(&fixture.ui, &confirmed);
    assert_eq!(
        fixture.provider.get_audio(confirmed.id).unwrap().notes,
        "original baseline"
    );
    for evict_saved_cache in [false, true] {
        fixture.ui.set_edit_notes("new unsaved edit".into());
        fixture.ui.set_metadata_dirty(true);
        fixture.controller.capture_metadata(&fixture.ui);
        if evict_saved_cache {
            let mut state = fixture.controller.metadata.lock().unwrap();
            for id in -((PAGE_SIZE + 1) as i64)..0 {
                state.enqueue(id, MetadataWrite::QuickAction(5));
                let job = state.next().unwrap();
                state.complete(
                    &job,
                    &Ok(AudioFile {
                        id,
                        ..Default::default()
                    }),
                );
            }
            assert!(!state.saved.contains_key(&confirmed.id));
        }
        let before = fixture.controller.metadata.lock().unwrap().sequence;
        fixture.ui.invoke_restore_metadata();
        let weak = fixture.ui.as_weak();
        run_until(&fixture.ui, move || {
            !weak.upgrade().unwrap().get_metadata_dirty()
        });
        assert_eq!(fixture.ui.get_edit_notes(), confirmed.notes);
        assert!(fixture.ui.get_edit_favorite());
        assert_eq!(
            fixture
                .controller
                .enqueue_current_metadata(&fixture.ui)
                .unwrap(),
            before
        );
        assert!(
            fixture
                .controller
                .metadata
                .lock()
                .unwrap()
                .drafts
                .is_empty()
        );
    }
    assert_no_mutations(&fixture);
    fixture.controller.shutdown();
    fixture.ui.hide().unwrap();
}
