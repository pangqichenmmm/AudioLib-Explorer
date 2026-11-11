// crates/audiolib-remote/src/resume_regression_tests.rs
//! Snapshot recovery regressions using only owned SQLite files and loopback HTTP.
use crate::test_server::{TestRequest, TestResponse, TestServer};
use crate::{
    CacheState, CacheUpdate, RemoteCache, RemoteClient, RemoteError, SyncProgress,
    apply_server_write, sync_library,
};
use audiolib_core::DataProvider;
use audiolib_db::SqliteProvider;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const LIBRARY: &str = "resume-regression-library";

fn valid_snapshot(directory: &Path) -> Vec<u8> {
    let path = directory.join("legacy-snapshot.db");
    let connection = rusqlite::Connection::open(&path).unwrap();
    audiolib_db::register_library_functions(&connection).unwrap();
    connection
        .execute_batch(include_str!(
            "../../audiolib-db/tests/fixtures/schema-v5.sql"
        ))
        .unwrap();
    drop(connection);
    std::fs::read(path).unwrap()
}

fn library_response(revision: u64) -> TestResponse {
    TestResponse::json(
        200,
        &format!(r#"{{"library_id":"{LIBRARY}","schema_version":5,"revision":{revision}}}"#),
    )
}

fn snapshot_response(body: Vec<u8>, revision: u64) -> TestResponse {
    TestResponse::bytes(200, body)
        .with_header("X-Library-Id", LIBRARY)
        .with_header("X-Library-Revision", &revision.to_string())
        .with_header("X-Schema-Version", "5")
}

fn ordinary_response(request: &TestRequest, revision: u64) -> TestResponse {
    match request.path.as_str() {
        "/api/v1/health" => crate::test_server::marker_health(),
        "/api/v1/capabilities" => crate::test_server::marker_capabilities(serde_json::json!({})),
        "/api/v1/library" => library_response(revision),
        "/api/v1/roots" => TestResponse::json(200, "[]"),
        _ => panic!("unexpected request {}", request.path),
    }
}

fn modern_snapshot_response(
    body: Vec<u8>,
    offset: Option<usize>,
    wire_length: usize,
    output_length: usize,
) -> TestResponse {
    use audiolib_core::compatibility::{
        LIBRARY_FORMAT_HEADER, LibraryFormat, PLAN_B_FORMAT, SERVER_COMPATIBILITY_HEADER,
        SNAPSHOT_FORMAT_HEADER, compatibility_manifest,
    };
    let mut reply = TestResponse::bytes(if offset.is_some() { 206 } else { 200 }, body)
        .with_header("X-Library-Id", LIBRARY)
        .with_header("X-Library-Revision", "7")
        .with_header("X-Schema-Version", &audiolib_db::SCHEMA_VERSION.to_string())
        .with_header("X-Snapshot-Id", "modern-pinned-object")
        .with_header("ETag", "\"modern-pinned-object\"")
        .with_header("Accept-Ranges", "bytes")
        .with_header("Content-Encoding", "zstd")
        .with_header("X-Uncompressed-Length", &output_length.to_string())
        .with_header(SNAPSHOT_FORMAT_HEADER, PLAN_B_FORMAT)
        .with_header(
            LIBRARY_FORMAT_HEADER,
            &serde_json::to_string(&LibraryFormat::target()).unwrap(),
        )
        .with_header(
            SERVER_COMPATIBILITY_HEADER,
            &serde_json::to_string(&compatibility_manifest()).unwrap(),
        )
        .with_header("x-search-storage-revision", "1")
        .with_header("x-tokenization-revision", "3")
        .with_header("x-source-search-revision", "2")
        .with_header("x-source-tags-revision", "2");
    if let Some(offset) = offset {
        reply = reply.with_header(
            "Content-Range",
            &format!("bytes {offset}-{}/{wire_length}", wire_length - 1),
        );
    }
    reply
}

#[test]
fn modern_pinned_resume_keeps_its_format_when_the_live_revision_advances() {
    use audiolib_core::compatibility::{
        ACCEPT_FORMATS_HEADER, CLIENT_COMPATIBILITY_HEADER, LibraryFormat, PLAN_B_FORMAT,
        SnapshotFormat, compatibility_manifest,
    };
    let directory = tempfile::tempdir().unwrap();
    let source = SqliteProvider::open(&directory.path().join("modern.db")).unwrap();
    let backup = directory.path().join("modern-backup.db");
    source.backup_library(&backup).unwrap();
    drop(source);
    let bytes = std::fs::read(backup).unwrap();
    let packed = zstd::encode_all(bytes.as_slice(), 3).unwrap();
    let cut = packed.len() / 2;
    let offsets = Arc::new(Mutex::new(Vec::new()));
    let revision = Arc::new(AtomicUsize::new(7));
    let incompatible = Arc::new(AtomicBool::new(false));
    let server = {
        let offsets = offsets.clone();
        let revision = revision.clone();
        let incompatible = incompatible.clone();
        TestServer::start(move |request| {
            if request.path.starts_with("/api/v1/library/snapshot") {
                let catalog: audiolib_core::compatibility::CompatibilityManifest =
                    serde_json::from_str(request.header(CLIENT_COMPATIBILITY_HEADER).unwrap())
                        .unwrap();
                assert_eq!(catalog, compatibility_manifest());
                assert!(
                    request
                        .header(ACCEPT_FORMATS_HEADER)
                        .unwrap()
                        .contains(PLAN_B_FORMAT)
                );
                let offset = request.header("Range").map(|range| {
                    range
                        .strip_prefix("bytes=")
                        .unwrap()
                        .strip_suffix('-')
                        .unwrap()
                        .parse::<usize>()
                        .unwrap()
                });
                offsets.lock().unwrap().push(offset);
                if let Some(offset) = offset {
                    assert_eq!(request.header("If-Range"), Some("\"modern-pinned-object\""));
                    assert!(request.path.ends_with("?snapshot_id=modern-pinned-object"));
                    modern_snapshot_response(
                        packed[offset..].to_vec(),
                        Some(offset),
                        packed.len(),
                        bytes.len(),
                    )
                } else {
                    modern_snapshot_response(packed.clone(), None, packed.len(), bytes.len())
                        .cut_after(cut)
                }
            } else {
                match request.path.as_str() {
                    "/api/v1/health" => TestResponse::json(200, &serde_json::json!({"status":"ok","version":if incompatible.load(Ordering::SeqCst) { "99.0.0" } else { env!("CARGO_PKG_VERSION") }}).to_string()),
                    "/api/v1/capabilities" => TestResponse::json(200, &serde_json::json!({"compatibility":compatibility_manifest()}).to_string()),
                    "/api/v1/library" => TestResponse::json(200, &serde_json::json!({
                        "library_id":LIBRARY,"schema_version":audiolib_db::SCHEMA_VERSION,"revision":revision.load(Ordering::SeqCst),
                        "snapshot_format":SnapshotFormat { id:PLAN_B_FORMAT.into(),format:LibraryFormat::target() }
                    }).to_string()),
                    "/api/v1/roots" => TestResponse::json(200, "[]"),
                    _ => panic!("unexpected request {}", request.path),
                }
            }
        })
    };
    let cache = RemoteCache::new(&directory.path().join("client"));
    let client = RemoteClient::new(&server.url, "").unwrap();
    {
        let mut session =
            crate::snapshot_transfer::TransferSession::open(&cache, client.api_url(), false)
                .unwrap();
        session.bind_library(LIBRARY).unwrap();
        let error = session
            .download(
                &client,
                &directory.path().join("interrupted.db"),
                &mut |_| {},
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert!(error.is_unreachable(), "{error:?}");
        assert_eq!(session.pending().downloaded_bytes, cut as u64);
    }
    incompatible.store(true, Ordering::SeqCst);
    let error =
        sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap_err();
    assert!(
        matches!(error, RemoteError::PeerIncompatible { .. }),
        "{error:?}"
    );
    assert_eq!(
        cache
            .pending_sync(client.api_url())
            .unwrap()
            .unwrap()
            .downloaded_bytes,
        cut as u64,
        "a failed peer preflight must not discard a valid durable prefix"
    );
    assert_eq!(*offsets.lock().unwrap(), vec![None]);
    incompatible.store(false, Ordering::SeqCst);
    revision.store(8, Ordering::SeqCst);
    let outcome =
        sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
    assert_eq!(
        outcome.state.revision, 7,
        "the cache must name the actual pinned revision"
    );
    assert_eq!(outcome.state.schema_version, audiolib_db::SCHEMA_VERSION);
    let format = outcome.state.format.unwrap();
    assert_eq!(format.source, Some(LibraryFormat::target()));
    assert_eq!(format.local, LibraryFormat::target());
    assert_eq!(*offsets.lock().unwrap(), vec![None, Some(cut)]);
}

#[test]
fn pausing_at_durable_flush_replays_complete_bytes_without_another_download() {
    let directory = tempfile::tempdir().unwrap();
    let good = valid_snapshot(directory.path());
    let packed = zstd::encode_all(good.as_slice(), 3).unwrap();
    let wire_length = packed.len() as u64;
    let requests = Arc::new(AtomicUsize::new(0));
    let server = {
        let requests = requests.clone();
        TestServer::start(move |request| {
            if request.path == "/api/v1/library/snapshot" {
                requests.fetch_add(1, Ordering::SeqCst);
                snapshot_response(packed.clone(), 7)
                    .with_header("Content-Encoding", "zstd")
                    .with_header("X-Uncompressed-Length", &good.len().to_string())
                    .with_header("ETag", "\"complete-wire-object\"")
                    .with_header("X-Snapshot-Id", "complete-wire-object")
                    .with_header("Accept-Ranges", "bytes")
            } else {
                ordinary_response(request, 7)
            }
        })
    };
    let cache = RemoteCache::new(&directory.path().join("client"));
    let client = RemoteClient::new(&server.url, "").unwrap();
    let control = crate::SyncControl::new();
    let paused = crate::sync_library_with_options(
        &client,
        &cache,
        &mut |event| {
            if event == SyncProgress::Writing {
                control.pause();
            }
        },
        &control,
        crate::SyncOptions::default(),
    )
    .unwrap();
    assert!(matches!(paused, crate::SyncRunOutcome::Paused(_)));
    let pending = cache.pending_sync(client.api_url()).unwrap().unwrap();
    assert!(pending.manual_paused);
    assert_eq!(pending.downloaded_bytes, wire_length);
    assert!(cache.active().is_none());

    let mut events = Vec::new();
    let resumed = crate::sync_library_with_options(
        &client,
        &cache,
        &mut |event| events.push(event),
        &crate::SyncControl::new(),
        crate::SyncOptions::default(),
    )
    .unwrap();
    let crate::SyncRunOutcome::Completed(outcome) = resumed else {
        panic!("resume must complete");
    };
    assert!(events.contains(&SyncProgress::Writing));
    assert!(events.contains(&SyncProgress::Replaying {
        received: wire_length,
        total: Some(wire_length),
    }));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(SqliteProvider::open_read_only(&cache.database_path(&outcome.state)).is_ok());
}

#[test]
fn rejected_complete_sqlite_spool_does_not_poison_the_next_sync() {
    let directory = tempfile::tempdir().unwrap();
    let good = valid_snapshot(directory.path());
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(AtomicUsize::new(0));
    let server = {
        let ranges = ranges.clone();
        let requests = requests.clone();
        TestServer::start(move |request| {
            if request.path.starts_with("/api/v1/library/snapshot") {
                ranges
                    .lock()
                    .unwrap()
                    .push(request.header("Range").map(str::to_owned));
                let first = requests.fetch_add(1, Ordering::SeqCst) == 0;
                snapshot_response(
                    if first {
                        vec![0x5a; 8192]
                    } else {
                        good.clone()
                    },
                    7,
                )
                .with_header("ETag", "\"fixed-snapshot\"")
                .with_header("X-Snapshot-Id", "fixed-snapshot")
                .with_header("Accept-Ranges", "bytes")
            } else {
                ordinary_response(request, 7)
            }
        })
    };
    let cache = RemoteCache::new(&directory.path().join("client"));
    let client = RemoteClient::new(&server.url, "").unwrap();
    let cancel = AtomicBool::new(false);
    let error = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap_err();
    assert!(matches!(error, RemoteError::Snapshot(_)), "{error:?}");
    assert!(cache.active().is_none());
    assert_eq!(
        cache
            .pending_sync(client.api_url())
            .unwrap()
            .unwrap()
            .downloaded_bytes,
        0,
        "a rejected SQLite object must not remain a reusable complete prefix"
    );

    let outcome = sync_library(&client, &cache, &mut |_| {}, &cancel, false).unwrap();
    assert!(outcome.downloaded);
    assert_eq!(outcome.state.revision, 7);
    assert!(SqliteProvider::open_read_only(&cache.database_path(&outcome.state)).is_ok());
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(*ranges.lock().unwrap(), vec![None, None]);
}

#[test]
fn legacy_unknown_length_zstd_truncation_retries_a_full_download() {
    let directory = tempfile::tempdir().unwrap();
    let good = valid_snapshot(directory.path());
    let packed = zstd::encode_all(good.as_slice(), 3).unwrap();
    let output_length = good.len();
    let requests = Arc::new(AtomicUsize::new(0));
    let server = {
        let requests = requests.clone();
        TestServer::start(move |request| {
            if request.path == "/api/v1/library/snapshot" {
                assert!(request.header("Range").is_none());
                assert!(request.header("If-Range").is_none());
                let first = requests.fetch_add(1, Ordering::SeqCst) == 0;
                let response = snapshot_response(packed.clone(), 7)
                    .with_header("Content-Encoding", "zstd")
                    .with_header("X-Uncompressed-Length", &output_length.to_string())
                    .without_content_length();
                if first {
                    response.cut_after(packed.len() / 2)
                } else {
                    response
                }
            } else {
                ordinary_response(request, 7)
            }
        })
    };
    let cache = RemoteCache::new(&directory.path().join("client"));
    let client = RemoteClient::new(&server.url, "").unwrap();
    let outcome =
        sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
    assert!(outcome.downloaded);
    assert_eq!(outcome.state.revision, 7);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert!(SqliteProvider::open_read_only(&cache.database_path(&outcome.state)).is_ok());
}

#[test]
fn pinned_old_revision_cannot_replace_a_cache_advanced_during_download() {
    let directory = tempfile::tempdir().unwrap();
    let good = valid_snapshot(directory.path());
    let server = {
        let good = good.clone();
        TestServer::start(move |request| {
            if request.path == "/api/v1/library/snapshot" {
                snapshot_response(good.clone(), 7)
            } else {
                ordinary_response(request, 7)
            }
        })
    };
    let cache = RemoteCache::new(&directory.path().join("client"));
    let original = CacheState {
        library_id: LIBRARY.into(),
        revision: 7,
        schema_version: 5,
        database_file: "original-r7.db".into(),
        server_url: server.url.clone(),
        ..Default::default()
    };
    std::fs::create_dir_all(cache.library_dir(LIBRARY)).unwrap();
    let original_path = cache.database_path(&original);
    std::fs::write(&original_path, &good).unwrap();
    cache.save_state(&original).unwrap();
    let client = RemoteClient::new(&server.url, "").unwrap();
    let mut advanced = false;
    let result = sync_library(
        &client,
        &cache,
        &mut |progress| {
            if progress == SyncProgress::Validating && !advanced {
                assert_eq!(
                    apply_server_write(&cache, LIBRARY, &[], 7, 10),
                    CacheUpdate::Applied { revision: 10 }
                );
                advanced = true;
            }
        },
        &AtomicBool::new(false),
        false,
    );
    assert!(
        advanced,
        "the cache must advance before snapshot publication"
    );
    assert!(
        matches!(result, Err(RemoteError::Snapshot(_))),
        "{result:?}"
    );
    let kept = cache.active().unwrap();
    assert_eq!(kept.revision, 10);
    assert_eq!(kept.database_file, original.database_file);
    assert_eq!(std::fs::read(original_path).unwrap(), good);
    assert_eq!(
        cache
            .pending_sync(client.api_url())
            .unwrap()
            .unwrap()
            .downloaded_bytes,
        0
    );
}

fn transfer_files(cache: &RemoteCache) -> (std::path::PathBuf, std::path::PathBuf) {
    let directory = std::fs::read_dir(cache.root().join(".transfers"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap();
    (
        directory.join("payload.part"),
        directory.join("checkpoint.json"),
    )
}

#[test]
fn published_transfer_reuses_verified_cache_until_provider_swap_acknowledges_it() {
    let directory = tempfile::tempdir().unwrap();
    let good = valid_snapshot(directory.path());
    let requests = Arc::new(AtomicUsize::new(0));
    let server = {
        let requests = requests.clone();
        TestServer::start(move |request| {
            if request.path == "/api/v1/library/snapshot" {
                requests.fetch_add(1, Ordering::SeqCst);
                snapshot_response(good.clone(), 7)
            } else {
                ordinary_response(request, 7)
            }
        })
    };
    let cache = RemoteCache::new(&directory.path().join("client"));
    let client = RemoteClient::new(&server.url, "").unwrap();
    let first = sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
    let (payload, checkpoint) = transfer_files(&cache);
    assert!(
        payload.is_file(),
        "publication must not acknowledge the provider swap"
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
    assert_eq!(
        receipt["published_database_file"],
        first.state.database_file
    );
    assert!(cache.pending_sync(client.api_url()).unwrap().is_none());
    // Simulate an abandoned/failed provider handoff, then a fresh process's sync.
    let reopened = RemoteCache::new(&directory.path().join("client"));
    let reused = sync_library(
        &client,
        &reopened,
        &mut |_| {},
        &AtomicBool::new(false),
        false,
    )
    .unwrap();
    assert!(!reused.downloaded);
    assert_eq!(reused.state.database_file, first.state.database_file);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(payload.is_file());
    let _provider = SqliteProvider::open_read_only(&reopened.database_path(&reused.state)).unwrap();
    reopened.cleanup_after_swap(&reused.state);
    assert!(!payload.exists());
    assert!(!checkpoint.exists());
}

#[test]
fn old_provider_cleanup_cannot_erase_a_new_partial_transfer_or_its_pause() {
    let directory = tempfile::tempdir().unwrap();
    let good = valid_snapshot(directory.path());
    let revision = Arc::new(AtomicUsize::new(7));
    let server = {
        let revision = revision.clone();
        TestServer::start(move |request| {
            let revision = revision.load(Ordering::SeqCst) as u64;
            if request.path == "/api/v1/library/snapshot" {
                snapshot_response(good.clone(), revision)
                    .with_header("ETag", &format!("\"snapshot-r{revision}\""))
                    .with_header("X-Snapshot-Id", &format!("snapshot-r{revision}"))
                    .with_header("Accept-Ranges", "bytes")
            } else {
                ordinary_response(request, revision)
            }
        })
    };
    let cache = RemoteCache::new(&directory.path().join("client"));
    let client = RemoteClient::new(&server.url, "").unwrap();
    let first = sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
    revision.store(8, Ordering::SeqCst);
    let control = crate::SyncControl::new();
    let next = crate::sync_library_with_options(
        &client,
        &cache,
        &mut |event| {
            if matches!(event, SyncProgress::Downloading { received, .. } if received > 0) {
                control.pause();
            }
        },
        &control,
        crate::SyncOptions::default(),
    )
    .unwrap();
    let crate::SyncRunOutcome::Paused(pending) = next else {
        panic!("the replacement transfer must pause");
    };
    assert!(pending.manual_paused);
    assert!(pending.downloaded_bytes > 0);
    let (payload, checkpoint) = transfer_files(&cache);
    let before = std::fs::read(&checkpoint).unwrap();
    let receipt: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert!(receipt["published_database_file"].is_null());
    assert_eq!(receipt["object"]["revision"], 8);
    cache.cleanup_after_swap(&first.state);
    assert!(payload.exists());
    assert_eq!(std::fs::read(checkpoint).unwrap(), before);
    assert_eq!(cache.pending_sync(client.api_url()).unwrap(), Some(pending));
}

#[test]
fn a_new_pause_before_download_invalidates_the_old_publication_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let good = valid_snapshot(directory.path());
    let server = TestServer::start(move |request| {
        if request.path == "/api/v1/library/snapshot" {
            snapshot_response(good.clone(), 7)
        } else {
            ordinary_response(request, 7)
        }
    });
    let cache = RemoteCache::new(&directory.path().join("client"));
    let client = RemoteClient::new(&server.url, "").unwrap();
    let first = sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
    {
        let mut session =
            crate::snapshot_transfer::TransferSession::open(&cache, client.api_url(), false)
                .unwrap();
        session.set_paused(true).unwrap();
    }
    let (payload, checkpoint) = transfer_files(&cache);
    let before = std::fs::read(&checkpoint).unwrap();
    cache.finish_sync_transfer(&first.state).unwrap();
    assert!(payload.exists());
    assert_eq!(std::fs::read(checkpoint).unwrap(), before);
    assert!(
        cache
            .pending_sync(client.api_url())
            .unwrap()
            .unwrap()
            .manual_paused
    );
}

#[test]
fn complete_content_length_with_a_truncated_zstd_frame_is_not_a_transient_retry() {
    let directory = tempfile::tempdir().unwrap();
    let good = valid_snapshot(directory.path());
    let packed = zstd::encode_all(good.as_slice(), 3).unwrap();
    let truncated = packed[..packed.len() / 2].to_vec();
    let requests = Arc::new(AtomicUsize::new(0));
    let server = {
        let requests = requests.clone();
        TestServer::start(move |request| {
            if request.path == "/api/v1/library/snapshot" {
                requests.fetch_add(1, Ordering::SeqCst);
                // The transport delivers its entire stated body, but the frame is bad.
                snapshot_response(truncated.clone(), 7)
                    .with_header("Content-Encoding", "zstd")
                    .with_header("X-Uncompressed-Length", &good.len().to_string())
            } else {
                ordinary_response(request, 7)
            }
        })
    };
    let cache = RemoteCache::new(&directory.path().join("client"));
    let client = RemoteClient::new(&server.url, "").unwrap();
    let error =
        sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap_err();
    assert!(matches!(error, RemoteError::Protocol(_)), "{error:?}");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(cache.active().is_none());
    assert_eq!(
        cache
            .pending_sync(client.api_url())
            .unwrap()
            .unwrap()
            .downloaded_bytes,
        0
    );
}

#[test]
fn pruning_skips_live_publication_then_preserves_the_newly_recorded_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let good = valid_snapshot(directory.path());
    let cache = RemoteCache::new(&directory.path().join("client"));
    let old = CacheState {
        library_id: LIBRARY.into(),
        revision: 7,
        schema_version: 5,
        database_file: "library-r7-old.db".into(),
        server_url: "http://example.invalid/api/v1".into(),
        ..Default::default()
    };
    let library = cache.library_dir(LIBRARY);
    std::fs::create_dir_all(&library).unwrap();
    std::fs::write(cache.database_path(&old), &good).unwrap();
    cache.save_state(&old).unwrap();
    let stale = library.join("library-r6-1.db");
    std::fs::write(&stale, &good).unwrap();
    let new = CacheState {
        revision: 8,
        database_file: "library-r8-new.db".into(),
        ..old.clone()
    };
    let publication = crate::writes::cache_publication_lease(&cache, LIBRARY).unwrap();
    std::fs::write(cache.database_path(&new), &good).unwrap();
    // Its final name exists before cache.json is published. Neither caller may prune.
    cache.prune(LIBRARY, &[&old.database_file]);
    cache.cleanup_after_swap(&old);
    assert!(cache.database_path(&new).exists());
    assert!(stale.exists(), "a busy cleanup skips all destructive work");
    cache.save_state(&new).unwrap();
    drop(publication);
    cache.cleanup_after_swap(&old);
    assert!(cache.database_path(&old).exists());
    assert!(cache.database_path(&new).exists());
    assert!(!stale.exists());
    assert_eq!(cache.state(LIBRARY).unwrap(), new);
}

#[test]
fn replaced_library_cleanup_skips_its_publishers_and_rechecks_activation() {
    let directory = tempfile::tempdir().unwrap();
    let good = valid_snapshot(directory.path());
    let cache = RemoteCache::new(&directory.path().join("client"));
    let old = CacheState {
        library_id: "old-library".into(),
        revision: 7,
        schema_version: 5,
        database_file: "library-r7-old.db".into(),
        server_url: "http://example.invalid/api/v1".into(),
        ..Default::default()
    };
    let active = CacheState {
        library_id: "new-library".into(),
        database_file: "library-r7-new.db".into(),
        ..old.clone()
    };
    for state in [&old, &active] {
        std::fs::create_dir_all(cache.library_dir(&state.library_id)).unwrap();
        std::fs::write(cache.database_path(state), &good).unwrap();
        cache.save_state(state).unwrap();
    }
    let publication = crate::writes::cache_publication_lease(&cache, &old.library_id).unwrap();
    assert!(cache.discard_replaced(&active).is_empty());
    assert!(cache.database_path(&old).exists());
    // The candidate becomes the current cache while its publisher still holds the lock.
    cache.save_state(&old).unwrap();
    drop(publication);
    assert!(cache.discard_replaced(&active).is_empty());
    assert!(cache.database_path(&old).exists());
    cache.save_state(&active).unwrap();
    assert_eq!(
        cache.discard_replaced(&active),
        vec![old.library_id.clone()]
    );
    assert!(!cache.library_dir(&old.library_id).exists());
}
