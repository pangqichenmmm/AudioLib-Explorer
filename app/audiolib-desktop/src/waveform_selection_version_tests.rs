// app/audiolib-desktop/src/waveform_selection_version_tests.rs
//! Same-size remote replacements, progressive guards and verified offline reuse.
use super::*;
use audiolib_audio::{WaveformData, WaveformPeak, WaveformRequest};
use std::sync::atomic::AtomicUsize;

struct Mock {
    url: String,
    version: Arc<AtomicUsize>,
    offline: Arc<AtomicBool>,
    flip_during_waveform: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn mock() -> Mock {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let version = Arc::new(AtomicUsize::new(1));
    let offline = Arc::new(AtomicBool::new(false));
    let flip = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let state = (version.clone(), offline.clone(), flip.clone(), stop.clone());
    let worker = std::thread::spawn(move || {
        while !state.3.load(Ordering::Acquire) {
            let mut socket = match listener.accept() {
                Ok((socket, _)) => socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                Err(error) => panic!("mock accept: {error}"),
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut head = Vec::new();
            let mut byte = [0];
            while !head.ends_with(b"\r\n\r\n") {
                if socket.read_exact(&mut byte).is_err() {
                    break;
                }
                head.push(byte[0]);
                assert!(head.len() < 16 * 1024);
            }
            let head = String::from_utf8(head).unwrap();
            let first = head.lines().next().unwrap_or_default();
            let is_head = first.starts_with("HEAD ");
            let version = state.0.load(Ordering::Acquire);
            if state.1.load(Ordering::Acquire) {
                let body = br#"{"error_code":"ROOT_OFFLINE","message":"fixture offline"}"#;
                let _ = write!(
                    socket,
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if !is_head {
                    let _ = socket.write_all(body);
                }
                continue;
            }
            let path = first.split_whitespace().nth(1).unwrap_or_default();
            let is_waveform = path.contains("/waveform");
            let body = if is_waveform {
                let query = path
                    .split_once('?')
                    .map(|(_, query)| query)
                    .unwrap_or_default();
                let number = |key: &str, default: u64| {
                    query
                        .split('&')
                        .filter_map(|part| part.split_once('='))
                        .find(|(name, _)| *name == key)
                        .and_then(|(_, value)| value.parse().ok())
                        .unwrap_or(default)
                };
                let start = number("start_frame", 0);
                let end = number("end_frame", 1000);
                let buckets = number("buckets", 64).max(1);
                let frames = end - start;
                let spp = frames.div_ceil(buckets).max(1);
                let peak = WaveformPeak {
                    min: -(version as f32) / 10.0,
                    max: version as f32 / 10.0,
                    rms: 0.05,
                    ..Default::default()
                };
                let data = WaveformData {
                    sample_rate: 1000,
                    channels: 1,
                    frames,
                    samples_per_pixel: spp as u32,
                    peaks: vec![vec![peak; frames.div_ceil(spp) as usize]],
                };
                let mut json = serde_json::to_value(data).unwrap();
                json["start_frame"] = start.into();
                json["end_frame"] = end.into();
                json["total_frames"] = 1000.into();
                if state.2.swap(false, Ordering::AcqRel) {
                    state.0.fetch_add(1, Ordering::AcqRel);
                }
                serde_json::to_vec(&json).unwrap()
            } else {
                wav(&[version as i16 * 100; 1000], 1000)
            };
            let etag = if is_waveform {
                format!("\"waveform-v{version}\"")
            } else if path.contains("/preview") {
                format!("\"preview-v{version}\"")
            } else {
                format!("\"original-v{version}\"")
            };
            let _ = write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nETag: {}\r\nContent-Disposition: attachment; filename=original.wav\r\nConnection: close\r\n\r\n",
                if is_waveform {
                    "application/json"
                } else {
                    "audio/wav"
                },
                body.len(),
                etag
            );
            if !is_head {
                let _ = socket.write_all(&body);
            }
        }
    });
    Mock {
        url,
        version,
        offline,
        flip_during_waveform: flip,
        stop,
        worker: Some(worker),
    }
}

fn remote(f: &Fixture, server: &Mock) -> AudioFile {
    f.c.config.lock().unwrap().data_source.server_url = server.url.clone();
    f.c.remote_mode.store(true, Ordering::Release);
    f.c.remote.lock().unwrap().state = Some(audiolib_remote::CacheState {
        library_id: "versioned-wave-library".into(),
        revision: 1,
        database_file: "unchanged-r1.db".into(),
        server_url: server.url.clone(),
        ..Default::default()
    });
    let mut file = f.a.clone();
    file.file_path = "/server/Displayed A.wav".into();
    file
}

fn segment(
    f: &Fixture,
    version: &audiolib_remote::OriginalVersion,
    start: u64,
) -> Result<WaveformData, String> {
    f.c.remote_waveform_versioned_segment(
        (41, 1000),
        (
            WaveformRequest {
                start_frame: start,
                end_frame: Some(start + 64),
                samples_per_pixel: 1,
                include_colors: false,
            },
            64,
        ),
        &AtomicBool::new(false),
        &mut true,
        version,
    )
}

#[test]
fn same_size_remote_replacement_rejects_old_selection_and_offline_reuses_exact_version() {
    isolated("version_tests::child_same_size_remote_version");
}

#[test]
#[ignore = "isolated same-size replacement and verified offline regression"]
fn child_same_size_remote_version() {
    let f = fixture();
    let server = mock();
    let file = remote(&f, &server);
    let cancel = AtomicBool::new(false);
    let mut old = WaveSource::capture(&f.c, &file);
    old.bind_source_version(&f.c, &cancel).unwrap();
    let v1 = old.remote_version.clone().unwrap();
    let first = segment(&f, &v1, 0).unwrap();
    assert_eq!(first.peaks[0][0].max, 0.1);
    let context = f.c.remote_drag_source().unwrap();
    let stage = tempfile::tempdir_in(&f.root).unwrap();
    let original =
        f.c.prepare_remote_slice(
            &context,
            &file,
            &stage.path().join("original-v1.audio"),
            Some(&v1),
            &mut |_| {},
            &cancel,
        )
        .unwrap();
    assert_eq!(original.length, file.file_size);
    server.version.store(2, Ordering::Release);
    assert!(
        old.check_remote_version(&f.c, &cancel)
            .unwrap_err()
            .contains("original changed")
    );
    assert!(
        segment(&f, &v1, 64).is_err(),
        "A later segment may not mix version 2 into version 1"
    );
    assert!(
        f.c.prepare_remote_slice(
            &context,
            &file,
            &stage.path().join("wrong-version.audio"),
            Some(&v1),
            &mut |_| {},
            &cancel
        )
        .is_err()
    );
    assert!(!stage.path().join("wrong-version.audio").exists());
    let mut fresh = WaveSource::capture(&f.c, &file);
    fresh.bind_source_version(&f.c, &cancel).unwrap();
    let v2 = fresh.remote_version.clone().unwrap();
    assert_eq!(
        v1.length, v2.length,
        "Fixture replacement must have exactly the same byte length"
    );
    assert_ne!(old.key, fresh.key);
    let second = segment(&f, &v2, 0).unwrap();
    assert_eq!(
        second.peaks[0][0].max, 0.2,
        "v2 cannot reuse old memory/disk Waveform v1"
    );

    // Prime a complete v2 original, then simulate a disconnected server. Clearing
    // memory forces reuse of the exact versioned Waveform disk receipt.
    let _original_v2 =
        f.c.prepare_remote_slice(
            &context,
            &file,
            &stage.path().join("original-v2.audio"),
            Some(&v2),
            &mut |_| {},
            &cancel,
        )
        .unwrap();
    server.offline.store(true, Ordering::Release);
    f.c.clear_remote_waveform_memory_for_test();
    assert_eq!(segment(&f, &v2, 0).unwrap().peaks[0][0].max, 0.2);
    let offline =
        f.c.prepare_remote_slice(
            &context,
            &file,
            &stage.path().join("offline.audio"),
            Some(&v2),
            &mut |_| {},
            &cancel,
        )
        .unwrap();
    assert!(offline.offline);
    assert_eq!(offline.etag.as_deref(), Some(v2.etag.as_str()));
    // Keep guards alive until all exports have finished; no live cache bytes are
    // interpreted through a filename token to recover their original ETag.
    assert_eq!(original.etag.as_deref(), Some(v1.etag.as_str()));
}

#[test]
fn remote_replacement_during_a_segment_never_publishes_or_caches_it() {
    isolated("version_tests::child_replacement_during_segment");
}

#[test]
#[ignore = "isolated mid-segment source replacement regression"]
fn child_replacement_during_segment() {
    let f = fixture();
    let server = mock();
    let file = remote(&f, &server);
    let mut source = WaveSource::capture(&f.c, &file);
    source
        .bind_source_version(&f.c, &AtomicBool::new(false))
        .unwrap();
    let version = source.remote_version.unwrap();
    server.flip_during_waveform.store(true, Ordering::Release);
    assert!(segment(&f, &version, 0).is_err());
    assert_eq!(f.c.clear_remote_waveform_memory_for_test(), 0);
    let cache =
        f.c.config
            .lock()
            .unwrap()
            .paths
            .cache
            .join("waveforms/server");
    assert!(!cache.exists() || std::fs::read_dir(cache).unwrap().next().is_none());
}

#[test]
fn verified_low_quality_preview_keeps_its_representation_and_rejects_stale_bytes() {
    isolated("version_tests::child_preview_representation_version");
}

#[test]
#[ignore = "isolated preview representation proof and stale auto-play admission"]
fn child_preview_representation_version() {
    let f = fixture();
    let server = mock();
    let file = remote(&f, &server);
    let cancel = AtomicBool::new(false);
    let mut source = WaveSource::capture(&f.c, &file);
    source.bind_source_version(&f.c, &cancel).unwrap();
    let stream = audiolib_remote::MemoryStream::start(
        audiolib_remote::MemoryCache::global(),
        "verified-preview-v1",
        audiolib_remote::StreamSource::new(
            format!(
                "{}/api/v1/audio/41/preview?codec=pcm&sr=1000&bits=16",
                server.url
            ),
            audiolib_remote::SourceAuth::None,
        ),
    )
    .unwrap();
    stream.wait_complete(Duration::from_secs(3)).unwrap();
    let path = f.root.join("virtual-preview.wav");
    *f.c.remote_stream.lock().unwrap() = Some(remote_mode::RemotePlayback {
        path: path.clone(),
        file_id: 41,
        byte_rate: 2000,
        stream,
    });
    source.check_preview_version(&f.c, &path, &cancel).unwrap();
    assert!(
        f.c.remote_stream
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .stream
            .source()
            .url
            .contains("/preview?"),
        "A verified representation must preserve the user's preview route"
    );
    server.version.store(2, Ordering::Release);
    assert!(source.check_preview_version(&f.c, &path, &cancel).is_err());
    let mut refreshed = WaveSource::capture(&f.c, &file);
    refreshed.bind_source_version(&f.c, &cancel).unwrap();
    assert!(
        refreshed
            .check_preview_version(&f.c, &path, &cancel)
            .is_err(),
        "A stable v2 original cannot authorize the cached v1 preview representation"
    );
    assert_eq!(f.c.audio.status().play_session_id, 0);
}
