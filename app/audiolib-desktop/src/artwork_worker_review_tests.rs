// app/audiolib-desktop/src/artwork_worker_review_tests.rs
//! Exercise cover action cancellation, immutable destinations and close draining.
use super::*;
use std::sync::mpsc;

fn background_ready(ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "Artwork worker did not settle");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn cancelled_and_failed_cover_actions_resume_automatic_loading() {
    isolated("album_review::management::worker_review::child_cancel_and_error_resume");
}

#[test]
#[ignore = "isolated artwork worker regression"]
fn child_cancel_and_error_resume() {
    let fixture = fixture();
    let file = selected_artwork_file(&fixture);
    let controller = &fixture.controller;
    let (manual, _) = override_paths(controller, &file);
    manual_cover(&manual);
    let generation = controller.source_generation.load(Ordering::Acquire);
    let context = controller.artwork_action_context(Some(&file));
    let cancelled = controller.begin_artwork_action();
    cancelled.complete(controller.perform_artwork_action(context, 2, None, false));
    assert!(controller.wait_artwork_actions().is_ok());
    let invalid = fixture.root.join("invalid.png");
    std::fs::write(&invalid, b"broken picture").unwrap();
    let context = controller.artwork_action_context(Some(&file));
    let rejected = controller.begin_artwork_action();
    let result = controller.perform_artwork_action(context, 2, Some(invalid), false);
    assert!(result.is_err());
    rejected.complete(result);
    assert!(
        controller.wait_artwork_actions().is_ok(),
        "Rejected picture is not an unsaved edit"
    );
    for result in [Ok(false), Err("Synthetic picture error".to_string())] {
        let cache = controller.artwork_cache_lock.lock().unwrap();
        fixture.ui.set_album_cover_ready(false);
        controller.queue_artwork(&file);
        let cancelled = controller.artwork_cancel.lock().unwrap().clone();
        controller.cancel_artwork();
        assert!(cancelled.load(Ordering::Acquire));
        fixture.ui.set_artwork_busy(true);
        controller.finish_artwork_action(&fixture.ui, generation, false, result.clone());
        assert!(!fixture.ui.get_artwork_busy());
        let resumed = controller.artwork_cancel.lock().unwrap().clone();
        assert!(!Arc::ptr_eq(&cancelled, &resumed));
        assert!(!resumed.load(Ordering::Acquire));
        if let Err(error) = result {
            assert_eq!(fixture.ui.get_artwork_message().as_str(), error);
        }
        drop(cache);
        wait_for(&fixture.ui, |ui| ui.get_album_cover_ready());
    }
    finish(&fixture);
}

#[test]
fn admitted_reload_keeps_the_original_source_destination() {
    isolated("album_review::management::worker_review::child_reload_source_destination");
}

#[test]
#[ignore = "isolated artwork worker regression"]
fn child_reload_source_destination() {
    let fixture = fixture();
    let file = selected_artwork_file(&fixture);
    let controller = &fixture.controller;
    let old_path = controller.cached_artwork_path(&file).unwrap();
    let other = Arc::new(SqliteProvider::open(&fixture.root.join("other.db")).unwrap());
    *controller.sqlite.write().unwrap() = other.clone();
    let new_path = controller.cached_artwork_path(&file).unwrap();
    *controller.sqlite.write().unwrap() = fixture.provider.clone();
    manual_cover(&old_path);
    manual_cover(&new_path);
    let context = controller.artwork_action_context(Some(&file));
    let guard = controller.begin_artwork_action();
    let cache = controller.artwork_cache_lock.lock().unwrap();
    let worker = controller.clone();
    let operation = std::thread::spawn(move || {
        let result = worker.perform_artwork_action(context, 0, None, false);
        guard.complete(result.clone());
        result
    });
    controller.source_generation.fetch_add(1, Ordering::AcqRel);
    *controller.sqlite.write().unwrap() = other;
    drop(cache);
    assert_eq!(operation.join().unwrap(), Ok(true));
    assert!(!old_path.exists());
    assert!(
        new_path.is_file(),
        "Old reload must not delete the new source's cover"
    );
    assert!(controller.wait_artwork_actions().is_ok());
    *controller.sqlite.write().unwrap() = fixture.provider.clone();
    finish(&fixture);
}

#[test]
fn disposable_cache_errors_do_not_leave_unsaved_artwork_failures() {
    isolated("album_review::management::worker_review::child_cache_errors_allow_close");
}

#[test]
#[ignore = "isolated artwork worker regression"]
fn child_cache_errors_allow_close() {
    let fixture = fixture();
    let file = selected_artwork_file(&fixture);
    let controller = &fixture.controller;
    let generated = controller.cached_artwork_path(&file).unwrap();
    // A directory occupying the expected PNG pathname makes remove_file fail on
    // every supported host, without changing permissions outside this fixture.
    std::fs::create_dir_all(&generated).unwrap();
    let context = controller.artwork_action_context(Some(&file));
    let reload = controller.begin_artwork_action();
    let result = controller.perform_artwork_action(context, 0, None, false);
    assert!(result.is_err());
    reload.complete(result);
    assert!(controller.wait_artwork_actions().is_ok());
    std::fs::remove_dir(&generated).unwrap();

    let directory = generated.parent().unwrap();
    std::fs::remove_dir(directory).unwrap();
    std::fs::write(directory, b"invalid disposable cache directory").unwrap();
    let context = controller.artwork_action_context(Some(&file));
    let clear = controller.begin_artwork_action();
    let result = controller.perform_artwork_action(context, 1, None, false);
    assert!(result.is_err());
    clear.complete(result);
    assert!(controller.wait_artwork_actions().is_ok());
    controller.request_close(&fixture.ui);
    background_ready(|| controller.close_completion.lock().unwrap().is_some());
    assert!(
        controller
            .close_completion
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .is_ok()
    );
    std::fs::remove_file(directory).unwrap();
    finish(&fixture);
}

#[test]
fn close_waits_for_admitted_cover_writes_and_retains_failures() {
    isolated("album_review::management::worker_review::child_cover_close_drain");
}

#[test]
#[ignore = "isolated artwork worker regression"]
fn child_cover_close_drain() {
    let fixture = fixture();
    let file = selected_artwork_file(&fixture);
    let controller = &fixture.controller;
    let (manual, hidden) = override_paths(controller, &file);
    manual_cover(&manual);
    let cache = controller.artwork_cache_lock.lock().unwrap();
    fixture.ui.invoke_artwork_action_requested(3);
    assert_eq!(controller.artwork_actions.lock().unwrap().pending, 1);
    let waiter = controller.clone();
    let (drained_tx, drained_rx) = mpsc::channel();
    let drain = std::thread::spawn(move || drained_tx.send(waiter.wait_artwork_actions()).unwrap());
    controller.request_close(&fixture.ui);
    assert!(controller.closing.load(Ordering::Acquire));
    assert!(drained_rx.recv_timeout(Duration::from_millis(100)).is_err());
    assert!(controller.close_completion.lock().unwrap().is_none());
    assert!(!hidden.exists());
    drop(cache);
    assert!(
        drained_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .is_ok()
    );
    drain.join().unwrap();
    background_ready(|| controller.close_completion.lock().unwrap().is_some());
    assert!(
        controller
            .close_completion
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .is_ok()
    );
    assert!(
        hidden.is_file(),
        "Close must finish the accepted hide operation"
    );

    // An I/O failure remains observable by close until an explicit successful
    // artwork operation resolves it. A picker cancellation is only a no-op.
    controller.closing.store(false, Ordering::Release);
    fixture.ui.set_closing(false);
    std::fs::create_dir_all(&manual).unwrap();
    let context = controller.artwork_action_context(Some(&file));
    let guard = controller.begin_artwork_action();
    let failed = controller.perform_artwork_action(context, 4, None, false);
    assert!(failed.is_err());
    guard.complete(failed);
    let cancellation = controller.begin_artwork_action();
    cancellation.complete(Ok(false));
    let error = controller.wait_artwork_actions().unwrap_err();
    assert!(error.contains("Artwork settings were not saved"));
    controller.request_close(&fixture.ui);
    background_ready(|| controller.close_completion.lock().unwrap().is_some());
    let close_error = controller.close_completion.lock().unwrap().take().unwrap();
    assert!(close_error.is_err());
    controller.finish_close(&fixture.ui, close_error);
    assert!(!controller.closing.load(Ordering::Acquire));
    assert!(
        fixture
            .ui
            .get_status()
            .contains("Artwork settings were not saved")
    );
    std::fs::remove_dir(&manual).unwrap();
    let context = controller.artwork_action_context(Some(&file));
    let retry = controller.begin_artwork_action();
    let result = controller.perform_artwork_action(context, 4, None, false);
    assert_eq!(result, Ok(true));
    retry.complete(result);
    assert!(controller.wait_artwork_actions().is_ok());
    finish(&fixture);
}

#[test]
fn slow_http_artwork_does_not_hold_the_cache_mutex() {
    isolated("album_review::management::worker_review::child_slow_http_cache_lock");
}

#[test]
#[ignore = "isolated artwork worker regression"]
fn child_slow_http_cache_lock() {
    use std::io::{Read, Write};
    let fixture = fixture();
    let file = selected_artwork_file(&fixture);
    let controller = &fixture.controller;
    *controller.selected.lock().unwrap() = Some(file.clone());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (started_tx, started_rx) = mpsc::channel();
    let server_stop = stop.clone();
    let server_release = release.clone();
    let server = std::thread::spawn(move || {
        let mut handlers = Vec::new();
        while !server_stop.load(Ordering::Acquire) {
            let (mut socket, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("Synthetic artwork server: {error}"),
            };
            // Accepted sockets inherit O_NONBLOCK on macOS/BSD; the handler reads synchronously.
            socket.set_nonblocking(false).unwrap();
            let started = started_tx.clone();
            let release = server_release.clone();
            handlers.push(std::thread::spawn(move || {
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if socket.read(&mut byte).unwrap_or(0) == 0 {
                        return;
                    }
                    request.push(byte[0]);
                    assert!(request.len() < 16 * 1024);
                }
                let request = String::from_utf8(request).unwrap();
                if request.starts_with("GET /api/v1/capabilities ") {
                    let body = serde_json::to_vec(&audiolib_remote::Capabilities {
                        compatibility: Some(audiolib_core::compatibility::compatibility_manifest()),
                        ..Default::default()
                    }).unwrap();
                    let header = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    socket.write_all(header.as_bytes()).unwrap();
                    socket.write_all(&body).unwrap();
                } else if request.contains("/artwork?side=512 ") {
                    started.send(()).unwrap();
                    let (released, wake) = &*release;
                    let mut released = released.lock().unwrap();
                    while !*released {
                        released = wake.wait(released).unwrap();
                    }
                    let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                } else {
                    let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
            }));
        }
        for handler in handlers {
            handler.join().unwrap();
        }
    });
    controller.config.lock().unwrap().data_source.server_url = url;
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(audiolib_remote::CacheState {
        library_id: "held-artwork".into(),
        schema_version: 7,
        ..Default::default()
    });
    controller.refresh_server_manage();
    let ready = controller.clone();
    wait_for(&fixture.ui, move |_| {
        ready.remote_artwork_context().is_some()
    });
    let job = artwork_job(controller, &file);
    let worker = controller.clone();
    let loading = std::thread::spawn(move || worker.load_artwork(&job));
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let cache = controller
        .artwork_cache_lock
        .try_lock()
        .expect("HTTP must not hold the cache mutex");
    drop(cache);
    // Clear is a real UI action and must finish before the held HTTP response.
    fixture.ui.invoke_artwork_action_requested(1);
    background_ready(|| controller.artwork_actions.lock().unwrap().pending == 0);
    assert!(controller.wait_artwork_actions().is_ok());
    let (released, wake) = &*release;
    *released.lock().unwrap() = true;
    wake.notify_all();
    assert!(loading.join().unwrap().is_none());
    stop.store(true, Ordering::Release);
    server.join().unwrap();
    controller.remote_mode.store(false, Ordering::Release);
    controller.remote.lock().unwrap().state = None;
    finish(&fixture);
}
