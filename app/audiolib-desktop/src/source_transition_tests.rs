// app/audiolib-desktop/src/source_transition_tests.rs
//! Isolated source, draft, selection cancellation and remote viewport regressions.
use super::super::*;
use super::*;
use audiolib_core::config::{ActiveSource, ServerProfile};
use std::io::{Read, Write};

const ROOT_ENV: &str = "AUDIOLIB_SOURCE_TRANSITION_TEST_ROOT";

fn isolated(child: &str) {
    isolated_module("source_transition_tests", child);
}

pub(super) fn isolated_module(module: &str, child: &str) {
    let root = std::env::temp_dir().join(format!(
        "audiolib-source-transition-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("controller::remote_mode::{module}::{child}"),
            "--ignored",
            "--test-threads=1",
        ])
        .env(ROOT_ENV, &root)
        .output()
        .unwrap();
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    if root.exists() {
        remove_dir_all_retry(&root);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{child}: {stdout}\n{stderr}"
    );
}

/// Test-only cleanup that tolerates transient Windows sharing violations
/// (OS error 32) left behind by handles that are still being released.
/// Retries with growing backoff (~10 attempts, capped at ~2s per wait) and
/// never panics: a leftover temp directory must not fail a passing test.
pub(super) fn remove_dir_all_retry(path: &Path) {
    const ATTEMPTS: u32 = 10;
    for attempt in 0..ATTEMPTS {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) if error.raw_os_error() == Some(32) && attempt + 1 < ATTEMPTS => {
                let wait = (50u64 << attempt).min(2000);
                std::thread::sleep(std::time::Duration::from_millis(wait));
            }
            Err(error) => {
                eprintln!("warning: could not remove {}: {error}", path.display());
                return;
            }
        }
    }
}

pub(super) fn root() -> PathBuf {
    let root = PathBuf::from(std::env::var_os(ROOT_ENV).unwrap());
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

pub(super) fn configuration(preferences: &Path) -> AppConfig {
    std::fs::create_dir_all(preferences).unwrap();
    let config = AppConfig {
        paths: audiolib_core::config::AppPaths {
            database: preferences.join("library.db"),
            cache: preferences.join("cache"),
            logs: preferences.join("logs"),
            settings: preferences.join("settings.json"),
            legacy_database: None,
            audio_terms: None,
        },
        language: "en".into(),
        playback: serde_json::to_value(PlaybackConfig {
            volume: 0.0,
            ..Default::default()
        })
        .unwrap(),
        audition_play_on_single_click: false,
        play_history_limit: audiolib_core::config::PLAY_HISTORY_LIMIT_PER_LIBRARY,
        pregenerate_list_waveforms: false,
        hold_list_waveform_batches_after_audition: false,
        max_search_tasks: 6,
        ui_scale_percent: 0,
        theme: audiolib_core::config::DEFAULT_THEME.into(),
        ui_font_size: audiolib_core::config::DEFAULT_FONT_SIZE,
        list_font_size: audiolib_core::config::DEFAULT_FONT_SIZE,
        audio_table: Default::default(),
        spectral_peaks: Default::default(),
        window_layout: Default::default(),
        allowed_scan_roots: Vec::new(),
        data_source: Default::default(),
        data_source_fallback: None,
    };
    config.save().unwrap();
    config
}

pub(super) fn request(socket: &mut std::net::TcpStream) -> String {
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bytes = Vec::new();
    let mut chunk = [0; 2048];
    loop {
        assert!(
            Instant::now() < deadline,
            "mock HTTP request read timed out"
        );
        let count = match socket.read(&mut chunk) {
            Ok(0) => panic!("request ended before its body"),
            Ok(count) => count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => panic!("read mock HTTP request: {error}"),
        };
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..end]);
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("Content-Length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                return String::from_utf8(bytes).unwrap();
            }
        }
    }
}

pub(super) fn accept(listener: &std::net::TcpListener) -> std::net::TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                // Windows sockets inherit the listener's nonblocking mode.
                socket.set_nonblocking(false).unwrap();
                return socket;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "mock HTTP request timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("accept mock HTTP: {error}"),
        }
    }
}

pub(super) fn respond(socket: &mut std::net::TcpStream, status: &str, body: &str) {
    write!(socket, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

fn legacy_peer_preflight(listener: &std::net::TcpListener) {
    for (path, status, body) in [
        (
            "/api/v1/health",
            "200 OK",
            serde_json::json!({"status":"ok","version":audiolib_core::compatibility::compatibility_manifest().application_version}).to_string(),
        ),
        (
            "/api/v1/capabilities",
            "200 OK",
            serde_json::json!({"compatibility":audiolib_core::compatibility::compatibility_manifest()}).to_string(),
        ),
    ] {
        let mut socket = accept(listener);
        assert!(request(&mut socket).starts_with(&format!("GET {path} ")));
        respond(&mut socket, status, &body);
    }
}

pub(super) fn run_until(ui: &AppWindow, ready: impl Fn() -> bool + 'static) {
    let started = Instant::now();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(10),
        move || {
            assert!(
                started.elapsed() < Duration::from_secs(12),
                "GUI transition timed out"
            );
            if ready() {
                slint::quit_event_loop().unwrap();
            }
        },
    );
    ui.show().unwrap();
    slint::run_event_loop().unwrap();
    timer.stop();
}

fn selected(controller: &Controller, ui: &AppWindow, notes: &str) {
    let file = AudioFile {
        id: 1,
        filename: "server A sound.wav".into(),
        ..Default::default()
    };
    *controller.selected.lock().unwrap() = Some(file.clone());
    present_file(ui, &file);
    ui.set_edit_notes(notes.into());
    ui.set_metadata_dirty(true);
}

pub(super) fn indexed_sounds(root: &Path) -> (Arc<SqliteProvider>, AudioFile) {
    let sounds = root.join("sounds");
    std::fs::create_dir_all(&sounds).unwrap();
    let mut wav = Vec::new();
    wav.extend(b"RIFF");
    wav.extend(996u32.to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(48_000u32.to_le_bytes());
    wav.extend(96_000u32.to_le_bytes());
    wav.extend(2u16.to_le_bytes());
    wav.extend(16u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(960u32.to_le_bytes());
    wav.resize(1004, 0);
    for filename in ["metal.wav", "wind.wav"] {
        std::fs::write(sounds.join(filename), &wav).unwrap();
    }
    let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
    ScanService::new(provider.clone())
        .scan(
            &sounds,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let file = provider
        .search(&SearchRequest {
            query: "metal".into(),
            ..Default::default()
        })
        .unwrap()
        .results
        .remove(0);
    (provider, file)
}

#[test]
fn unchanged_sources_resume_cancelled_reads_without_replaying_completed_auditions() {
    isolated("child_unchanged_sources_resume_reads");
    isolated("child_unchanged_sources_without_output");
}

#[test]
#[ignore = "isolated Slint unchanged-source read recovery helper"]
fn child_unchanged_sources_resume_reads() {
    unchanged_sources_resume_reads(false);
}

#[test]
#[ignore = "isolated Slint unchanged-source recovery without an output device"]
fn child_unchanged_sources_without_output() {
    unchanged_sources_resume_reads(true);
}

fn unchanged_sources_resume_reads(missing_output: bool) {
    use std::cell::Cell;
    use std::rc::Rc;

    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let (provider, file) = indexed_sounds(&root);
    let mut config = configuration(&root.join("preferences"));
    config.paths.database = provider.database_path().into();
    config.data_source.profiles.push(ServerProfile {
        id: "inactive".into(),
        name: "Inactive".into(),
        server_url: "http://127.0.0.1:8790".into(),
        ..Default::default()
    });
    let playback = PlaybackConfig {
        device_id: missing_output.then(|| "audiolib-regression-unavailable-output".into()),
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider.clone(), engine.handle(), config, playback);
    controller.select_id(file.id, true);
    let original_barrier = controller
        .latest_selection
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .barrier;
    ui.invoke_sm_open_requested(0);
    ui.invoke_sm_source_selected(1);
    ui.set_ds_name("Renamed inactive".into());
    ui.set_query("metal".into());
    controller.request_search(&ui, false, true);
    let cancelled_query = controller.search_cancel.lock().unwrap().clone();
    ui.invoke_sm_save_requested();
    assert!(controller.source_changing.load(Ordering::Acquire));
    assert!(cancelled_query.load(Ordering::Acquire));
    assert_eq!(
        controller
            .latest_selection
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .barrier,
        original_barrier,
        "The original selection FIFO barrier was lost"
    );
    let stage = Rc::new(Cell::new(0));
    let next = stage.clone();
    let audio_generation = Rc::new(Cell::new(0));
    let audio_before_save = audio_generation.clone();
    let window = ui.as_weak();
    let c = controller.clone();
    let id = file.id;
    run_until(&ui, move || {
        let Some(ui) = window.upgrade() else {
            return false;
        };
        if missing_output
            && next.get() == 0
            && !ui
                .get_status()
                .contains("Selected output device is unavailable")
        {
            return false;
        }
        // Playback errors can replace the shared Ready text on a machine without
        // an output device. Observe the current search publication itself instead.
        let published = !c.source_changing.load(Ordering::Acquire)
            && !ui.get_searching()
            && ui.get_rows().row_count() == 1
            && ui.get_rows().row_data(0).unwrap().id.as_str() == id.to_string()
            && ui.get_folders().row_count() == 1
            && c.tree.lock().unwrap().matches[0].file_count == 1
            && c.search_publication
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|state| {
                    state.generation == c.search_generation.load(Ordering::Acquire)
                        && state.page == SearchPagePhase::Published
                        && state.model_published
                })
            && c.latest_selection.lock().unwrap().is_none()
            && ui.get_selected_id().as_str() == id.to_string()
            && ui.get_waveform_ready();
        if !published {
            return false;
        }
        match next.get() {
            0 => {
                // Stop after the completed audition's Play has been consumed. Saving
                // another profile must neither Load again nor reissue that Play.
                let stopped = c
                    .audio
                    .send_and_wait(AudioCommand::Stop, Duration::from_secs(4))
                    .unwrap();
                assert_eq!(stopped.state, PlaybackState::Stopped);
                audio_before_save.set(stopped.generation);
                ui.set_ds_name("Renamed again".into());
                c.request_search(&ui, false, true);
                ui.invoke_sm_save_requested();
                next.set(1);
                false
            }
            1 => {
                let status = c.audio.status();
                assert_eq!(
                    status.generation,
                    audio_before_save.get(),
                    "Completed audition was reloaded"
                );
                assert_eq!(
                    status.state,
                    PlaybackState::Stopped,
                    "Completed Play was replayed"
                );
                c.request_search(&ui, false, true);
                c.activate_data_source(ActiveSource::Local);
                next.set(2);
                false
            }
            _ => {
                let status = c.audio.status();
                assert_eq!(status.generation, audio_before_save.get());
                assert_eq!(status.state, PlaybackState::Stopped);
                true
            }
        }
    });
    assert_eq!(stage.get(), 2);
    assert!(
        Arc::ptr_eq(&provider, &controller.provider()),
        "An unchanged provider was swapped"
    );
    assert_eq!(
        controller.metadata.lock().unwrap().sequence,
        0,
        "Read recovery enqueued Metadata writes"
    );
    controller.select_id(id, true);
    assert!(controller.latest_selection.lock().unwrap().is_some());
    ui.invoke_stop_requested();
    assert!(
        controller.latest_selection.lock().unwrap().is_none(),
        "Stop left a remembered Play to resume"
    );
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
fn late_invalid_draft_terminates_selection_and_cannot_replay_on_profile_save() {
    isolated("child_late_invalid_draft_consumes_selection");
}

#[test]
fn stop_cancels_pending_local_selection_and_allows_a_fresh_load() {
    isolated("child_stop_cancels_pending_local_selection");
}

#[test]
#[ignore = "isolated Slint pending selection Stop helper"]
fn child_stop_cancels_pending_local_selection() {
    use std::cell::Cell;
    use std::rc::Rc;

    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let (provider, metal) = indexed_sounds(&root);
    let wind = provider
        .search(&SearchRequest {
            query: "wind".into(),
            ..Default::default()
        })
        .unwrap()
        .results
        .remove(0);
    let mut config = configuration(&root.join("preferences"));
    config.paths.database = provider.database_path().into();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
    *controller.selected.lock().unwrap() = Some(wind.clone());
    present_file(&ui, &wind);
    let loaded = controller
        .audio
        .send_and_wait(
            AudioCommand::Load(PathBuf::from(&wind.file_path)),
            Duration::from_secs(4),
        )
        .unwrap();
    controller.select_id(metal.id, true);
    let pending_generation = controller.selection_generation.load(Ordering::Acquire);
    let cancel = controller.remote_audio_cancel.lock().unwrap().clone();
    // No event loop has run: the pending Metadata publication must lose to Stop.
    ui.invoke_stop_requested();
    assert!(controller.selection_generation.load(Ordering::Acquire) > pending_generation);
    assert!(cancel.load(Ordering::Acquire));
    assert!(controller.latest_selection.lock().unwrap().is_none());
    assert!(controller.selection_job.lock().unwrap().is_none());
    assert!(controller.selection_audio_job.lock().unwrap().is_none());
    let stopped = controller
        .audio
        .send_and_wait(AudioCommand::Synchronize, Duration::from_secs(4))
        .unwrap();
    assert_eq!(stopped.state, PlaybackState::Stopped);
    assert_eq!(stopped.generation, loaded.generation);

    let phase = Rc::new(Cell::new(0));
    let next = phase.clone();
    let cancelled_at = Rc::new(Cell::new(Instant::now()));
    let cancelled_since = cancelled_at.clone();
    let c = controller.clone();
    let window = ui.as_weak();
    let wind_path = PathBuf::from(&wind.file_path);
    let metal_path = PathBuf::from(&metal.file_path);
    let wind_id = wind.id;
    let metal_id = metal.id;
    run_until(&ui, move || {
        let Some(ui) = window.upgrade() else {
            return false;
        };
        let status = c.audio.status();
        match next.get() {
            0 | 2 => {
                let generation = loaded.generation + u64::from(next.get() == 2);
                let path = if next.get() == 0 {
                    &wind_path
                } else {
                    &metal_path
                };
                assert_eq!(
                    status.generation, generation,
                    "Stop was followed by a stale Load"
                );
                assert_eq!(status.file_path.as_ref(), Some(path));
                assert_eq!(
                    status.state,
                    PlaybackState::Stopped,
                    "Stop was followed by a stale Play"
                );
                assert_eq!(status.play_session_id, 0);
                assert!(c.latest_selection.lock().unwrap().is_none());
                if cancelled_since.get().elapsed() < Duration::from_millis(250) {
                    return false;
                }
                c.select_id(if next.get() == 0 { metal_id } else { wind_id }, false);
                next.set(next.get() + 1);
                false
            }
            1 | 3 => {
                if c.latest_selection.lock().unwrap().is_some() || !ui.get_audio_ready() {
                    return false;
                }
                let generation = loaded.generation + if next.get() == 1 { 1 } else { 2 };
                let path = if next.get() == 1 {
                    &metal_path
                } else {
                    &wind_path
                };
                assert_eq!(status.generation, generation);
                assert_eq!(status.file_path.as_ref(), Some(path));
                assert_eq!(status.state, PlaybackState::Ready);
                assert_eq!(status.play_session_id, 0);
                if next.get() == 3 {
                    return true;
                }
                // IPC Stop shares the same cancellation and FIFO ordering contract.
                c.select_id(wind_id, true);
                let pending = c.selection_generation.load(Ordering::Acquire);
                let reply = c.ipc(serde_json::json!({"command": "stop"}));
                assert_eq!(reply["success"], true);
                assert!(c.selection_generation.load(Ordering::Acquire) > pending);
                cancelled_since.set(Instant::now());
                next.set(2);
                false
            }
            _ => unreachable!(),
        }
    });
    assert_eq!(phase.get(), 3);
    assert_eq!(controller.metadata.lock().unwrap().sequence, 0);
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
fn stop_during_acknowledged_load_prevents_its_late_play() {
    isolated("child_stop_during_acknowledged_load");
}

#[test]
fn delayed_ipc_reload_play_is_cancelled_by_stop() {
    isolated("child_delayed_ipc_reload_play_is_cancelled_by_stop");
}

#[test]
fn stop_after_selection_publication_keeps_its_reload_until_a_current_load_succeeds() {
    isolated("child_stop_after_selection_publication_keeps_reload");
}

#[test]
fn stale_remote_load_failure_cannot_replace_a_published_selection_retry_marker() {
    isolated("child_stale_remote_load_failure_keeps_published_reload");
}

#[test]
#[ignore = "isolated Slint stale remote Load failure marker helper"]
fn child_stale_remote_load_failure_keeps_published_reload() {
    published_selection_stop_fixture(true);
}

#[test]
#[ignore = "isolated Slint published selection Stop reload helper"]
fn child_stop_after_selection_publication_keeps_reload() {
    published_selection_stop_fixture(false);
}

fn published_selection_stop_fixture(fail_old_load: bool) {
    use std::cell::Cell;
    use std::rc::Rc;

    fn block_next_engine_open(
        path: &Path,
        fail_first: bool,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::SyncSender<()>,
    ) {
        let bytes = Arc::new(std::fs::read(path).unwrap());
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let release_rx = Mutex::new(release_rx);
        let first = AtomicBool::new(true);
        let opener: audiolib_audio::SourceOpener = Arc::new(move || {
            // Waveform workers may read the same owned WAV without delaying the UI.
            if std::thread::current().name() == Some("audio-control")
                && first.swap(false, Ordering::AcqRel)
            {
                entered_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(4))
                    .unwrap();
                if fail_first {
                    return Err(std::io::Error::other("owned stale A Load failure"));
                }
            }
            let source: Box<dyn audiolib_audio::ByteSource> =
                Box::new(std::io::Cursor::new(bytes.as_ref().clone()));
            Ok((source, bytes.len() as u64))
        });
        audiolib_audio::register_stream_source(path, opener);
        (entered_rx, release_tx)
    }

    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let (provider, metal) = indexed_sounds(&root);
    let wind = provider
        .search(&SearchRequest {
            query: "wind".into(),
            ..Default::default()
        })
        .unwrap()
        .results
        .remove(0);
    let metal_path = PathBuf::from(&metal.file_path);
    let wind_path = PathBuf::from(&wind.file_path);
    let mut config = configuration(&root.join("preferences"));
    config.paths.database = provider.database_path().into();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider.clone(), engine.handle(), config, playback);
    *controller.selected.lock().unwrap() = Some(wind.clone());
    present_file(&ui, &wind);
    let loaded = controller
        .audio
        .send_and_wait(
            AudioCommand::Load(wind_path.clone()),
            Duration::from_secs(4),
        )
        .unwrap();
    let (old_entered, old_release) = block_next_engine_open(&wind_path, fail_old_load);
    let (new_entered, new_release) = block_next_engine_open(&metal_path, false);
    let generation = controller
        .selection_generation
        .fetch_add(1, Ordering::AcqRel)
        + 1;
    *controller.latest_selection.lock().unwrap() = Some(SelectionJob {
        id: wind.id,
        play: fail_old_load,
        generation,
        barrier: 0,
        provider,
    });
    let cancel = Arc::new(AtomicBool::new(false));
    *controller.remote_audio_cancel.lock().unwrap() = cancel.clone();
    // Use the actual bounded worker: its A Load waits while B can be published.
    *controller.selection_audio_job.lock().unwrap() = Some(SelectionAudioJob {
        file: wind,
        play: fail_old_load,
        // Exercise the remote ACK error branch with the same owned bytes, no HTTP.
        remote: fail_old_load,
        generation,
        cancel,
    });
    controller.selection_audio_wake.notify_one();
    old_entered.recv_timeout(Duration::from_secs(4)).unwrap();
    controller.select_id(metal.id, false);
    let phase = Rc::new(Cell::new(0));
    let next = phase.clone();
    let c = controller.clone();
    let window = ui.as_weak();
    let id = metal.id;
    let starts = controller.audio.subscribe_playback_starts();
    let expected_a = wind_path.clone();
    let expected_b = metal_path.clone();
    run_until(&ui, move || {
        let Some(ui) = window.upgrade() else {
            return false;
        };
        let status = c.audio.status();
        match next.get() {
            0 => {
                if ui.get_selected_id().as_str() != id.to_string() {
                    return false;
                }
                assert!(
                    c.selection_audio_job
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|job| job.file.id == id)
                );
                assert_eq!(*c.audio_reload.lock().unwrap(), Some(id));
                ui.invoke_stop_requested();
                assert!(c.selection_audio_job.lock().unwrap().is_none());
                assert_eq!(*c.audio_reload.lock().unwrap(), Some(id));
                // An already cancelled sentinel proves the bounded worker processed
                // A's ACK/error before we inspect B's marker; it cannot issue a Load.
                *c.selection_audio_job.lock().unwrap() = Some(SelectionAudioJob {
                    file: metal.clone(),
                    play: false,
                    remote: false,
                    generation,
                    cancel: Arc::new(AtomicBool::new(true)),
                });
                c.selection_audio_wake.notify_one();
                old_release.send(()).unwrap();
                next.set(1);
                false
            }
            1 => {
                if status.state != PlaybackState::Stopped
                    || c.selection_audio_job.lock().unwrap().is_some()
                {
                    return false;
                }
                assert_eq!(status.generation, loaded.generation + 1);
                assert_eq!(status.file_path.as_ref(), Some(&expected_a));
                assert_eq!(*c.audio_reload.lock().unwrap(), Some(id));
                ui.invoke_play_requested();
                next.set(2);
                false
            }
            2 => {
                match new_entered.try_recv() {
                    Ok(()) => {}
                    Err(std::sync::mpsc::TryRecvError::Empty) => return false,
                    Err(error) => panic!("new Load opener: {error}"),
                }
                let loading = c.audio.status();
                assert_eq!(
                    loading.file_path.as_ref(),
                    Some(&expected_b),
                    "Play loaded the previous source"
                );
                assert_eq!(loading.generation, loaded.generation + 2);
                // Cancel before Load's acknowledgment: this fixture opens no device.
                ui.invoke_stop_requested();
                new_release.send(()).unwrap();
                next.set(3);
                false
            }
            3 => {
                if status.state != PlaybackState::Stopped {
                    return false;
                }
                assert_eq!(status.file_path.as_ref(), Some(&expected_b));
                assert_eq!(
                    *c.audio_reload.lock().unwrap(),
                    Some(id),
                    "Stale same-ID Load acknowledgment consumed the current marker"
                );
                c.select_id(id, false);
                next.set(4);
                false
            }
            4 => {
                if c.latest_selection.lock().unwrap().is_some() || !ui.get_audio_ready() {
                    return false;
                }
                assert_eq!(status.generation, loaded.generation + 3);
                assert_eq!(status.file_path.as_ref(), Some(&expected_b));
                assert_eq!(status.state, PlaybackState::Ready);
                assert!(c.audio_reload.lock().unwrap().is_none());
                assert!(matches!(
                    starts.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ));
                true
            }
            _ => unreachable!(),
        }
    });
    assert_eq!(phase.get(), 4);
    controller.shutdown();
    ui.hide().unwrap();
    audiolib_audio::unregister_stream_source(&wind_path);
    audiolib_audio::unregister_stream_source(&metal_path);
}

#[test]
#[ignore = "isolated Slint deferred IPC Play cancellation helper"]
fn child_delayed_ipc_reload_play_is_cancelled_by_stop() {
    use std::cell::Cell;
    use std::rc::Rc;

    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let (provider, metal) = indexed_sounds(&root);
    let wind = provider
        .search(&SearchRequest {
            query: "wind".into(),
            ..Default::default()
        })
        .unwrap()
        .results
        .remove(0);
    let mut config = configuration(&root.join("preferences"));
    config.paths.database = provider.database_path().into();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
    let loaded = controller
        .audio
        .send_and_wait(
            AudioCommand::Load(PathBuf::from(&wind.file_path)),
            Duration::from_secs(4),
        )
        .unwrap();
    // This is the state after a failed server audition: B is displayed while
    // the engine retains A. Owned local bytes keep the control regression offline.
    *controller.selected.lock().unwrap() = Some(metal.clone());
    present_file(&ui, &metal);
    controller.mark_audio_reload(metal.id);
    let before_play = controller.selection_generation.load(Ordering::Acquire);
    let reply = controller.ipc(serde_json::json!({"command": "play"}));
    assert_eq!(reply["success"], true);
    assert_eq!(
        controller.selection_generation.load(Ordering::Acquire),
        before_play
    );
    assert_eq!(*controller.audio_reload.lock().unwrap(), Some(metal.id));
    // The deferred Play is already queued, but its event-loop callback has not run.
    let reply = controller.ipc(serde_json::json!({"command": "stop"}));
    assert_eq!(reply["success"], true);
    let stopped_generation = controller.selection_generation.load(Ordering::Acquire);
    assert!(stopped_generation > before_play);
    let cancelled_at = Rc::new(Cell::new(Instant::now()));
    let cancelled_since = cancelled_at.clone();
    let phase = Rc::new(Cell::new(0));
    let next = phase.clone();
    let c = controller.clone();
    let window = ui.as_weak();
    let metal_id = metal.id;
    let metal_path = PathBuf::from(&metal.file_path);
    let wind_path = PathBuf::from(&wind.file_path);
    let starts = controller.audio.subscribe_playback_starts();
    let stopped_at = Rc::new(Cell::new(stopped_generation));
    let stopped_selection = stopped_at.clone();
    run_until(&ui, move || {
        let Some(ui) = window.upgrade() else {
            return false;
        };
        let status = c.audio.status();
        if next.get() == 0 || next.get() == 2 {
            let audio_generation = loaded.generation + if next.get() == 0 { 0 } else { 2 };
            assert_eq!(
                c.selection_generation.load(Ordering::Acquire),
                stopped_selection.get()
            );
            assert_eq!(
                status.generation, audio_generation,
                "Cancelled Play reloaded after Stop"
            );
            assert_eq!(status.file_path.as_ref(), Some(&wind_path));
            assert_eq!(status.state, PlaybackState::Stopped);
            assert_eq!(status.play_session_id, 0);
            assert!(c.latest_selection.lock().unwrap().is_none());
            assert_eq!(*c.audio_reload.lock().unwrap(), Some(metal_id));
            assert!(matches!(
                starts.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
            if cancelled_since.get().elapsed() < Duration::from_millis(250) {
                return false;
            }
            if next.get() == 0 {
                let reply = c.ipc(serde_json::json!({"command": "play"}));
                assert_eq!(reply["success"], true);
            } else {
                ui.invoke_play_requested();
            }
            next.set(next.get() + 1);
            return false;
        }
        let audio_generation = loaded.generation + if next.get() == 1 { 1 } else { 3 };
        // audio_ready is still true from the previous Load right after Play is
        // sent, so wait for the reload's own Load generation before asserting.
        if c.latest_selection.lock().unwrap().is_some()
            || !ui.get_audio_ready()
            || status.generation < audio_generation
        {
            return false;
        }
        assert_eq!(status.generation, audio_generation);
        assert_eq!(status.file_path.as_ref(), Some(&metal_path));
        assert!(
            status.error.is_none(),
            "Quiet audition failed: {:?}",
            status.error
        );
        assert!(
            c.audio_reload.lock().unwrap().is_none(),
            "Successful Load retained a reload marker"
        );
        let event = match starts.try_recv() {
            Ok(event) => event,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(error) => panic!("playback event channel: {error}"),
        };
        match event {
            AudioEvent::PlaybackStarted {
                generation,
                play_session_id,
                file_path,
            } => {
                assert_eq!(generation, audio_generation);
                assert_eq!(play_session_id, 1);
                assert_eq!(
                    file_path, metal_path,
                    "Play resumed the previous engine source"
                );
            }
            event => panic!("unexpected audition event: {event:?}"),
        }
        c.audio
            .send_and_wait(AudioCommand::Stop, Duration::from_secs(4))
            .unwrap();
        if next.get() == 3 {
            return true;
        }
        // Recreate the same failed-audition identity, now use the GUI Play button.
        c.audio
            .send_and_wait(
                AudioCommand::Load(wind_path.clone()),
                Duration::from_secs(4),
            )
            .unwrap();
        c.mark_audio_reload(metal_id);
        ui.invoke_play_requested();
        ui.invoke_stop_requested();
        c.audio
            .send_and_wait(AudioCommand::Synchronize, Duration::from_secs(4))
            .unwrap();
        assert_eq!(*c.audio_reload.lock().unwrap(), Some(metal_id));
        stopped_selection.set(c.selection_generation.load(Ordering::Acquire));
        cancelled_since.set(Instant::now());
        next.set(2);
        false
    });
    assert_eq!(phase.get(), 3);
    assert_eq!(controller.metadata.lock().unwrap().sequence, 0);
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
#[ignore = "isolated Slint acknowledged Load Stop ordering helper"]
fn child_stop_during_acknowledged_load() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let (provider, mut file) = indexed_sounds(&root);
    let bytes = Arc::new(std::fs::read(&file.file_path).unwrap());
    let path = root.join("blocked-opener.wav");
    file.file_path = path.to_string_lossy().into_owned();
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = Mutex::new(release_rx);
    let first = AtomicBool::new(true);
    let opener: audiolib_audio::SourceOpener = Arc::new(move || {
        if first.swap(false, Ordering::AcqRel) {
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(4))
                .unwrap();
        }
        let source: Box<dyn audiolib_audio::ByteSource> =
            Box::new(std::io::Cursor::new(bytes.as_ref().clone()));
        Ok((source, bytes.len() as u64))
    });
    audiolib_audio::register_stream_source(&path, opener);
    let mut config = configuration(&root.join("preferences"));
    config.paths.database = provider.database_path().into();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider.clone(), engine.handle(), config, playback);
    let generation = controller
        .selection_generation
        .fetch_add(1, Ordering::AcqRel)
        + 1;
    *controller.latest_selection.lock().unwrap() = Some(SelectionJob {
        id: file.id,
        play: true,
        generation,
        barrier: 0,
        provider,
    });
    let cancel = Arc::new(AtomicBool::new(false));
    *controller.remote_audio_cancel.lock().unwrap() = cancel.clone();
    controller.mark_audio_reload(file.id);
    let reload_id = file.id;
    let loading = controller.clone();
    let worker = std::thread::spawn(move || {
        loading.load_selection_audio(SelectionAudioJob {
            file,
            play: true,
            remote: false,
            generation,
            cancel,
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(4)).unwrap();
    let started = Instant::now();
    ui.invoke_stop_requested();
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "Stop waited for the Load acknowledgment"
    );
    release_tx.send(()).unwrap();
    worker.join().unwrap();
    let status = controller
        .audio
        .send_and_wait(AudioCommand::Synchronize, Duration::from_secs(4))
        .unwrap();
    assert_eq!(status.file_path.as_ref(), Some(&path));
    assert_eq!(status.state, PlaybackState::Stopped);
    assert_eq!(status.play_session_id, 0);
    assert!(controller.latest_selection.lock().unwrap().is_none());
    assert_eq!(
        *controller.audio_reload.lock().unwrap(),
        Some(reload_id),
        "A cancelled same-ID Load cleared the retry marker"
    );
    controller.shutdown();
    audiolib_audio::unregister_stream_source(&path);
}

#[test]
#[ignore = "isolated Slint selection validation failure helper"]
fn child_late_invalid_draft_consumes_selection() {
    use std::cell::Cell;
    use std::rc::Rc;

    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let (provider, metal) = indexed_sounds(&root);
    let wind = provider
        .search(&SearchRequest {
            query: "wind".into(),
            ..Default::default()
        })
        .unwrap()
        .results
        .remove(0);
    let mut config = configuration(&root.join("preferences"));
    config.paths.database = provider.database_path().into();
    config.data_source.profiles.push(ServerProfile {
        id: "inactive".into(),
        name: "Inactive".into(),
        server_url: "http://127.0.0.1:8790".into(),
        ..Default::default()
    });
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
    *controller.selected.lock().unwrap() = Some(wind.clone());
    present_file(&ui, &wind);
    let loaded = controller
        .audio
        .send_and_wait(
            AudioCommand::Load(PathBuf::from(&wind.file_path)),
            Duration::from_secs(4),
        )
        .unwrap();
    let audio_generation = loaded.generation;
    controller.select_id(metal.id, true);
    // The request was valid. These edits arrive before its event-loop publication.
    ui.set_edit_tags("x".repeat(16_385).into());
    ui.set_metadata_dirty(true);
    let stage = Rc::new(Cell::new(0));
    let next = stage.clone();
    let c = controller.clone();
    let window = ui.as_weak();
    let displayed_id = wind.id;
    run_until(&ui, move || {
        let Some(ui) = window.upgrade() else {
            return false;
        };
        if next.get() == 0 {
            if c.latest_selection.lock().unwrap().is_some() {
                return false;
            }
            assert!(
                c.metadata
                    .lock()
                    .unwrap()
                    .drafts
                    .get(&displayed_id)
                    .is_some_and(|draft| draft
                        .patch
                        .tags
                        .as_ref()
                        .is_some_and(|tags| tags.len() == 16_385)),
                "Late invalid draft was not captured by the failed selection"
            );
            assert_eq!(ui.get_selected_id().as_str(), displayed_id.to_string());
            assert_eq!(c.metadata.lock().unwrap().sequence, 0);
            // Correct the invalid draft and use the actual inactive-profile Save.
            ui.set_edit_tags("".into());
            ui.set_metadata_dirty(true);
            ui.invoke_sm_open_requested(0);
            ui.invoke_sm_source_selected(1);
            ui.set_ds_name("Saved after validation failure".into());
            ui.invoke_sm_save_requested();
            next.set(1);
            return false;
        }
        if c.source_changing.load(Ordering::Acquire)
            || ui.get_searching()
            || !ui.get_status().starts_with("Ready")
            || !c
                .search_publication
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|state| state.model_published)
        {
            return false;
        }
        let status = c.audio.status();
        assert_eq!(
            status.generation, audio_generation,
            "Rejected selection was reloaded"
        );
        assert_eq!(
            status.state,
            PlaybackState::Ready,
            "Rejected Play was resumed"
        );
        assert_eq!(ui.get_selected_id().as_str(), displayed_id.to_string());
        assert!(c.latest_selection.lock().unwrap().is_none());
        true
    });
    assert_eq!(stage.get(), 1);
    assert_eq!(
        controller.metadata.lock().unwrap().sequence,
        1,
        "Recovery retried or added Metadata writes"
    );
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
fn same_snapshot_source_success_resumes_the_cancelled_query_and_folder_model() {
    isolated("child_same_snapshot_resumes_reads");
}

#[test]
#[ignore = "isolated Slint same-snapshot read recovery helper"]
fn child_same_snapshot_resumes_reads() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let (provider, file) = indexed_sounds(&root);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        let request = request(&mut socket);
        assert!(request.starts_with("GET /api/v1/capabilities"), "{request}");
        respond(
            &mut socket,
            "403 Forbidden",
            r#"{"error_code":"FORBIDDEN","message":"test capabilities refused"}"#,
        );
    });
    let mut config = configuration(&root.join("preferences"));
    config.paths.database = provider.database_path().into();
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.server_url = url.clone();
    config.data_source.poll_interval_secs = 0;
    let state = CacheState {
        library_id: "unchanged-library".into(),
        database_file: "snapshot.db".into(),
        server_url: url,
        revision: 1,
        ..Default::default()
    };
    let cache = RemoteCache::new(&config.data_root());
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider.clone(), engine.handle(), config, playback);
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(state.clone());
    ui.set_query("metal".into());
    controller.request_search(&ui, false, true);
    let cancelled_query = controller.search_cancel.lock().unwrap().clone();
    let barrier = controller.begin_source_change(&ui).unwrap();
    controller.wait_metadata(barrier, true).unwrap();
    controller.complete_sync(
        &ui,
        Ok(audiolib_remote::SyncOutcome {
            state,
            downloaded: false,
            library_changed: false,
            previous: None,
            elapsed_ms: 0,
        }),
        None,
        &cache,
    );
    assert!(cancelled_query.load(Ordering::Acquire));
    assert!(!controller.source_changing.load(Ordering::Acquire));
    let window = ui.as_weak();
    let c = controller.clone();
    run_until(&ui, move || {
        let Some(ui) = window.upgrade() else {
            return false;
        };
        !ui.get_searching()
            && ui.get_status().starts_with("Ready")
            && ui.get_rows().row_count() == 1
            && ui.get_rows().row_data(0).unwrap().id.as_str() == file.id.to_string()
            && ui.get_folders().row_count() == 1
            && c.tree.lock().unwrap().matches[0].file_count == 1
            && c.search_publication
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|state| state.model_published)
    });
    server.join().unwrap();
    assert!(Arc::ptr_eq(&provider, &controller.provider()));
    assert_eq!(controller.metadata.lock().unwrap().sequence, 0);
    controller.shutdown();
    ui.hide().unwrap();
}

fn cached_library(config: &AppConfig, name: &str, server_url: &str) -> CacheState {
    let source = SqliteProvider::open(&config.data_root().join(format!("{name}.db"))).unwrap();
    let info = source.library_info().unwrap();
    let cache = RemoteCache::new(&config.data_root());
    let state = CacheState {
        library_id: format!("test-library-{name}"),
        schema_version: info.schema_version,
        revision: 1,
        database_file: "snapshot.db".into(),
        server_url: server_url.into(),
        ..Default::default()
    };
    let path = cache.database_path(&state);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    source.backup_library(&path).unwrap();
    cache.save_state(&state).unwrap();
    state
}

fn reject_cancelled_request(socket: &mut std::net::TcpStream, body: &str) {
    if let Err(error) = write!(
        socket,
        "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ) {
        assert!(
            matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotConnected
            ),
            "send response after client cancellation: {error}"
        );
    }
}

#[test]
fn gui_defers_server_cache_to_manager_and_close_drains_cache_worker() {
    isolated("child_deferred_server_cache");
}

#[test]
fn gui_rejected_new_snapshot_keeps_the_previous_cache_available() {
    isolated("child_rejected_snapshot_cache");
    isolated("child_unsupported_snapshot_cache");
}

#[test]
#[ignore = "isolated rejected download fallback helper"]
fn child_rejected_snapshot_cache() {
    rejected_snapshot_cache(false);
}

#[test]
#[ignore = "isolated unsupported server schema fallback helper"]
fn child_unsupported_snapshot_cache() {
    rejected_snapshot_cache(true);
}

fn rejected_snapshot_cache(unsupported: bool) {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let mut config = configuration(&root.join("preferences"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.server_url = format!("http://{}", listener.local_addr().unwrap());
    config.data_source.poll_interval_secs = 0;
    config.data_source.migrate_profiles();
    let state = cached_library(&config, "fallback", &config.data_source.server_url);
    let cache = RemoteCache::new(&config.data_root());
    let path = cache.database_path(&state);
    let original = std::fs::read(&path).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let finished = stop.clone();
    let server = std::thread::spawn(move || {
        let mut downloads = 0;
        while !finished.load(Ordering::Acquire) {
            let (mut socket, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("mock server accept: {error}"),
            };
            socket.set_nonblocking(false).unwrap();
            let request = request(&mut socket);
            let path = request.split_whitespace().nth(1).unwrap();
            match path {
                "/api/v1/health" => respond(
                    &mut socket,
                    "200 OK",
                    &serde_json::json!({"status":"ok","version":audiolib_core::compatibility::compatibility_manifest().application_version}).to_string(),
                ),
                "/api/v1/library" => respond(
                    &mut socket,
                    "200 OK",
                    &format!(
                        r#"{{"library_id":"test-library-fallback","schema_version":{},"revision":2}}"#,
                        if unsupported { 99 } else { 5 }
                    ),
                ),
                "/api/v1/capabilities" => respond(
                    &mut socket,
                    "200 OK",
                    &serde_json::json!({"compatibility":audiolib_core::compatibility::compatibility_manifest()}).to_string(),
                ),
                "/api/v1/roots" => respond(&mut socket, "200 OK", "[]"),
                "/api/v1/library/snapshot" => {
                    downloads += 1;
                    respond(&mut socket, "200 OK", "invalid SQLite snapshot");
                }
                _ => respond(
                    &mut socket,
                    "403 Forbidden",
                    r#"{"error_code":"FORBIDDEN","message":"test capability disabled"}"#,
                ),
            }
        }
        downloads
    });
    let ui = AppWindow::new().unwrap();
    let (provider, deferred) = crate::startup::show_while_opening(&ui, config.clone())
        .unwrap()
        .unwrap();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let controller = Controller::new_with_source(
        &ui,
        provider,
        engine.handle(),
        config.clone(),
        playback,
        deferred,
    );
    let c = controller.clone();
    let window = ui.as_weak();
    run_until(&ui, move || {
        // Worker drain precedes the queued UI completion and provider handoff.
        !c.remote_busy.load(Ordering::Acquire)
            && !c.library_busy.load(Ordering::Acquire)
            && window.upgrade().is_some_and(|ui| !ui.get_ds_syncing())
    });
    assert_eq!(
        controller.remote.lock().unwrap().state.as_ref(),
        Some(&state)
    );
    assert_eq!(controller.provider().database_path(), path);
    assert!(ui.get_ds_message().contains("offline cache"));
    assert_eq!(cache.active().unwrap(), state);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!config.paths.database.exists());
    controller.shutdown();
    ui.hide().unwrap();
    stop.store(true, Ordering::Release);
    assert_eq!(server.join().unwrap(), usize::from(!unsupported));
}

#[test]
fn cache_provider_open_keeps_manager_progress_until_the_swap_finishes() {
    isolated("child_cache_provider_progress");
}

#[test]
fn local_source_switch_recovers_from_unfinished_or_failed_server_startup_sync() {
    isolated("child_local_switch_during_startup_sync");
    isolated("child_local_switch_after_failed_startup_sync");
}

#[test]
#[ignore = "isolated local source recovery during a held server request"]
fn child_local_switch_during_startup_sync() {
    local_switch_from_startup_sync(false);
}

#[test]
#[ignore = "isolated local source recovery after a failed server request"]
fn child_local_switch_after_failed_startup_sync() {
    local_switch_from_startup_sync(true);
}

fn local_switch_from_startup_sync(failed: bool) {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let (local, file) = indexed_sounds(&root.join("local"));
    let mut config = configuration(&root.join("preferences"));
    config.paths.database = local.database_path().into();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.server_url = format!("http://{}", listener.local_addr().unwrap());
    config.data_source.poll_interval_secs = 0;
    config.data_source.migrate_profiles();
    config.save().unwrap();
    let (entered, request_entered) = std::sync::mpsc::sync_channel(1);
    let (release, wait_release) = std::sync::mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        let request = request(&mut socket);
        assert!(request.starts_with("GET /api/v1/health"), "{request}");
        entered.send(()).unwrap();
        wait_release.recv_timeout(Duration::from_secs(15)).unwrap();
        reject_cancelled_request(
            &mut socket,
            r#"{"error_code":"FORBIDDEN","message":"held startup sync rejected"}"#,
        );
    });
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
    let (provider, startup) = crate::startup::show_while_opening(&ui, config.clone())
        .unwrap()
        .unwrap();
    assert!(startup.as_ref().unwrap().state.is_none());
    assert_ne!(provider.database_path(), local.database_path());
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let controller = Controller::new_with_source(
        &ui,
        provider,
        engine.handle(),
        config.clone(),
        playback,
        startup,
    );
    request_entered
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(ui.get_library_visible() && ui.get_ds_syncing());
    assert!(controller.remote.lock().unwrap().state.is_none());
    let old_cancel = controller.remote_cancel.lock().unwrap().clone();
    let generation = controller.remote_generation.load(Ordering::Acquire);
    let release = std::rc::Rc::new(std::cell::RefCell::new(Some(release)));
    if failed {
        release.borrow_mut().take().unwrap().send(()).unwrap();
    }
    let requested = std::rc::Rc::new(std::cell::Cell::new(false));
    let switched = std::rc::Rc::new(std::cell::Cell::new(false));
    let weak = ui.as_weak();
    let c = controller.clone();
    let expected_path = local.database_path().to_path_buf();
    run_until(&ui, move || {
        let ui = weak.upgrade().unwrap();
        if !requested.get() {
            if failed && (c.remote_busy.load(Ordering::Acquire) || ui.get_ds_syncing()) {
                return false;
            }
            if failed {
                assert!(c.remote.lock().unwrap().last_error.is_some());
            } else {
                assert!(c.remote_busy.load(Ordering::Acquire));
                assert!(ui.get_ds_syncing());
                // Exercise all cancellation display fields, including a download or
                // decompression update queued by a still-running sync.
                ui.set_ds_progress(0.4);
                ui.set_ds_decompression_info("Old server snapshot".into());
                ui.set_ds_decompression_progress(0.5);
            }
            crate::ui_tests::click_label(&ui, "Local settings");
            assert_eq!(ui.get_sm_selected(), 0);
            crate::ui_tests::click_label(&ui, "Switch to this source");
            assert!(old_cancel.cancellation().load(Ordering::Acquire));
            assert!(c.remote_generation.load(Ordering::Acquire) > generation);
            assert!(!ui.get_ds_syncing());
            assert_eq!(ui.get_ds_progress(), 0.0);
            assert_eq!(ui.get_ds_decompression_info(), "");
            assert_eq!(ui.get_ds_decompression_progress(), -1.0);
            requested.set(true);
            return false;
        }
        if !c.available() || c.remote_read_only() || c.provider().database_path() != expected_path {
            return false;
        }
        if !switched.get() {
            assert_eq!(
                c.config.lock().unwrap().data_source.active_source(),
                ActiveSource::Local
            );
            if !failed {
                // Local activation must finish before this server response is
                // released, even if cancellation already drained the HTTP worker.
                ui.set_ds_message("Local choice remains active".into());
                release.borrow_mut().take().unwrap().send(()).unwrap();
            }
            switched.set(true);
        }
        !c.remote_busy.load(Ordering::Acquire)
            && c.remote_plays.lock().unwrap().publication_gates == 0
    });
    server.join().unwrap();
    assert!(!controller.remote_read_only());
    assert!(controller.remote.lock().unwrap().state.is_none());
    assert!(!ui.get_ds_syncing());
    assert!(!ui.get_ds_applying());
    if !failed {
        assert_eq!(ui.get_ds_message(), "Local choice remains active");
    }
    assert_eq!(
        AppConfig::load_in(root.join("preferences"))
            .unwrap()
            .data_source
            .active_source(),
        ActiveSource::Local
    );
    let results = controller
        .provider()
        .search(&SearchRequest {
            query: "metal".into(),
            ..Default::default()
        })
        .unwrap()
        .results;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, file.id);
    assert_eq!(results[0].file_path, file.file_path);
    assert!(RemoteCache::new(&config.data_root()).active().is_none());
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
fn local_source_switch_supersedes_a_pending_explicit_server_activation() {
    isolated("child_local_switch_during_server_activation");
}

#[test]
#[ignore = "isolated local recovery from an explicit server activation"]
fn child_local_switch_during_server_activation() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let (local, file) = indexed_sounds(&root.join("local"));
    let mut config = configuration(&root.join("preferences"));
    config.paths.database = local.database_path().into();
    config.data_source.poll_interval_secs = 0;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config.data_source.profiles = vec![ServerProfile {
        id: "pending".into(),
        name: "Pending server".into(),
        server_url: format!("http://{}", listener.local_addr().unwrap()),
        ..Default::default()
    }];
    config.save().unwrap();
    let (entered, request_entered) = std::sync::mpsc::sync_channel(1);
    let (release, wait_release) = std::sync::mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        assert!(request(&mut socket).starts_with("GET /api/v1/health"));
        entered.send(()).unwrap();
        wait_release.recv_timeout(Duration::from_secs(15)).unwrap();
        reject_cancelled_request(
            &mut socket,
            r#"{"error_code":"FORBIDDEN","message":"superseded server activation"}"#,
        );
    });
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
    let controller = Controller::new(&ui, local.clone(), engine.handle(), config, playback);
    *controller.selected.lock().unwrap() = Some(file.clone());
    present_file(&ui, &file);
    ui.set_edit_notes("Keep the local draft before switching".into());
    ui.set_metadata_dirty(true);
    ui.show().unwrap();
    ui.invoke_open_library(0);
    crate::ui_tests::click_label(&ui, "Server settings");
    crate::ui_tests::click_label(&ui, "Switch to this source");
    assert!(controller.source_changing.load(Ordering::Acquire));
    let requested = std::rc::Rc::new(std::cell::Cell::new(false));
    let released = std::rc::Rc::new(std::cell::Cell::new(false));
    let weak = ui.as_weak();
    let c = controller.clone();
    run_until(&ui, move || {
        let ui = weak.upgrade().unwrap();
        if !requested.get() {
            match request_entered.try_recv() {
                Ok(()) => {}
                Err(std::sync::mpsc::TryRecvError::Empty) => return false,
                Err(error) => panic!("server activation request: {error}"),
            }
            assert!(ui.get_ds_source_syncing());
            assert!(ui.get_ds_applying() && ui.get_library_busy());
            assert!(c.source_changing.load(Ordering::Acquire));
            assert!(!c.remote_read_only());
            assert_eq!(c.metadata.lock().unwrap().sequence, 1);
            assert_eq!(c.metadata.lock().unwrap().completed, 1);
            assert_eq!(
                c.config.lock().unwrap().data_source.active_source(),
                ActiveSource::Profile("pending".into())
            );
            let cancel = c.remote_cancel.lock().unwrap().clone();
            crate::ui_tests::click_label(&ui, "Local settings");
            assert_eq!(ui.get_sm_selected(), 0);
            crate::ui_tests::click_label(&ui, "Switch to this source");
            assert!(cancel.cancellation().load(Ordering::Acquire));
            assert!(!ui.get_ds_source_syncing());
            assert!(!ui.get_ds_syncing());
            requested.set(true);
            return false;
        }
        if !c.available() {
            return false;
        }
        assert_eq!(
            c.config.lock().unwrap().data_source.active_source(),
            ActiveSource::Local
        );
        assert!(!c.remote_read_only());
        if !released.get() {
            assert_eq!(c.metadata.lock().unwrap().sequence, 1);
            ui.set_ds_message("Local activation superseded server".into());
            release.send(()).unwrap();
            released.set(true);
        }
        !c.remote_busy.load(Ordering::Acquire)
            && c.remote_plays.lock().unwrap().publication_gates == 0
    });
    server.join().unwrap();
    assert!(Arc::ptr_eq(&local, &controller.provider()));
    assert_eq!(ui.get_ds_message(), "Local activation superseded server");
    assert_eq!(
        AppConfig::load_in(root.join("preferences"))
            .unwrap()
            .data_source
            .active_source(),
        ActiveSource::Local
    );
    let result = local
        .search(&SearchRequest {
            query: "metal".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        result.results[0].notes,
        "Keep the local draft before switching"
    );
    assert!(controller.remote.lock().unwrap().source_rollback.is_none());
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
fn local_source_switch_invalidates_a_server_sync_waiting_for_an_old_worker() {
    isolated("child_local_switch_during_sync_retry");
}

#[test]
#[ignore = "isolated local recovery before a delayed server sync retry"]
fn child_local_switch_during_sync_retry() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let (local, _) = indexed_sounds(&root.join("local"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut config = configuration(&root.join("preferences"));
    config.paths.database = local.database_path().into();
    config.data_source.poll_interval_secs = 0;
    config.data_source.profiles = vec![ServerProfile {
        id: "waiting".into(),
        name: "Waiting server".into(),
        server_url: format!("http://{}", listener.local_addr().unwrap()),
        ..Default::default()
    }];
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
    let controller = Controller::new(&ui, local.clone(), engine.handle(), config, playback);
    // An older sync owns this flag until its worker drains. The selected server
    // must wait instead of starting another sync against the shared cache.
    controller.remote_busy.store(true, Ordering::Release);
    ui.show().unwrap();
    ui.invoke_open_library(0);
    crate::ui_tests::click_label(&ui, "Server settings");
    crate::ui_tests::click_label(&ui, "Switch to this source");
    let requested = std::rc::Rc::new(std::cell::Cell::new(false));
    let released = std::rc::Rc::new(std::cell::Cell::new(None::<Instant>));
    let weak = ui.as_weak();
    let c = controller.clone();
    run_until(&ui, move || {
        let ui = weak.upgrade().unwrap();
        if !requested.get() {
            if !ui.get_ds_source_syncing() {
                return false;
            }
            assert_eq!(
                c.config.lock().unwrap().data_source.active_source(),
                ActiveSource::Profile("waiting".into())
            );
            crate::ui_tests::click_label(&ui, "Local settings");
            crate::ui_tests::click_label(&ui, "Switch to this source");
            assert!(!ui.get_ds_source_syncing());
            requested.set(true);
            return false;
        }
        if !c.available() {
            return false;
        }
        if released.get().is_none() {
            assert_eq!(
                c.config.lock().unwrap().data_source.active_source(),
                ActiveSource::Local
            );
            assert!(c.remote_busy.load(Ordering::Acquire));
            c.remote_busy.store(false, Ordering::Release);
            released.set(Some(Instant::now()));
        }
        // Allow the retry's 50ms polling interval and queued UI callback to run.
        released
            .get()
            .is_some_and(|time| time.elapsed() >= Duration::from_millis(200))
    });
    assert!(!ui.get_ds_syncing());
    assert!(!ui.get_ds_source_syncing());
    assert!(!controller.remote_busy.load(Ordering::Acquire));
    assert!(Arc::ptr_eq(&local, &controller.provider()));
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    assert_eq!(
        AppConfig::load_in(root.join("preferences"))
            .unwrap()
            .data_source
            .active_source(),
        ActiveSource::Local
    );
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
#[ignore = "isolated manager provider-open progress helper"]
fn child_cache_provider_progress() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let mut config = configuration(&root.join("preferences"));
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.poll_interval_secs = 0;
    let state = cached_library(&config, "opening", "http://127.0.0.1:1");
    let cache = RemoteCache::new(&config.data_root());
    let placeholder = open_remote_placeholder(&config).unwrap();
    let ui = AppWindow::new().unwrap();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let controller = Controller::new(&ui, placeholder.provider, engine.handle(), config, playback);
    ui.set_library_visible(true);
    controller.swap_to(
        &ui,
        DataSourceSwitch::Server {
            database: cache.database_path(&state),
            state: Some(Box::new(state.clone())),
        },
    );
    // Completion is queued on the event loop, so this observes the real opening
    // interval without introducing timing sleeps or blocking a worker's locks.
    assert!(ui.get_library_busy());
    assert!(ui.get_ds_syncing());
    assert_eq!(ui.get_ds_progress(), -1.0);
    assert_eq!(ui.get_ds_sync_info(), "Opening the cached library…");
    let c = controller.clone();
    run_until(&ui, move || !c.library_busy.load(Ordering::Acquire));
    assert!(!ui.get_ds_syncing());
    assert_eq!(
        controller.remote.lock().unwrap().state.as_ref(),
        Some(&state)
    );
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
#[ignore = "isolated deferred server startup and close helper"]
fn child_deferred_server_cache() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let mut config = configuration(&root.join("preferences"));
    let offline = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.server_url = format!("http://{}", offline.local_addr().unwrap());
    config.data_source.poll_interval_secs = 0;
    drop(offline);
    config.data_source.migrate_profiles();
    let state = cached_library(&config, "offline", &config.data_source.server_url);
    let cache = RemoteCache::new(&config.data_root());
    let path = cache.database_path(&state);
    let original = std::fs::read(&path).unwrap();
    let ui = AppWindow::new().unwrap();
    let (provider, deferred) = crate::startup::show_while_opening(&ui, config.clone())
        .unwrap()
        .unwrap();
    assert!(!ui.get_startup_busy());
    assert!(deferred.as_ref().unwrap().state.is_none());
    assert_ne!(provider.database_path(), path.as_path());
    assert!(!config.paths.database.exists());
    assert_eq!(cache.active().unwrap(), state);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let controller = Controller::new_with_source(
        &ui,
        provider,
        engine.handle(),
        config.clone(),
        playback,
        deferred,
    );
    // A matching cached snapshot opens the main window; the sync runs in the background.
    assert!(!ui.get_library_visible() && ui.get_ds_syncing());
    let c = controller.clone();
    let weak = ui.as_weak();
    let waiting = std::rc::Rc::new(std::cell::Cell::new(None::<Instant>));
    let started = waiting.clone();
    let released = std::rc::Rc::new(std::cell::Cell::new(false));
    let finished = released.clone();
    let deadline = Instant::now() + Duration::from_secs(12);
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(10),
        move || {
            assert!(Instant::now() < deadline, "Deferred cache/close timed out");
            let ui = weak.upgrade().unwrap();
            if let Some(start) = started.get() {
                if !finished.get() && start.elapsed() >= Duration::from_millis(200) {
                    assert!(ui.window().is_visible());
                    c.remote_busy.store(false, Ordering::Release);
                    finished.set(true);
                }
            } else if !c.remote_busy.load(Ordering::Acquire)
                && !c.library_busy.load(Ordering::Acquire)
                && c.remote.lock().unwrap().state.is_some()
            {
                // Hold an in-flight cache worker while requesting close; only release
                // it after observing the window still visible. Use one event loop so
                // repeated helper Quit events cannot short-circuit the close assertion.
                c.remote_busy.store(true, Ordering::Release);
                c.request_close(&ui);
                started.set(Some(Instant::now()));
            }
        },
    );
    ui.show().unwrap();
    slint::run_event_loop().unwrap();
    timer.stop();
    assert!(released.get());
    assert_eq!(
        controller
            .remote
            .lock()
            .unwrap()
            .state
            .as_ref()
            .unwrap()
            .library_id,
        state.library_id
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!config.paths.database.exists());
    assert!(!controller.remote_busy.load(Ordering::Acquire));
    assert!(!ui.window().is_visible());
    controller.shutdown();
}

#[test]
fn startup_refuses_another_servers_active_snapshot_and_accepts_effective_same_url() {
    let root = std::env::temp_dir().join(format!(
        "audiolib-source-startup-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut config = configuration(&root.join("preferences"));
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.server_url = "http://localhost".into();
    let state = cached_library(&config, "server-b", "http://127.0.0.1:8799");
    let cache = RemoteCache::new(&config.data_root());
    let path = cache.database_path(&state);
    let bytes = std::fs::read(&path).unwrap();
    let startup = open_remote_startup(&config).unwrap();
    assert!(
        startup.state.is_none(),
        "another server's IDs were accepted"
    );
    assert_ne!(
        startup.provider.database_path(),
        path.canonicalize().unwrap()
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "mismatched cache was mutated"
    );
    drop(startup);
    let mut matching = state;
    matching.server_url = "HTTP://LOCALHOST:80/api/v1/".into();
    cache.save_state(&matching).unwrap();
    let startup = open_remote_startup(&config).unwrap();
    assert_eq!(
        startup.state.as_ref().unwrap().library_id,
        matching.library_id
    );
    drop(startup);
    drop(cache);
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    remove_dir_all_retry(&root);
}

#[test]
fn invalid_new_snapshot_rolls_back_both_connection_and_active_cache_for_restart() {
    isolated("child_invalid_snapshot_rollback");
}

#[test]
fn failed_local_escape_restores_cache_after_the_cancelled_sync_publishes() {
    isolated("child_failed_local_escape_cache_rollback");
}

#[test]
#[ignore = "isolated failed local recovery with a pending cache publication"]
fn child_failed_local_escape_cache_rollback() {
    failed_local_escape_cache_rollback(false);
}

#[test]
fn close_during_failed_local_escape_uses_the_bounded_remote_cache_drain() {
    isolated("child_close_during_failed_local_escape");
}

#[test]
#[ignore = "isolated native close during a failed local recovery cache drain"]
fn child_close_during_failed_local_escape() {
    failed_local_escape_cache_rollback(true);
}

fn failed_local_escape_cache_rollback(close_during_rollback: bool) {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let mut config = configuration(&root.join("preferences"));
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.server_url = "http://127.0.0.1:18790".into();
    config.data_source.poll_interval_secs = 0;
    config.data_source.active = Some("a".into());
    config.data_source.profiles = vec![
        ServerProfile {
            id: "a".into(),
            name: "Original server".into(),
            server_url: config.data_source.server_url.clone(),
            ..Default::default()
        },
        ServerProfile {
            id: "b".into(),
            name: "Pending server".into(),
            server_url: "http://127.0.0.1:18791".into(),
            ..Default::default()
        },
    ];
    config.save().unwrap();
    let state_a = cached_library(&config, "original", &config.data_source.server_url);
    let state_b = cached_library(&config, "pending", "http://127.0.0.1:18791");
    let cache = RemoteCache::new(&config.data_root());
    cache.save_state(&state_a).unwrap();
    // The path exists, so the local-source preflight accepts it, but opening it
    // must fail without replacing the currently displayed server provider.
    std::fs::write(&config.paths.database, b"invalid local SQLite library").unwrap();
    let provider =
        Arc::new(SqliteProvider::open_read_only(&cache.database_path(&state_a)).unwrap());
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
    let controller = Controller::new(&ui, provider.clone(), engine.handle(), config, playback);
    let close_grace = Duration::from_millis(100);
    *controller.remote_close_grace.lock().unwrap() = close_grace;
    let close_controller = controller.clone();
    let close_ui = ui.as_weak();
    ui.window().on_close_requested(move || {
        if let Some(ui) = close_ui.upgrade() {
            close_controller.request_close(&ui);
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(state_a.clone());
    let barrier = controller.begin_source_change(&ui).unwrap();
    controller.wait_transition_metadata(barrier).unwrap();
    let mut pending = controller.config.lock().unwrap().data_source.clone();
    let mut secrets = RemoteSecrets::default();
    audiolib_remote::activate_source(
        &mut pending,
        &mut secrets,
        &ActiveSource::Profile("b".into()),
    )
    .unwrap();
    controller
        .save_data_source(pending, secrets, false, true)
        .unwrap();
    // Model an old sync already inside its non-cancellable state-file write.
    // Its final B publication happens only after the failed local handoff has
    // entered rollback, and its busy ownership ends after that publication.
    controller.remote_busy.store(true, Ordering::Release);
    let (release, wait_release) = std::sync::mpsc::sync_channel(1);
    let old_cache = cache.clone();
    let old_controller = controller.clone();
    let publisher = std::thread::spawn(move || {
        wait_release.recv_timeout(Duration::from_secs(8)).unwrap();
        old_cache.save_state(&state_b).unwrap();
        old_controller.remote_busy.store(false, Ordering::Release);
    });
    ui.set_ds_applying(true);
    ui.set_ds_syncing(true);
    ui.set_ds_source_syncing(true);
    ui.show().unwrap();
    ui.invoke_open_data_source();
    crate::ui_tests::click_label(&ui, "Local settings");
    crate::ui_tests::click_label(&ui, "Switch to this source");
    let rollback_started = std::rc::Rc::new(std::cell::Cell::new(None::<Instant>));
    let released = std::rc::Rc::new(std::cell::Cell::new(false));
    let close_requested = std::rc::Rc::new(std::cell::Cell::new(None::<Instant>));
    let requested = close_requested.clone();
    let release_cache = release.clone();
    let weak = ui.as_weak();
    let c = controller.clone();
    run_until(&ui, move || {
        if requested.get().is_some() {
            // Let the ordinary close poll consume its bounded grace and quit the
            // native event loop without releasing this synthetic cache worker.
            assert!(c.remote_busy.load(Ordering::Acquire));
            return false;
        }
        if !released.get() {
            if c.remote.lock().unwrap().source_rollback.is_some() {
                return false;
            }
            assert!(c.source_changing.load(Ordering::Acquire));
            assert!(c.remote_busy.load(Ordering::Acquire));
            assert!(
                !c.available(),
                "rollback finished before cache work drained"
            );
            if c.config.lock().unwrap().data_source.active_source()
                != ActiveSource::Profile("a".into())
            {
                return false;
            }
            // Keep exercising the event loop while the synthetic publication is
            // pending; rollback must retain its source gate throughout this gap.
            let started = rollback_started.get().unwrap_or_else(|| {
                let now = Instant::now();
                rollback_started.set(Some(now));
                now
            });
            if started.elapsed() < Duration::from_millis(200) {
                return false;
            }
            if close_during_rollback {
                requested.set(Some(Instant::now()));
                weak.upgrade()
                    .unwrap()
                    .window()
                    .dispatch_event(slint::platform::WindowEvent::CloseRequested);
                assert!(c.close_after_operation.load(Ordering::Acquire));
                return false;
            }
            release_cache.send(()).unwrap();
            released.set(true);
        }
        !c.source_changing.load(Ordering::Acquire)
    });
    if close_during_rollback {
        let elapsed = close_requested.get().unwrap().elapsed();
        assert!(elapsed >= close_grace);
        assert!(elapsed < Duration::from_secs(3), "close took {elapsed:?}");
        assert!(!ui.window().is_visible());
        assert!(controller.closing.load(Ordering::Acquire));
        assert!(!controller.source_changing.load(Ordering::Acquire));
        assert!(controller.remote_busy.load(Ordering::Acquire));
        release.send(()).unwrap();
    }
    publisher.join().unwrap();
    assert_eq!(controller.available(), !close_during_rollback);
    assert!(controller.remote_read_only());
    assert!(Arc::ptr_eq(&provider, &controller.provider()));
    assert_eq!(cache.state(&state_a.library_id).unwrap(), state_a);
    let persisted = AppConfig::load_in(controller.data_root()).unwrap();
    assert_eq!(
        persisted.data_source.active_source(),
        ActiveSource::Profile("a".into())
    );
    let restarted = open_remote_startup(&persisted).unwrap();
    if close_during_rollback {
        // An exit during an unfinished cache publication can leave B active, but
        // endpoint validation must reject it instead of opening B with A's URL.
        assert!(restarted.state.is_none());
    } else {
        assert_eq!(cache.active().unwrap(), state_a);
        assert_eq!(restarted.state.unwrap().library_id, state_a.library_id);
    }
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
#[ignore = "isolated Slint source rollback helper"]
fn child_invalid_snapshot_rollback() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let mut config = configuration(&root.join("preferences"));
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.server_url = "http://127.0.0.1:18790".into();
    config.data_source.poll_interval_secs = 0;
    config.save().unwrap();
    let state_a = cached_library(&config, "a", &config.data_source.server_url);
    let cache = RemoteCache::new(&config.data_root());
    let provider =
        Arc::new(SqliteProvider::open_read_only(&cache.database_path(&state_a)).unwrap());
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider.clone(), engine.handle(), config, playback);
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(state_a.clone());
    let barrier = controller.begin_source_change(&ui).unwrap();
    controller.wait_metadata(barrier, true).unwrap();
    let mut settings_b = controller.config.lock().unwrap().data_source.clone();
    settings_b.server_url = "http://127.0.0.1:18791".into();
    controller
        .save_data_source(settings_b, RemoteSecrets::default(), false, false)
        .unwrap();
    let state_b = cached_library(
        &controller.config.lock().unwrap(),
        "b",
        "http://127.0.0.1:18791",
    );
    assert_eq!(cache.active_library_id().unwrap(), state_b.library_id);
    let invalid = root.join("invalid-snapshot.db");
    std::fs::write(&invalid, b"invalid SQLite snapshot").unwrap();
    controller.run_library_operation(LibraryOperation::DataSource(DataSourceSwitch::Server {
        database: invalid,
        state: Some(Box::new(state_b)),
    }));
    let c = controller.clone();
    run_until(&ui, move || !c.source_changing.load(Ordering::Acquire));
    assert!(controller.available());
    assert_eq!(
        controller.provider().database_path(),
        provider.database_path()
    );
    assert_eq!(cache.active().unwrap().library_id, state_a.library_id);
    let persisted = AppConfig::load_in(controller.data_root()).unwrap();
    assert_eq!(persisted.data_source.server_url, "http://127.0.0.1:18790");
    let restarted = open_remote_startup(&persisted).unwrap();
    assert_eq!(
        restarted.state.as_ref().unwrap().library_id,
        state_a.library_id
    );
    assert_eq!(restarted.provider.database_path(), provider.database_path());
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
fn source_switch_drains_old_server_and_rolls_back_a_failed_new_connection() {
    isolated("child_source_switch");
}

#[test]
#[ignore = "isolated Slint source transition helper"]
fn child_source_switch() {
    source_switch_flow(false);
}

#[test]
fn edited_active_profile_save_and_switch_keeps_the_original_rollback() {
    isolated("child_edited_active_profile_switch");
}

#[test]
#[ignore = "isolated Slint active profile save/switch helper"]
fn child_edited_active_profile_switch() {
    source_switch_flow(true);
}

fn source_switch_flow(edit_active_profile: bool) {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url_a = format!("http://{}", a.local_addr().unwrap());
    let url_b = format!("http://{}", b.local_addr().unwrap());
    let (sent, received) = std::sync::mpsc::channel();
    let (release, wait_release) = std::sync::mpsc::channel();
    let server_a = std::thread::spawn(move || {
        let mut preflight = accept(&a);
        assert!(request(&mut preflight).starts_with("GET /api/v1/library"));
        respond(
            &mut preflight,
            "200 OK",
            r#"{"library_id":"a-library","identity_bound_writes":true,"revision":1,"schema_version":5}"#,
        );
        legacy_peer_preflight(&a);
        let mut socket = accept(&a);
        sent.send(request(&mut socket)).unwrap();
        wait_release.recv_timeout(Duration::from_secs(8)).unwrap();
        let file = AudioFile {
            id: 1,
            notes: "draft belongs to A".into(),
            ..Default::default()
        };
        let body = serde_json::json!({"file":file,"revision_before":1,"revision":2}).to_string();
        respond(&mut socket, "200 OK", &body);
    });
    let server_b = std::thread::spawn(move || {
        let mut socket = accept(&b);
        let request = request(&mut socket);
        respond(
            &mut socket,
            "403 Forbidden",
            r#"{"error_code":"FORBIDDEN","message":"new source refused"}"#,
        );
        request
    });
    let mut config = configuration(&root.join("preferences"));
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.server_url = url_a.clone();
    config.data_source.poll_interval_secs = 0;
    config.data_source.active = Some("a".into());
    config.data_source.profiles = vec![
        ServerProfile {
            id: "a".into(),
            name: "A".into(),
            server_url: url_a.clone(),
            ..Default::default()
        },
        ServerProfile {
            id: "b".into(),
            name: "B".into(),
            server_url: url_b.clone(),
            ..Default::default()
        },
    ];
    if edit_active_profile {
        config.data_source.profiles.truncate(1);
    }
    let provider = Arc::new(SqliteProvider::open(&root.join("a.db")).unwrap());
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider.clone(), engine.handle(), config, playback);
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(CacheState {
        library_id: "a-library".into(),
        revision: 1,
        database_file: "a.db".into(),
        ..Default::default()
    });
    selected(&controller, &ui, "draft belongs to A");
    if edit_active_profile {
        ui.invoke_sm_open_requested(0);
        assert_eq!(ui.get_ds_server_url(), url_a);
        ui.set_ds_server_url(url_b.clone().into());
        ui.set_ds_dirty(true);
        // The actual source-manager entrance calls save_selected(AfterSave::Switch).
        ui.invoke_sm_switch_requested();
    } else {
        controller.activate_data_source(ActiveSource::Profile("b".into()));
    }
    assert!(controller.source_changing.load(Ordering::Acquire));
    assert!(!controller.available() && ui.get_library_busy());
    let request_a = received.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        request_a.starts_with("PATCH /api/v1/audio/1/metadata"),
        "{request_a}"
    );
    assert!(request_a.contains("draft belongs to A"));
    assert_eq!(
        controller.config.lock().unwrap().data_source.server_url,
        url_a
    );
    assert_eq!(
        controller
            .config
            .lock()
            .unwrap()
            .data_source
            .profile("a")
            .unwrap()
            .server_url,
        url_a
    );
    let sequence = controller.metadata.lock().unwrap().sequence;
    controller.file_action(1, 5);
    controller.save_metadata(&ui);
    assert_eq!(
        controller.metadata.lock().unwrap().sequence,
        sequence,
        "transition accepted a new write"
    );
    release.send(()).unwrap();
    let c = controller.clone();
    run_until(&ui, move || !c.source_changing.load(Ordering::Acquire));
    server_a.join().unwrap();
    let request_b = server_b.join().unwrap();
    assert!(
        request_b.starts_with("GET /api/v1/health"),
        "old Metadata was routed to B: {request_b}"
    );
    assert!(request_b.contains(&format!("host: {}", url_b.strip_prefix("http://").unwrap())));
    assert!(!request_b.contains("PATCH "), "{request_b}");
    {
        let state = controller.metadata.lock().unwrap();
        assert_eq!(state.completed, sequence, "old write was not drained");
    }
    assert_eq!(
        controller.config.lock().unwrap().data_source.server_url,
        url_a
    );
    assert_eq!(
        AppConfig::load_in(root.join("preferences"))
            .unwrap()
            .data_source
            .active_source(),
        ActiveSource::Profile("a".into())
    );
    assert_eq!(
        controller.provider().database_path(),
        provider.database_path()
    );
    assert_eq!(
        controller
            .remote
            .lock()
            .unwrap()
            .state
            .as_ref()
            .unwrap()
            .library_id,
        "a-library"
    );
    assert!(
        controller.metadata.lock().unwrap().drafts.is_empty(),
        "A save did not complete"
    );
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
fn failed_annotation_preserves_the_snapshot_editor_and_draft() {
    isolated("child_failed_annotation_snapshot");
}

#[test]
#[ignore = "isolated Slint snapshot helper"]
fn child_failed_annotation_snapshot() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let old_snapshot = root.join("snapshot-r1.db");
    let next_snapshot = root.join("snapshot-r2.db");
    drop(SqliteProvider::open(&old_snapshot).unwrap());
    drop(SqliteProvider::open(&next_snapshot).unwrap());
    let provider = Arc::new(SqliteProvider::open_read_only(&old_snapshot).unwrap());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut preflight = accept(&listener);
        assert!(request(&mut preflight).starts_with("GET /api/v1/library"));
        respond(
            &mut preflight,
            "200 OK",
            r#"{"library_id":"same-library","identity_bound_writes":true,"revision":1,"schema_version":5}"#,
        );
        legacy_peer_preflight(&listener);
        let mut socket = accept(&listener);
        let request = request(&mut socket);
        respond(
            &mut socket,
            "403 Forbidden",
            r#"{"error_code":"FORBIDDEN","message":"Metadata denied"}"#,
        );
        request
    });
    let mut config = configuration(&root.join("preferences"));
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.server_url = url;
    config.data_source.poll_interval_secs = 0;
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider.clone(), engine.handle(), config, playback);
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(CacheState {
        library_id: "same-library".into(),
        revision: 1,
        database_file: "snapshot-r1.db".into(),
        ..Default::default()
    });
    selected(&controller, &ui, "failed draft must survive");
    controller.run_library_operation(LibraryOperation::DataSource(DataSourceSwitch::Server {
        database: next_snapshot,
        state: Some(Box::new(CacheState {
            library_id: "same-library".into(),
            revision: 2,
            database_file: "snapshot-r2.db".into(),
            ..Default::default()
        })),
    }));
    let c = controller.clone();
    run_until(&ui, move || !c.library_busy.load(Ordering::Acquire));
    let request = server.join().unwrap();
    assert!(
        request.starts_with("PATCH /api/v1/audio/1/metadata"),
        "{request}"
    );
    assert!(request.contains("failed draft must survive"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("x-audiolib-library-id: same-library")
    );
    assert_eq!(
        controller.provider().database_path(),
        provider.database_path()
    );
    assert_eq!(
        controller
            .remote
            .lock()
            .unwrap()
            .state
            .as_ref()
            .unwrap()
            .revision,
        1
    );
    let state = controller.metadata.lock().unwrap();
    assert_eq!(
        state.drafts.get(&1).unwrap().patch.notes.as_deref(),
        Some("failed draft must survive")
    );
    assert!(state.failures.contains_key(&1));
    assert_eq!(state.completed, state.sequence, "FIFO did not drain");
    drop(state);
    assert_eq!(ui.get_selected_id(), "1");
    assert_eq!(ui.get_edit_notes(), "failed draft must survive");
    assert!(ui.get_metadata_dirty() && controller.available());
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
fn rapid_selection_stays_bounded_and_superseded_barriers_cancel() {
    isolated("child_bounded_selections");
}

#[test]
#[ignore = "isolated Slint selection helper"]
fn child_bounded_selections() {
    i_slint_backend_testing::init_no_event_loop();
    let root = root();
    let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let config = configuration(&root.join("preferences"));
    let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
    std::thread::sleep(Duration::from_millis(100));
    #[cfg(windows)]
    let baseline = process_threads();
    {
        // An outstanding FIFO sequence models a blocked Metadata HTTP request.
        let mut state = controller.metadata.lock().unwrap();
        state.sequence = 1;
        state.completed = 0;
    }
    for id in 1..=1000 {
        controller.select_id(id, false);
    }
    std::thread::sleep(Duration::from_millis(100));
    #[cfg(windows)]
    assert!(
        process_threads() <= baseline + 4,
        "rapid selection created unbounded native threads"
    );
    let generation = controller.selection_generation.load(Ordering::Acquire);
    let c = controller.clone();
    let (sent, received) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        sent.send(c.wait_selection_metadata(1, generation)).unwrap();
    });
    controller
        .selection_generation
        .fetch_add(1, Ordering::AcqRel);
    controller.metadata_wake.notify_all();
    assert_eq!(
        received.recv_timeout(Duration::from_millis(500)).unwrap(),
        None
    );
    waiter.join().unwrap();
    assert_eq!(
        controller.metadata.lock().unwrap().completed,
        0,
        "cancellation required the write to finish"
    );
    controller.shutdown();
}

#[cfg(windows)]
fn process_threads() -> usize {
    let script = format!("(Get-Process -Id {}).Threads.Count", std::process::id());
    let output = std::process::Command::new("powershell")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &script,
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn ready_preview_zoom_pan_peaks_and_resize_request_original_waveform() {
    isolated("child_ready_preview_viewport");
}

#[test]
#[ignore = "isolated Slint remote viewport helper"]
fn child_ready_preview_viewport() {
    i_slint_backend_testing::init_no_event_loop();
    let root = root();
    let mut config = configuration(&root.join("preferences"));
    let virtual_path = config.data_root().join("remote/library/stream/42-pcm.wav");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.server_url = format!("http://{}", listener.local_addr().unwrap());
    config.data_source.poll_interval_secs = 0;
    let (sent, received) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        for _ in 0..5 {
            let mut socket = accept(&listener);
            sent.send(request(&mut socket)).unwrap();
            respond(
                &mut socket,
                "403 Forbidden",
                r#"{"error_code":"FORBIDDEN","message":"no Waveform in test"}"#,
            );
        }
    });
    let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let mut wav = Vec::new();
    let length = 24_000_u32 * 2 * 2;
    wav.extend(b"RIFF");
    wav.extend((36 + length).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16_u32.to_le_bytes());
    wav.extend(1_u16.to_le_bytes());
    wav.extend(1_u16.to_le_bytes());
    wav.extend(24_000_u32.to_le_bytes());
    wav.extend(48_000_u32.to_le_bytes());
    wav.extend(2_u16.to_le_bytes());
    wav.extend(16_u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(length.to_le_bytes());
    wav.resize(44 + length as usize, 0);
    let external_path = root.join("external.wav");
    std::fs::write(&external_path, &wav).unwrap();
    audiolib_audio::register_stream_source(
        virtual_path.clone(),
        Arc::new(move || {
            Ok((
                Box::new(std::io::Cursor::new(wav.clone())),
                wav.len() as u64,
            ))
        }),
    );
    let status = engine
        .handle()
        .send_and_wait(
            AudioCommand::Load(virtual_path.clone()),
            Duration::from_secs(4),
        )
        .unwrap();
    assert_eq!(status.state, PlaybackState::Ready);
    assert_eq!(status.sample_rate, 24_000);
    assert!(!virtual_path.exists());
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote_busy.store(true, Ordering::Release);
    let file = AudioFile {
        id: 42,
        filename: "original.wav".into(),
        file_path: "/server/original.wav".into(),
        sample_rate: 96_000,
        channels: 6,
        duration: 2.0,
        ..Default::default()
    };
    {
        let mut remote = controller.remote.lock().unwrap();
        remote.state = Some(CacheState {
            library_id: "library".into(),
            revision: 1,
            ..Default::default()
        });
        remote
            .audio_files
            .insert(virtual_path.clone(), file.clone());
    }
    // Even without an inspector selection, the loaded virtual stream retains source identity.
    let source = controller.waveform_file(&status).unwrap();
    assert_eq!(source.id, 42);
    assert_eq!(source.sample_rate, 96_000);
    assert_eq!(source.channels, 6);
    *controller.selected.lock().unwrap() = Some(file.clone());
    let external = engine
        .handle()
        .send_and_wait(
            AudioCommand::Load(external_path.clone()),
            Duration::from_secs(4),
        )
        .unwrap();
    assert_eq!(external.state, PlaybackState::Ready);
    let external_waveform = controller.waveform_file(&external).unwrap();
    assert_eq!(
        external_waveform.id, 0,
        "the inspector overrode an external IPC path"
    );
    assert_eq!(external_waveform.file_path, external_path.to_string_lossy());
    assert_eq!(external_waveform.sample_rate, 24_000);
    let remote = engine
        .handle()
        .send_and_wait(
            AudioCommand::Load(virtual_path.clone()),
            Duration::from_secs(4),
        )
        .unwrap();
    assert_eq!(remote.state, PlaybackState::Ready);
    assert_eq!(controller.waveform_file(&remote).unwrap().id, 42);
    assert_eq!(
        controller.waveform_file(&remote).unwrap().sample_rate,
        96_000
    );
    *controller.wave_view.lock().unwrap() = (file.file_path.clone(), WaveView::default());
    for action in 0..5 {
        match action {
            0 => ui.invoke_waveform_zoom_requested(2.0),
            1 => ui.invoke_waveform_wheel_zoom_requested(2.0, 0.5),
            2 => ui.invoke_waveform_pan_requested(0.75),
            3 => {
                ui.set_peak_mode(1);
                ui.invoke_peak_style_requested();
            }
            _ => ui.invoke_waveform_resize_requested(777, 123),
        }
        let (start, end) = controller.wave_view.lock().unwrap().1.range(file.duration);
        let start = (start * 96_000.0).round() as u64;
        let end = (end * 96_000.0).round() as u64;
        let width = controller.waveform_size.lock().unwrap().0;
        let request_plan = WaveformRequest {
            start_frame: start,
            end_frame: Some(end),
            samples_per_pixel: (end - start).div_ceil(u64::from(width)).max(1) as u32,
            include_colors: true,
        };
        // Use the production range planner: its first half-second rounds upward
        // to a bucket boundary, so the HTTP endpoint is not the whole viewport.
        let mut first = None;
        crate::progressive_waveform::load(
            request_plan,
            96_000,
            &AtomicBool::new(false),
            |part, buckets| {
                first = Some((part, buckets));
                Err("Captured first source segment".into())
            },
            |_, _| unreachable!(),
        )
        .unwrap_err();
        let (part, buckets) = first.unwrap();
        let request = received.recv_timeout(Duration::from_secs(6)).unwrap();
        assert!(
            request.starts_with("GET /api/v1/audio/42/waveform?"),
            "{request}"
        );
        assert!(
            request.contains(&format!("start_frame={}", part.start_frame)),
            "{request}"
        );
        assert!(
            request.contains(&format!("end_frame={}", part.end_frame.unwrap())),
            "{request}"
        );
        assert!(request.contains(&format!("buckets={buckets}")), "{request}");
        assert!(
            !virtual_path.exists(),
            "Waveform created a local audition copy"
        );
    }
    server.join().unwrap();
    controller.shutdown();
    audiolib_audio::unregister_stream_source(&virtual_path);
}
