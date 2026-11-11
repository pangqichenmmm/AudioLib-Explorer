// app/audiolib-desktop/src/source_consistency_tests.rs
//! Isolated loopback regressions for snapshot audio, source-bound polls and drag downloads.
use super::super::*;
use super::source_transition_tests::{
    accept, configuration, isolated_module, request, respond, root, run_until,
};
use super::*;
use std::io::{Read, Write};
use std::sync::atomic::AtomicUsize;

fn fixture(url: &str) -> (Arc<Controller>, AppWindow, audiolib_audio::AudioEngine) {
    let root = root();
    let mut config = configuration(&root.join("preferences"));
    config.data_source.server_url = url.into();
    config.data_source.poll_interval_secs = 0;
    let provider = Arc::new(SqliteProvider::open(&root.join("snapshot.db")).unwrap());
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(CacheState {
        library_id: "library-a".into(),
        revision: 1,
        database_file: "library-r1.db".into(),
        server_url: url.into(),
        ..Default::default()
    });
    (controller, ui, engine)
}

fn wav(sample: i16) -> Vec<u8> {
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
    for _ in 0..480 {
        wav.extend(sample.to_le_bytes());
    }
    wav
}

fn file(bytes: &[u8]) -> AudioFile {
    AudioFile::from(audiolib_core::AudioFileData {
        id: 42,
        filename: "sound-a.wav".into(),
        file_path: "/sounds/sound-a.wav".into(),
        file_size: bytes.len() as u64,
        sample_rate: 48_000,
        bit_depth: 16,
        channels: 1,
        duration: 0.01,
        ..Default::default()
    })
}

fn binary(socket: &mut std::net::TcpStream, body: &[u8], etag: &str, head: bool) {
    write!(
        socket,
        "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nETag: \"{etag}\"\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    if !head {
        socket.write_all(body).unwrap();
    }
}

fn stream_bytes(controller: &Controller) -> Vec<u8> {
    let stream = controller
        .remote_stream
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .stream
        .clone();
    stream.wait_complete(Duration::from_secs(5)).unwrap();
    let mut bytes = Vec::new();
    stream.reader().unwrap().read_to_end(&mut bytes).unwrap();
    bytes
}

#[test]
fn a_new_snapshot_refetches_same_size_audio_while_annotations_keep_reusing_it() {
    isolated_module("source_consistency_tests", "child_snapshot_audio");
}

#[test]
#[ignore = "isolated snapshot audio identity regression"]
fn child_snapshot_audio() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let old = wav(12);
    let new = wav(25);
    assert_eq!(old.len(), new.len());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = requests.clone();
    let server = {
        let (old, new) = (old.clone(), new.clone());
        std::thread::spawn(move || {
            for (body, etag) in [(old, "old"), (new, "new")] {
                let mut socket = accept(&listener);
                let head = request(&mut socket);
                assert!(head.starts_with("GET /api/v1/audio/42/stream "), "{head}");
                seen.fetch_add(1, Ordering::AcqRel);
                binary(&mut socket, &body, etag, false);
            }
        })
    };
    let (controller, ui, _engine) = fixture(&url);
    let file = file(&old);
    controller
        .playable_path(&file, &AtomicBool::new(false), 0)
        .unwrap();
    assert_eq!(stream_bytes(&controller), old);
    // A server annotation increments revision but leaves the opened snapshot identity.
    controller
        .remote
        .lock()
        .unwrap()
        .state
        .as_mut()
        .unwrap()
        .revision = 2;
    controller
        .playable_path(&file, &AtomicBool::new(false), 0)
        .unwrap();
    assert_eq!(stream_bytes(&controller), old);
    assert_eq!(requests.load(Ordering::Acquire), 1);
    // A rescan sync replaces the snapshot but keeps the same record ID and byte length.
    {
        let mut remote = controller.remote.lock().unwrap();
        let state = remote.state.as_mut().unwrap();
        state.revision = 3;
        state.database_file = "library-r3.db".into();
    }
    controller
        .playable_path(&file, &AtomicBool::new(false), 0)
        .unwrap();
    assert_eq!(stream_bytes(&controller), new);
    assert_eq!(requests.load(Ordering::Acquire), 2);
    server.join().unwrap();
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
fn delayed_poll_roots_cannot_cross_a_source_change_with_the_same_library_identity() {
    isolated_module("source_consistency_tests", "child_delayed_poll_roots");
}

#[test]
#[ignore = "isolated delayed poll publication regression"]
fn child_delayed_poll_roots() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (waiting, waited) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        assert!(request(&mut socket).starts_with("GET /api/v1/roots "));
        waiting.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
        respond(
            &mut socket,
            "200 OK",
            r#"[{"name":"sounds","server_path":"/sounds","online":false,"status":"offline"}]"#,
        );
    });
    let (controller, ui, _engine) = fixture(&url);
    let checked = Some(CachedIdentity {
        library_id: "library-a".into(),
        revision: 1,
    });
    let completed = Arc::new(AtomicBool::new(false));
    let delivered = completed.clone();
    let c = controller.clone();
    let client = RemoteClient::new(&url, "").unwrap();
    let worker = std::thread::spawn(move || {
        let roots = Some(client.roots().unwrap());
        let c2 = c.clone();
        post(c.ui.clone(), move |ui| {
            c2.apply_revision_poll(
                &ui,
                RevisionPoll {
                    source_generation: 0,
                    checked,
                    check: RevisionCheck::UpToDate { revision: 1 },
                    roots,
                    next_check: None,
                    auto_due: false,
                },
            );
            delivered.store(true, Ordering::Release);
        });
    });
    waited.recv_timeout(Duration::from_secs(5)).unwrap();
    // Two saved profiles can serve the same library/revision, so identity alone is
    // insufficient. The source change must discard this already-running A poll.
    controller.source_generation.fetch_add(1, Ordering::AcqRel);
    controller.config.lock().unwrap().data_source.server_url = "http://127.0.0.1:9".into();
    let online = RemoteRoot {
        name: "sounds".into(),
        server_path: "/sounds".into(),
        online: Some(true),
        status: Some("online".into()),
        ..Default::default()
    };
    controller.remote.lock().unwrap().live_roots = Some(vec![online.clone()]);
    release.send(()).unwrap();
    run_until(&ui, move || completed.load(Ordering::Acquire));
    assert_eq!(
        controller.remote.lock().unwrap().live_roots,
        Some(vec![online])
    );
    assert!(controller.remote.lock().unwrap().check.is_none());
    assert!(ui.get_remote_notice().is_empty());
    // Current source results still publish both its roots and revision check.
    controller.apply_revision_poll(
        &ui,
        RevisionPoll {
            source_generation: 1,
            checked: Some(CachedIdentity {
                library_id: "library-a".into(),
                revision: 1,
            }),
            check: RevisionCheck::UpToDate { revision: 1 },
            roots: Some(vec![]),
            next_check: Some(123),
            auto_due: false,
        },
    );
    assert_eq!(controller.remote.lock().unwrap().live_roots, Some(vec![]));
    assert_eq!(
        controller.remote.lock().unwrap().check,
        Some(RevisionCheck::UpToDate { revision: 1 })
    );
    server.join().unwrap();
    worker.join().unwrap();
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
fn drag_downloads_keep_the_captured_server_and_source_changes_cancel_publication() {
    isolated_module("source_consistency_tests", "child_drag_source");
}

#[test]
#[ignore = "isolated source-bound drag download regression"]
fn child_drag_source() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let bytes = wav(35);
    let listener_a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let listener_b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url_a = format!("http://{}", listener_a.local_addr().unwrap());
    let url_b = format!("http://{}", listener_b.local_addr().unwrap());
    let server = {
        let bytes = bytes.clone();
        std::thread::spawn(move || {
            for method in ["HEAD", "GET"] {
                let mut socket = accept(&listener_a);
                let head = request(&mut socket);
                assert!(
                    head.starts_with(&format!("{method} /api/v1/audio/42/stream?download=1 ")),
                    "{head}"
                );
                binary(&mut socket, &bytes, "a", method == "HEAD");
            }
        })
    };
    let (controller, ui, _engine) = fixture(&url_a);
    let captured = controller.remote_drag_source().unwrap();
    let source_generation = controller.source_generation.load(Ordering::Acquire);
    let library_generation = controller.library_generation.load(Ordering::Acquire);
    let pending = Arc::new(AtomicBool::new(false));
    *controller.drag_cancel.lock().unwrap() = pending.clone();
    let barrier = controller.begin_source_change(&ui).unwrap();
    controller.wait_metadata(barrier, true).unwrap();
    assert!(
        pending.load(Ordering::Acquire),
        "a source change left the drag alive"
    );
    assert!(!controller.drag_request_current(source_generation, library_generation, &pending));
    controller.config.lock().unwrap().data_source.server_url = url_b;
    controller.remote.lock().unwrap().state = Some(CacheState {
        library_id: "library-b".into(),
        revision: 1,
        database_file: "library-b-r1.db".into(),
        ..Default::default()
    });
    // Even a delayed worker which reaches preparation must use its captured A context,
    // and source-generation validation still refuses its eventual publication.
    let paths = controller
        .prepare_remote_drag(
            &captured,
            &[file(&bytes)],
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(std::fs::read(&paths[0]).unwrap(), bytes);
    assert!(
        paths[0].starts_with(
            controller
                .config
                .lock()
                .unwrap()
                .paths
                .cache
                .join("library-a")
        )
    );
    assert!(!controller.drag_request_current(
        source_generation,
        library_generation,
        &AtomicBool::new(false)
    ));
    listener_b.set_nonblocking(true).unwrap();
    assert!(
        matches!(listener_b.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    server.join().unwrap();
    controller.finish_source_change(&ui, true);
    controller.shutdown();
    ui.hide().unwrap();
}
