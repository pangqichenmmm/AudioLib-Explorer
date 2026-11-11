// app/audiolib-desktop/src/artwork_busy_tests.rs
//! Directory covers treat a full server slot as a short retry, not a user-facing failure.
use super::*;
use std::io::{Read, Write};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ArtworkMode {
    /// This many 503 BUSY replies (`Retry-After: 1`), then a PNG.
    BusyThenImage {
        busy_left: u32,
    },
    RootOffline,
    LibraryChanged,
    /// 409 `BUSY`: the variant is Busy, so it is retried. The message is irrelevant.
    ConflictThenImage {
        busy_left: u32,
    },
    /// 503 `UNAVAILABLE` whose message says "busy". Not [`RemoteError::Busy`].
    MisleadingUnavailable,
    Unauthorized,
    /// 200 PNG content type with a body that is not an image: protocol error.
    BadImage,
    /// 503 `BUSY` until the retry cap. The cached file must stay.
    AlwaysBusy,
    /// 503 BUSY with a long `Retry-After`, so cancellation must cut the wait short.
    LongBusy,
}

struct ArtworkHit {
    at: Instant,
    if_none_match: Option<String>,
}

struct ArtworkScript {
    mode: Mutex<ArtworkMode>,
    hits: Mutex<Vec<ArtworkHit>>,
    png: Vec<u8>,
}

fn wait_for_artwork_request(script: &ArtworkScript) {
    let deadline = Instant::now() + Duration::from_secs(12);
    while script.hits.lock().unwrap().is_empty() {
        assert!(
            Instant::now() < deadline,
            "Synthetic artwork server did not receive the request"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn request_header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

fn cover_png() -> Vec<u8> {
    let pixels = image::RgbaImage::from_pixel(2, 2, image::Rgba([12, 34, 56, 255]));
    let mut png = std::io::Cursor::new(Vec::new());
    pixels.write_to(&mut png, image::ImageFormat::Png).unwrap();
    png.into_inner()
}

fn respond(
    socket: &mut std::net::TcpStream,
    status: u16,
    content_type: &str,
    extra: &str,
    body: &[u8],
) {
    let head = format!(
        "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(head.as_bytes());
    let _ = socket.write_all(body);
}

#[test]
fn remote_cover_busy_retries_quietly_and_leaves_409_and_offline_alone() {
    isolated("album_review::management::busy::child_remote_cover_busy");
}

#[test]
#[ignore = "isolated remote cover busy backoff"]
fn child_remote_cover_busy() {
    let fixture = fixture();
    let file = selected_artwork_file(&fixture);
    let controller = &fixture.controller;
    *controller.selected.lock().unwrap() = None;
    controller.cancel_artwork();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let capabilities = serde_json::to_vec(&audiolib_remote::Capabilities {
        compatibility: Some(audiolib_core::compatibility::compatibility_manifest()),
        ..Default::default()
    })
    .unwrap();
    let script = Arc::new(ArtworkScript {
        mode: Mutex::new(ArtworkMode::BusyThenImage { busy_left: 1 }),
        hits: Mutex::new(Vec::new()),
        png: cover_png(),
    });
    let server_stop = stop.clone();
    let server_script = script.clone();
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
            let capabilities = capabilities.clone();
            let script = server_script.clone();
            handlers.push(std::thread::spawn(move || {
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if socket.read(&mut byte).unwrap_or(0) == 0 {
                        return;
                    }
                    request.push(byte[0]);
                    if request.len() > 16 * 1024 {
                        return;
                    }
                }
                let request = String::from_utf8_lossy(&request);
                let line = request.lines().next().unwrap_or("");
                if line.starts_with("GET /api/v1/capabilities ") {
                    respond(
                        &mut socket,
                        200,
                        "application/json",
                        "",
                        &capabilities,
                    );
                    return;
                }
                if !line.contains("/artwork") {
                    respond(&mut socket, 404, "application/json", "", b"{}");
                    return;
                }
                script.hits.lock().unwrap().push(ArtworkHit {
                    at: Instant::now(),
                    if_none_match: request_header(&request, "if-none-match").map(str::to_owned),
                });
                let mode = *script.mode.lock().unwrap();
                match mode {
                    ArtworkMode::BusyThenImage { busy_left } if busy_left > 0 => {
                        *script.mode.lock().unwrap() = ArtworkMode::BusyThenImage {
                            busy_left: busy_left - 1,
                        };
                        respond(
                            &mut socket,
                            503,
                            "application/json",
                            "Retry-After: 1\r\n",
                            br#"{"error_code":"BUSY","message":"slot queue elapsed"}"#,
                        );
                    }
                    ArtworkMode::BusyThenImage { .. } => {
                        respond(
                            &mut socket,
                            200,
                            "image/png",
                            "X-Library-Id: held-artwork\r\nETag: \"cover-busy\"\r\n",
                            &script.png,
                        );
                    }
                    ArtworkMode::RootOffline => respond(
                        &mut socket,
                        503,
                        "application/json",
                        "Retry-After: 5\r\n",
                        br#"{"error_code":"ROOT_OFFLINE","message":"Library root \"SFX\" is offline: missing"}"#,
                    ),
                    ArtworkMode::LibraryChanged => respond(
                        &mut socket,
                        409,
                        "application/json",
                        "",
                        br#"{"error_code":"LIBRARY_CHANGED","message":"The server library changed"}"#,
                    ),
                    ArtworkMode::ConflictThenImage { busy_left } if busy_left > 0 => {
                        *script.mode.lock().unwrap() = ArtworkMode::ConflictThenImage {
                            busy_left: busy_left - 1,
                        };
                        respond(
                            &mut socket,
                            409,
                            "application/json",
                            "Retry-After: 1\r\n",
                            br#"{"error_code":"BUSY","message":"conflict"}"#,
                        );
                    }
                    ArtworkMode::ConflictThenImage { .. } => {
                        respond(
                            &mut socket,
                            200,
                            "image/png",
                            "X-Library-Id: held-artwork\r\nETag: \"cover-busy\"\r\n",
                            &script.png,
                        );
                    }
                    ArtworkMode::MisleadingUnavailable => respond(
                        &mut socket,
                        503,
                        "application/json",
                        "Retry-After: 1\r\n",
                        br#"{"error_code":"UNAVAILABLE","message":"server busy please retry"}"#,
                    ),
                    ArtworkMode::Unauthorized => respond(
                        &mut socket,
                        401,
                        "application/json",
                        "",
                        br#"{"error_code":"UNAUTHORIZED","message":"token"}"#,
                    ),
                    ArtworkMode::BadImage => respond(
                        &mut socket,
                        200,
                        "image/png",
                        "X-Library-Id: held-artwork\r\n",
                        b"not-an-image",
                    ),
                    ArtworkMode::AlwaysBusy => respond(
                        &mut socket,
                        503,
                        "application/json",
                        "Retry-After: 1\r\n",
                        br#"{"error_code":"BUSY","message":"slot queue elapsed"}"#,
                    ),
                    ArtworkMode::LongBusy => respond(
                        &mut socket,
                        503,
                        "application/json",
                        "Retry-After: 30\r\n",
                        br#"{"error_code":"BUSY","message":"Artwork requests are busy; retry shortly"}"#,
                    ),
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
    *controller.selected.lock().unwrap() = Some(file.clone());
    let cached_path = controller.cached_artwork_path(&file).unwrap();
    std::fs::create_dir_all(cached_path.parent().unwrap()).unwrap();
    let cached_pixels = image::RgbaImage::from_pixel(2, 2, image::Rgba([9, 8, 7, 255]));
    crate::artwork::save_thumbnail(&cached_path, &cached_pixels).unwrap();
    std::fs::write(cached_path.with_extension("etag"), "\"cached-cover\"").unwrap();

    {
        *script.mode.lock().unwrap() = ArtworkMode::BusyThenImage { busy_left: 1 };
        script.hits.lock().unwrap().clear();
        let job = artwork_job(controller, &file);
        let pixels = controller.load_artwork(&job).expect("retried cover");
        assert_eq!(pixels.get_pixel(0, 0).0, [12, 34, 56, 255]);
        assert!(job.notice.lock().unwrap().is_none());
        let hits = script.hits.lock().unwrap();
        assert_eq!(hits.len(), 2, "503 BUSY + Retry-After: 1 is one retry");
        assert!(
            hits.iter()
                .all(|hit| hit.if_none_match.as_deref() == Some("\"cached-cover\"")),
            "every retry keeps If-None-Match: {:?}",
            hits.iter()
                .map(|hit| &hit.if_none_match)
                .collect::<Vec<_>>()
        );
        let gap = hits[1].at.saturating_duration_since(hits[0].at);
        assert!(
            gap >= Duration::from_millis(800),
            "Retry-After: 1 is one second, waited {gap:?}"
        );
        assert!(gap < Duration::from_secs(3), "{gap:?}");
    }

    {
        *script.mode.lock().unwrap() = ArtworkMode::RootOffline;
        script.hits.lock().unwrap().clear();
        let before = std::fs::read(&cached_path).unwrap();
        let job = artwork_job(controller, &file);
        assert!(controller.load_artwork(&job).is_some());
        assert!(job.notice.lock().unwrap().is_none());
        assert_eq!(script.hits.lock().unwrap().len(), 1);
        assert_eq!(std::fs::read(&cached_path).unwrap(), before);
    }

    {
        *script.mode.lock().unwrap() = ArtworkMode::LibraryChanged;
        script.hits.lock().unwrap().clear();
        let job = artwork_job(controller, &file);
        assert!(controller.load_artwork(&job).is_none());
        assert!(job.notice.lock().unwrap().is_none());
        assert_eq!(script.hits.lock().unwrap().len(), 1);
    }

    {
        *script.mode.lock().unwrap() = ArtworkMode::ConflictThenImage { busy_left: 1 };
        script.hits.lock().unwrap().clear();
        let job = artwork_job(controller, &file);
        assert!(controller.load_artwork(&job).is_some());
        assert!(job.notice.lock().unwrap().is_none());
        assert_eq!(
            script.hits.lock().unwrap().len(),
            2,
            "409 BUSY is RemoteError::Busy and is retried from the error code"
        );
    }

    {
        *script.mode.lock().unwrap() = ArtworkMode::MisleadingUnavailable;
        script.hits.lock().unwrap().clear();
        let job = artwork_job(controller, &file);
        let notice = controller.load_artwork(&job);
        assert!(notice.is_some(), "the previous cache is kept");
        let notice = job.notice.lock().unwrap().clone().unwrap();
        assert!(
            notice.contains("Directory cover could not load")
                || notice.contains("目录封面加载失败"),
            "{notice}"
        );
        assert_eq!(script.hits.lock().unwrap().len(), 1);
    }

    {
        *script.mode.lock().unwrap() = ArtworkMode::Unauthorized;
        script.hits.lock().unwrap().clear();
        let job = artwork_job(controller, &file);
        assert!(controller.load_artwork(&job).is_some());
        assert!(job.notice.lock().unwrap().is_some());
        assert_eq!(script.hits.lock().unwrap().len(), 1);
    }

    {
        *script.mode.lock().unwrap() = ArtworkMode::BadImage;
        script.hits.lock().unwrap().clear();
        let job = artwork_job(controller, &file);
        assert!(controller.load_artwork(&job).is_some());
        assert!(job.notice.lock().unwrap().is_some());
        assert_eq!(script.hits.lock().unwrap().len(), 1);
    }

    {
        *script.mode.lock().unwrap() = ArtworkMode::AlwaysBusy;
        script.hits.lock().unwrap().clear();
        let job = artwork_job(controller, &file);
        let pixels = controller.load_artwork(&job).expect("old cache");
        assert_eq!(pixels.get_pixel(0, 0).0, [12, 34, 56, 255]);
        assert!(
            job.notice.lock().unwrap().is_none(),
            "exhausted BUSY must not ask the user to sync"
        );
        assert_eq!(script.hits.lock().unwrap().len(), 4);
        assert!(
            cached_path.is_file(),
            "exhausted BUSY must not delete the cached cover"
        );
    }

    {
        *script.mode.lock().unwrap() = ArtworkMode::LongBusy;
        script.hits.lock().unwrap().clear();
        let job = artwork_job(controller, &file);
        let cancel = job.cancel.clone();
        let worker = controller.clone();
        let loading = std::thread::spawn(move || worker.load_artwork(&job));
        // Exercise cancellation of an admitted HTTP request, rather than
        // racing worker startup against the first UI timer tick.
        wait_for_artwork_request(&script);
        let started = Instant::now();
        wait_for(&fixture.ui, |_| true);
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "the cover retry wait must not block the UI event loop"
        );
        cancel.store(true, Ordering::Release);
        assert!(loading.join().unwrap().is_none());
        assert_eq!(script.hits.lock().unwrap().len(), 1);
    }

    {
        *script.mode.lock().unwrap() = ArtworkMode::LongBusy;
        script.hits.lock().unwrap().clear();
        let job = artwork_job(controller, &file);
        assert!(!job.cancel.load(Ordering::Acquire));
        let worker = controller.clone();
        let received = script.clone();
        let replacing = std::thread::spawn(move || {
            wait_for_artwork_request(&received);
            worker.artwork_epoch.fetch_add(1, Ordering::AcqRel);
        });
        let started = Instant::now();
        assert!(controller.load_artwork(&job).is_none());
        replacing.join().unwrap();
        assert!(job.notice.lock().unwrap().is_none());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a replaced artwork generation must not wait out Retry-After"
        );
        assert_eq!(script.hits.lock().unwrap().len(), 1);
        assert!(!job.cancel.load(Ordering::Acquire));
    }

    {
        *script.mode.lock().unwrap() = ArtworkMode::LongBusy;
        script.hits.lock().unwrap().clear();
        let job = artwork_job(controller, &file);
        let cancel = job.cancel.clone();
        let received = script.clone();
        let cancelling = std::thread::spawn(move || {
            wait_for_artwork_request(&received);
            cancel.store(true, Ordering::Release);
        });
        let started = Instant::now();
        assert!(controller.load_artwork(&job).is_none());
        cancelling.join().unwrap();
        assert!(job.notice.lock().unwrap().is_none());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a stale cover request must not wait out Retry-After"
        );
        assert_eq!(script.hits.lock().unwrap().len(), 1);
    }

    stop.store(true, Ordering::Release);
    server.join().unwrap();
    controller.remote_mode.store(false, Ordering::Release);
    controller.remote.lock().unwrap().state = None;
    finish(&fixture);
}
