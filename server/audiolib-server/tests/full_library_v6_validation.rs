// server/audiolib-server/tests/full_library_v6_validation.rs
//! Manual release-only acceptance on a fresh, explicitly owned full-library copy.
//!
//! WARNING: AppState creates server sidecars and may change SQLite journal mode.
//! AUDIOLIB_VALIDATION_SERVER_DATABASE must name an absolute, disposable owned copy,
//! never a working library, migration source, or server database. The report must
//! be a new absolute path in AUDIOLIB_VALIDATION_SERVER_REPORT. This test never
//! connects to an external service, scans roots, reads WAVs, or exports record DTOs.
//! Measurements are in-process axum Router latency, not network/server-host latency.
use audiolib_core::compatibility::{
    CATALOG_REVISION, LibraryAccess, LibraryFormat, PLAN_B_FORMAT, compatibility_manifest,
};
use audiolib_server::{AppState, ServerConfig, VERSION, router};
use axum::{
    Router,
    body::Body,
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
use tower::ServiceExt;

const DATABASE_ENV: &str = "AUDIOLIB_VALIDATION_SERVER_DATABASE";
const REPORT_ENV: &str = "AUDIOLIB_VALIDATION_SERVER_REPORT";
const EXPECTED_RECORDS: u64 = 909_342;
const REPEATED_REQUESTS: usize = 100;

fn explicit_absolute_path(name: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(name).unwrap_or_else(|| {
        panic!("Manual validation requires {name}; supply an explicitly owned copy/report path")
    }));
    assert!(
        path.is_absolute(),
        "{name} must be an explicit absolute path"
    );
    path
}

fn appended(path: &Path, suffix: &str) -> PathBuf {
    let mut result = path.as_os_str().to_os_string();
    result.push(suffix);
    result.into()
}

fn data_version(connection: &Connection) -> i64 {
    connection
        .pragma_query_value(None, "data_version", |row| row.get(0))
        .unwrap()
}

/// A nearest-rank percentile: P95 of 100 samples is the 95th sorted sample.
fn timings(samples: &[f64]) -> Value {
    assert!(!samples.is_empty());
    let mut sorted = samples.to_vec();
    assert!(
        sorted
            .iter()
            .all(|sample| sample.is_finite() && *sample >= 0.0)
    );
    sorted.sort_by(f64::total_cmp);
    let percentile = |percent: usize| sorted[(sorted.len() * percent).div_ceil(100) - 1];
    json!({
        "samples":samples.len(),
        "p50_ms":percentile(50),
        "p95_ms":percentile(95),
        "max_ms":sorted[sorted.len()-1],
        "percentile":"nearest_rank",
    })
}

/// Includes middleware, handler, JSON serialization and body collection; excludes
/// request construction and the test's JSON parsing/assertions after collection.
async fn exchange(app: &Router, request: Request<Body>) -> (Value, f64) {
    let started = Instant::now();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "validation route refused"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    (serde_json::from_slice(&body).unwrap(), elapsed_ms)
}

async fn get(app: &Router, path: &str) -> (Value, f64) {
    exchange(app, Request::get(path).body(Body::empty()).unwrap()).await
}

fn assert_library(value: &Value, revision: u64) {
    assert_eq!(value["schema_version"], audiolib_db::SCHEMA_VERSION);
    assert_eq!(value["snapshot_format"]["id"], PLAN_B_FORMAT);
    assert_eq!(
        value["snapshot_format"]["format"]["features"]["play_history_retired"],
        2
    );
    assert_eq!(value["stats"]["audio_files"], EXPECTED_RECORDS);
    assert_eq!(value["revision"], revision);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual release-only full-library benchmark: requires explicit owned DATABASE and new REPORT env paths"]
async fn full_library_v6_router_benchmark() {
    if cfg!(debug_assertions) {
        panic!("Use cargo test --release; debug timings are not full-library acceptance");
    }
    let database = explicit_absolute_path(DATABASE_ENV);
    let report_path = explicit_absolute_path(REPORT_ENV);
    assert!(
        database.is_file(),
        "The owned database copy must already exist"
    );
    let database = database.canonicalize().unwrap();
    let parent = database.parent().unwrap();
    for suffix in [
        "-wal",
        "-shm",
        "-journal",
        ".server-state.json",
        ".stats.json",
    ] {
        assert!(
            !appended(&database, suffix).exists(),
            "Use a fresh dedicated owned copy without SQLite/server sidecars"
        );
    }
    for name in ["admin-settings.json", "tokens.json"] {
        assert!(
            !parent.join(name).exists(),
            "Use an owned validation directory without persisted server configuration"
        );
    }
    let inspection = audiolib_db::migration::inspect(
        &database,
        LibraryAccess::Read,
        false,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(inspection.format, LibraryFormat::target());
    assert!(inspection.decision.direct_use);
    assert_eq!(
        LibraryFormat::target().snapshot_format_id().as_deref(),
        Some(PLAN_B_FORMAT)
    );
    let mut report_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&report_path)
        .expect("The explicit report path must be new; existing reports are never overwritten");
    let database_bytes = fs::metadata(&database).unwrap().len();
    let runtime = tempfile::Builder::new()
        .prefix("owned-server-validation-")
        .tempdir_in(parent)
        .unwrap();
    let config = ServerConfig::from_toml_str(
        &format!(
            "bind='127.0.0.1:0'\nauth_mode='off'\nmanage_mode='off'\n\
             library_db={}\nsnapshot_dir={}\nwaveform_cache_dir={}\n\
             ffmpeg_path={}\npregenerate_list_waveforms=false\nroots=[]\n",
            serde_json::to_string(&database).unwrap(),
            serde_json::to_string(&runtime.path().join("snapshots")).unwrap(),
            serde_json::to_string(&runtime.path().join("waveforms")).unwrap(),
            serde_json::to_string(&runtime.path().join("no-ffmpeg-configured")).unwrap(),
        ),
        runtime.path(),
    )
    .unwrap();
    assert!(config.roots.is_empty());
    let opening = Instant::now();
    let state = AppState::open(&config, None).unwrap();
    let startup_ms = opening.elapsed().as_secs_f64() * 1000.0;
    let observer =
        Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let sqlite_version: String = observer
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .unwrap();
    let retired_columns: i64 = observer
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('audio_files')
             WHERE name IN ('play_count','last_played_at')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retired_columns, 0);
    let retired_table: i64 = observer
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name='play_history'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retired_table, 0);
    let before = state.poll_revision().unwrap();
    let before_data_version = data_version(&observer);
    let app = router(state.clone());
    let (health, _) = get(&app, "/api/v1/health").await;
    assert_eq!(health["status"], "ok");
    assert_eq!(health["version"], VERSION);
    assert_eq!(health["schema_version"], audiolib_db::SCHEMA_VERSION);
    assert_eq!(health["compatibility_catalog_revision"], CATALOG_REVISION);
    let (capabilities, _) = get(&app, "/api/v1/capabilities").await;
    assert_eq!(
        capabilities["compatibility"],
        json!(compatibility_manifest())
    );
    assert!(!appended(&database, ".stats.json").exists());
    let (first, first_ms) = get(&app, "/api/v1/library").await;
    assert_library(&first, before.revision);
    let stats_path = appended(&database, ".stats.json");
    let cached_stats = fs::read(&stats_path).unwrap();
    let mut samples = Vec::with_capacity(REPEATED_REQUESTS);
    for _ in 0..REPEATED_REQUESTS {
        let (value, elapsed_ms) = get(&app, "/api/v1/library").await;
        assert_library(&value, before.revision);
        samples.push(elapsed_ms);
    }
    assert_eq!(fs::read(&stats_path).unwrap(), cached_stats);
    let audio_id: i64 = observer
        .query_row(
            "SELECT id FROM audio_files WHERE id>0 ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for _ in 0..2 {
        // Headerless legacy compatibility request: only the existing library-ID
        // binding is sent. It must retain HTTP 200 without recording playback.
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/v1/audio/{audio_id}/plays"))
            .header("x-audiolib-library-id", &before.library_id)
            .body(Body::empty())
            .unwrap();
        let (reply, _) = exchange(&app, request).await;
        assert_eq!(reply["file"]["id"], audio_id);
        assert!(reply["file"].get("play_count").is_none());
        assert!(reply["file"]["attributes"].get("last_played_at").is_none());
        assert_eq!(reply["revision_before"], before.revision);
        assert_eq!(reply["revision"], before.revision);
    }
    let (history, _) = get(&app, "/api/v1/history/plays?limit=100&offset=0").await;
    assert_eq!(history, json!([]));
    let after = state.poll_revision().unwrap();
    let after_data_version = data_version(&observer);
    assert_eq!(after, before);
    assert_eq!(after_data_version, before_data_version);
    assert_eq!(fs::read(&stats_path).unwrap(), cached_stats);
    let report = json!({
        "complete":true,
        "identity":"owned-full-library-p1-copy",
        "application_version":VERSION,
        "build_commit":audiolib_server::COMMIT,
        "sqlite_version":sqlite_version,
        "database_bytes_before_startup":database_bytes,
        "format":inspection.format,
        "schema_version":audiolib_db::SCHEMA_VERSION,
        "retirement_revision":2,
        "audio_files":EXPECTED_RECORDS,
        "stats":first["stats"],
        "health":{
            "status":health["status"], "version":health["version"],
            "schema_version":health["schema_version"],
            "compatibility_catalog_revision":health["compatibility_catalog_revision"],
        },
        "startup_ms":startup_ms,
        "library_first_request_ms":first_ms,
        "library_repeated":timings(&samples),
        "library_repeated_samples_ms":samples,
        "stats_sidecar_unchanged_after_first_request":true,
        "legacy_plays_requests":2,
        "legacy_history_empty":true,
        "response_omits_play_count_and_last_played_at":true,
        "revision_before":before.revision,
        "revision_after":after.revision,
        "observer_data_version_before":before_data_version,
        "observer_data_version_after":after_data_version,
        "conditions":{
            "build_profile":"release",
            "measurement":"in-process axum Router; not network latency",
            "timer":"middleware, handler, serialization and body collection; excludes test JSON parsing",
            "same_appstate_and_router":true,
            "concurrent_requests":1,
            "background_stats_warmup":false,
            "stats_sidecar_at_start":false,
            "system_cache":"not cleared or controlled",
            "configured_scan_roots":0,
            "waveform_pregeneration":false,
            "audio_or_network_access_requested":false,
            "invariance_window":"after AppState startup through all requests, before graceful shutdown checkpoint",
            "scope":"API timing and no-op revision/data_version evidence; full business digests are verified by the migration acceptance runner",
        },
    });
    drop(app);
    state.shutdown(Duration::from_secs(10)).unwrap();
    serde_json::to_writer_pretty(&mut report_file, &report).unwrap();
    report_file.write_all(b"\n").unwrap();
    report_file.sync_all().unwrap();
    // Do not print record JSON, source paths, or response error payloads.
    eprintln!(
        "Owned full-library server validation passed: {EXPECTED_RECORDS} records, {REPEATED_REQUESTS} cached requests"
    );
}
