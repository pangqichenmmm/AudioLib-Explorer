// app/audiolib-cli/tests/remote_identity.rs
//! Isolated CLI regressions for server/library identity checks before transmitting IDs.
use audiolib_core::DataProvider;
use audiolib_db::SqliteProvider;
use audiolib_remote::{CacheState, RemoteCache};
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

#[allow(dead_code)]
#[path = "../../../crates/audiolib-remote/src/test_server.rs"]
mod test_server;
use test_server::{TestResponse, TestServer};

const LIBRARY: &str = "cli-library-A";

fn prepare_cache(root: &Path, cached_url: &str, configured_url: &str) -> RemoteCache {
    let cache = RemoteCache::new(root);
    let directory = cache.library_dir(LIBRARY);
    std::fs::create_dir_all(&directory).unwrap();
    let database = directory.join("library-r1-test.db");
    drop(SqliteProvider::open(&database).unwrap());
    let conn = rusqlite::Connection::open(&database).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO audio_files(id,file_path,path_key,filename,user_notes)
         VALUES(7,'/A/original.wav','/A/original.wav','original.wav','A notes');",
    )
    .unwrap();
    conn.query_row("PRAGMA journal_mode=DELETE", [], |row| {
        row.get::<_, String>(0)
    })
    .unwrap();
    drop(conn);
    cache
        .save_state(&CacheState {
            library_id: LIBRARY.into(),
            revision: 1,
            schema_version: audiolib_db::SCHEMA_VERSION,
            database_file: "library-r1-test.db".into(),
            server_url: cached_url.into(),
            ..Default::default()
        })
        .unwrap();
    std::fs::write(
        root.join("settings.json"),
        serde_json::to_vec(&json!({
            "language": "en",
            "data_source": {"mode": "server", "server_url": configured_url}
        }))
        .unwrap(),
    )
    .unwrap();
    cache
}

fn cli(root: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_AudioLibCli"))
        .env("AUDIOLIB_DATA_DIR", root)
        .args(args)
        .output()
        .unwrap();
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "Invalid CLI reply: {error}; stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(output.status.success(), reply["success"] == true);
    reply
}

fn rejected_id_commands(root: &Path, message: &str) {
    for args in [
        vec!["remote", "edit", "7", "--notes", "wrong notes"],
        vec!["remote", "download", "7"],
    ] {
        let reply = cli(root, &args);
        assert_eq!(reply["success"], false, "{reply}");
        assert!(
            reply["error"].as_str().unwrap().contains(message),
            "{reply}"
        );
        assert!(reply["error"].as_str().unwrap().contains("remote sync"));
    }
}

#[test]
fn server_switch_rejects_edit_play_and_download_without_transmitting_ids() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server = {
        let requests = requests.clone();
        TestServer::start(move |request| {
            requests.lock().unwrap().push(request.path.clone());
            TestResponse::json(500, "{}")
        })
    };
    let cache = prepare_cache(
        directory.path(),
        "http://server-A.invalid/api/v1",
        &server.url,
    );
    let state = cache.active().unwrap();
    let before = std::fs::read(cache.database_path(&state)).unwrap();
    rejected_id_commands(directory.path(), "another server");
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(cache.active().unwrap(), state);
    assert_eq!(std::fs::read(cache.database_path(&state)).unwrap(), before);
}

#[test]
fn replaced_library_at_same_url_rejects_id_commands_and_preserves_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server = {
        let requests = requests.clone();
        TestServer::start(move |request| {
            requests.lock().unwrap().push(request.path.clone());
            if request.method == "GET" && request.path == "/api/v1/library" {
                TestResponse::json(
                    200,
                    r#"{"library_id":"replacement-B","schema_version":3,"revision":1}"#,
                )
            } else {
                TestResponse::json(500, "{}")
            }
        })
    };
    let cache = prepare_cache(directory.path(), &server.url, &server.url);
    let state = cache.active().unwrap();
    let before = std::fs::read(cache.database_path(&state)).unwrap();
    rejected_id_commands(directory.path(), "library changed");
    assert_eq!(requests.lock().unwrap().as_slice(), ["/api/v1/library"; 2]);
    assert_eq!(cache.active().unwrap(), state);
    assert_eq!(std::fs::read(cache.database_path(&state)).unwrap(), before);
}

#[test]
fn matching_library_with_normalized_url_allows_edits_plays_and_downloads() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let revision = Arc::new(Mutex::new(1u64));
    let server = {
        let requests = requests.clone();
        let revision = revision.clone();
        TestServer::start(move |request| {
            if let Some(response) = test_server::marker_handshake(&request.path) {
                return response;
            }
            requests
                .lock()
                .unwrap()
                .push((request.method.clone(), request.path.clone()));
            let mut revision = revision.lock().unwrap();
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/api/v1/library") => TestResponse::json(
                    200,
                    &json!({"library_id": LIBRARY, "identity_bound_writes":true, "schema_version":5,"revision":*revision})
                        .to_string(),
                ),
                ("PATCH", "/api/v1/audio/7/metadata") => {
                    assert_eq!(request.header(audiolib_remote::LIBRARY_ID_HEADER), Some(LIBRARY));
                    if request.method == "PATCH" {
                        let patch: Value = serde_json::from_slice(&request.body).unwrap();
                        assert_eq!(patch["notes"], "saved notes");
                        assert!(patch.get("rating").is_none());
                    }
                    let before = *revision;
                    *revision += 1;
                    TestResponse::json(
                        200,
                        &json!({
                            "file": audiolib_core::AudioFile {
                                id: 7,
                                file_path: "/A/original.wav".into(),
                                filename: "original.wav".into(),
                                notes: "saved notes".into(),
                                attributes: json!({"modified_at":"2026-10-24 12:34:56.789"}),
                                ..Default::default()
                            },
                            "revision_before": before,
                            "revision": *revision
                        })
                        .to_string(),
                    )
                }
                (_, "/api/v1/audio/7/stream?download=1") => TestResponse::bytes(200, vec![42; 52])
                    .with_header("ETag", "\"test-v1\"")
                    .with_header("Content-Disposition", "attachment; filename=original.wav"),
                _ => TestResponse::json(404, "{}"),
            }
        })
    };
    let cached_url = format!("{}/api/v1/", server.url.replace("http://", "HTTP://"));
    let cache = prepare_cache(directory.path(), &cached_url, &server.url);
    for args in [
        vec!["remote", "edit", "7", "--notes", "saved notes"],
        vec!["remote", "download", "7"],
    ] {
        assert_eq!(cli(directory.path(), &args)["success"], true);
    }
    let state = cache.active().unwrap();
    assert_eq!(state.revision, 2);
    let snapshot = SqliteProvider::open_read_only(&cache.database_path(&state)).unwrap();
    let file = snapshot.get_audio(7).unwrap();
    assert_eq!(file.notes, "saved notes");
    assert!(
        serde_json::to_value(&file)
            .unwrap()
            .get("play_count")
            .is_none()
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0], ("GET".into(), "/api/v1/library".into()));
    let mutations = requests
        .iter()
        .filter(|(method, _)| matches!(method.as_str(), "PATCH" | "POST"))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        mutations,
        [("PATCH".into(), "/api/v1/audio/7/metadata".into()),]
    );
    assert!(
        requests
            .iter()
            .any(|(method, path)| method == "GET" && path.ends_with("?download=1"))
    );
}

#[test]
fn legacy_server_remains_readable_but_cli_never_sends_cached_id_writes() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server = {
        let requests = requests.clone();
        TestServer::start(move |request| {
            requests
                .lock()
                .unwrap()
                .push((request.method.clone(), request.path.clone()));
            if request.method == "GET" && request.path == "/api/v1/library" {
                TestResponse::json(
                    200,
                    &json!({"library_id":LIBRARY,"schema_version":5,"revision":1}).to_string(),
                )
            } else {
                TestResponse::json(500, "{}")
            }
        })
    };
    let cache = prepare_cache(directory.path(), &server.url, &server.url);
    let state = cache.active().unwrap();
    let before = std::fs::read(cache.database_path(&state)).unwrap();
    let reply = cli(
        directory.path(),
        &["remote", "edit", "7", "--notes", "saved notes"],
    );
    assert_eq!(reply["success"], false, "{reply}");
    assert!(
        reply["error"]
            .as_str()
            .unwrap()
            .contains("update the server"),
        "{reply}"
    );
    assert!(
        requests
            .lock()
            .unwrap()
            .iter()
            .all(|(method, path)| method == "GET" && path == "/api/v1/library")
    );
    assert_eq!(cache.active().unwrap(), state);
    assert_eq!(std::fs::read(cache.database_path(&state)).unwrap(), before);
}

#[test]
fn remote_test_reports_incompatible_schema_and_retains_api_and_cache_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server = {
        let requests = requests.clone();
        TestServer::start(move |request| {
            requests
                .lock()
                .unwrap()
                .push((request.method.clone(), request.path.clone()));
            match request.path.as_str() {
                "/api/v1/health" => TestResponse::json(200, r#"{"status":"ok","version":"0.4.0"}"#),
                "/api/v1/library" => TestResponse::json(
                    200,
                    &json!({"library_id": LIBRARY, "schema_version":audiolib_db::SCHEMA_VERSION+1,"revision":282}).to_string(),
                ),
                "/api/v1/roots" => TestResponse::json(200, "[]"),
                _ => TestResponse::json(500, "{}"),
            }
        })
    };
    let cache = prepare_cache(directory.path(), &server.url, &server.url);
    let state = cache.active().unwrap();
    let before = std::fs::read(cache.database_path(&state)).unwrap();
    let reply = cli(directory.path(), &["remote", "test"]);
    assert_eq!(reply["success"], false, "{reply}");
    assert_eq!(reply["error_code"], "REMOTE_TEST_FAILED");
    assert_eq!(
        reply["data"]["library"]["schema_version"],
        audiolib_db::SCHEMA_VERSION + 1
    );
    assert_eq!(reply["data"]["server_version"], "0.4.0");
    assert!(reply["data"].get("unauthorized").is_none());
    let steps = reply["data"]["steps"].as_array().unwrap();
    assert!(
        steps
            .iter()
            .any(|step| step["step"] == "health" && step["ok"] == true)
    );
    let library = steps.iter().find(|step| step["step"] == "library").unwrap();
    assert_eq!(library["ok"], false);
    assert!(
        library["message"]
            .as_str()
            .unwrap()
            .contains(&format!("schema {}", audiolib_db::SCHEMA_VERSION + 1))
    );
    assert!(library["message"].as_str().unwrap().contains(&format!(
        "newer than this client supports ({})",
        audiolib_db::SCHEMA_VERSION
    )));
    assert_eq!(cache.active().unwrap(), state);
    assert_eq!(std::fs::read(cache.database_path(&state)).unwrap(), before);
    assert_eq!(
        requests.lock().unwrap().as_slice(),
        [
            ("GET".into(), "/api/v1/health".into()),
            ("GET".into(), "/api/v1/library".into()),
            ("GET".into(), "/api/v1/roots".into()),
        ]
    );
}
