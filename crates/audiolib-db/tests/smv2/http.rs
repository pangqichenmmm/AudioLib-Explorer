// crates/audiolib-db/tests/smv2/http.rs
//! Plain HTTP/1.0 client for native migration acceptance (no client dependency).
//!
//! Env: AUDIOLIB_SMV2_ADMIN_URL (http://host:port of a disposable test instance),
//! AUDIOLIB_SMV2_ADMIN_DB (the library file that instance serves; the tests drive
//! the migration on it with `audiolib-migrate` and mutate it), optional
//! AUDIOLIB_SMV2_ADMIN_TOKEN (Bearer).
//!
//! Library endpoints use the native server API, each overridable with
//! AUDIOLIB_SMV2_HTTP_<NAME> (e.g. AUDIOLIB_SMV2_HTTP_PLAYBACK).
//! `{id}` is replaced with a library row id.
//! Optional AUDIOLIB_SMV2_RESET_SERVER_CMD runs before each HTTP fixture opens.
//! It must synchronously restart the disposable server on the same URL and replace
//! ADMIN_DB with a fresh owned schema-8 copy. Assertions stay in smv2_mg.rs.
//!
//! Search `scope` (server 2026-11-03): `main|all|path`; omit = `main` (= schema 8).
//! MAIN_SEARCH omits scope on purpose. Vendor import: `/api/vendor-import` (+ `/*`).
use audiolib_core::compatibility::{
    ACCEPT_FORMATS_HEADER, CLIENT_COMPATIBILITY_HEADER, SEARCH_STORAGE_MAX,
    SEARCH_STORAGE_MAX_HEADER, SNAPSHOT_SCHEMA_MAX, SNAPSHOT_SCHEMA_MAX_HEADER, TOKENIZATION_MAX,
    TOKENIZATION_MAX_HEADER, compatibility_manifest,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    time::Duration,
};

pub const PENDING: &str =
    "pending server migration HTTP (set AUDIOLIB_SMV2_ADMIN_URL, AUDIOLIB_SMV2_ADMIN_DB)";

pub const ADMIN: &str = "/admin/api/migration";

/// (name, method, default path, body). Writes are the vendor import and annotations.
pub const ENDPOINTS: &[(&str, &str, &str, Option<&str>)] = &[
    ("LIST", "GET", "/api/v1/search?limit=5", None),
    (
        // omit scope (= main / s8)
        "MAIN_SEARCH",
        "GET",
        "/api/v1/search?q=zildjian",
        None,
    ),
    (
        "PATH_SEARCH",
        "GET",
        "/api/v1/search?q=Urban&scope=path",
        None,
    ),
    (
        "ALL_SEARCH",
        "GET",
        "/api/v1/search?q=zildjian&scope=all",
        None,
    ),
    ("PLAYBACK", "GET", "/api/v1/audio/{id}/stream", None),
    ("ARTWORK", "GET", "/api/v1/audio/{id}/artwork", None),
    ("VENDOR_IMPORT", "POST", "/api/vendor-import", Some("{}")),
    (
        "ANNOTATIONS",
        "PATCH",
        "/api/v1/audio/{id}/metadata",
        Some(r#"{"tags":"smv2"}"#),
    ),
];

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
    pub fn json(&self) -> Value {
        serde_json::from_str(self.body.trim()).unwrap_or(Value::Null)
    }
}

pub struct Server {
    host: String,
    token: Option<String>,
    library_id: String,
    pub db: PathBuf,
}

impl Server {
    pub fn from_env() -> Server {
        if let Some(command) = std::env::var_os("AUDIOLIB_SMV2_RESET_SERVER_CMD") {
            let status = std::process::Command::new(command)
                .status()
                .expect("reset disposable server");
            assert!(status.success(), "disposable server reset failed: {status}");
        }
        let url = std::env::var("AUDIOLIB_SMV2_ADMIN_URL").expect("AUDIOLIB_SMV2_ADMIN_URL");
        let host = url
            .strip_prefix("http://")
            .expect("AUDIOLIB_SMV2_ADMIN_URL must be http://host:port")
            .trim_end_matches('/')
            .to_string();
        let mut server = Server {
            host,
            token: std::env::var("AUDIOLIB_SMV2_ADMIN_TOKEN").ok(),
            library_id: String::new(),
            db: std::env::var("AUDIOLIB_SMV2_ADMIN_DB")
                .expect("AUDIOLIB_SMV2_ADMIN_DB")
                .into(),
        };
        let library = server.request("GET", "/api/v1/library", None);
        assert_eq!(
            library.status, 200,
            "fixture library unavailable: {library:?}"
        );
        server.library_id = library.json()["library_id"]
            .as_str()
            .expect("served library identity")
            .to_owned();
        server
    }

    pub fn request(&self, method: &str, path: &str, body: Option<&str>) -> Response {
        let mut s = TcpStream::connect(&self.host).expect("connect AUDIOLIB_SMV2_ADMIN_URL");
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let auth = self
            .token
            .as_deref()
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let body = body.unwrap_or("");
        let manifest = compatibility_manifest();
        let accepted = manifest.snapshot_read_formats.join(",");
        let catalog = serde_json::to_string(&manifest).unwrap();
        write!(
            s,
            "{method} {path} HTTP/1.0\r\nHost: {}\r\nAccept: application/json\r\n{auth}X-AudioLib-Admin: 1\r\nX-AudioLib-Library-Id: {}\r\n{CLIENT_COMPATIBILITY_HEADER}: {catalog}\r\n{ACCEPT_FORMATS_HEADER}: {accepted}\r\n{SNAPSHOT_SCHEMA_MAX_HEADER}: {SNAPSHOT_SCHEMA_MAX}\r\n{SEARCH_STORAGE_MAX_HEADER}: {SEARCH_STORAGE_MAX}\r\n{TOKENIZATION_MAX_HEADER}: {TOKENIZATION_MAX}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            self.host,
            self.library_id,
            body.len()
        )
        .unwrap();
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).unwrap();
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("HTTP response");
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|l| l.split(' ').nth(1))
            .and_then(|c| c.parse().ok())
            .expect("status line");
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        Response {
            status,
            headers,
            body: String::from_utf8_lossy(&raw[split + 4..]).into_owned(),
        }
    }

    pub fn status(&self) -> Response {
        self.request("GET", &format!("{ADMIN}/status"), None)
    }

    pub fn admin(&self, action: &str) -> Response {
        self.request("POST", &format!("{ADMIN}/{action}"), Some("{}"))
    }

    /// Call a library endpoint by name (see [`ENDPOINTS`]).
    pub fn call(&self, name: &str, id: i64) -> Response {
        let (_, method, default, body) = ENDPOINTS
            .iter()
            .find(|e| e.0 == name)
            .unwrap_or_else(|| panic!("unknown endpoint {name}"));
        let path = std::env::var(format!("AUDIOLIB_SMV2_HTTP_{name}"))
            .unwrap_or_else(|_| default.to_string())
            .replace("{id}", &id.to_string());
        self.request(method, &path, *body)
    }
}
