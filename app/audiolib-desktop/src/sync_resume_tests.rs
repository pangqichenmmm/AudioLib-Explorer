// app/audiolib-desktop/src/sync_resume_tests.rs
//! Owned loopback regressions for pause, startup suppression and source-sync close.
use super::super::*;
use super::source_transition_tests::{
    CapabilityProbeServer, accept, configuration, indexed_sounds, isolated_module, request,
    respond, root, run_until,
};
use super::*;
use audiolib_core::config::{ActiveSource, ServerProfile};

#[test]
fn pending_sync_publication_blocks_automatic_sync_and_defers_explicit_continue() {
    isolated_module("sync_resume_tests", "child_pending_sync_publication");
}

#[test]
#[ignore = "isolated Slint sync admission while a prior publication is queued"]
fn child_pending_sync_publication() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut config = configuration(&root.join("preferences"));
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.server_url = url;
    config.data_source.poll_interval_secs = 0;
    config.data_source.auto_sync = true;
    config.data_source.migrate_profiles();
    config.save().unwrap();
    let placeholder = open_remote_placeholder(&config).unwrap();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, placeholder.provider, engine.handle(), config, playback);
    controller.remote_mode.store(true, Ordering::Release);
    controller.publish_data_source(&ui);
    // Deterministically hold the same guard as a finished sync whose final UI
    // publication has not run. Its owned worker has already released busy.
    controller.remote_plays.lock().unwrap().publication_gates += 1;
    let publication = RemotePlayPublicationGate(controller.clone());
    ui.set_ds_syncing(true);
    let generation = controller.remote_generation.load(Ordering::Acquire);
    controller.apply_revision_check(
        &ui,
        None,
        RevisionCheck::NewRevision {
            cached: 0,
            server: 1,
        },
        None,
        true,
    );
    assert_eq!(
        controller.remote_generation.load(Ordering::Acquire),
        generation
    );
    assert!(!controller.remote_busy.load(Ordering::Acquire));
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    // An explicit continue waits for publication instead of disappearing while
    // busy or replacing the generation that still needs to open its cache.
    controller.start_sync_requested(false, false, false);
    assert_eq!(
        controller.remote_generation.load(Ordering::Acquire),
        generation
    );
    assert!(!controller.remote_busy.load(Ordering::Acquire));
    let released = Arc::new(AtomicBool::new(false));
    let server_released = released.clone();
    let server = std::thread::spawn(move || {
        let mut health = accept(&listener);
        assert!(server_released.load(Ordering::Acquire));
        assert!(request(&mut health).starts_with("GET /api/v1/health"));
        respond(
            &mut health,
            "200 OK",
            &serde_json::json!({"status":"ok","version":env!("CARGO_PKG_VERSION")}).to_string(),
        );
        let mut capabilities = accept(&listener);
        assert!(request(&mut capabilities).starts_with("GET /api/v1/capabilities "));
        respond(
            &mut capabilities,
            "200 OK",
            &serde_json::json!({
                "compatibility": audiolib_core::compatibility::compatibility_manifest(),
            })
            .to_string(),
        );
        let mut library = accept(&listener);
        assert!(request(&mut library).starts_with("GET /api/v1/library"));
        respond(
            &mut library,
            "401 Unauthorized",
            r#"{"error_code":"UNAUTHORIZED","message":"test completed"}"#,
        );
    });
    slint::Timer::single_shot(Duration::from_millis(100), move || {
        released.store(true, Ordering::Release);
        drop(publication);
    });
    let c = controller.clone();
    let weak = ui.as_weak();
    run_until(&ui, move || {
        c.remote_generation.load(Ordering::Acquire) > generation
            && !c.remote_sync_pending()
            && !weak.upgrade().unwrap().get_ds_syncing()
            && c.remote.lock().unwrap().last_error.is_some()
    });
    server.join().unwrap();
    assert_eq!(
        controller.remote_generation.load(Ordering::Acquire),
        generation + 1
    );
    assert_eq!(controller.remote_plays.lock().unwrap().publication_gates, 0);
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
fn pause_during_source_activation_releases_the_source_gate_and_keeps_progress() {
    isolated_module("sync_resume_tests", "child_pause_source_activation");
}

#[test]
#[ignore = "isolated Slint pause during a silent source activation"]
fn child_pause_source_activation() {
    held_source_activation(false);
}

#[test]
fn close_during_source_activation_cancels_silent_headers_without_recapturing_drafts() {
    isolated_module("sync_resume_tests", "child_close_source_activation");
}

#[test]
#[ignore = "isolated Slint close during a silent source activation"]
fn child_close_source_activation() {
    held_source_activation(true);
}

fn held_source_activation(close: bool) {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let (local, file) = indexed_sounds(&root.join("local"));
    let mut config = configuration(&root.join("preferences"));
    config.paths.database = local.database_path().into();
    config.data_source.poll_interval_secs = 0;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    config.data_source.profiles = vec![ServerProfile {
        id: "pending".into(),
        name: "Pending server".into(),
        server_url: url.clone(),
        ..Default::default()
    }];
    config.save().unwrap();
    let (entered, request_entered) = std::sync::mpsc::sync_channel(1);
    let (release, wait_release) = std::sync::mpsc::sync_channel(1);
    let (closed, observed_close) = std::sync::mpsc::sync_channel(1);
    let observed_close = std::rc::Rc::new(observed_close);
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        assert!(request(&mut socket).starts_with("GET /api/v1/health"));
        entered.send(()).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        // Never answer: observe the client's Abort, then retain the socket until
        // every final source/provider assertion has completed.
        match socket.peek(&mut [0u8; 1]) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::NotConnected
                ) => {}
            other => panic!("silent health connection did not close after cancellation: {other:?}"),
        }
        closed.send(Instant::now()).unwrap();
        wait_release.recv_timeout(Duration::from_secs(15)).unwrap();
        drop(socket);
    });
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1280.0, 900.0));
    let controller = Controller::new(
        &ui,
        local.clone(),
        engine.handle(),
        config.clone(),
        playback,
    );
    *controller.selected.lock().unwrap() = Some(file.clone());
    present_file(&ui, &file);
    ui.set_edit_notes("Save this local draft before changing source".into());
    ui.set_metadata_dirty(true);
    ui.show().unwrap();
    ui.invoke_open_library(0);
    crate::ui_tests::click_label(&ui, "Server settings");
    crate::ui_tests::click_label(&ui, "Switch to this source");
    let stopped = std::rc::Rc::new(std::cell::Cell::new(false));
    let requested_at = std::rc::Rc::new(std::cell::Cell::new(Instant::now()));
    let weak = ui.as_weak();
    let c = controller.clone();
    let stopped_observed = stopped.clone();
    let started_observed = requested_at.clone();
    let close_before_action = observed_close.clone();
    run_until(&ui, move || {
        let ui = weak.upgrade().unwrap();
        if !stopped_observed.get() {
            match request_entered.try_recv() {
                Ok(()) => {}
                Err(std::sync::mpsc::TryRecvError::Empty) => return false,
                Err(error) => panic!("activation request: {error}"),
            }
            assert!(ui.get_ds_source_syncing());
            assert!(c.source_changing.load(Ordering::Acquire));
            assert!(
                matches!(
                    close_before_action.try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ),
                "the silent health connection must remain open until pause or close"
            );
            started_observed.set(Instant::now());
            if close {
                c.request_close(&ui);
                assert!(c.closing.load(Ordering::Acquire));
            } else {
                crate::ui_tests::click_label(&ui, "Pause sync");
                assert!(ui.get_ds_sync_pausing());
                assert!(c.remote_cancel.lock().unwrap().is_paused());
            }
            stopped_observed.set(true);
            return false;
        }
        !close && ui.get_ds_sync_paused() && !ui.get_ds_syncing() && c.available()
    });
    assert!(stopped.get());
    let closed_at = observed_close
        .recv_timeout(Duration::from_secs(3))
        .expect("the silent health server must observe the cancelled connection close");
    // The three-second bound covers network cancellation. First-time schema-current
    // placeholder creation and provider/Metadata draining retain run_until's 12s bound.
    assert!(closed_at >= requested_at.get());
    assert!(closed_at.duration_since(requested_at.get()) < Duration::from_secs(3));
    assert!(!controller.remote_busy.load(Ordering::Acquire));
    assert_eq!(controller.metadata.lock().unwrap().sequence, 1);
    assert_eq!(
        local.get_audio(file.id).unwrap().notes,
        "Save this local draft before changing source"
    );
    if close {
        assert!(!ui.window().is_visible());
        assert!(
            controller
                .remote_cancel
                .lock()
                .unwrap()
                .cancellation()
                .load(Ordering::Acquire)
        );
    } else {
        assert!(controller.remote_read_only());
        assert!(!controller.source_changing.load(Ordering::Acquire));
        assert!(!ui.get_ds_applying());
        assert!(ui.get_ds_sync_pending());
        assert!(ui.get_ds_sync_info().contains("Sync paused"));
        assert_eq!(
            controller.provider().database_path(),
            RemoteCache::new(&config.data_root())
                .placeholder_database()
                .unwrap()
        );
        let pending = RemoteCache::new(&config.data_root())
            .pending_sync(&url)
            .unwrap()
            .unwrap();
        assert!(pending.manual_paused);
        assert_eq!(pending.downloaded_bytes, 0);
        let generation = controller.remote_generation.load(Ordering::Acquire);
        controller.start_sync(false, false);
        assert_eq!(
            controller.remote_generation.load(Ordering::Acquire),
            generation
        );
        assert!(!controller.remote_busy.load(Ordering::Acquire));
        assert_eq!(
            controller
                .config
                .lock()
                .unwrap()
                .data_source
                .active_source(),
            ActiveSource::Profile("pending".into())
        );
    }
    release.send(()).unwrap();
    server.join().unwrap();
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
fn manual_pause_survives_startup_and_does_not_block_revision_status_updates() {
    isolated_module("sync_resume_tests", "child_paused_startup");
}

#[test]
#[ignore = "isolated Slint restart with a persisted manual pause"]
fn child_paused_startup() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = root();
    let server = CapabilityProbeServer::start();
    let url = server.url.clone();
    let mut config = configuration(&root.join("preferences"));
    config.data_source.mode = DataSourceMode::Server;
    config.data_source.server_url = url.clone();
    config.data_source.poll_interval_secs = 0;
    config.data_source.auto_sync = true;
    config.data_source.migrate_profiles();
    config.save().unwrap();
    let cache = RemoteCache::new(&config.data_root());
    let cached_database = cache.library_dir("paused-cache").join("library-r1.db");
    std::fs::create_dir_all(cached_database.parent().unwrap()).unwrap();
    drop(SqliteProvider::open(&cached_database).unwrap());
    cache
        .save_state(&CacheState {
            library_id: "paused-cache".into(),
            server_url: url.clone(),
            revision: 1,
            schema_version: audiolib_db::SCHEMA_VERSION,
            database_file: "library-r1.db".into(),
            ..Default::default()
        })
        .unwrap();
    let control = SyncControl::new();
    control.pause();
    assert!(matches!(
        sync_library_with_options(
            &RemoteClient::new(&url, "").unwrap(),
            &cache,
            &mut |_| {},
            &control,
            SyncOptions::default()
        )
        .unwrap(),
        SyncRunOutcome::Paused(_)
    ));
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let startup = open_remote_placeholder(&config).unwrap();
    let controller = Controller::new_with_source(
        &ui,
        startup.provider.clone(),
        engine.handle(),
        config,
        playback,
        Some(startup),
    );
    assert!(!ui.get_library_visible());
    assert!(controller.remote.lock().unwrap().startup_check);
    let weak = ui.as_weak();
    let c = controller.clone();
    run_until(&ui, move || {
        let ui = weak.upgrade().unwrap();
        ui.get_ds_sync_paused() && !ui.get_ds_syncing() && c.available()
    });
    assert!(!ui.get_library_visible());
    assert!(!controller.remote.lock().unwrap().startup_check);
    let generation = controller.remote_generation.load(Ordering::Acquire);
    controller.apply_revision_check(
        &ui,
        None,
        RevisionCheck::NewRevision {
            cached: 0,
            server: 2,
        },
        None,
        true,
    );
    assert_eq!(
        controller.remote_generation.load(Ordering::Acquire),
        generation
    );
    assert!(ui.get_remote_notice().contains("Sync paused"));
    // Opening the retained cache may probe /capabilities, while the strict mock
    // still refuses all sync/download and playback/mutation requests.
    assert!(cache.pending_sync(&url).unwrap().unwrap().manual_paused);
    controller.shutdown();
    ui.hide().unwrap();
    server.finish();
}
