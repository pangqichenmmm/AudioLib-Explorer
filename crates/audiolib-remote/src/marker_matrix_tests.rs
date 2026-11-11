// crates/audiolib-remote/src/marker_matrix_tests.rs
//! Client side of the Compatibility_Markers.md §8 test matrix. Rows 7 and 11 (unknown
//! format ID, header/file mismatch) are
//! `compatibility::tests::matrix_row_7_unknown_format_id_refuses_the_snapshot_and_keeps_the_cache`
//! and `compatibility::tests::matrix_row_11_snapshot_header_file_mismatch_keeps_the_cache`;
//! row 3's old-client half is the shipped client's own check and cannot run here.
use crate::{
    CacheState, RemoteCache, RemoteClient, RemoteError,
    error::PeerIncompatibility,
    sync_library,
    test_server::{TestResponse, TestServer, marker_catalog},
};
use audiolib_core::{
    MetadataPatch,
    compatibility::{
        LIBRARY_FORMAT_HEADER, LibraryFormat, PLAN_B_FORMAT, SERVER_COMPATIBILITY_HEADER,
        SNAPSHOT_FORMAT_HEADER, SnapshotFormat,
    },
};
use audiolib_db::SqliteProvider;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

const LIBRARY: &str = "matrix";

/// What the fake server says about itself.
#[derive(Clone)]
struct Peer {
    version: String,
    /// `/capabilities` `compatibility`; `None` = `404` (catalog-less 0.4.11/0.5.x).
    catalog: Option<serde_json::Value>,
    /// Error code the server answers snapshot and writes with, if it refuses us.
    refuses: Option<&'static str>,
    capabilities: serde_json::Value,
}

impl Peer {
    fn marker(version: &str) -> Self {
        let mut catalog = marker_catalog();
        catalog["application_version"] = version.into();
        Self {
            version: version.into(),
            catalog: Some(catalog),
            refuses: None,
            capabilities: serde_json::json!({}),
        }
    }
}

struct Fixture {
    server: TestServer,
    downloads: Arc<AtomicUsize>,
    writes: Arc<AtomicUsize>,
    peer: Arc<Mutex<Peer>>,
    _root: tempfile::TempDir,
    cache: RemoteCache,
}

fn fixture(peer: Peer) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("server.db");
    drop(SqliteProvider::open(&source).unwrap());
    let bytes = std::fs::read(&source).unwrap();
    let (downloads, writes) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let peer = Arc::new(Mutex::new(peer));
    let server = {
        let (downloads, writes, peer) = (downloads.clone(), writes.clone(), peer.clone());
        TestServer::start(move |request| {
            let peer = peer.lock().unwrap().clone();
            let refused = |code: &str| {
                TestResponse::json(
                    409,
                    &serde_json::json!({"error_code":code,"message":"refused by server"})
                        .to_string(),
                )
            };
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/api/v1/health") => TestResponse::json(
                    200,
                    &serde_json::json!({"status":"ok","version":peer.version}).to_string(),
                ),
                ("GET", "/api/v1/capabilities") => match &peer.catalog {
                    Some(catalog) => {
                        let mut body = peer.capabilities.clone();
                        body["compatibility"] = catalog.clone();
                        TestResponse::json(200, &body.to_string())
                    }
                    None => TestResponse::json(404, "{}"),
                },
                ("GET", "/api/v1/library") => TestResponse::json(
                    200,
                    &serde_json::json!({
                        "library_id":LIBRARY, "revision":3, "schema_version":audiolib_db::SCHEMA_VERSION,
                        "identity_bound_writes":true,
                        "snapshot_format":SnapshotFormat{id:PLAN_B_FORMAT.into(),format:LibraryFormat::target()},
                    })
                    .to_string(),
                ),
                ("GET", "/api/v1/roots") => TestResponse::json(200, "[]"),
                ("GET", "/api/v1/library/snapshot") => {
                    assert!(
                        request
                            .header(audiolib_core::compatibility::CLIENT_COMPATIBILITY_HEADER)
                            .is_some()
                    );
                    if let Some(code) = peer.refuses {
                        return refused(code);
                    }
                    downloads.fetch_add(1, Ordering::Relaxed);
                    let mut reply = TestResponse::bytes(200, bytes.clone())
                        .with_header("x-library-id", LIBRARY)
                        .with_header("x-library-revision", "3");
                    for (name, value) in [
                        (SNAPSHOT_FORMAT_HEADER, PLAN_B_FORMAT.to_owned()),
                        (
                            LIBRARY_FORMAT_HEADER,
                            serde_json::to_string(&LibraryFormat::target()).unwrap(),
                        ),
                        (
                            SERVER_COMPATIBILITY_HEADER,
                            peer.catalog.as_ref().map(|c| c.to_string()).unwrap_or_default(),
                        ),
                        ("x-schema-version", audiolib_db::SCHEMA_VERSION.to_string()),
                        ("x-search-storage-revision", "1".into()),
                        ("x-tokenization-revision", "3".into()),
                        ("x-source-search-revision", "2".into()),
                        ("x-source-tags-revision", "2".into()),
                    ] {
                        reply = reply.with_header(name, &value);
                    }
                    reply
                }
                ("PATCH", _) | ("POST", _) | ("DELETE", _) => {
                    assert!(
                        request
                            .header(audiolib_core::compatibility::CLIENT_COMPATIBILITY_HEADER)
                            .is_some(),
                        "every mutation carries the client markers"
                    );
                    if let Some(code) = peer.refuses {
                        return refused(code);
                    }
                    writes.fetch_add(1, Ordering::Relaxed);
                    TestResponse::json(
                        400,
                        r#"{"error_code":"VALIDATION","message":"write reached handler"}"#,
                    )
                }
                _ => TestResponse::json(404, "{}"),
            }
        })
    };
    let cache = RemoteCache::new(&root.path().join("cache"));
    Fixture {
        server,
        downloads,
        writes,
        peer,
        _root: root,
        cache,
    }
}

impl Fixture {
    fn client(&self) -> RemoteClient {
        RemoteClient::new(&self.server.url, "")
            .unwrap()
            .with_library_id(LIBRARY)
            .unwrap()
    }
    fn sync(&self) -> crate::Result<crate::SyncOutcome> {
        sync_library(
            &self.client(),
            &self.cache,
            &mut |_| {},
            &AtomicBool::new(false),
            true,
        )
    }
    fn write(&self) -> RemoteError {
        self.client()
            .patch_metadata(
                1,
                &MetadataPatch {
                    notes: Some("x".into()),
                    ..Default::default()
                },
            )
            .unwrap_err()
    }
    fn active(&self) -> Option<CacheState> {
        self.cache.active()
    }
}

fn reason(error: &RemoteError) -> Option<PeerIncompatibility> {
    match error {
        RemoteError::PeerIncompatible { reason, .. } => Some(*reason),
        _ => None,
    }
}

/// Probe, snapshot and mutation all refuse with `expected`, nothing is downloaded or
/// written, the cache is untouched and the message names who must upgrade.
fn assert_refused(fixture: &Fixture, expected: PeerIncompatibility, text: &str) {
    let before = fixture.active();
    let error = fixture.sync().unwrap_err();
    assert_eq!(reason(&error), Some(expected), "{error:?}");
    assert!(
        error.localized(false).contains(text),
        "{}",
        error.localized(false)
    );
    assert!(error.localized(true).contains("本地缓存已保留"));
    let error = fixture.write();
    assert_eq!(reason(&error), Some(expected), "{error:?}");
    assert!(!error.write_may_have_committed());
    assert!(
        fixture
            .client()
            .capabilities()
            .is_ok_and(|caps| crate::compatibility::peer(
                &fixture.peer.lock().unwrap().version,
                caps.compatibility.as_ref()
            )
            .is_err())
            || fixture.peer.lock().unwrap().refuses.is_some()
    );
    assert_eq!(fixture.writes.load(Ordering::Relaxed), 0);
    assert_eq!(fixture.active(), before);
}

#[test]
fn row_1_and_4_marker_peers_of_any_version_sync_and_write() {
    for version in [env!("CARGO_PKG_VERSION"), "0.7.2", "0.8.0"] {
        let fixture = fixture(Peer::marker(version));
        let outcome = fixture.sync().unwrap();
        assert!(outcome.downloaded, "{version}");
        assert_eq!(outcome.state.server_version, version);
        assert!(matches!(fixture.write(), RemoteError::Validation(_)));
        assert_eq!(fixture.writes.load(Ordering::Relaxed), 1);
    }
}

#[test]
fn row_2_and_10_unmarked_servers_must_upgrade() {
    for (version, catalog) in [
        // 0.6.x–0.7.x: a range-based catalog without markers.
        (
            "0.7.0",
            Some(serde_json::json!({
                "application_version":"0.7.0","catalog_revision":1,
                "peer_application_ranges":["=0.4.11","=0.5.0","=0.5.1",">=0.6.0, <0.7.0","=0.7.0"],
                "api_protocols":["api/v1"],"snapshot_read_formats":[PLAN_B_FORMAT],
                "database_write_formats":[PLAN_B_FORMAT],"migration_source_schemas":[3,4,5,6],
                "migration_target_format":PLAN_B_FORMAT,"minimum_sqlite_version":"3.54.1",
            })),
        ),
        (
            "0.6.5",
            Some(serde_json::json!({"application_version":"0.6.5","api_protocols":["api/v1"]})),
        ),
        // 0.5.1: no /capabilities at all.
        ("0.5.1", None),
    ] {
        let mut peer = Peer::marker(version);
        peer.catalog = catalog;
        let fixture = fixture(peer);
        assert_refused(
            &fixture,
            PeerIncompatibility::ServerUpgradeRequired,
            "Upgrade AudioLib Server",
        );
        assert_eq!(fixture.downloads.load(Ordering::Relaxed), 0, "{version}");
    }
}

#[test]
fn row_3_server_refusing_this_client_asks_for_a_client_update() {
    let mut peer = Peer::marker("0.8.0");
    peer.refuses = Some(audiolib_core::compatibility::PEER_UPGRADE_REQUIRED);
    let fixture = fixture(peer);
    assert_refused(
        &fixture,
        PeerIncompatibility::ClientUpgradeRequired,
        "Update AudioLib Explorer",
    );
}

#[test]
fn row_5_disjoint_protocols_name_the_side_to_upgrade() {
    // Server newer: this client must update.
    let mut newer = Peer::marker("0.9.0");
    let catalog = newer.catalog.as_mut().unwrap();
    catalog["protocol"] = serde_json::json!({"min":2,"max":2});
    catalog["api_protocols"] = serde_json::json!(["api/v2"]);
    let fixture_newer = fixture(newer);
    assert_refused(
        &fixture_newer,
        PeerIncompatibility::ClientUpgradeRequired,
        "Update AudioLib Explorer",
    );
    // Server older (its own check sends PROTOCOL_UNSUPPORTED, or ours detects it).
    let mut older = Peer::marker("0.7.5");
    let catalog = older.catalog.as_mut().unwrap();
    catalog["protocol"] = serde_json::json!({"min":0,"max":0});
    catalog["api_protocols"] = serde_json::json!(["api/v0"]);
    assert_refused(
        &fixture(older),
        PeerIncompatibility::ServerUpgradeRequired,
        "Upgrade AudioLib Server",
    );
    let mut refusing = Peer::marker("0.7.5");
    refusing.refuses = Some(audiolib_core::compatibility::PROTOCOL_UNSUPPORTED);
    assert_refused(
        &fixture(refusing),
        PeerIncompatibility::ServerUpgradeRequired,
        "Upgrade AudioLib Server",
    );
}

#[test]
fn row_6_catalog_revision_differences_are_ignored() {
    let mut peer = Peer::marker("0.7.4");
    peer.catalog.as_mut().unwrap()["catalog_revision"] = 42.into();
    peer.catalog.as_mut().unwrap()["peer_application_ranges"] = serde_json::json!(["<0.1.0"]);
    assert!(fixture(peer).sync().unwrap().downloaded);
}

#[test]
fn row_8_and_12_feature_gaps_only_degrade_that_function() {
    let list = serde_json::json!({"list_waveforms":{"version":1,"buckets":[128],"max_ids":200,"encoding":"i16le-minmax-base64","bundle":true},
        "list_waveform_jobs":{"version":1,"modes":["missing"],"rate_setting":true}});
    let mut missing = Peer::marker("0.8.0");
    missing.capabilities = list.clone();
    let features = &mut missing.catalog.as_mut().unwrap()["features"];
    features
        .as_object_mut()
        .unwrap()
        .remove("waveform_bundle_strong_etag");
    let mut newer_revision = Peer::marker("0.8.0");
    newer_revision.capabilities = list;
    newer_revision.catalog.as_mut().unwrap()["features"]["list_waveform_jobs"] = 2.into();
    for (peer, disabled) in [
        (missing, "waveform_bundle_strong_etag"),
        (newer_revision, "list_waveform_jobs"),
    ] {
        let fixture = fixture(peer);
        let capabilities = fixture.client().capabilities().unwrap();
        assert!(!capabilities.feature_enabled(disabled));
        assert!(capabilities.feature_enabled("list_waveforms"));
        assert!(capabilities.list_waveforms_usable());
        let enabled = crate::compatibility::enabled_features(capabilities.compatibility.as_ref());
        assert!(!enabled.contains(disabled) && enabled.contains("list_waveforms"));
        // Sync and writes are unaffected.
        assert!(fixture.sync().unwrap().downloaded);
        assert!(matches!(fixture.write(), RemoteError::Validation(_)));
    }
}

#[test]
fn row_9_blocked_server_reply_is_explained() {
    let mut peer = Peer::marker("0.8.0");
    peer.refuses = Some(audiolib_core::compatibility::PEER_VERSION_BLOCKED);
    assert_refused(
        &fixture(peer),
        PeerIncompatibility::Blocked,
        "known to be faulty",
    );
}
