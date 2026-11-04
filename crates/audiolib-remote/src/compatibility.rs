// crates/audiolib-remote/src/compatibility.rs
//! Version, response and actual-file checks before publishing a remote cache.
use crate::{
    client::{RemoteLibrary, SnapshotMeta},
    error::{RemoteError, Result},
};
use audiolib_core::compatibility::{
    CompatibilityManifest, LIBRARY_FORMAT_HEADER, LibraryAccess, LibraryFormat,
    MAX_COMPATIBILITY_HEADER_BYTES, PeerRejection, SERVER_COMPATIBILITY_HEADER,
    SNAPSHOT_FORMAT_HEADER, SnapshotFormat, check_peer, compatibility_manifest,
};
use std::{path::Path, sync::atomic::AtomicBool};
use ureq::http::HeaderMap;

fn invalid(message: impl Into<String>) -> RemoteError {
    RemoteError::Snapshot(message.into())
}

/// Marker-based server check (Compatibility_Markers.md §4): blocked list, marker presence,
/// version/catalog consistency and protocol intersection. Never compares version ranges.
pub(crate) fn peer(version: &str, catalog: Option<&CompatibilityManifest>) -> Result<()> {
    use crate::error::PeerIncompatibility as Kind;
    let (reason, detail) = match check_peer(&compatibility_manifest(), version, catalog) {
        Ok(()) => return Ok(()),
        Err(PeerRejection::BlockedVersion) => {
            (Kind::Blocked, format!("server {version} is blocked"))
        }
        Err(PeerRejection::PeerUpgradeRequired) => (
            Kind::ServerUpgradeRequired,
            format!("server {version} has no compatibility markers"),
        ),
        Err(PeerRejection::ProtocolPeerTooOld) => (
            Kind::ServerUpgradeRequired,
            format!("server {version} protocol is older than this client's"),
        ),
        Err(PeerRejection::ProtocolPeerTooNew) => (
            Kind::ClientUpgradeRequired,
            format!("server {version} protocol is newer than this client's"),
        ),
        Err(PeerRejection::Inconsistent) => (
            Kind::Inconsistent,
            format!("server reports {version} but its catalog disagrees"),
        ),
    };
    Err(RemoteError::PeerIncompatible { reason, detail })
}

/// Optional features usable with this server: declared by both sides at the same revision
/// (§4, §9.5). Anything else is only hidden/degraded; it never blocks sync.
pub fn enabled_features(
    server: Option<&CompatibilityManifest>,
) -> std::collections::BTreeSet<String> {
    let local = compatibility_manifest();
    let Some(server) = server else {
        return Default::default();
    };
    local
        .features
        .iter()
        .filter(|(name, revision)| server.features.get(*name) == Some(*revision))
        .map(|(name, _)| name.clone())
        .collect()
}

/// Format check of `/library`. Peers are admitted by markers alone; a published schema
/// 3–5 library may still omit its descriptor because [`downloaded`] verifies the bytes.
pub(crate) fn library(library: &RemoteLibrary) -> Result<()> {
    match &library.snapshot_format {
        Some(format)
            if i64::from(format.format.schema_version) == library.schema_version
                && format.is_readable_by(&compatibility_manifest()) =>
        {
            Ok(())
        }
        // The published format before the current target may still omit its descriptor;
        // bytes are verified after download. The current target must declare one.
        None if (3..=5).contains(&library.schema_version) => Ok(()),
        _ => Err(invalid(
            "Server library format is missing, inconsistent or unsupported; the existing cache was retained",
        )),
    }
}

/// Recheck a live peer for probes, polling and each mutation. Never cache admission:
/// a server can upgrade while a client keeps the same library and revision open.
pub(crate) fn live_library(
    client: &crate::RemoteClient,
    observed: &RemoteLibrary,
    version: Option<&str>,
) -> Result<()> {
    library(observed)?;
    let version = match version {
        Some(version) => version.to_owned(),
        None => client.health()?.version,
    };
    let capabilities = client.capabilities()?;
    peer(&version, capabilities.compatibility.as_ref())?;
    library(observed)
}

/// Send on each mutation so the serving process also checks the peer after preflight.
pub(crate) fn client_header() -> Result<String> {
    serde_json::to_string(&compatibility_manifest())
        .map_err(|_| RemoteError::Protocol("Cannot serialize client compatibility catalog".into()))
}

fn single<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() || value.as_bytes().len() > MAX_COMPATIBILITY_HEADER_BYTES {
        return Err(invalid(format!(
            "Duplicate or oversized snapshot header {name}"
        )));
    }
    Ok(Some(value.to_str().map_err(|_| {
        invalid(format!("Invalid snapshot header {name}"))
    })?))
}

fn number(headers: &HeaderMap, name: &str) -> Result<Option<u32>> {
    single(headers, name)?
        .map(|value| {
            value
                .parse()
                .map_err(|_| invalid(format!("Invalid snapshot header {name}")))
        })
        .transpose()
}

/// Reject contradictory/unsupported headers before any destination or partial file is created.
pub(crate) fn response_format(headers: &HeaderMap) -> Result<Option<SnapshotFormat>> {
    let schema = number(headers, "x-schema-version")?;
    let id = single(headers, SNAPSHOT_FORMAT_HEADER)?;
    let body = single(headers, LIBRARY_FORMAT_HEADER)?;
    let catalog = single(headers, SERVER_COMPATIBILITY_HEADER)?;
    if id.is_none() && body.is_none() && catalog.is_none() {
        if schema.is_some_and(|schema| !(3..=5).contains(&schema)) {
            return Err(invalid(
                "Snapshot requires a modern supported format declaration",
            ));
        }
        return Ok(None);
    }
    let (Some(id), Some(body), Some(catalog)) = (id, body, catalog) else {
        return Err(invalid("Incomplete snapshot compatibility headers"));
    };
    let format: LibraryFormat =
        serde_json::from_str(body).map_err(|_| invalid("Invalid snapshot format JSON"))?;
    let peer_catalog: CompatibilityManifest =
        serde_json::from_str(catalog).map_err(|_| invalid("Invalid snapshot server catalog"))?;
    peer(&peer_catalog.application_version, Some(&peer_catalog))?;
    let descriptor = SnapshotFormat {
        id: id.into(),
        format,
    };
    if !descriptor.is_readable_by(&compatibility_manifest())
        || schema != Some(descriptor.format.schema_version)
    {
        return Err(invalid(
            "Snapshot format ID, schema and supported catalog disagree",
        ));
    }
    for (header, expected) in [
        (
            "x-search-storage-revision",
            descriptor
                .format
                .features
                .get("search_storage")
                .copied()
                .unwrap_or(0),
        ),
        (
            "x-tokenization-revision",
            descriptor
                .format
                .features
                .get("tokenization")
                .copied()
                .unwrap_or(0),
        ),
        (
            "x-source-search-revision",
            descriptor.format.source_search_revision.unwrap_or(0),
        ),
        (
            "x-source-tags-revision",
            descriptor.format.source_tags_revision.unwrap_or(0),
        ),
    ] {
        if number(headers, header)? != Some(expected) {
            return Err(invalid(format!(
                "Snapshot revision header {header} disagrees with its format"
            )));
        }
    }
    Ok(Some(descriptor))
}

/// Inspect original downloaded bytes before any owned derived-state conversion.
pub(crate) fn downloaded(
    path: &Path,
    library: &RemoteLibrary,
    meta: &SnapshotMeta,
    cancel: &AtomicBool,
) -> Result<LibraryFormat> {
    // Supported published legacy servers supply these identity headers too.
    // A single-flight snapshot may predate /library; never publish that older
    // response or infer its identity/revision from unrelated metadata.
    if meta.library_id.as_deref() != Some(library.library_id.as_str())
        || meta
            .revision
            .is_none_or(|revision| revision < library.revision)
    {
        return Err(invalid(
            "Server snapshot identity or revision changed during negotiation; sync again",
        ));
    }
    let inspection = audiolib_db::migration::inspect(path, LibraryAccess::Read, false, cancel)?;
    if meta
        .schema_version
        .is_some_and(|version| version != i64::from(inspection.format.schema_version))
    {
        return Err(invalid(
            "Snapshot schema header disagrees with the downloaded database",
        ));
    }
    if let Some(described) = &meta.snapshot_format {
        if inspection.format != described.format || !inspection.evidence.search_ready {
            return Err(invalid(
                "Snapshot format headers disagree with the downloaded database",
            ));
        }
    } else if inspection.format.schema_version >= 6 {
        return Err(invalid(
            "Schema 6 bytes arrived without a supported format declaration",
        ));
    }
    if let Some(expected) = &library.snapshot_format {
        if meta.snapshot_format.as_ref() != Some(expected) {
            return Err(invalid(
                "Server snapshot identity, revision or format changed during negotiation; sync again",
            ));
        }
    } else if inspection.format.schema_version != library.schema_version as u32 {
        return Err(invalid(
            "Server library schema changed during download; sync again",
        ));
    }
    Ok(inspection.format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CacheState, RemoteCache, RemoteClient, sync_library,
        test_server::{TestResponse, TestServer},
    };
    use audiolib_core::compatibility::PLAN_B_FORMAT;
    use audiolib_db::SqliteProvider;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use ureq::http::HeaderValue;

    fn descriptor() -> SnapshotFormat {
        SnapshotFormat {
            id: PLAN_B_FORMAT.into(),
            format: LibraryFormat::target(),
        }
    }

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            (SNAPSHOT_FORMAT_HEADER, PLAN_B_FORMAT.to_owned()),
            (
                LIBRARY_FORMAT_HEADER,
                serde_json::to_string(&LibraryFormat::target()).unwrap(),
            ),
            (
                SERVER_COMPATIBILITY_HEADER,
                serde_json::to_string(&compatibility_manifest()).unwrap(),
            ),
            ("x-schema-version", audiolib_db::SCHEMA_VERSION.to_string()),
            ("x-search-storage-revision", "1".into()),
            ("x-tokenization-revision", "3".into()),
            ("x-source-search-revision", "2".into()),
            ("x-source-tags-revision", "2".into()),
        ] {
            headers.insert(name, HeaderValue::from_str(&value).unwrap());
        }
        headers
    }

    #[test]
    fn plain_and_experimental_snapshot_descriptors_cannot_be_interchanged() {
        use audiolib_core::compatibility::{METADATA_STORAGE_FEATURE, PLAN_B_V8_COMPRESSED_FORMAT};
        let mut declared = headers();
        let plain = descriptor();
        assert!(!plain.format.features.contains_key(METADATA_STORAGE_FEATURE));
        assert_eq!(response_format(&declared).unwrap(), Some(plain.clone()));
        let history: serde_json::Value = serde_json::from_str(include_str!(
            "../../audiolib-core/tests/fixtures/compatibility/format_fingerprint.json"
        ))
        .unwrap();
        let legacy = SnapshotFormat {
            id: PLAN_B_V8_COMPRESSED_FORMAT.into(),
            format: serde_json::from_value(history["legacy_compressed"].clone()).unwrap(),
        };
        declared.insert(
            SNAPSHOT_FORMAT_HEADER,
            HeaderValue::from_static(PLAN_B_V8_COMPRESSED_FORMAT),
        );
        assert!(
            response_format(&declared).is_err(),
            "a z1 ID cannot describe plain bytes"
        );
        declared.insert(
            LIBRARY_FORMAT_HEADER,
            HeaderValue::from_str(&serde_json::to_string(&legacy.format).unwrap()).unwrap(),
        );
        assert_eq!(response_format(&declared).unwrap(), Some(legacy));
        declared.insert(
            SNAPSHOT_FORMAT_HEADER,
            HeaderValue::from_static(PLAN_B_FORMAT),
        );
        assert!(
            response_format(&declared).is_err(),
            "a plain ID cannot hide the z1 requirement"
        );
        let mut experimental_server = compatibility_manifest();
        experimental_server
            .features
            .insert(METADATA_STORAGE_FEATURE.into(), 1);
        let enabled = enabled_features(Some(&experimental_server));
        assert!(!enabled.contains(METADATA_STORAGE_FEATURE));
        assert!(enabled.contains(audiolib_core::compatibility::ALBUM_ENTITIES_FEATURE));
    }

    #[test]
    fn write_header_carries_marker_revision_and_protocol() {
        let header: serde_json::Value = serde_json::from_str(&client_header().unwrap()).unwrap();
        let ours = compatibility_manifest();
        assert_eq!(
            header["marker_revision"],
            serde_json::json!(ours.marker_revision)
        );
        assert!(header["marker_revision"].as_u64().is_some());
        assert!(header["protocol"]["min"].as_u64().is_some());
        assert!(header["protocol"]["max"].as_u64().is_some());
    }

    #[test]
    fn headers_require_exact_format_revisions_and_bidirectional_versions() {
        let valid = headers();
        assert_eq!(response_format(&valid).unwrap(), Some(descriptor()));
        for name in [
            LIBRARY_FORMAT_HEADER,
            SERVER_COMPATIBILITY_HEADER,
            "x-source-tags-revision",
        ] {
            let mut missing = valid.clone();
            missing.remove(name);
            assert!(response_format(&missing).is_err(), "{name}");
        }
        let mut duplicate = valid.clone();
        duplicate.append(
            SNAPSHOT_FORMAT_HEADER,
            HeaderValue::from_static(PLAN_B_FORMAT),
        );
        assert!(response_format(&duplicate).is_err());
        let mut mismatch = valid.clone();
        mismatch.insert("x-schema-version", HeaderValue::from_static("5"));
        assert!(response_format(&mismatch).is_err());
        // Version ranges no longer decide; markers do.
        let mut ranges = compatibility_manifest();
        ranges.peer_application_ranges = vec!["<0.4.0".into()];
        peer(&ranges.application_version, Some(&ranges)).unwrap();
        for catalogless in ["0.4.11", "0.5.0", "0.5.1", "0.5.2", "99.0.0"] {
            assert!(matches!(
                peer(catalogless, None),
                Err(RemoteError::PeerIncompatible {
                    reason: crate::error::PeerIncompatibility::ServerUpgradeRequired,
                    ..
                })
            ));
        }
    }

    #[test]
    fn live_upgrade_is_rechecked_by_poll_probe_and_every_write() {
        use crate::{CachedIdentity, RevisionCheck, check_revision, test_connection};
        use audiolib_core::{MetadataPatch, config::DataSourceSettings};
        for rejection in ["version", "unmarked", "protocol", "format"] {
            let upgraded = Arc::new(AtomicBool::new(false));
            let changed = upgraded.clone();
            let writes = Arc::new(AtomicUsize::new(0));
            let observed = writes.clone();
            let server = TestServer::start(move |request| {
                let upgraded = changed.load(Ordering::Acquire);
                match (request.method.as_str(), request.path.as_str()) {
                    ("GET", "/api/v1/health") => TestResponse::json(200, &serde_json::json!({
                        "status":"ok", "version": if upgraded && rejection == "version" { "99.0.0" } else { env!("CARGO_PKG_VERSION") },
                    }).to_string()),
                    ("GET", "/api/v1/capabilities") => {
                        let mut catalog = compatibility_manifest();
                        if upgraded && rejection == "unmarked" {
                            catalog.marker_revision = None;
                        }
                        if upgraded && rejection == "protocol" {
                            catalog.protocol = Some(audiolib_core::compatibility::ProtocolRange { min: 2, max: 2 });
                        }
                        TestResponse::json(200, &serde_json::json!({"compatibility":catalog}).to_string())
                    }
                    ("GET", "/api/v1/library") => {
                        let mut format = descriptor();
                        if upgraded && rejection == "format" {
                            format.format.features.insert("future_feature".into(), 1);
                        }
                        TestResponse::json(200, &serde_json::json!({
                            "library_id":"stable", "revision":8, "schema_version":audiolib_db::SCHEMA_VERSION,
                            "identity_bound_writes":true, "snapshot_format":format,
                        }).to_string())
                    }
                    ("GET", "/api/v1/roots") => TestResponse::json(200, "[]"),
                    ("PATCH", _) | ("POST", _) | ("DELETE", _) => {
                        assert!(request.header(audiolib_core::compatibility::CLIENT_COMPATIBILITY_HEADER).is_some());
                        observed.fetch_add(1, Ordering::Relaxed);
                        TestResponse::json(400, r#"{"error_code":"VALIDATION","message":"test write reached handler"}"#)
                    }
                    _ => TestResponse::json(404, "{}"),
                }
            });
            let client = RemoteClient::new(&server.url, "")
                .unwrap()
                .with_library_id("stable")
                .unwrap();
            let cached = CachedIdentity {
                library_id: "stable".into(),
                revision: 8,
            };
            assert_eq!(
                check_revision(&client, Some(&cached), false),
                RevisionCheck::UpToDate { revision: 8 }
            );
            let patch = MetadataPatch {
                notes: Some("change".into()),
                ..Default::default()
            };
            assert!(matches!(
                client.patch_metadata(1, &patch),
                Err(RemoteError::Validation(_))
            ));
            upgraded.store(true, Ordering::Release);
            assert!(
                matches!(
                    check_revision(&client, Some(&cached), false),
                    RevisionCheck::Failed(_)
                ),
                "{rejection}"
            );
            let settings = DataSourceSettings {
                server_url: server.url.clone(),
                ..Default::default()
            };
            let report = test_connection(
                &settings,
                &crate::RemoteSecrets::default(),
                &[],
                None,
                false,
            );
            assert!(!report.ok(), "{rejection}");
            assert!(
                report
                    .steps
                    .iter()
                    .any(|step| step.step == crate::ProbeStep::Library
                        && !step.ok
                        && !step.skipped)
            );
            for error in [
                client.patch_metadata(1, &patch).unwrap_err(),
                client.remove_audio(&[1]).unwrap_err(),
            ] {
                assert!(
                    matches!(
                        error,
                        RemoteError::Snapshot(_) | RemoteError::PeerIncompatible { .. }
                    ),
                    "{rejection}: {error}"
                );
                assert!(!error.write_may_have_committed());
            }
            let root = tempfile::tempdir().unwrap();
            let cache = RemoteCache::new(root.path());
            let retained = CacheState {
                library_id: "stable".into(),
                revision: 8,
                schema_version: 6,
                database_file: "retained.db".into(),
                ..Default::default()
            };
            let path = cache.database_path(&retained);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            drop(SqliteProvider::open(&path).unwrap());
            cache.save_state(&retained).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let writer = crate::RemoteWriter::new(client.clone(), cache.clone(), "stable");
            assert!(matches!(
                writer.update_metadata(1, &patch),
                Err(RemoteError::Snapshot(_) | RemoteError::PeerIncompatible { .. })
            ));
            assert_eq!(cache.active().unwrap(), retained);
            assert!(std::fs::read(&path).unwrap() == bytes);
            assert_eq!(writes.load(Ordering::Relaxed), 1, "{rejection}");
        }
    }

    /// Spec §8: a refused negotiation or download never replaces the active cache.
    /// `version`: inconsistent server catalog; `format`: unknown format ID (row 7);
    /// `header`/`bytes`: snapshot headers disagree with the file (row 11).
    fn assert_refused_download_preserves_the_active_cache(mode: &'static str) {
        let root = tempfile::tempdir().unwrap();
        let cache = RemoteCache::new(root.path());
        let old = CacheState {
            library_id: "retained".into(),
            schema_version: 6,
            revision: 7,
            database_file: "retained.db".into(),
            ..Default::default()
        };
        let cached_path = cache.database_path(&old);
        std::fs::create_dir_all(cached_path.parent().unwrap()).unwrap();
        drop(SqliteProvider::open(&cached_path).unwrap());
        cache.save_state(&old).unwrap();
        let retained = std::fs::read(&cached_path).unwrap();
        let source = root.path().join("server.db");
        drop(SqliteProvider::open(&source).unwrap());
        if mode == "bytes" {
            rusqlite::Connection::open(&source)
                .unwrap()
                .execute(
                    "INSERT INTO audiolib_schema_features VALUES('future_required_feature',1)",
                    [],
                )
                .unwrap();
        }
        let bytes = std::fs::read(source).unwrap();
        let downloads = Arc::new(AtomicUsize::new(0));
        let observed = downloads.clone();
        let server = TestServer::start(move |request| {
            match request.path.as_str() {
                "/api/v1/health" => TestResponse::json(200, &serde_json::json!({
                    "status":"ok", "version": if mode == "version" { "99.0.0" } else { env!("CARGO_PKG_VERSION") },
                }).to_string()),
                "/api/v1/capabilities" => TestResponse::json(200, &serde_json::json!({
                    "compatibility":compatibility_manifest(),
                }).to_string()),
                "/api/v1/library" => {
                    let mut format = descriptor();
                    if mode == "format" {
                        format.format.features.insert("future_required_feature".into(),1);
                    }
                    TestResponse::json(200, &serde_json::json!({
                        "library_id":"incoming", "revision":8, "schema_version":audiolib_db::SCHEMA_VERSION, "snapshot_format":format,
                    }).to_string())
                }
                "/api/v1/roots" => TestResponse::json(200, "[]"),
                "/api/v1/library/snapshot" => {
                    observed.fetch_add(1, Ordering::Relaxed);
                    assert!(request.header(audiolib_core::compatibility::ACCEPT_FORMATS_HEADER).unwrap().contains(PLAN_B_FORMAT));
                    let mut reply = TestResponse::bytes(200, bytes.clone())
                        .with_header("x-library-id", "incoming")
                        .with_header("x-library-revision", "8");
                    for (name,value) in &headers() {
                        let value = if mode == "header" && name == "x-tokenization-revision" { "99" } else { value.to_str().unwrap() };
                        reply = reply.with_header(name.as_str(), value);
                    }
                    reply
                }
                _ => TestResponse::json(404, "{}"),
            }
        });
        let client = RemoteClient::new(&server.url, "").unwrap();
        assert!(
            matches!(
                sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), true),
                Err(RemoteError::Snapshot(_) | RemoteError::PeerIncompatible { .. })
            ),
            "{mode}"
        );
        assert_eq!(cache.active().unwrap(), old, "{mode}");
        assert_eq!(std::fs::read(&cached_path).unwrap(), retained, "{mode}");
        assert_eq!(
            downloads.load(Ordering::Relaxed),
            usize::from(matches!(mode, "header" | "bytes"))
        );
    }

    #[test]
    fn inconsistent_server_version_preserves_the_active_cache() {
        assert_refused_download_preserves_the_active_cache("version");
    }

    #[test]
    fn matrix_row_7_unknown_format_id_refuses_the_snapshot_and_keeps_the_cache() {
        assert_refused_download_preserves_the_active_cache("format");
    }

    #[test]
    fn matrix_row_11_snapshot_header_file_mismatch_keeps_the_cache() {
        // Header claims a different format than the file, and the file carries a
        // required feature its headers do not.
        assert_refused_download_preserves_the_active_cache("header");
        assert_refused_download_preserves_the_active_cache("bytes");
    }

    #[test]
    fn legacy_stale_or_unbound_download_preserves_the_active_cache() {
        // A marker server still serving a published schema-5 snapshot without headers.
        for version in [env!("CARGO_PKG_VERSION")] {
            for mode in [
                "stale",
                "missing_revision",
                "missing_identity",
                "other_identity",
            ] {
                let root = tempfile::tempdir().unwrap();
                let source = root.path().join("legacy.db");
                let conn = rusqlite::Connection::open(&source).unwrap();
                audiolib_db::register_library_functions(&conn).unwrap();
                conn.execute_batch(include_str!(
                    "../../audiolib-db/tests/fixtures/schema-v5.sql"
                ))
                .unwrap();
                conn.execute_batch(
                    "INSERT INTO audio_files(id,file_path,filename,filename_tokenized,user_notes,play_count)
                     VALUES(1,'legacy.wav','legacy.wav',audiolib_tokenize('legacy.wav'),'retained',17);",
                )
                .unwrap();
                drop(conn);
                let bytes = std::fs::read(&source).unwrap();
                let cache = RemoteCache::new(&root.path().join("cache"));
                let old = CacheState {
                    library_id: "legacy".into(),
                    revision: 11,
                    schema_version: 5,
                    database_file: "retained.db".into(),
                    ..Default::default()
                };
                let retained_path = cache.database_path(&old);
                std::fs::create_dir_all(retained_path.parent().unwrap()).unwrap();
                std::fs::write(&retained_path, &bytes).unwrap();
                cache.save_state(&old).unwrap();
                let downloads = Arc::new(AtomicUsize::new(0));
                let observed = downloads.clone();
                let server = TestServer::start(move |request| match request.path.as_str() {
                    "/api/v1/health" => TestResponse::json(
                        200,
                        &serde_json::json!({"status":"ok","version":version}).to_string(),
                    ),
                    "/api/v1/capabilities" => {
                        crate::test_server::marker_capabilities(serde_json::json!({}))
                    }
                    "/api/v1/library" => TestResponse::json(
                        200,
                        r#"{"library_id":"legacy","revision":12,"schema_version":5}"#,
                    ),
                    "/api/v1/roots" => TestResponse::json(200, "[]"),
                    "/api/v1/library/snapshot" => {
                        observed.fetch_add(1, Ordering::Relaxed);
                        let mut reply = TestResponse::bytes(200, bytes.clone())
                            .with_header("x-schema-version", "5");
                        if mode != "missing_revision" {
                            reply = reply.with_header(
                                "x-library-revision",
                                if mode == "stale" { "11" } else { "12" },
                            );
                        }
                        if mode != "missing_identity" {
                            reply = reply.with_header(
                                "x-library-id",
                                if mode == "other_identity" {
                                    "other"
                                } else {
                                    "legacy"
                                },
                            );
                        }
                        reply
                    }
                    _ => TestResponse::json(404, "{}"),
                });
                let result = sync_library(
                    &RemoteClient::new(&server.url, "").unwrap(),
                    &cache,
                    &mut |_| {},
                    &AtomicBool::new(false),
                    true,
                );
                assert!(
                    matches!(result, Err(RemoteError::Snapshot(_))),
                    "{version}/{mode}: {result:?}"
                );
                assert_eq!(
                    downloads.load(Ordering::Relaxed),
                    1,
                    "must reach snapshot admission"
                );
                assert_eq!(cache.active().unwrap(), old);
                assert_eq!(
                    std::fs::read(&retained_path).unwrap(),
                    std::fs::read(&source).unwrap()
                );
                assert_eq!(
                    std::fs::read_dir(cache.library_dir("legacy"))
                        .unwrap()
                        .count(),
                    2
                );
            }
        }
    }

    #[test]
    fn supported_legacy_peer_preserves_source_format_and_only_converts_stale_search() {
        use audiolib_core::{DataProvider, SearchRequest};
        for stale in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("legacy.db");
            let conn = rusqlite::Connection::open(&source).unwrap();
            audiolib_db::register_library_functions(&conn).unwrap();
            conn.execute_batch(include_str!(
                "../../audiolib-db/tests/fixtures/schema-v5.sql"
            ))
            .unwrap();
            conn.execute_batch(
            "INSERT INTO audio_files(id,file_path,filename,filename_tokenized,user_notes,play_count)
             VALUES(1,'legacy.wav','legacy.wav',audiolib_tokenize('legacy.wav'),'retained note',17);",
        ).unwrap();
            if stale {
                conn.execute(
                    "DELETE FROM audiolib_schema_features WHERE feature='tokenization'",
                    [],
                )
                .unwrap();
            }
            drop(conn);
            let bytes = std::fs::read(&source).unwrap();
            let source_format = audiolib_db::migration::inspect(
                &source,
                LibraryAccess::Read,
                false,
                &AtomicBool::new(false),
            )
            .unwrap()
            .format;
            let downloads = Arc::new(AtomicUsize::new(0));
            let observed = downloads.clone();
            let server = TestServer::start(move |request| match request.path.as_str() {
                "/api/v1/health" => crate::test_server::marker_health(),
                "/api/v1/capabilities" => {
                    crate::test_server::marker_capabilities(serde_json::json!({}))
                }
                "/api/v1/library" => TestResponse::json(
                    200,
                    r#"{"library_id":"legacy","revision":12,"schema_version":5}"#,
                ),
                "/api/v1/roots" => TestResponse::json(200, "[]"),
                "/api/v1/library/snapshot" => {
                    observed.fetch_add(1, Ordering::Relaxed);
                    TestResponse::bytes(200, bytes.clone())
                        .with_header("x-library-id", "legacy")
                        .with_header("x-library-revision", "12")
                        .with_header("x-schema-version", "5")
                }
                _ => TestResponse::json(404, "{}"),
            });
            let client = RemoteClient::new(&server.url, "").unwrap();
            let cache = RemoteCache::new(&root.path().join("cache"));
            if stale {
                // Existing cache and metadata survive cancellation after conversion,
                // and every backup/registry in the disposable job is cleaned together.
                let retained = CacheState {
                    library_id: "retained".into(),
                    revision: 4,
                    schema_version: 6,
                    database_file: "library-r4.db".into(),
                    ..Default::default()
                };
                let retained_path = cache.database_path(&retained);
                std::fs::create_dir_all(retained_path.parent().unwrap()).unwrap();
                drop(SqliteProvider::open(&retained_path).unwrap());
                cache.save_state(&retained).unwrap();
                let retained_bytes = std::fs::read(&retained_path).unwrap();
                let cancel = AtomicBool::new(false);
                let result = sync_library(
                    &client,
                    &cache,
                    &mut |stage| {
                        if matches!(stage, crate::SyncProgress::Saving) {
                            cancel.store(true, Ordering::Release);
                        }
                    },
                    &cancel,
                    false,
                );
                assert!(matches!(result, Err(RemoteError::Cancelled)), "{result:?}");
                assert_eq!(cache.active().unwrap(), retained);
                assert_eq!(std::fs::read(&retained_path).unwrap(), retained_bytes);
                assert_eq!(
                    std::fs::read_dir(cache.library_dir("legacy"))
                        .unwrap()
                        .count(),
                    0
                );
            }
            let outcome =
                sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
            assert_eq!(outcome.state.schema_version, audiolib_db::SCHEMA_VERSION);
            assert_eq!(outcome.state.revision, 12);
            let format = outcome.state.format.as_ref().unwrap();
            assert_eq!(format.source.as_ref(), Some(&source_format));
            assert_eq!(format.generation, 1);
            assert_eq!(
                format.local.schema_version as i64,
                outcome.state.schema_version
            );
            let path = cache.database_path(&outcome.state);
            let readonly = SqliteProvider::open_read_only(&path).unwrap();
            assert_eq!(readonly.database_path(), path);
            let value = serde_json::to_value(readonly.get_audio(1).unwrap()).unwrap();
            assert!(value.get("play_count").is_none());
            assert!(value["attributes"].get("last_played_at").is_none());
            assert_eq!(readonly.get_audio(1).unwrap().notes, "retained note");
            assert_eq!(
                readonly
                    .search(&SearchRequest {
                        query: "legacy".into(),
                        ..Default::default()
                    })
                    .unwrap()
                    .total_count,
                1
            );
            assert!(audiolib_db::migration::discover(&path).unwrap().is_none());
            let second =
                sync_library(&client, &cache, &mut |_| {}, &AtomicBool::new(false), false).unwrap();
            assert!(!second.downloaded, "{stale}");
            assert_eq!(second.state.format, outcome.state.format);
            assert_eq!(second.state.revision, 12);
            // Cancellation at Saving retains the complete wire object. Even a legacy
            // peer without Range support must be replayed locally, then converted in
            // a fresh owned job, rather than downloaded again.
            assert_eq!(downloads.load(Ordering::Relaxed), 1);
            assert!(
                !std::fs::read_dir(cache.library_dir("legacy"))
                    .unwrap()
                    .any(|entry| entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".snapshot-job-"))
            );
        }
    }
}
