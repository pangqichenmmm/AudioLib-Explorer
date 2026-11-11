//! Released clients 0.4.11–0.7.1 must still parse the pre-request probes so they reach
//! the request that answers `409 PEER_UPGRADE_REQUIRED` (Compatibility_Markers.md §4.2).
//! The structs below are verbatim copies of the client-side shapes at those tags.
#![allow(dead_code)]
use audiolib_core::compatibility::{CLIENT_COMPATIBILITY_HEADER, SERVER_COMPATIBILITY_HEADER};
use audiolib_db::SqliteProvider;
use audiolib_server::{AppState, BearerToken, ServerConfig, router};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde::Deserialize;
use std::collections::BTreeMap;
use tower::ServiceExt;

const TOKEN: &str = "legacy-shapes-test-token-0123456789";

/// `HealthInfo` (identical v0.4.11–v0.7.1).
#[derive(Debug, Default, Deserialize)]
struct HealthInfo {
    #[serde(default)]
    status: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    public_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ManageMode {
    #[default]
    All,
    Lan,
    Off,
    #[serde(other)]
    Unknown,
}

/// `Capabilities` v0.4.11 (oldest shape).
#[derive(Debug, Deserialize)]
struct CapabilitiesV0411 {
    #[serde(default)]
    manage_mode: ManageMode,
    #[serde(default)]
    external: Option<bool>,
    #[serde(default)]
    can_manage: Option<bool>,
    #[serde(default)]
    root_management: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ListWaveformsCapability {
    version: u32,
    buckets: Vec<u32>,
    max_ids: usize,
    encoding: String,
    encodings: Vec<String>,
    bundle: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WaveformJobsCapability {
    version: u32,
    modes: Vec<String>,
    rate_setting: bool,
}

/// `Capabilities` v0.5.0–v0.5.1 (adds the waveform capabilities).
#[derive(Debug, Deserialize)]
struct CapabilitiesV050 {
    #[serde(default)]
    manage_mode: ManageMode,
    #[serde(default)]
    external: Option<bool>,
    #[serde(default)]
    can_manage: Option<bool>,
    #[serde(default)]
    root_management: bool,
    #[serde(default)]
    list_waveforms: Option<ListWaveformsCapability>,
    #[serde(default)]
    pregenerate_list_waveforms: Option<bool>,
    #[serde(default)]
    list_waveform_jobs: Option<WaveformJobsCapability>,
}

/// `CompatibilityManifest` v0.6.0–v0.7.1 (identical across those tags). Every field
/// without `serde(default)` is required: a missing one fails the whole `/capabilities`.
#[derive(Debug, Deserialize)]
struct ManifestV060 {
    application_version: String,
    catalog_revision: u32,
    peer_application_ranges: Vec<String>,
    #[serde(default)]
    legacy_peer_application_ranges: Vec<String>,
    direct_use_application_ranges: Vec<String>,
    auto_migrate_application_ranges: Vec<String>,
    api_protocols: Vec<String>,
    snapshot_read_formats: Vec<String>,
    database_write_formats: Vec<String>,
    migration_source_schemas: Vec<u32>,
    migration_target_format: String,
    migration_protocols: BTreeMap<String, u32>,
    minimum_sqlite_version: String,
}

/// `Capabilities` v0.6.0–v0.7.1 (newest shape; adds `compatibility`).
#[derive(Debug, Deserialize)]
struct CapabilitiesV060 {
    #[serde(default)]
    manage_mode: ManageMode,
    #[serde(default)]
    external: Option<bool>,
    #[serde(default)]
    can_manage: Option<bool>,
    #[serde(default)]
    root_management: bool,
    #[serde(default)]
    list_waveforms: Option<ListWaveformsCapability>,
    #[serde(default)]
    pregenerate_list_waveforms: Option<bool>,
    #[serde(default)]
    list_waveform_jobs: Option<WaveformJobsCapability>,
    #[serde(default)]
    compatibility: Option<ManifestV060>,
}

/// `ErrorBody` v0.4.11 (`sub_code` added in v0.5.0, also defaulted).
#[derive(Debug, Default, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error_code: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    sub_code: String,
}

fn state(root: &std::path::Path) -> AppState {
    drop(SqliteProvider::open(&root.join("library.db")).unwrap());
    let config = ServerConfig::from_toml_str(
        "bind='127.0.0.1:0'\nlibrary_db='library.db'\nsnapshot_dir='snapshots'\nauth=true\n",
        root,
    )
    .unwrap();
    AppState::open(&config, BearerToken::new(TOKEN).unwrap()).unwrap()
}

async fn get(state: &AppState, path: &str, header: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut request = Request::get(path).header("Authorization", format!("Bearer {TOKEN}"));
    if let Some(value) = header {
        request = request.header(CLIENT_COMPATIBILITY_HEADER, value);
    }
    let response = router(state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    (
        status,
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
}

/// The catalog a real 0.7.1 client sends (shipped fixture).
fn client_071_catalog() -> String {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../crates/audiolib-core/tests/fixtures/compatibility/0.7.1.json"
    );
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    serde_json::to_string(&value).unwrap()
}

#[tokio::test]
async fn every_released_client_shape_parses_probes_and_reaches_the_upgrade_409() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());

    let (status, health) = get(&state, "/api/v1/health", None).await;
    assert_eq!(status, StatusCode::OK);
    let health: HealthInfo = serde_json::from_slice(&health).unwrap();
    assert!(!health.version.is_empty());

    let (status, body) = get(&state, "/api/v1/capabilities", None).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice::<CapabilitiesV0411>(&body).expect("v0.4.11 shape");
    let v050: CapabilitiesV050 = serde_json::from_slice(&body).expect("v0.5.x shape");
    assert!(v050.list_waveforms.is_some());
    let v060: CapabilitiesV060 = serde_json::from_slice(&body).expect("v0.6.0–v0.7.1 shape");
    let manifest = v060.compatibility.expect("manifest emitted");
    assert_eq!(manifest.catalog_revision, 2);
    assert!(manifest.api_protocols.contains(&"api/v1".to_owned()));
    assert!(!manifest.direct_use_application_ranges.is_empty());
    assert!(!manifest.auto_migrate_application_ranges.is_empty());

    // The old client then asks for the snapshot: no header (0.4.11–0.5.1) or its own
    // unmarked catalog (0.6.0–0.7.1). Both must get a parseable upgrade 409.
    let catalog = client_071_catalog();
    for header in [None, Some(catalog.as_str())] {
        let (status, body) = get(&state, "/api/v1/library/snapshot", header).await;
        assert_eq!(status, StatusCode::CONFLICT, "{header:?}");
        let error: ErrorBody = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.error_code, "PEER_UPGRADE_REQUIRED");
        assert!(!error.message.is_empty());
    }
}

/// The same catalog rides in `x-audiolib-server-compatibility`, which old clients parse
/// with the same struct and cap at 4096 bytes.
#[test]
fn server_catalog_header_parses_in_the_old_shape_and_fits() {
    let json =
        serde_json::to_string(&audiolib_core::compatibility::compatibility_manifest()).unwrap();
    assert!(
        json.len() <= 4096,
        "{SERVER_COMPATIBILITY_HEADER} {} bytes",
        json.len()
    );
    serde_json::from_str::<ManifestV060>(&json).expect("v0.6.0–v0.7.1 manifest shape");
}
