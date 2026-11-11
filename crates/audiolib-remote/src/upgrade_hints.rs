// crates/audiolib-remote/src/upgrade_hints.rs
//! Server upgrade hints seen on any response (`x-audiolib-upgrade-available`, the
//! snapshot format header) kept per API base for this process (one session), and the
//! client-side [`ServerUpgradeStatus`] built from `/capabilities` and `/health`.
//! Recording only parses bounded header values; it never blocks on I/O.
use crate::{Capabilities, RemoteClient};
use audiolib_core::compatibility::SNAPSHOT_FORMAT_HEADER;
use audiolib_core::server_upgrade::{
    self, PATH_SEARCH_HEADER, SchemaMigrationHeader, ServerUpgradeInput, ServerUpgradeStatus,
    UPGRADE_AVAILABLE_HEADER, UpgradeAvailable,
};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const MAX_FORMAT_HEADER_BYTES: usize = 256;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Hint {
    pub upgrade: Option<UpgradeAvailable>,
    pub snapshot_format: Option<String>,
    /// Schema-migration / legacy migration header of the latest response.
    pub migration: Option<SchemaMigrationHeader>,
    /// `x-audiolib-path-search: unavailable` seen since the migration started.
    pub path_search_unavailable: bool,
}

fn hints() -> &'static Mutex<HashMap<String, Hint>> {
    static HINTS: OnceLock<Mutex<HashMap<String, Hint>>> = OnceLock::new();
    HINTS.get_or_init(Default::default)
}

/// Remember the hints of one response from `api`.
pub(crate) fn record(api: &str, headers: &ureq::http::HeaderMap) {
    let upgrade = headers
        .get(UPGRADE_AVAILABLE_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(server_upgrade::parse_upgrade_available);
    let format = headers
        .get(SNAPSHOT_FORMAT_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= MAX_FORMAT_HEADER_BYTES)
        .map(str::to_string);
    let migration = server_upgrade::migration_header_from(|name| {
        headers.get(name).and_then(|value| value.to_str().ok())
    });
    let path_unavailable = headers
        .get(PATH_SEARCH_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("unavailable"));
    let mut hints = hints()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if upgrade.is_none() && format.is_none() && migration.is_none() && !path_unavailable {
        // An ordinary reply without migration headers ends a recorded migration.
        if let Some(hint) = hints.get_mut(api) {
            hint.migration = None;
            hint.path_search_unavailable = false;
        }
        return;
    }
    let hint = hints.entry(api.to_string()).or_default();
    if upgrade.is_some() {
        hint.upgrade = upgrade;
    }
    if format.is_some() {
        hint.snapshot_format = format;
    }
    if migration.is_none() && !path_unavailable {
        hint.path_search_unavailable = false;
    }
    hint.migration = migration;
    hint.path_search_unavailable |= path_unavailable;
}

pub(crate) fn hint(api: &str) -> Hint {
    hints()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(api)
        .cloned()
        .unwrap_or_default()
}

/// Gather the inputs for [`server_upgrade::evaluate`] from an already read
/// `/capabilities` reply plus `/health` and `/library` (both optional).
pub fn server_upgrade_status(
    client: &RemoteClient,
    capabilities: &Capabilities,
) -> ServerUpgradeStatus {
    let health = client.health().ok();
    let mut observed = Vec::new();
    if let Some(format) = client
        .library()
        .ok()
        .and_then(|library| library.snapshot_format)
    {
        observed.push(format.id);
    }
    let hint = hint(client.api_url());
    if let Some(format) = hint.snapshot_format.clone()
        && !observed.contains(&format)
    {
        observed.push(format);
    }
    let admin_migration = (capabilities.schema_migration.is_some() || hint.migration.is_some())
        .then(|| client.migration_status().ok())
        .flatten();
    let features = capabilities
        .compatibility
        .as_ref()
        .map(|manifest| manifest.features.clone())
        .unwrap_or_default();
    server_upgrade::evaluate(&ServerUpgradeInput {
        server_version: health
            .as_ref()
            .map(|health| health.version.clone())
            .filter(|version| !version.is_empty()),
        capabilities_known: capabilities.supported,
        features,
        library_schema: capabilities.library_schema.clone(),
        schema_migration: capabilities.schema_migration.clone(),
        vendor_import: capabilities.vendor_import.clone(),
        health_upgrade: health.and_then(|health| health.upgrade),
        admin_migration,
        migration_header: hint.migration,
        client_upgrade: capabilities.client_upgrade.clone(),
        path_search_header_unavailable: hint.path_search_unavailable,
        observed_formats: observed,
        upgrade_available: hint.upgrade,
        can_manage: capabilities.may_manage(),
    })
}

/// Run `op` again while the server refuses it for a migration (`503
/// migration_in_progress` / `migration_switching`), waiting its `Retry-After` (or the
/// agreed default) through `sleep`. Such a refusal wrote nothing, so a write is safe to
/// send again; other errors and the last refusal are returned unchanged.
pub fn retry_during_migration<T>(
    attempts: usize,
    mut sleep: impl FnMut(Duration),
    mut op: impl FnMut() -> crate::Result<T>,
) -> crate::Result<T> {
    let mut left = attempts.max(1);
    loop {
        match op() {
            Err(error) if left > 1 => match error.migration_refusal() {
                Some((refusal, retry_after)) => {
                    left -= 1;
                    sleep(Duration::from_secs(refusal.retry_after(retry_after)));
                }
                None => return Err(error),
            },
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{TestResponse, TestServer, marker_capabilities};
    use audiolib_core::server_upgrade::MIGRATION_HEADER;
    use audiolib_core::server_upgrade::{
        MigrationRefusal, MigrationState, Notice, PLAN_B_V9_FORMAT, PathSearch,
    };

    fn server(capabilities: serde_json::Value, health: serde_json::Value) -> TestServer {
        TestServer::start(move |request| match request.path.as_str() {
            "/api/v1/health" => TestResponse::json(200, &health.to_string()),
            "/api/v1/capabilities" => marker_capabilities(capabilities.clone())
                .with_header(UPGRADE_AVAILABLE_HEADER, "0.9.0"),
            _ => TestResponse::json(404, r#"{"error":{"code":"NOT_FOUND","message":"no"}}"#),
        })
    }

    #[test]
    fn upgrade_header_is_recorded_from_capabilities() {
        let server = server(
            serde_json::json!({}),
            serde_json::json!({"status":"ok","version":"0.9.0"}),
        );
        let client = RemoteClient::new(&server.url, "t").unwrap();
        let capabilities = client.capabilities().unwrap();
        let status = server_upgrade_status(&client, &capabilities);
        assert_eq!(
            status.notices,
            vec![Notice::ClientUpgradeAvailable {
                version: Some("0.9.0".into()),
                level: audiolib_core::server_upgrade::ClientUpgradeLevel::Recommended,
            }]
        );
        assert!(status.main_search);
        assert_eq!(status.path_search, PathSearch::NotOffered);
    }

    #[test]
    fn sp2_and_migration_fields_come_from_capabilities_and_health() {
        let server = server(
            serde_json::json!({
                "library_schema": {"current": 9, "source_search_rev": 1,
                    "snapshot_formats_served": ["ale-s9-cld1-t3-m2-p1-a1-sd2",
                        audiolib_core::compatibility::PLAN_B_FORMAT]},
                "schema_migration": {"state": "running", "endpoint": "/admin/api/migration"},
                "vendor_import": {"revision": 1, "max_file_bytes": 52428800, "max_rows": 200000,
                    "max_row_text_bytes": 262144}
            }),
            serde_json::json!({"status":"ok","version":"0.9.0",
                "upgrade": {"phase":"indexing","completed":3,"total":4}}),
        );
        let client = RemoteClient::new(&server.url, "t").unwrap();
        let capabilities = client.capabilities().unwrap();
        assert_eq!(
            capabilities.schema_migration.as_ref().unwrap().state,
            MigrationState::Running
        );
        assert_eq!(
            capabilities.vendor_import.as_ref().unwrap().max_rows,
            200_000
        );
        let status = server_upgrade_status(&client, &capabilities);
        assert_eq!(status.path_search, PathSearch::NeedsClientUpgrade);
        assert!(status.main_search);
        assert_eq!(status.migration.as_ref().unwrap().percent, Some(75));
        assert!(status.notices.contains(&Notice::PathSearchNeedsUpgrade));
        assert!(
            status
                .notices
                .contains(&Notice::Migrating { percent: Some(75) })
        );
        assert_ne!(PLAN_B_V9_FORMAT, "ale-s9-cld1-t3-m2-p1-a1-sd2");
    }

    #[test]
    fn server_without_capabilities_is_an_old_server() {
        let server = TestServer::start(|request| match request.path.as_str() {
            "/api/v1/health" => TestResponse::json(200, r#"{"status":"ok","version":"0.7.0"}"#),
            _ => TestResponse::json(404, r#"{"error":{"code":"NOT_FOUND","message":"no"}}"#),
        });
        let client = RemoteClient::new(&server.url, "t").unwrap();
        let capabilities = client.capabilities().unwrap();
        let status = server_upgrade_status(&client, &capabilities);
        assert!(status.notices.is_empty());
        assert!(status.main_search);
        assert_eq!(status.path_search, PathSearch::NotOffered);
    }

    #[test]
    fn structured_upgrade_and_schema_migration_headers() {
        use audiolib_core::server_upgrade::{
            ClientUpgradeLevel, SCHEMA_MIGRATION_HEADER, VENDOR_IMPORT_API,
            VENDOR_IMPORT_MAX_FILE_BYTES, VENDOR_IMPORT_MAX_ROW_TEXT_BYTES, VENDOR_IMPORT_MAX_ROWS,
        };
        assert_eq!(VENDOR_IMPORT_API, "/api/vendor-import");
        assert_eq!(VENDOR_IMPORT_MAX_FILE_BYTES, 52_428_800);
        assert_eq!(VENDOR_IMPORT_MAX_ROWS, 200_000);
        assert_eq!(VENDOR_IMPORT_MAX_ROW_TEXT_BYTES, 262_144);
        let server = TestServer::start(move |request| match request.path.as_str() {
            "/api/v1/health" => TestResponse::json(200, r#"{"status":"ok","version":"0.9.0"}"#),
            "/api/v1/capabilities" => marker_capabilities(serde_json::json!({
                "client_upgrade": {
                    "level": "recommended",
                    "reason": "snapshot_format_downgraded",
                    "missing_features": ["search_path"],
                    "min_client_version": "0.9.0",
                    "served_snapshot_format": "ale-s8-cld1-t3-m2-p1-a1"
                },
                "schema_migration": {"state": "deferred", "endpoint": "/admin/api/migration"}
            }))
            .with_header(
                UPGRADE_AVAILABLE_HEADER,
                r#"recommended; min=0.9.0; features="search_path""#,
            )
            .with_header(SCHEMA_MIGRATION_HEADER, "deferred; target=9"),
            _ => TestResponse::json(404, r#"{"error":{"code":"NOT_FOUND","message":"no"}}"#),
        });
        let client = RemoteClient::new(&server.url, "t").unwrap();
        let capabilities = client.capabilities().unwrap();
        assert_eq!(
            capabilities.client_upgrade.as_ref().unwrap().level,
            ClientUpgradeLevel::Recommended
        );
        let status = server_upgrade_status(&client, &capabilities);
        assert_eq!(
            status.migration.as_ref().unwrap().state,
            MigrationState::Deferred
        );
        assert!(status.notices.contains(&Notice::ClientUpgradeAvailable {
            version: Some("0.9.0".into()),
            level: ClientUpgradeLevel::Recommended,
        }));
        assert!(
            !status
                .notices
                .iter()
                .any(|n| matches!(n, Notice::Migrating { .. }))
        );
    }

    const RUNNING_REPORT: &str = r#"{"state":"running","error_code":null,
        "progress":{"done":1,"total":4},"phases":{"backup":10.5,"path_build":0,"optimize":0,"switch":0},
        "switch":{"source_search_rev":2,"features":[]},
        "batches":[{"rows":100,"ms":5.2}],"eta_s":30}"#;
    const DONE_REPORT: &str = r#"{"state":"done","error_code":null,
        "progress":{"done":4,"total":4},
        "phases":{"backup":10,"path_build":20,"optimize":3,"switch":1},
        "switch":{"source_search_rev":3,"user_version":9,"features":["search_path"]},
        "batches":[{"rows":4,"ms":1}],"eta_s":0}"#;
    const PAUSED_REPORT: &str = r#"{"state":"paused","error_code":"disk_full",
        "progress":{"done":2,"total":4},"phases":{"backup":10,"path_build":5,"optimize":0,"switch":0},
        "switch":{"source_search_rev":2,"features":[]},"batches":[],"eta_s":null}"#;

    /// Minimal check of a mock against doc/schemas/migration-report.schema.json:
    /// required keys, JSON types and the `progress`/`batches` item shapes.
    fn assert_matches_schema(report: &str) {
        let schema: serde_json::Value = serde_json::from_str(include_str!(
            "../../../doc/schemas/migration-report.schema.json"
        ))
        .unwrap();
        let report: serde_json::Value = serde_json::from_str(report).unwrap();
        for key in schema["required"].as_array().unwrap() {
            assert!(report.get(key.as_str().unwrap()).is_some(), "missing {key}");
        }
        let type_ok = |kind: &str, value: &serde_json::Value| match kind {
            "string" => value.is_string(),
            "null" => value.is_null(),
            "object" => value.is_object(),
            "array" => value.is_array(),
            "integer" => value.is_u64() || value.is_i64(),
            "number" => value.is_number(),
            _ => true,
        };
        let check = |rule: &serde_json::Value, value: &serde_json::Value, at: &str| {
            match &rule["type"] {
                serde_json::Value::String(kind) => assert!(type_ok(kind, value), "{at}"),
                serde_json::Value::Array(kinds) => assert!(
                    kinds.iter().any(|k| type_ok(k.as_str().unwrap(), value)),
                    "{at}"
                ),
                _ => {}
            }
            if let Some(allowed) = rule["enum"].as_array() {
                assert!(allowed.contains(value), "{at}: {value}");
            }
            if let Some(minimum) = rule["minimum"].as_f64()
                && let Some(number) = value.as_f64()
            {
                assert!(number >= minimum, "{at}");
            }
        };
        let properties = schema["properties"].as_object().unwrap();
        for (key, value) in report.as_object().unwrap() {
            let Some(rule) = properties.get(key) else {
                continue;
            };
            check(rule, value, key);
            if let Some(fields) = rule["properties"].as_object()
                && value.is_object()
            {
                for required in rule["required"].as_array().into_iter().flatten() {
                    assert!(
                        value.get(required.as_str().unwrap()).is_some(),
                        "{key}.{required}"
                    );
                }
                for (name, field) in value.as_object().unwrap() {
                    let field_rule = fields.get(name).unwrap_or(&rule["additionalProperties"]);
                    check(field_rule, field, &format!("{key}.{name}"));
                }
            }
            if let (Some(items), Some(values)) = (rule.get("items"), value.as_array()) {
                for item in values {
                    check(items, item, key);
                    for required in items["required"].as_array().into_iter().flatten() {
                        assert!(item.get(required.as_str().unwrap()).is_some(), "{key}[]");
                    }
                }
            }
        }
    }

    /// Check a mocked migration reply against doc/schemas/migration-http.schema.json.
    fn assert_matches_http_schema(error_code: Option<&str>, headers: &[(&str, &str)]) {
        let schema: serde_json::Value = serde_json::from_str(include_str!(
            "../../../doc/schemas/migration-http.schema.json"
        ))
        .unwrap();
        let report: serde_json::Value = serde_json::from_str(include_str!(
            "../../../doc/schemas/migration-report.schema.json"
        ))
        .unwrap();
        if let Some(code) = error_code {
            let codes = schema["$defs"]["error_body"]["properties"]["error_code"]["enum"]
                .as_array()
                .unwrap();
            assert!(codes.iter().any(|c| c == code), "{code}");
        }
        let rules = &schema["$defs"]["headers"]["properties"];
        for (name, value) in headers {
            let allowed = if *name == MIGRATION_HEADER {
                &report["properties"]["state"]["enum"]
            } else {
                &rules[*name]["enum"]
            };
            assert!(
                allowed.as_array().unwrap().iter().any(|v| v == value),
                "{name}: {value}"
            );
        }
    }

    #[test]
    fn mock_http_replies_match_the_http_schema() {
        assert_matches_http_schema(
            Some("migration_in_progress"),
            &[("retry-after", "30"), (MIGRATION_HEADER, "running")],
        );
        assert_matches_http_schema(
            Some("migration_switching"),
            &[("retry-after", "30"), (MIGRATION_HEADER, "switching")],
        );
        assert_matches_http_schema(
            None,
            &[
                (PATH_SEARCH_HEADER, "unavailable"),
                (MIGRATION_HEADER, "running"),
            ],
        );
    }

    #[test]
    fn mock_reports_match_the_schema_and_parse() {
        for report in [RUNNING_REPORT, DONE_REPORT, PAUSED_REPORT] {
            assert_matches_schema(report);
            let status: audiolib_core::server_upgrade::MigrationStatus =
                serde_json::from_str(report).unwrap();
            assert!(status.progress.is_some());
        }
        let done: audiolib_core::server_upgrade::MigrationStatus =
            serde_json::from_str(DONE_REPORT).unwrap();
        let switch = done.switch.unwrap();
        assert_eq!(switch.source_search_rev, Some(3));
        assert_eq!(switch.features, vec!["search_path".to_string()]);
        let paused: audiolib_core::server_upgrade::MigrationStatus =
            serde_json::from_str(PAUSED_REPORT).unwrap();
        assert_eq!(paused.kind(), MigrationState::Paused);
        assert_eq!(paused.error_code.as_deref(), Some("disk_full"));
        // Values outside the agreed lists still parse.
        let odd: audiolib_core::server_upgrade::MigrationStatus = serde_json::from_str(
            r#"{"state":"warming_up","error_code":"new_code","progress":{"done":0,"total":0},
            "phases":{"extra":1},"switch":null,"ids":[1],"next_cursor":null}"#,
        )
        .unwrap();
        assert_eq!(odd.kind(), MigrationState::Unknown);
    }

    fn migrating_server() -> TestServer {
        TestServer::start(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/api/v1/health") => {
                    TestResponse::json(200, r#"{"status":"ok","version":"0.9.0"}"#)
                        .with_header(MIGRATION_HEADER, "running")
                }
                ("GET", "/api/v1/capabilities") => marker_capabilities(serde_json::json!({
                    "library_schema": {"current": 9, "snapshot_formats_served":
                        [PLAN_B_V9_FORMAT, audiolib_core::compatibility::PLAN_B_FORMAT]},
                    "schema_migration": {"state": "running"}
                }))
                .with_header(MIGRATION_HEADER, "running")
                .with_header(PATH_SEARCH_HEADER, "unavailable"),
                ("GET", "/admin/api/migration/status") => TestResponse::json(200, RUNNING_REPORT),
                ("POST", "/admin/api/migration/start") => TestResponse::json(
                    409,
                    r#"{"error_code":"already_running","message":"running"}"#,
                ),
                ("GET", path) if path.starts_with("/api/v1/") => {
                    TestResponse::json(404, r#"{"error_code":"NOT_FOUND","message":"no"}"#)
                        .with_header(MIGRATION_HEADER, "running")
                }
                ("POST", "/admin/api/migration/pause") => {
                    assert_eq!(request.header("x-audiolib-admin"), Some("1"));
                    assert_eq!(request.header("content-type"), Some("application/json"));
                    assert_eq!(request.body, b"{}");
                    TestResponse::json(200, PAUSED_REPORT)
                }
                _ => TestResponse::json(404, r#"{"error_code":"NOT_FOUND","message":"no"}"#),
            },
        )
    }

    #[test]
    fn migration_headers_and_status_drive_progress() {
        let server = migrating_server();
        let client = RemoteClient::new(&server.url, "t").unwrap();
        let capabilities = client.capabilities().unwrap();
        let status = server_upgrade_status(&client, &capabilities);
        let migration = status.migration.as_ref().unwrap();
        assert_eq!(migration.percent, Some(25));
        assert_eq!(migration.phase.as_deref(), Some("path_build"));
        assert!(status.path_results_absent);
        assert!(status.main_search);
        assert!(
            status
                .notices
                .contains(&Notice::Migrating { percent: Some(25) })
        );
    }

    #[test]
    fn migration_controls_need_manage_permission() {
        let server = migrating_server();
        let client = RemoteClient::new(&server.url, "t").unwrap();
        let error = client.migration_action("pause", false).unwrap_err();
        assert!(matches!(error, crate::RemoteError::Forbidden(_)));
        assert!(client.migration_action("drop", true).is_err());
        assert_eq!(
            client.migration_action("pause", true).unwrap().state,
            "paused"
        );
        let again = client.migration_action("start", true).unwrap_err();
        assert_eq!(
            again.migration_error_code().as_deref(),
            Some("already_running")
        );
        assert_eq!(again.localized(true), "迁移已在进行中。");
    }

    #[test]
    fn migration_refusals_are_retried_after_the_server_delay() {
        let attempts = std::sync::atomic::AtomicUsize::new(0);
        let server = TestServer::start(move |_| {
            if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
                TestResponse::json(
                    503,
                    r#"{"error_code":"migration_switching","message":"switch"}"#,
                )
                .with_header("retry-after", "30")
            } else {
                TestResponse::json(200, r#"{"status":"ok","version":"0.9.0"}"#)
            }
        });
        let client = RemoteClient::new(&server.url, "t").unwrap();
        let mut waits = Vec::new();
        let health =
            retry_during_migration(5, |delay| waits.push(delay), || client.health()).unwrap();
        assert_eq!(health.version, "0.9.0");
        assert_eq!(waits, vec![Duration::from_secs(30); 2]);

        let path = TestServer::start(|_| {
            TestResponse::json(
                503,
                r#"{"error_code":"migration_in_progress","message":"later"}"#,
            )
            .with_header("retry-after", "30")
        });
        let client = RemoteClient::new(&path.url, "t").unwrap();
        let mut waits = Vec::new();
        let error =
            retry_during_migration(2, |delay| waits.push(delay), || client.health()).unwrap_err();
        assert_eq!(
            error.migration_refusal(),
            Some((MigrationRefusal::PathSearchInProgress, Some(30)))
        );
        assert!(!error.write_may_have_committed());
        assert_eq!(waits, vec![Duration::from_secs(30)]);
        assert!(error.localized(true).contains("路径搜索暂不可用"));
    }

    #[test]
    fn path_probe_uses_path_scope_and_preserves_retry_after_and_cancellation() {
        let server = TestServer::start(|request| {
            assert_eq!(request.path, "/api/v1/search");
            assert_eq!(request.method, "POST");
            assert!(
                request
                    .header(audiolib_core::compatibility::CLIENT_COMPATIBILITY_HEADER)
                    .is_some()
            );
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["scope"], "path");
            assert_eq!(body["limit"], 1);
            TestResponse::json(
                503,
                r#"{"error_code":"migration_in_progress","message":"wait"}"#,
            )
            .with_header("retry-after", "30")
            .with_header(MIGRATION_HEADER, "running")
        });
        let client = RemoteClient::new(&server.url, "").unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let error = client.probe_path_search(&cancel).unwrap_err();
        assert_eq!(
            error.migration_refusal(),
            Some((MigrationRefusal::PathSearchInProgress, Some(30)))
        );
        cancel.store(true, std::sync::atomic::Ordering::Release);
        assert!(matches!(
            client.probe_path_search(&cancel),
            Err(crate::RemoteError::Cancelled)
        ));
    }
}
