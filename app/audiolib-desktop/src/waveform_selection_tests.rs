// app/audiolib-desktop/src/waveform_selection_tests.rs
//! Isolated selection identity, preparation and preview-admission regressions.
use super::*;
use std::sync::mpsc;
#[path = "waveform_selection_version_tests.rs"]
mod version_tests;

const ROOT_ENV: &str = "AUDIOLIB_WAVE_SELECTION_TEST_ROOT";

fn isolated(child: &str) {
    let root = std::env::temp_dir().join(format!(
        "audiolib-wave-selection-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("controller::waveform_selection::integration_tests::{child}"),
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(ROOT_ENV, &root)
        .env("AUDIOLIB_DATA_DIR", root.join("preferences"))
        .output()
        .unwrap();
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    if root.exists() {
        let deadline = Instant::now() + Duration::from_secs(4);
        while let Err(error) = std::fs::remove_dir_all(&root) {
            if error.kind() == std::io::ErrorKind::NotFound {
                break;
            }
            if Instant::now() >= deadline {
                eprintln!("owned fixture retained at {}: {error}", root.display());
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{child}: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Fixture {
    root: PathBuf,
    c: Arc<Controller>,
    ui: AppWindow,
    _engine: audiolib_audio::AudioEngine,
    a: AudioFile,
    b: AudioFile,
}

fn wav(samples: &[i16], rate: u32) -> Vec<u8> {
    let size = u32::try_from(samples.len() * 2).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + size).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt \x10\0\0\0\x01\0\x01\0");
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * 2).to_le_bytes());
    bytes.extend_from_slice(b"\x02\0\x10\0data");
    bytes.extend_from_slice(&size.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

fn fixture() -> Fixture {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = PathBuf::from(std::env::var_os(ROOT_ENV).expect("isolated parent root"));
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    let sounds = root.join("sounds");
    let preferences = root.join("preferences");
    std::fs::create_dir_all(&sounds).unwrap();
    std::fs::create_dir_all(&preferences).unwrap();
    let playback = PlaybackConfig {
        volume: 0.0,
        // A deterministic failure to open output verifies admission without
        // depending on an installed audio device or opening a real stream.
        device_id: Some("waveform-selection-test-no-such-output-device".into()),
        ..Default::default()
    };
    std::fs::write(
        preferences.join("settings.json"),
        serde_json::to_vec(&serde_json::json!({
            "active_database": preferences.join("library.db"),
            "language": "en", "audition_play_on_single_click": false,
            "slice_export_directory": root.join("exports"), "playback": playback,
        }))
        .unwrap(),
    )
    .unwrap();
    let mut config = AppConfig::load_in(preferences.clone()).unwrap();
    config.slice_export_directory = Some(root.join("exports"));
    config.allowed_scan_roots = vec![sounds.clone()];
    let provider = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let c = Controller::new(&ui, provider, engine.handle(), config, playback);
    let make_file = |id, name: &str, samples: Vec<i16>| {
        let path = sounds.join(name);
        let bytes = wav(&samples, 1000);
        std::fs::write(&path, &bytes).unwrap();
        AudioFile::from(audiolib_core::AudioFileData {
            id,
            file_path: path.display().to_string(),
            filename: name.into(),
            file_size: bytes.len() as u64,
            sample_rate: 1000,
            bit_depth: 16,
            channels: 1,
            duration: samples.len() as f64 / 1000.0,
            ..Default::default()
        })
    };
    let a = make_file(41, "Displayed A.wav", (0..1000).collect());
    let b = make_file(42, "Inspector B.wav", vec![-2000; 1000]);
    let fixture = Fixture {
        root,
        c,
        ui,
        _engine: engine,
        a,
        b,
    };
    let weak = fixture.ui.as_weak();
    wait_for(&fixture.ui, move || {
        weak.upgrade()
            .is_some_and(|ui| !ui.get_search_workspace_loading() && !ui.get_searching())
    });
    fixture
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.c.shutdown();
        self.ui.hide().unwrap();
    }
}

fn wait_for(ui: &AppWindow, ready: impl Fn() -> bool + 'static) {
    let started = Instant::now();
    let completed = std::rc::Rc::new(std::cell::Cell::new(false));
    let observed = completed.clone();
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(10),
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            assert!(
                started.elapsed() < Duration::from_secs(12),
                "selection timed out: {}",
                ui.get_status()
            );
            if ready() && !observed.replace(true) {
                slint::quit_event_loop().unwrap();
            }
        },
    );
    ui.show().unwrap();
    slint::run_event_loop().unwrap();
    timer.stop();
    assert!(
        completed.get(),
        "event loop ended before selection completion"
    );
}

fn bind_displayed(f: &Fixture) -> WaveSource {
    // The Metadata inspector deliberately describes a different file. No editor
    // draft is dirty, so preview admission has no pending database writes.
    *f.c.selected.lock().unwrap() = Some(f.b.clone());
    f.ui.set_selected_id(f.b.id.to_string().into());
    f.ui.set_metadata_dirty(false);
    let mut source = WaveSource::capture(&f.c, &f.a);
    source.bind_local_version().unwrap();
    assert!(f.c.publish_wave_source(&f.ui, &source));
    f.ui.set_waveform_ready(true);
    source
}

fn selection(
    source: WaveSource,
    range: AudioFrameRange,
    revision: u64,
    generation: u64,
) -> Selection {
    Selection {
        source,
        range,
        revision,
        generation,
        cancel: Arc::new(AtomicBool::new(false)),
        render_cancel: Arc::new(AtomicBool::new(false)),
        preview_cancel: Arc::new(AtomicBool::new(false)),
        preview_ready: Arc::new(AtomicBool::new(true)),
    }
}

#[test]
fn displayed_waveform_drives_slice_and_preview_even_with_another_inspector() {
    isolated("child_displayed_source");
}

#[test]
#[ignore = "isolated Waveform selection source and exact slice regression"]
fn child_displayed_source() {
    let f = fixture();
    bind_displayed(&f);
    let starts = f.c.audio.subscribe_playback_starts();
    f.c.commit_wave_selection(0.2, 0.6);
    let c = f.c.clone();
    wait_for(&f.ui, move || {
        let state = c.slice.lock().unwrap();
        state.ready.is_some() && state.workers == 0
    });
    let selected = f.c.slice.lock().unwrap().selection.clone().unwrap();
    assert_eq!(selected.source.file.id, f.a.id);
    assert_eq!(
        (selected.range.start_frame, selected.range.end_frame),
        (200, 600)
    );
    let ready = f.c.slice.lock().unwrap().ready.clone().unwrap();
    assert!(
        ready
            .export_directory
            .starts_with(f.root.canonicalize().unwrap())
    );
    let info = audiolib_audio::probe(&ready.path).unwrap();
    assert_eq!(
        (info.sample_rate, info.channels, info.bit_depth, info.frames),
        (1000, 1, 16, 400)
    );
    let bytes = std::fs::read(&ready.path).unwrap();
    let data = bytes
        .windows(4)
        .position(|part| part == b"data")
        .expect("WAV data chunk");
    let samples: Vec<i16> = bytes[data + 8..data + 8 + 800]
        .chunks_exact(2)
        .map(|sample| i16::from_le_bytes(sample.try_into().unwrap()))
        .collect();
    assert_eq!(samples, (200..600).collect::<Vec<i16>>());
    // PlayRange sets the source range before the deliberately invalid device
    // rejects output. These assertions prove the preview path actually ran.
    let status = f.c.audio.status();
    assert_eq!(status.file_path, Some(PathBuf::from(&f.a.file_path)));
    assert_eq!(status.playback_range, Some(selected.range));
    assert_eq!(status.state, PlaybackState::Error);
    assert!(status.error.is_some());
    assert!(starts.try_iter().next().is_none());
    f.c.enqueue_stop()
        .unwrap()
        .wait(Duration::from_secs(4))
        .unwrap();
    assert!(
        f.c.selection_current(&selected),
        "Stop invalidated the durable selection"
    );
    assert!(f.c.slice.lock().unwrap().ready.is_some());
    assert!(f.ui.get_wave_selection_active());
    let stopped = f.c.audio.status();
    f.c.audio
        .send_and_wait(AudioCommand::Synchronize, Duration::from_secs(4))
        .unwrap();
    assert_eq!(f.c.audio.status().state, PlaybackState::Stopped);
    assert_eq!(f.c.audio.status().play_session_id, stopped.play_session_id);
}

#[test]
fn stop_cancels_deferred_preview_without_losing_selected_range_or_render() {
    isolated("child_stop_admission");
}

#[test]
#[ignore = "isolated Stop versus deferred selection preview admission regression"]
fn child_stop_admission() {
    let f = fixture();
    bind_displayed(&f);
    // Hold admission behind an owned artificial FIFO barrier. The metadata
    // worker has no job to complete it, so Stop necessarily wins the race.
    {
        let mut metadata = f.c.metadata.lock().unwrap();
        metadata.sequence += 1;
    }
    let starts = f.c.audio.subscribe_playback_starts();
    f.c.commit_wave_selection(0.1, 0.3);
    let selected = f.c.slice.lock().unwrap().selection.clone().unwrap();
    f.c.enqueue_stop()
        .unwrap()
        .wait(Duration::from_secs(4))
        .unwrap();
    {
        let mut metadata = f.c.metadata.lock().unwrap();
        metadata.completed = metadata.sequence;
    }
    f.c.metadata_wake.notify_all();
    let c = f.c.clone();
    wait_for(&f.ui, move || {
        let state = c.slice.lock().unwrap();
        state.workers == 0 && state.ready.is_some()
    });
    assert!(selected.preview_cancel.load(Ordering::Acquire));
    assert!(!selected.cancel.load(Ordering::Acquire));
    assert!(!selected.render_cancel.load(Ordering::Acquire));
    assert!(f.c.selection_current(&selected));
    assert!(f.ui.get_wave_selection_active());
    let status = f.c.audio.status();
    assert!(
        status.file_path.is_none(),
        "a cancelled preview still loaded its file"
    );
    assert_eq!(status.play_session_id, 0);
    assert!(starts.try_iter().next().is_none());
}

#[test]
fn same_source_retains_selection_new_source_invalidates_and_workers_drain() {
    isolated("child_identity_and_workers");
}

#[test]
#[ignore = "isolated selection identity, tiny representation and worker lifetime regression"]
fn child_identity_and_workers() {
    let f = fixture();
    let source = bind_displayed(&f);
    let range = AudioFrameRange {
        start_frame: 1,
        end_frame: 2,
        sample_rate: 192_000,
    };
    let selected = selection(
        source.clone(),
        range,
        7,
        f.c.selection_generation.load(Ordering::Acquire),
    );
    {
        let mut state = f.c.slice.lock().unwrap();
        state.revision = 7;
        state.selection = Some(selected.clone());
    }
    f.ui.set_wave_selection_active(true);
    assert!(!f.c.publish_wave_source(&f.ui, &source));
    assert_eq!(
        f.c.slice
            .lock()
            .unwrap()
            .selection
            .as_ref()
            .unwrap()
            .revision,
        7
    );
    assert!(f.c.slice_needs_original(&f.a, 48_000));
    assert!(!f.c.slice_needs_original(&f.a, 192_000));
    assert!(!f.c.slice_needs_original(&f.b, 48_000));
    f.c.library_generation.fetch_add(1, Ordering::AcqRel);
    assert!(!f.c.selection_current(&selected));
    assert!(
        !f.c.publish_wave_source(&f.ui, &source),
        "stale source was republished"
    );
    let replacement = WaveSource::capture(&f.c, &f.b);
    assert!(f.c.publish_wave_source(&f.ui, &replacement));
    assert!(selected.cancel.load(Ordering::Acquire));
    assert!(f.c.slice.lock().unwrap().selection.is_none());
    assert!(!f.ui.get_wave_selection_active());

    let guard = Worker::new(&f.c);
    let (started, observed_start) = mpsc::sync_channel(1);
    let (finished, observed_finish) = mpsc::sync_channel(1);
    let c = f.c.clone();
    let waiter = std::thread::spawn(move || {
        started.send(()).unwrap();
        c.wait_slice_workers();
        finished.send(()).unwrap();
    });
    observed_start.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(matches!(
        observed_finish.recv_timeout(Duration::from_millis(60)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    drop(guard);
    observed_finish
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    waiter.join().unwrap();
    assert_eq!(f.c.slice.lock().unwrap().workers, 0);

    // Server auditions can retain A while a search task restores inspector B.
    // Transport refresh must keep A's selected Waveform, including after Stop,
    // but a confirmed source B must immediately take precedence.
    f.c.remote_mode.store(true, Ordering::Release);
    let mut remote_a = f.a.clone();
    remote_a.file_path = "/server/Sounds/Displayed A.wav".into();
    let mut remote_b = f.b.clone();
    remote_b.file_path = "/server/Sounds/Inspector B.wav".into();
    let path_a = f.root.join("mapped-a-preview.wav");
    let path_b = f.root.join("mapped-b-preview.wav");
    f.c.set_remote_wave_file_for_test(path_a.clone(), remote_a.clone());
    f.c.set_remote_wave_file_for_test(path_b.clone(), remote_b.clone());
    *f.c.selected.lock().unwrap() = Some(remote_b.clone());
    let source_a = WaveSource::capture(&f.c, &remote_a);
    assert!(source_a.remote);
    assert!(f.c.publish_wave_source(&f.ui, &source_a));
    let range = AudioFrameRange {
        start_frame: 200,
        end_frame: 600,
        sample_rate: 1000,
    };
    let current = selection(
        source_a,
        range,
        f.c.slice.lock().unwrap().revision,
        f.c.selection_generation.load(Ordering::Acquire),
    );
    f.c.slice.lock().unwrap().selection = Some(current.clone());
    let status_a = audiolib_audio::PlaybackStatus {
        generation: 10,
        state: PlaybackState::Stopped,
        file_path: Some(path_a),
        sample_rate: 48_000,
        duration: 1.0,
        playback_range: Some(range),
        ..Default::default()
    };
    assert_eq!(
        f.c.waveform_file(&status_a).unwrap().file_path,
        remote_a.file_path
    );
    f.c.selection_generation.fetch_add(1, Ordering::AcqRel);
    assert_eq!(
        f.c.waveform_file(&status_a).unwrap().file_path,
        remote_a.file_path,
        "Stop's admission revision discarded the selected remote Waveform"
    );
    f.c.clear_wave_selection();
    assert_eq!(
        f.c.waveform_file(&status_a).unwrap().file_path,
        remote_b.file_path,
        "clearing a selection did not restore ordinary inspector precedence"
    );
    let active_a = selection(
        current.source.clone(),
        range,
        f.c.slice.lock().unwrap().revision,
        f.c.selection_generation.load(Ordering::Acquire),
    );
    f.c.slice.lock().unwrap().selection = Some(active_a.clone());
    assert!(f.c.selection_current(&active_a));
    let status_b = audiolib_audio::PlaybackStatus {
        generation: 11,
        file_path: Some(path_b),
        playback_range: None,
        ..status_a
    };
    assert_eq!(
        f.c.waveform_file(&status_b).unwrap().file_path,
        remote_b.file_path,
        "a confirmed B source was incorrectly replaced with the old A selection"
    );
}

#[test]
fn source_change_cancels_delayed_original_download_and_refuses_old_ready() {
    isolated("child_remote_invalidation");
}

#[test]
fn same_length_original_change_invalidates_rendered_waveform_and_refuses_slice() {
    isolated("child_local_version_change");
}

#[test]
fn cancelled_completed_drag_copy_cannot_be_republished_by_its_delayed_callback() {
    isolated("child_cancelled_publication_callback");
}

#[test]
#[ignore = "isolated cancellation after copy completion but before UI publication regression"]
fn child_cancelled_publication_callback() {
    let f = fixture();
    bind_displayed(&f);
    f.c.commit_wave_selection(0.2, 0.6);
    let c = f.c.clone();
    wait_for(&f.ui, move || {
        let state = c.slice.lock().unwrap();
        state.ready.is_some() && state.workers == 0
    });
    let (selected, directory) = {
        let mut state = f.c.slice.lock().unwrap();
        state.drag_requested = true;
        (
            state.selection.clone().unwrap(),
            state.ready.as_ref().unwrap().export_directory.clone(),
        )
    };
    f.c.publish_slice_drag(selected.clone());
    // Deliberately do not pump the event loop: the real numbered-file copy has
    // completed, but its UI publication callback is still queued behind Cancel.
    f.c.wait_slice_workers();
    assert!(f.c.drag_busy.load(Ordering::Acquire));
    let unpublished: Vec<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
        })
        .collect();
    assert_eq!(
        unpublished.len(),
        1,
        "the real numbered copy was not prepared"
    );
    f.c.cancel_slice_preparation();
    assert!(selected.render_cancel.load(Ordering::Acquire));
    // The cancelled token is visible before any callback runs, so this test can
    // never enter native drag, irrespective of the real mouse button state.
    let c = f.c.clone();
    wait_for(&f.ui, move || !c.drag_busy.load(Ordering::Acquire));
    let state = f.c.slice.lock().unwrap();
    assert!(state.published.is_none());
    assert!(state.ready.is_none());
    assert!(!state.publishing);
    drop(state);
    assert!(
        unpublished.iter().all(|path| !path.exists()),
        "a cancelled delayed callback retained an unaccepted export"
    );
    assert!(std::fs::read_dir(&directory).unwrap().all(|entry| {
        let path = entry.unwrap().path();
        !path.is_file()
            || !path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
    }));
    assert_eq!(
        std::fs::read(&f.a.file_path).unwrap(),
        wav(&(0..1000).collect::<Vec<i16>>(), 1000)
    );
}

#[test]
#[ignore = "isolated same-length original replacement versus rendered Waveform regression"]
fn child_local_version_change() {
    let f = fixture();
    let source = bind_displayed(&f);
    assert!(source.local_stamp.is_some());
    source.check_local_version().unwrap();
    let selected = selection(
        source.clone(),
        AudioFrameRange {
            start_frame: 200,
            end_frame: 600,
            sample_rate: 1000,
        },
        f.c.slice.lock().unwrap().revision,
        f.c.selection_generation.load(Ordering::Acquire),
    );
    f.c.slice.lock().unwrap().selection = Some(selected.clone());
    let path = Path::new(&f.a.file_path);
    let old_size = std::fs::metadata(path).unwrap().len();
    let old_time = source.local_stamp.as_ref().unwrap().modified.unwrap();
    // Keep the duration, frame count and byte length identical. An explicit owned
    // timestamp change avoids relying on filesystem clock granularity or sleeps.
    std::fs::write(path, wav(&vec![-1234; 1000], 1000)).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(old_time + Duration::from_secs(4))
        .unwrap();
    assert_eq!(std::fs::metadata(path).unwrap().len(), old_size);
    assert!(source.check_local_version().is_err());
    f.ui.set_wave_selection_active(true);
    f.ui.set_wave_selection_preparing(true);
    f.c.prepare_wave_slice(selected.clone());
    let c = f.c.clone();
    let weak = f.ui.as_weak();
    wait_for(&f.ui, move || {
        c.slice.lock().unwrap().workers == 0
            && weak
                .upgrade()
                .is_some_and(|ui| !ui.get_wave_selection_preparing())
    });
    assert!(
        f.c.slice.lock().unwrap().ready.is_none(),
        "a slice was prepared from bytes newer than its displayed Waveform"
    );
    assert!(
        f.ui.get_wave_selection_progress()
            .contains("original changed")
    );
    assert!(f.c.audio.status().file_path.is_none());

    let mut refreshed = WaveSource::capture(&f.c, &f.a);
    refreshed.bind_local_version().unwrap();
    assert_ne!(source.key, refreshed.key);
    assert!(f.c.publish_wave_source(&f.ui, &refreshed));
    assert!(selected.cancel.load(Ordering::Acquire));
    assert!(f.c.slice.lock().unwrap().selection.is_none());
}

#[test]
#[ignore = "isolated source-bound delayed original-download cancellation regression"]
fn child_remote_invalidation() {
    let f = fixture();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (requested, observed_request) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let bytes = wav(&(0..1000).collect::<Vec<i16>>(), 1000);
    let server = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "original HEAD was never requested"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept original request: {error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut header = Vec::new();
        let mut byte = [0_u8; 1];
        while !header.ends_with(b"\r\n\r\n") {
            assert!(header.len() < 16 * 1024);
            socket.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
        }
        let header = String::from_utf8(header).unwrap();
        assert!(
            header.starts_with("HEAD /api/v1/audio/41/stream?download=1 "),
            "{header}"
        );
        requested.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(8)).unwrap();
        // Cancellation may already have closed the socket. The response merely
        // releases the mock; no original or preview is served after invalidation.
        let _ = write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nETag: \"owned-original\"\r\nConnection: close\r\n\r\n",
            bytes.len()
        );
    });
    f.c.config.lock().unwrap().data_source.server_url = url.clone();
    f.c.remote_mode.store(true, Ordering::Release);
    f.c.remote.lock().unwrap().state = Some(audiolib_remote::CacheState {
        library_id: "wave-selection-library".into(),
        revision: 1,
        database_file: "wave-selection-r1.db".into(),
        server_url: url,
        ..Default::default()
    });
    let mut file = f.a.clone();
    file.file_path = "/sounds/Displayed A.wav".into();
    let source = WaveSource::capture(&f.c, &file);
    assert!(source.remote);
    assert!(f.c.publish_wave_source(&f.ui, &source));
    let selected = selection(
        source,
        AudioFrameRange {
            start_frame: 200,
            end_frame: 600,
            sample_rate: 1000,
        },
        1,
        f.c.selection_generation.load(Ordering::Acquire),
    );
    {
        let mut state = f.c.slice.lock().unwrap();
        state.revision = 1;
        state.selection = Some(selected.clone());
    }
    f.c.prepare_wave_slice(selected.clone());
    let observed_request = std::sync::Mutex::new(observed_request);
    wait_for(&f.ui, move || {
        observed_request.lock().unwrap().try_recv().is_ok()
    });
    f.c.source_generation.fetch_add(1, Ordering::AcqRel);
    f.c.cancel_all_slices();
    assert!(selected.cancel.load(Ordering::Acquire));
    assert!(
        selected.render_cancel.load(Ordering::Acquire),
        "source invalidation left an original download running"
    );
    assert!(!f.c.selection_current(&selected));
    release.send(()).unwrap();
    let c = f.c.clone();
    wait_for(&f.ui, move || c.slice.lock().unwrap().workers == 0);
    server.join().unwrap();
    assert!(f.c.slice.lock().unwrap().ready.is_none());
    assert!(f.c.slice.lock().unwrap().selection.is_none());
    assert!(!f.ui.get_wave_selection_active());
    assert_eq!(f.c.audio.status().play_session_id, 0);
}
