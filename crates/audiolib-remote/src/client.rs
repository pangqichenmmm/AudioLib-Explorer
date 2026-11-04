// crates/audiolib-remote/src/client.rs
//! Blocking HTTP client for the AudioLib server API (`<server_url>/api/v1`).
use crate::abort::Timeouts;
use crate::error::{RemoteError, Result};
use audiolib_core::AudioFile;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// TCP connect budget for every request; a LAN server answers far faster.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const JSON_BODY_TIMEOUT: Duration = Duration::from_secs(30);
/// `GET /library` computes statistics over the whole table: seconds on a large library,
/// about 30 s cold. The server cuts it off with `408` at 30 s, the client waits longer.
const LIBRARY_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
const LIBRARY_BODY_TIMEOUT: Duration = Duration::from_secs(60);
const SNAPSHOT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(240);
const MAX_JSON_BYTES: u64 = 16 * 1024 * 1024;
/// Metadata writes: the server answers `408 TIMEOUT` after 30 s, so wait a little longer
/// for that classified reply before giving up locally (both mean "outcome uncertain").
const WRITE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(45);
/// The server's request body limit (`413 PAYLOAD_TOO_LARGE` above it).
pub const MAX_WRITE_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Required on mutations: identifies the cached library to which record/session IDs belong.
pub const LIBRARY_ID_HEADER: &str = "X-AudioLib-Library-Id";

/// Reply to `PATCH /audio/{id}/metadata` (§6.3): the record
/// after the write and the library revision before/after it.
#[derive(Clone, Debug, Deserialize)]
pub struct FileWrite {
    pub file: audiolib_core::AudioFile,
    pub revision_before: u64,
    pub revision: u64,
}

/// Reply to `POST /audio/metadata/batch`: `files` in request order, de-duplicated.
#[derive(Clone, Debug, Deserialize)]
pub struct BatchWrite {
    pub report: audiolib_core::BatchMetadataReport,
    #[serde(default)]
    pub files: Vec<audiolib_core::AudioFile>,
    pub revision_before: u64,
    pub revision: u64,
}

/// `MetadataPatch` as JSON with omitted (unchanged) fields left out; `color: ""` clears.
pub(crate) fn patch_json(patch: &audiolib_core::MetadataPatch) -> serde_json::Value {
    let mut value = serde_json::to_value(patch).unwrap_or_default();
    if let serde_json::Value::Object(map) = &mut value {
        map.retain(|_, field| !field.is_null());
    }
    value
}

/// `GET /health` reply.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthInfo {
    /// `ok`, `starting` (library still opening) or `error` (open failed, retrying).
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub version: String,
    /// Fixed summary for `error` (never contains paths or secrets).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// External address configured on the server's admin page (§6.2), e.g.
    /// `https://audiolib.example.com:8443`; `null` when not configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
}

impl HealthInfo {
    /// The server listens but its library is not open yet (`starting` or `error`).
    pub fn not_ready(&self) -> bool {
        matches!(self.status.as_str(), "starting" | "error")
    }
}

/// `GET /library` reply: `LibraryInfo`-like fields plus identity and revision.
/// Unknown fields are ignored; absent optional fields default.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RemoteLibrary {
    /// Actual completed format, independently checked again in the downloaded file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_format: Option<audiolib_core::compatibility::SnapshotFormat>,
    /// Stable identity generated when the server library was created.
    pub library_id: String,
    /// The server atomically enforces `X-AudioLib-Library-Id` on every mutation.
    /// Older servers omit this capability and remain readable; cached-ID writes are refused.
    #[serde(default)]
    pub identity_bound_writes: bool,
    #[serde(default)]
    pub schema_version: i64,
    /// Monotonic, conservative revision: any server write, scan, restore or external
    /// database change bumps it (it may bump without a visible change, never misses one).
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub sqlite_version: String,
    #[serde(default)]
    pub database_bytes: u64,
    #[serde(default)]
    pub stats: serde_json::Value,
}

impl RemoteLibrary {
    /// Present audio record count from `stats.audio_files`, if reported.
    pub fn audio_files(&self) -> Option<u64> {
        self.stats
            .get("audio_files")
            .and_then(serde_json::Value::as_u64)
    }
}

/// One server scan root. `server_path` is the absolute server (macOS) prefix stored in
/// the snapshot; `webdav_url` is only the server's suggested default.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteRoot {
    pub name: String,
    #[serde(default, alias = "path")]
    pub server_path: String,
    #[serde(default)]
    pub webdav_url: Option<String>,
    #[serde(default)]
    pub file_count: Option<u64>,
    /// §6.4 root status (absent on older servers = online); see [`RemoteRoot::state`].
    /// `available` / `reachable` are accepted as aliases of `online`.
    #[serde(default, alias = "available", alias = "reachable")]
    pub online: Option<bool>,
    /// `checking` | `online` | `waking` | `offline`.
    #[serde(default)]
    pub status: Option<String>,
    /// Reason when offline (no paths).
    #[serde(default)]
    pub status_message: Option<String>,
    /// Unix seconds of the server's last completed check.
    #[serde(default)]
    pub checked_at: Option<u64>,
    /// API §6.4 (absent on older servers): records under this root whose file is
    /// missing, the latest scan session of the root (`null` = never scanned) and whether
    /// a scan of it is running now (also while paused).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_scan: Option<crate::manage::RootScan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scanning: Option<bool>,
}

/// Waveform response body limit (JSON after decompression).
const MAX_WAVEFORM_BYTES: u64 = 64 * 1024 * 1024;
/// The server decodes the whole file for an uncached waveform: allow a slow NAS.
const WAVEFORM_TIMEOUT: Duration = Duration::from_secs(120);

/// Parameters of `GET /audio/{id}/waveform` (§9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaveformQuery {
    /// Columns, clamped to the server's `64..=8192`.
    pub buckets: u32,
    pub colors: bool,
    pub start_frame: Option<u64>,
    pub end_frame: Option<u64>,
}

/// Reply of [`RemoteClient::waveform`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaveformReply {
    /// `304`: the copy with the `If-None-Match` ETag is current.
    NotModified,
    /// The JSON body (decompressed) and its ETag.
    Data { json: Vec<u8>, etag: Option<String> },
}

/// Identity headers returned with `GET /library/snapshot`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SnapshotMeta {
    /// Validated format declared by a modern server before reading the body.
    pub snapshot_format: Option<audiolib_core::compatibility::SnapshotFormat>,
    pub bytes: u64,
    /// `X-Library-Id`
    pub library_id: Option<String>,
    /// `X-Library-Revision`: the revision the streamed snapshot was taken at.
    pub revision: Option<u64>,
    /// `X-Schema-Version`
    pub schema_version: Option<i64>,
}

/// Snapshot transfer counters, emitted synchronously on the calling worker thread.
/// Compressed snapshots are decoded while downloading, so these events interleave;
/// callers should retain both counters instead of treating them as sequential stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotProgress {
    /// Response-body bytes read from the network, against `Content-Length` when known.
    Downloading { received: u64, total: Option<u64> },
    /// Database bytes written by the zstd decoder, against `X-Uncompressed-Length`.
    /// Identity responses emit only [`Self::Downloading`].
    Decompressing { written: u64, total: Option<u64> },
}

/// Normalize a configured server URL (`http(s)://host[:port][/prefix]`, optionally with a
/// pasted `/api/v1`) to its API base (`.../api/v1`, no trailing '/'). See
/// [`audiolib_core::server_url`].
pub fn api_base_url(server_url: &str) -> Result<String> {
    audiolib_core::server_url::api_base_url(server_url)
        .map_err(|error| RemoteError::Validation(error.to_string()))
}

/// Blocking API client. The bearer token is held privately and never formatted.
#[derive(Clone)]
pub struct RemoteClient {
    pub(crate) api: String,
    pub(crate) token: String,
    // Timeout profiles over the shared keep-alive agent ([`crate::abort::shared_agent`]).
    pub(crate) agent: Timeouts,
    library: Timeouts,
    pub(crate) bulk: Timeouts,
    pub(crate) writes: Timeouts,
    expected_library_id: Option<String>,
    library_changed: Arc<AtomicBool>,
}

impl std::fmt::Debug for RemoteClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteClient")
            .field("api", &self.api)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// True for the server's own challenge, `WWW-Authenticate: Bearer realm="audiolib"`.
fn is_audiolib_bearer_realm(value: &str) -> bool {
    let value = value.trim();
    value.len() > 6
        && value[..6].eq_ignore_ascii_case("bearer")
        && value[6..]
            .split(',')
            .filter_map(|part| part.split_once('='))
            .any(|(key, val)| {
                key.trim().eq_ignore_ascii_case("realm")
                    && val.trim().trim_matches('"') == "audiolib"
            })
}

/// Suffix (the `.local` API URL) that makes a 401 message carry the LAN-IP hint. `None`
/// when the URL is not `.local` or the 401 came from the AudioLib server itself (its
/// `Bearer realm="audiolib"` challenge or a JSON `UNAUTHORIZED` body).
fn mdns_unauthorized_suffix(api: &str, server_realm: bool, body: &str) -> Option<String> {
    if !crate::error::has_mdns_host(api) || server_realm {
        return None;
    }
    let parsed: crate::error::ErrorBody = serde_json::from_str(body).unwrap_or_default();
    if parsed.error_code == "UNAUTHORIZED" {
        return None;
    }
    Some(format!(" ({api})"))
}

/// `Retry-After` in delta seconds (HTTP dates are ignored), capped at one hour.
pub(crate) fn retry_after(headers: &ureq::http::HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|seconds| seconds.min(3600))
}

/// Request timeouts on the shared keep-alive agent: the response head within `response`,
/// the body within `body`.
pub(crate) fn agent(response: Duration, body: Duration) -> Timeouts {
    Timeouts::new(response, Some(body))
}

/// How often [`call_cancellable`] checks `cancel` and reports the wait.
pub(crate) const WAIT_TICK: Duration = Duration::from_millis(100);

/// Run a blocking HTTP exchange on a helper thread so the caller can stop waiting at once
/// when `cancel` is set (a sleeping NAS may take tens of seconds before the first byte)
/// and can report how long the server has been silent (`waiting`, every [`WAIT_TICK`]).
/// The exchange builds its agent with [`crate::abort::abortable_agent`] and the given
/// [`Abort`](crate::abort::Abort): a cancel shuts its connection down, so the helper
/// thread ends at once and the server sees the request go away.
pub(crate) fn call_cancellable<T: Send + 'static>(
    exchange: impl FnOnce(&crate::abort::Abort) -> Result<T> + Send + 'static,
    cancel: &AtomicBool,
    waiting: &mut dyn FnMut(Duration),
) -> Result<T> {
    if cancel.load(Ordering::Acquire) {
        return Err(RemoteError::Cancelled);
    }
    let abort = crate::abort::Abort::default();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let handle = abort.clone();
    std::thread::Builder::new()
        .name("audiolib-http-call".into())
        .spawn(move || {
            let _ = sender.send(exchange(&handle));
        })?;
    let started = std::time::Instant::now();
    loop {
        match receiver.recv_timeout(WAIT_TICK) {
            Ok(result) => {
                if cancel.load(Ordering::Acquire) {
                    abort.abort();
                    return Err(RemoteError::Cancelled);
                }
                return result;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::Acquire) {
                    abort.abort();
                    return Err(RemoteError::Cancelled);
                }
                waiting(started.elapsed());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(RemoteError::Protocol("HTTP request thread failed".into()));
            }
        }
    }
}

/// Keep the HTTP exchange and file writes on the caller while a scoped watcher shuts
/// its sockets down on cancellation. The watcher always joins before returning, and
/// the caller has already closed/removed any partial file when cancellation returns.
pub(crate) fn cancellable_io<T>(
    cancel: &AtomicBool,
    exchange: impl FnOnce() -> Result<T>,
) -> Result<T> {
    cancellable_io_with_stall(cancel, Duration::from_secs(30), exchange)
}

/// Longest wait for the next snapshot body byte. A slow but moving transfer never trips
/// it and the whole download has no total limit (snapshots can be many GB); a silent
/// connection fails with a [`RemoteError::Timeout`] that the resumable sync retries
/// from its committed prefix with `Range`.
pub(crate) const SNAPSHOT_READ_STALL_TIMEOUT: Duration = Duration::from_secs(90);

#[cfg(test)]
thread_local! {
    static SNAPSHOT_READ_STALL_OVERRIDE: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Override [`SNAPSHOT_READ_STALL_TIMEOUT`] for snapshot downloads on this test thread.
#[cfg(test)]
pub(crate) fn override_snapshot_read_stall(stall: Option<Duration>) {
    SNAPSHOT_READ_STALL_OVERRIDE.with(|value| value.set(stall));
}

pub(crate) fn snapshot_read_stall() -> Duration {
    #[cfg(test)]
    if let Some(stall) = SNAPSHOT_READ_STALL_OVERRIDE.with(std::cell::Cell::get) {
        return stall;
    }
    SNAPSHOT_READ_STALL_TIMEOUT
}

/// [`cancellable_io`] with an explicit per-read stall limit (never a total limit).
pub(crate) fn cancellable_io_with_stall<T>(
    cancel: &AtomicBool,
    read_stall: Duration,
    exchange: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if cancel.load(Ordering::Acquire) {
        return Err(RemoteError::Cancelled);
    }
    std::thread::scope(|scope| {
        let abort = crate::abort::Abort::default();
        let handle = abort.clone();
        let (done, waiting) = std::sync::mpsc::sync_channel::<()>(1);
        std::thread::Builder::new()
            .name("audiolib-snapshot-cancel".into())
            .spawn_scoped(scope, move || {
                loop {
                    if cancel.load(Ordering::Acquire) {
                        handle.abort();
                        break;
                    }
                    match waiting.recv_timeout(WAIT_TICK) {
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        _ => break,
                    }
                }
            })?;
        let _transport_scope = abort.enter(Some(read_stall));
        let result = exchange();
        let _ = done.send(());
        match result {
            Err(_) if cancel.load(Ordering::Acquire) => Err(RemoteError::Cancelled),
            other => other,
        }
    })
}

impl RemoteClient {
    /// Build a client for `server_url` (origin or `/api/v1` base) and bearer token. The
    /// token is optional: when empty, requests carry no `Authorization` header and a `401`
    /// is reported as [`RemoteError::TokenRequired`].
    pub fn new(server_url: &str, token: &str) -> Result<Self> {
        if token.contains(['\r', '\n']) {
            return Err(RemoteError::Validation(
                "Token contains a line break".into(),
            ));
        }
        Ok(Self {
            api: api_base_url(server_url)?,
            token: token.trim().to_owned(),
            agent: agent(RESPONSE_TIMEOUT, JSON_BODY_TIMEOUT),
            library: agent(LIBRARY_RESPONSE_TIMEOUT, LIBRARY_BODY_TIMEOUT),
            bulk: Timeouts::new(SNAPSHOT_RESPONSE_TIMEOUT, None),
            writes: agent(WRITE_RESPONSE_TIMEOUT, JSON_BODY_TIMEOUT),
            expected_library_id: None,
            library_changed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Bind cached IDs to their library. A conflicting rebind permanently refuses writes;
    /// create a fresh client after switching snapshots instead of replacing its identity.
    pub(crate) fn bind_library(mut self, library_id: &str) -> Self {
        match &self.expected_library_id {
            Some(expected) if expected != library_id => {
                self.library_changed.store(true, Ordering::Release);
            }
            None => {
                self.expected_library_id = Some(library_id.to_owned());
                // Unbound clones used for reads need not share a writer's invalidation.
                self.library_changed = Arc::new(AtomicBool::new(false));
            }
            _ => {}
        }
        self
    }

    /// Bind management requests to the displayed snapshot's library identity. This
    /// never replaces an existing binding; use a fresh client after switching libraries.
    pub fn with_library_id(self, library_id: &str) -> Result<Self> {
        if library_id.trim().is_empty() || ureq::http::HeaderValue::from_str(library_id).is_err() {
            return Err(RemoteError::Validation(
                "Invalid cached library identity".into(),
            ));
        }
        let client = self.bind_library(library_id);
        if client.library_changed.load(Ordering::Acquire) {
            return Err(RemoteError::LibraryChanged(
                "The client is bound to a different server library; sync and create a new client"
                    .into(),
            ));
        }
        Ok(client)
    }

    /// Check identity before every mutation, including on older servers that ignore the
    /// identity header. A mismatch observed by any bound clone remains invalidated.
    pub(crate) fn write_library_id(&self) -> Result<String> {
        if self.library_changed.load(Ordering::Acquire) {
            return Err(RemoteError::LibraryChanged(
                "The server library changed; sync and reopen the library before writing".into(),
            ));
        }
        let library = self.library()?;
        if let Some(expected) = &self.expected_library_id
            && (expected.trim().is_empty() || expected != &library.library_id)
        {
            self.library_changed.store(true, Ordering::Release);
            return Err(RemoteError::LibraryChanged(
                "The cached library no longer matches the server; sync before writing".into(),
            ));
        }
        if self.library_changed.load(Ordering::Acquire) {
            return Err(RemoteError::LibraryChanged(
                "The server library changed; sync before writing".into(),
            ));
        }
        if self.expected_library_id.is_some() && !library.identity_bound_writes {
            return Err(RemoteError::Unsupported(
                "The server cannot bind writes to the cached library identity; update the server before editing or managing this library".into(),
            ));
        }
        crate::compatibility::live_library(self, &library, None)?;
        Ok(self
            .expected_library_id
            .clone()
            .unwrap_or(library.library_id))
    }

    /// API base URL, e.g. `http://host:8790/api/v1`.
    /// Range stream source for a record: `GET <api>/audio/{id}/stream` with the token.
    pub fn stream_source(&self, id: i64) -> crate::stream::StreamSource {
        crate::stream::StreamSource::new(
            format!("{}/audio/{id}/stream", self.api),
            if self.token.is_empty() {
                crate::stream::SourceAuth::None
            } else {
                crate::stream::SourceAuth::Bearer(self.token.clone())
            },
        )
    }

    pub fn api_url(&self) -> &str {
        &self.api
    }

    fn request(
        &self,
        agent: &Timeouts,
        path: &str,
        authenticated: bool,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let url = format!("{}{path}", self.api);
        let response = crate::abort::call_fresh(|| {
            let mut request = agent.get(&url);
            if authenticated && !self.token.is_empty() {
                request = request.header("Authorization", &format!("Bearer {}", self.token));
            }
            request
        })
        .map_err(|error| RemoteError::from_transport(error, &url))?;
        self.checked(response, &format!("GET {path}"))
    }

    /// Classify an error status (keeping `Retry-After`); a `401` without a configured
    /// token becomes [`RemoteError::TokenRequired`].
    pub(crate) fn checked(
        &self,
        mut response: ureq::http::Response<ureq::Body>,
        context: &str,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let status = response.status().as_u16();
        if status >= 400 {
            let retry_after = retry_after(response.headers());
            let realm = response
                .headers()
                .get_all("www-authenticate")
                .iter()
                .filter_map(|value| value.to_str().ok())
                .any(is_audiolib_bearer_realm);
            let body = response
                .body_mut()
                .with_config()
                .limit(64 * 1024)
                .read_to_string()
                .unwrap_or_default();
            let mut error = RemoteError::from_response(status, &body, context, retry_after);
            // Keep the `.local` server URL in a 401 so the message can suggest the LAN IP,
            // but only when the 401 is not the server's own (a fake-IP proxy answering for
            // the name returns its own 401); a real wrong token must not get the hint.
            if let RemoteError::Unauthorized(message) = &mut error
                && let Some(suffix) = mdns_unauthorized_suffix(&self.api, realm, &body)
            {
                message.push_str(&suffix);
            }
            if matches!(error, RemoteError::LibraryChanged(_)) {
                self.library_changed.store(true, Ordering::Release);
            }
            return Err(if self.token.is_empty() {
                error.without_token()
            } else {
                error
            });
        }
        // No caller of `checked` sends a conditional header, so `304 Not Modified` (like
        // any other 1xx/3xx; redirects are not followed) carries no usable body and is a
        // protocol error here. Conditional requests use [`Self::checked_conditional`].
        if status == 304 {
            return Err(RemoteError::Protocol(format!(
                "{context}: unexpected HTTP 304 Not Modified to an unconditional request"
            )));
        }
        if !(200..300).contains(&status) {
            return Err(RemoteError::Protocol(format!("{context}: HTTP {status}")));
        }
        Ok(response)
    }

    /// [`Self::checked`] for a request that may carry `If-None-Match`: `304 Not Modified`
    /// is `Ok(None)` (the cached copy is current) when `conditional` is set, and a protocol
    /// error otherwise. Every other status is classified like [`Self::checked`].
    pub(crate) fn checked_conditional(
        &self,
        response: ureq::http::Response<ureq::Body>,
        context: &str,
        conditional: bool,
    ) -> Result<Option<ureq::http::Response<ureq::Body>>> {
        if response.status().as_u16() == 304 {
            return if conditional {
                Ok(None)
            } else {
                Err(RemoteError::Protocol(format!(
                    "{context}: HTTP 304 Not Modified without If-None-Match"
                )))
            };
        }
        self.checked(response, context).map(Some)
    }

    /// One non-idempotent JSON write (never retried here). A transport failure or an
    /// unreadable reply leaves the outcome uncertain; see
    /// [`RemoteError::write_may_have_committed`].
    fn write<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T> {
        let url = format!("{}{path}", self.api);
        let context = format!("{method} {path}");
        let bytes = match body {
            Some(body) => {
                let bytes = serde_json::to_vec(body)
                    .map_err(|error| RemoteError::Validation(format!("{context}: {error}")))?;
                if bytes.len() > MAX_WRITE_BODY_BYTES {
                    return Err(RemoteError::PayloadTooLarge(format!(
                        "{context}: {} bytes exceeds the {MAX_WRITE_BODY_BYTES}-byte request limit",
                        bytes.len()
                    )));
                }
                Some(bytes)
            }
            None => None,
        };
        let authorization = (!self.token.is_empty()).then(|| format!("Bearer {}", self.token));
        let library_id = self.write_library_id()?;
        let compatibility = crate::compatibility::client_header()?;
        let result = match method {
            "PATCH" => {
                let mut request = self
                    .writes
                    .patch(&url)
                    .header(LIBRARY_ID_HEADER, &library_id)
                    .header(
                        audiolib_core::compatibility::CLIENT_COMPATIBILITY_HEADER,
                        &compatibility,
                    );
                if let Some(value) = &authorization {
                    request = request.header("Authorization", value);
                }
                request
                    .header("Content-Type", "application/json")
                    .send(bytes.unwrap_or_default().as_slice())
            }
            _ => {
                let mut request = self
                    .writes
                    .post(&url)
                    .header(LIBRARY_ID_HEADER, &library_id)
                    .header(
                        audiolib_core::compatibility::CLIENT_COMPATIBILITY_HEADER,
                        &compatibility,
                    );
                if let Some(value) = &authorization {
                    request = request.header("Authorization", value);
                }
                match bytes {
                    Some(bytes) => request
                        .header("Content-Type", "application/json")
                        .send(bytes.as_slice()),
                    None => request.send_empty(),
                }
            }
        };
        let response = result.map_err(|error| RemoteError::from_transport(error, &url))?;
        let mut response = self.checked(response, &context)?;
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_JSON_BYTES)
            .read_to_string()
            .map_err(|error| match RemoteError::from_transport(error, &url) {
                RemoteError::Timeout(detail) => RemoteError::Timeout(detail),
                other => RemoteError::Protocol(format!("{context}: {other}")),
            })?;
        serde_json::from_str(&text)
            .map_err(|error| RemoteError::Protocol(format!("{context}: {error}")))
    }

    /// `PATCH /audio/{id}/metadata`: omitted fields stay unchanged, `color: ""` clears.
    /// Validated locally first with the server's rules.
    pub fn patch_metadata(
        &self,
        id: i64,
        patch: &audiolib_core::MetadataPatch,
    ) -> Result<FileWrite> {
        patch
            .validate()
            .map_err(|error| RemoteError::Validation(error.to_string()))?;
        if id <= 0 {
            return Err(RemoteError::Validation(format!("invalid audio id {id}")));
        }
        self.write(
            "PATCH",
            &format!("/audio/{id}/metadata"),
            Some(&patch_json(patch)),
        )
    }

    /// `POST /audio/metadata/batch` (one server transaction; any unknown id → `404`).
    pub fn batch_metadata(
        &self,
        request: &audiolib_core::BatchMetadataRequest,
    ) -> Result<BatchWrite> {
        request
            .validate()
            .map_err(|error| RemoteError::Validation(error.to_string()))?;
        let body = serde_json::json!({
            "ids": request.ids,
            "patch": patch_json(&request.patch),
            "tag_mode": request.tag_mode,
        });
        self.write("POST", "/audio/metadata/batch", Some(&body))
    }

    /// Audition source of a record for `route` (§8, §8.1): the original `/stream`, or a
    /// `/preview` (a FLAC preview is decoded to PCM and needs the record's duration). A
    /// large file is fetched [`READ_AHEAD_SECONDS`](crate::stream::READ_AHEAD_SECONDS)
    /// ahead of the player.
    pub fn audio_source(
        &self,
        file: &AudioFile,
        route: &crate::route::AudioRoute,
    ) -> crate::stream::StreamSource {
        let id = file.id;
        let mut source = self.stream_source(id);
        source.read_ahead =
            Some(route.output_bytes_per_second(file) * crate::stream::READ_AHEAD_SECONDS);
        if let Some(query) = route.preview_query() {
            source.url = format!("{}/audio/{id}/preview?{query}", self.api);
            if matches!(route, crate::route::AudioRoute::Flac(_)) {
                source.flac_duration = Some(Duration::from_secs_f64(file.duration.max(0.0)));
            }
        }
        source
    }

    /// `GET /audio/{id}/waveform` (§9) with `If-None-Match` and `Accept-Encoding: zstd`.
    /// `cancel` closes the connection at once.
    pub fn waveform(
        &self,
        id: i64,
        query: &WaveformQuery,
        if_none_match: Option<&str>,
        cancel: &AtomicBool,
    ) -> Result<WaveformReply> {
        let mut path = format!(
            "/audio/{id}/waveform?buckets={}&colors={}",
            query.buckets.clamp(64, 8192),
            u8::from(query.colors)
        );
        if let Some(start) = query.start_frame {
            path.push_str(&format!("&start_frame={start}"));
        }
        if let Some(end) = query.end_frame {
            path.push_str(&format!("&end_frame={end}"));
        }
        let url = format!("{}{path}", self.api);
        let token = (!self.token.is_empty()).then(|| format!("Bearer {}", self.token));
        let if_none_match = if_none_match.map(str::to_owned);
        let context = format!("GET {path}");
        let (status, etag, encoding, body, retry) = call_cancellable(
            move |abort| {
                let agent =
                    crate::abort::abortable_agent(WAVEFORM_TIMEOUT, WAVEFORM_TIMEOUT, abort);
                let mut response = crate::abort::call_fresh(|| {
                    let mut request = agent.get(&url).header("Accept-Encoding", "zstd");
                    if let Some(token) = &token {
                        request = request.header("Authorization", token);
                    }
                    if let Some(etag) = &if_none_match {
                        request = request.header("If-None-Match", etag);
                    }
                    request
                })
                .map_err(|error| RemoteError::from_transport(error, &url))?;
                let header = |name: &str| {
                    response
                        .headers()
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .map(|value| value.trim().to_owned())
                };
                let (etag, encoding) = (header("etag"), header("content-encoding"));
                let retry = retry_after(response.headers());
                let body = response
                    .body_mut()
                    .with_config()
                    .limit(MAX_WAVEFORM_BYTES)
                    .read_to_vec()
                    .map_err(|error| RemoteError::from_transport(error, &url))?;
                Ok((response.status().as_u16(), etag, encoding, body, retry))
            },
            cancel,
            &mut |_| {},
        )?;
        match status {
            304 => Ok(WaveformReply::NotModified),
            200 => {
                let json = match encoding.as_deref() {
                    Some(value) if value.eq_ignore_ascii_case("zstd") => {
                        let mut json = Vec::new();
                        zstd::stream::read::Decoder::new(body.as_slice())
                            .and_then(|decoder| {
                                decoder.take(MAX_WAVEFORM_BYTES).read_to_end(&mut json)
                            })
                            .map_err(|error| {
                                RemoteError::Protocol(format!("{context}: zstd: {error}"))
                            })?;
                        json
                    }
                    None => body,
                    Some(value) if value.eq_ignore_ascii_case("identity") => body,
                    Some(other) => {
                        return Err(RemoteError::Protocol(format!(
                            "{context}: unexpected Content-Encoding {other}"
                        )));
                    }
                };
                Ok(WaveformReply::Data { json, etag })
            }
            401 if self.token.is_empty() => Err(RemoteError::TokenRequired(context)),
            _ => Err(RemoteError::from_audio_response(
                status,
                &String::from_utf8_lossy(&body),
                &context,
                retry,
            )),
        }
    }

    /// Download source for a record: `GET <api>/audio/{id}/stream?download=1`.
    pub fn download_source(&self, id: i64) -> crate::stream::StreamSource {
        let mut source = self.stream_source(id);
        source.url.push_str("?download=1");
        source
    }

    fn json<T: serde::de::DeserializeOwned>(&self, path: &str, authenticated: bool) -> Result<T> {
        self.json_on(&self.agent, path, authenticated)
    }

    fn json_on<T: serde::de::DeserializeOwned>(
        &self,
        agent: &Timeouts,
        path: &str,
        authenticated: bool,
    ) -> Result<T> {
        let mut response = self.request(agent, path, authenticated)?;
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_JSON_BYTES)
            .read_to_string()
            .map_err(|error| RemoteError::from_transport(error, path))?;
        serde_json::from_str(&body)
            .map_err(|error| RemoteError::Protocol(format!("GET {path}: {error}")))
    }

    /// `GET /health` (no authentication).
    pub fn health(&self) -> Result<HealthInfo> {
        self.json("/health", false)
    }

    /// A short JSON exchange cancellable during both response headers and body reads.
    fn read_cancellable<T: Send + 'static>(
        &self,
        cancel: &AtomicBool,
        read: impl FnOnce(Self) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let client = self.clone();
        call_cancellable(
            move |abort| {
                let _scope = abort.enter(None);
                read(client)
            },
            cancel,
            &mut |_| {},
        )
    }

    pub(crate) fn health_cancellable(&self, cancel: &AtomicBool) -> Result<HealthInfo> {
        self.read_cancellable(cancel, |client| client.health())
    }

    pub(crate) fn library_cancellable(&self, cancel: &AtomicBool) -> Result<RemoteLibrary> {
        self.read_cancellable(cancel, |client| client.library())
    }

    pub(crate) fn capabilities_cancellable(
        &self,
        cancel: &AtomicBool,
    ) -> Result<crate::manage::Capabilities> {
        self.read_cancellable(cancel, |client| client.capabilities())
    }

    pub(crate) fn roots_cancellable(&self, cancel: &AtomicBool) -> Result<Vec<RemoteRoot>> {
        self.read_cancellable(cancel, |client| client.roots())
    }

    /// `GET /library`: identity, schema and revision of the server library.
    pub fn library(&self) -> Result<RemoteLibrary> {
        let library: RemoteLibrary = self.json_on(&self.library, "/library", true)?;
        if library.library_id.trim().is_empty() {
            return Err(RemoteError::Protocol(
                "GET /library: library_id is missing".into(),
            ));
        }
        if self
            .expected_library_id
            .as_ref()
            .is_some_and(|expected| expected != &library.library_id)
        {
            self.library_changed.store(true, Ordering::Release);
        }
        Ok(library)
    }

    /// `GET /roots`: accepts a bare array or an object with a `roots` array.
    pub fn roots(&self) -> Result<Vec<RemoteRoot>> {
        let value: serde_json::Value = self.json("/roots", true)?;
        let array = match value {
            serde_json::Value::Object(mut object) => object
                .remove("roots")
                .unwrap_or(serde_json::Value::Array(Vec::new())),
            other => other,
        };
        serde_json::from_value(array)
            .map_err(|error| RemoteError::Protocol(format!("GET /roots: {error}")))
    }

    /// `POST /roots/{name}/check` (§6.4): re-check one root now (the server answers within
    /// about 2 s, with the state at that point if the check is still running) and return
    /// its updated `/roots` entry. [`RemoteError::NotFound`] for an unknown root, and also
    /// from servers that predate the endpoint.
    pub fn check_root(&self, name: &str) -> Result<RemoteRoot> {
        let path = format!("/roots/{}/check", crate::util::encode_path_segment(name));
        let url = format!("{}{path}", self.api);
        let context = format!("POST {path}");
        let mut request = self.agent.post(&url).header(
            audiolib_core::compatibility::CLIENT_COMPATIBILITY_HEADER,
            &crate::compatibility::client_header()?,
        );
        if !self.token.is_empty() {
            request = request.header("Authorization", &format!("Bearer {}", self.token));
        }
        let response = request
            .send_empty()
            .map_err(|error| RemoteError::from_transport(error, &url))?;
        let mut response = self.checked(response, &context)?;
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_JSON_BYTES)
            .read_to_string()
            .map_err(|error| RemoteError::from_transport(error, &url))?;
        serde_json::from_str(&body)
            .map_err(|error| RemoteError::Protocol(format!("{context}: {error}")))
    }

    /// Stream `GET /library/snapshot` into `destination` through a sibling `.part`
    /// file, then atomically rename. `progress` reports independent network and zstd
    /// output counters per chunk; their totals remain unknown when headers omit them.
    /// `cancel` closes the request socket during headers and body reads; a failure
    /// preserves an existing target and removes the partial file before returning.
    /// A `409 BUSY` (server snapshot slots exhausted) is returned as [`RemoteError::Busy`]
    /// so callers can retry with backoff.
    pub fn download_snapshot(
        &self,
        destination: &Path,
        progress: &mut dyn FnMut(SnapshotProgress),
        cancel: &AtomicBool,
    ) -> Result<SnapshotMeta> {
        self.download_snapshot_staged(destination, progress, &mut |_| {}, cancel)
    }

    /// [`Self::download_snapshot`], also reporting [`DownloadStage::Writing`] before the
    /// file is flushed to disk. A cancel during the flush returns at once; the abandoned
    /// flush finishes on its own thread and then removes the partial file.
    pub fn download_snapshot_staged(
        &self,
        destination: &Path,
        progress: &mut dyn FnMut(SnapshotProgress),
        stage: &mut dyn FnMut(DownloadStage),
        cancel: &AtomicBool,
    ) -> Result<SnapshotMeta> {
        cancellable_io_with_stall(cancel, snapshot_read_stall(), || {
            self.download_snapshot_io(destination, progress, stage, cancel)
        })
    }

    fn download_snapshot_io(
        &self,
        destination: &Path,
        progress: &mut dyn FnMut(SnapshotProgress),
        stage: &mut dyn FnMut(DownloadStage),
        cancel: &AtomicBool,
    ) -> Result<SnapshotMeta> {
        let url = format!("{}/library/snapshot", self.api);
        let catalog = audiolib_core::compatibility::compatibility_manifest();
        let catalog_json = serde_json::to_string(&catalog)
            .map_err(|error| RemoteError::Protocol(error.to_string()))?;
        let accepted = catalog.snapshot_read_formats.join(", ");
        let response = crate::abort::call_fresh(|| {
            let mut request = self
                .bulk
                .get(&url)
                .header("Accept-Encoding", "zstd")
                .header(
                    audiolib_core::compatibility::CLIENT_COMPATIBILITY_HEADER,
                    &catalog_json,
                )
                .header(
                    audiolib_core::compatibility::ACCEPT_FORMATS_HEADER,
                    &accepted,
                )
                .header(
                    audiolib_core::compatibility::SNAPSHOT_SCHEMA_MAX_HEADER,
                    &audiolib_core::compatibility::SNAPSHOT_SCHEMA_MAX.to_string(),
                )
                .header(
                    audiolib_core::compatibility::SEARCH_STORAGE_MAX_HEADER,
                    &audiolib_core::compatibility::SEARCH_STORAGE_MAX.to_string(),
                )
                .header(
                    audiolib_core::compatibility::TOKENIZATION_MAX_HEADER,
                    &audiolib_core::compatibility::TOKENIZATION_MAX.to_string(),
                );
            if !self.token.is_empty() {
                request = request.header("Authorization", &format!("Bearer {}", self.token));
            }
            request
        })
        .map_err(|error| RemoteError::from_transport(error, &url))?;
        let response = self.checked(response, "GET /library/snapshot")?;
        let snapshot_format = crate::compatibility::response_format(response.headers())?;
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .map(str::to_owned)
        };
        // With `Content-Encoding: zstd`, Content-Length is the compressed size and
        // X-Uncompressed-Length the database size (decompression, preallocation, check).
        let compressed = match header("content-encoding").as_deref() {
            None => false,
            Some(value) if value.eq_ignore_ascii_case("identity") => false,
            Some(value) if value.eq_ignore_ascii_case("zstd") => true,
            Some(other) => {
                return Err(RemoteError::Protocol(format!(
                    "snapshot: unexpected Content-Encoding {other}"
                )));
            }
        };
        let wire_length = header("content-length").and_then(|value| value.parse::<u64>().ok());
        let total = if compressed {
            header("x-uncompressed-length").and_then(|value| value.parse::<u64>().ok())
        } else {
            wire_length
        };
        let mut meta = SnapshotMeta {
            snapshot_format,
            bytes: 0,
            library_id: header("x-library-id").filter(|value| !value.is_empty()),
            revision: header("x-library-revision").and_then(|value| value.parse().ok()),
            schema_version: header("x-schema-version").and_then(|value| value.parse().ok()),
        };
        let parent = destination.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        // `<name>.download.part-<pid>`: recognised (and removed once stale) by `prune`.
        let mut partial = destination.as_os_str().to_owned();
        partial.push(format!(".part-{}", std::process::id()));
        let partial = std::path::PathBuf::from(partial);
        let result = (|| -> Result<u64> {
            let mut file = std::fs::File::create(&partial)?;
            if let Some(total) = total.filter(|_| compressed) {
                // Reserve the space up front (the file is written sequentially over it).
                file.set_len(total)?;
            }
            let wire = Counted::new(response.into_body().into_reader());
            let wire_count = wire.count.clone();
            let mut reader: Box<dyn Read> =
                if compressed {
                    Box::new(zstd::stream::read::Decoder::new(wire).map_err(|error| {
                        RemoteError::Protocol(format!("snapshot: zstd: {error}"))
                    })?)
                } else {
                    Box::new(wire)
                };
            let mut buffer = vec![0u8; 256 * 1024];
            let mut received = 0u64;
            let mut throughput = crate::bandwidth::Throughput::new();
            let mut counted = 0;
            progress(SnapshotProgress::Downloading {
                received: 0,
                total: wire_length,
            });
            if compressed {
                progress(SnapshotProgress::Decompressing { written: 0, total });
            }
            let download_started = std::time::Instant::now();
            tracing::info!(?total, compressed, "Snapshot stage start: downloading");
            loop {
                if cancel.load(Ordering::Acquire) {
                    return Err(RemoteError::Cancelled);
                }
                let read = reader.read(&mut buffer).map_err(|error| {
                    if cancel.load(Ordering::Acquire) {
                        RemoteError::Cancelled
                    } else if error.kind() == std::io::ErrorKind::Other && compressed {
                        RemoteError::Protocol(format!("snapshot: zstd: {error}"))
                    } else {
                        RemoteError::Unreachable(format!("snapshot: {error}"))
                    }
                })?;
                let wire_now = wire_count.load(Ordering::Acquire);
                throughput.add(wire_now - counted, crate::bandwidth::bandwidth());
                counted = wire_now;
                // Report the wire counter even on EOF: the decoder may consume its
                // final frame bytes without producing another database chunk.
                progress(SnapshotProgress::Downloading {
                    received: wire_now,
                    total: wire_length,
                });
                if read == 0 {
                    break;
                }
                file.write_all(&buffer[..read])?;
                received += read as u64;
                if compressed {
                    progress(SnapshotProgress::Decompressing {
                        written: received,
                        total,
                    });
                }
            }
            throughput.pause(crate::bandwidth::bandwidth());
            // The last update always reaches the caller (UI throttles drop repeats).
            if compressed {
                progress(SnapshotProgress::Decompressing {
                    written: received,
                    total,
                });
            } else {
                progress(SnapshotProgress::Downloading {
                    received,
                    total: wire_length,
                });
            }
            tracing::info!(
                received,
                elapsed_ms = download_started.elapsed().as_millis() as u64,
                "Snapshot stage end: downloading"
            );
            let wire_received = wire_count.load(Ordering::Acquire);
            if wire_length.is_some_and(|length| length != wire_received) {
                return Err(RemoteError::Protocol(format!(
                    "snapshot truncated: received {wire_received} of {} bytes",
                    wire_length.unwrap_or_default()
                )));
            }
            if total.is_some_and(|total| total != received) {
                return Err(RemoteError::Protocol(format!(
                    "snapshot size mismatch: {received} bytes after decompression, {} expected",
                    total.unwrap_or_default()
                )));
            }
            if received == 0 {
                return Err(RemoteError::Protocol("snapshot is empty".into()));
            }
            stage(DownloadStage::Writing);
            flush_abandonable(file, &partial, cancel)?;
            if cancel.load(Ordering::Acquire) {
                return Err(RemoteError::Cancelled);
            }
            let renamed = std::time::Instant::now();
            std::fs::rename(&partial, destination)?;
            tracing::info!(
                elapsed_ms = renamed.elapsed().as_millis() as u64,
                "Snapshot stage end: rename"
            );
            Ok(received)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        meta.bytes = result?;
        Ok(meta)
    }
}

/// Stages of [`RemoteClient::download_snapshot_staged`] after the byte stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadStage {
    /// Flushing the downloaded file to disk (`sync_all`), which can take long on
    /// slow or busy disks.
    Writing,
}

/// `sync_all` on a worker thread the caller can abandon on cancel: the worker keeps
/// the file, finishes the flush, closes it and then deletes `partial` itself (a file
/// still open cannot be removed on Windows).
pub(crate) fn flush_abandonable(
    file: std::fs::File,
    partial: &Path,
    cancel: &AtomicBool,
) -> Result<()> {
    let started = std::time::Instant::now();
    tracing::info!("Snapshot stage start: writing to disk");
    let abandoned = std::sync::Arc::new(AtomicBool::new(false));
    let (done, finished) = std::sync::mpsc::sync_channel::<std::io::Result<()>>(1);
    let worker_abandoned = abandoned.clone();
    let worker_partial = partial.to_path_buf();
    std::thread::Builder::new()
        .name("audiolib-snapshot-flush".into())
        .spawn(move || {
            let result = file.sync_all();
            drop(file);
            if worker_abandoned.load(Ordering::Acquire) {
                let _ = std::fs::remove_file(&worker_partial);
            }
            let _ = done.send(result);
        })?;
    loop {
        match finished.recv_timeout(WAIT_TICK) {
            Ok(result) => {
                tracing::info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    ok = result.is_ok(),
                    "Snapshot stage end: writing to disk"
                );
                return Ok(result?);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::Acquire) {
                    abandoned.store(true, Ordering::Release);
                    tracing::info!(
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "Snapshot stage cancelled: writing to disk"
                    );
                    return Err(RemoteError::Cancelled);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(RemoteError::Snapshot(
                    "snapshot flush worker stopped".into(),
                ));
            }
        }
    }
}

/// Reader counting the bytes that pass (the compressed size of a zstd snapshot).
struct Counted<R> {
    inner: R,
    count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<R: Read> Counted<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            count: Default::default(),
        }
    }
}

impl<R: Read> Read for Counted<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.count.fetch_add(read as u64, Ordering::AcqRel);
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{TestResponse, TestServer};

    #[test]
    fn cancelling_a_waiting_call_closes_its_connection() {
        let (url, closed) = crate::abort::tests::silent_server();
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let cancelled_at = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            flag.store(true, Ordering::Release);
            std::time::Instant::now()
        });
        let result = call_cancellable(
            move |abort| {
                crate::abort::abortable_agent(
                    Duration::from_secs(60),
                    Duration::from_secs(60),
                    abort,
                )
                .get(&url)
                .call()
                .map(|_| ())
                .map_err(|error| RemoteError::Protocol(error.to_string()))
            },
            &cancel,
            &mut |_| {},
        );
        assert!(matches!(result, Err(RemoteError::Cancelled)), "{result:?}");
        let cancelled_at = cancelled_at.join().unwrap();
        let seen = closed
            .recv_timeout(Duration::from_secs(5))
            .expect("the server must see the connection closed");
        assert!(seen.duration_since(cancelled_at) < Duration::from_secs(2));
    }

    fn api_server() -> TestServer {
        TestServer::start(|request| {
            let authorized = request.header("authorization") == Some("Bearer secret-token");
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/api/v1/health") => {
                    TestResponse::json(200, r#"{"status":"ok","version":"0.3.5","extra":true}"#)
                }
                (_, _) if !authorized => TestResponse::json(
                    401,
                    r#"{"error_code":"UNAUTHORIZED","message":"Missing or invalid token"}"#,
                ),
                ("GET", "/api/v1/library") => TestResponse::json(
                    200,
                    r#"{"library_id":"lib-1","schema_version":3,"revision":7,"database":"/srv/library.db","sqlite_version":"3.50","database_bytes":4096,"wal_bytes":0,"stats":{"audio_files":12,"folders":2,"total_duration":1.0,"total_bytes":5,"missing_files":0},"future_field":[1]}"#,
                ),
                ("GET", "/api/v1/roots") => TestResponse::json(
                    200,
                    r#"[{"name":"SFX","server_path":"/Volumes/Sound/SFX","webdav_url":"http://nas/dav/SFX","file_count":25,"online":false,"status":"offline","status_message":"Root directory not found (volume not mounted?)","checked_at":1704067200},{"name":"Foley","path":"/Volumes/Sound/Foley","available":true}]"#,
                ),
                ("GET", "/api/v1/library/snapshot") => TestResponse::bytes(200, vec![7u8; 300_000])
                    .with_header("X-Library-Id", "lib-1")
                    .with_header("X-Library-Revision", "8")
                    .with_header("X-Schema-Version", "3"),
                _ => TestResponse::json(404, r#"{"error_code":"NOT_FOUND","message":"no route"}"#),
            }
        })
    }

    #[test]
    fn api_base_url_appends_version_once_and_rejects_bad_urls() {
        assert_eq!(
            api_base_url("http://h:8790").unwrap(),
            "http://h:8790/api/v1"
        );
        assert_eq!(
            api_base_url("http://h:8790/").unwrap(),
            "http://h:8790/api/v1"
        );
        assert_eq!(api_base_url("http://h/api/v1/").unwrap(), "http://h/api/v1");
        assert_eq!(
            api_base_url(" https://audiolib.example.com:8443/audiolib/ ").unwrap(),
            "https://audiolib.example.com:8443/audiolib/api/v1"
        );
        assert!(api_base_url("h:8790").is_err());
        assert!(api_base_url("https://host:99999").is_err());
        assert!(RemoteClient::new("http://h", "a\nb").is_err());
    }

    #[test]
    fn root_check_posts_and_returns_the_updated_root() {
        let server = TestServer::start(|request| {
            match (request.method.as_str(), request.path.as_str()) {
                ("POST", "/api/v1/roots/%E9%9F%B3%E6%95%88/check") => {
                    assert_eq!(request.header("authorization"), Some("Bearer secret-token"));
                    TestResponse::json(
                        200,
                        r#"{"name":"音效","path":"/Volumes/SFX","online":true,"status":"online","file_count":3}"#,
                    )
                }
                _ => TestResponse::json(
                    404,
                    r#"{"error_code":"NOT_FOUND","message":"Unknown root"}"#,
                ),
            }
        });
        let client = RemoteClient::new(&server.url, "secret-token").unwrap();
        let root = client.check_root("音效").unwrap();
        assert_eq!(root.name, "音效");
        assert_eq!(root.server_path, "/Volumes/SFX");
        assert_eq!(root.state(), crate::RootState::Online);
        assert!(matches!(
            client.check_root("Nope"),
            Err(RemoteError::NotFound(_))
        ));
    }

    #[test]
    fn client_reads_tolerant_dtos_with_bearer_authentication() {
        let server = api_server();
        let client = RemoteClient::new(&server.url, "secret-token").unwrap();
        assert_eq!(client.health().unwrap().version, "0.3.5");
        let library = client.library().unwrap();
        assert_eq!(library.library_id, "lib-1");
        assert_eq!((library.schema_version, library.revision), (3, 7));
        assert_eq!(library.audio_files(), Some(12));
        let roots = client.roots().unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].webdav_url.as_deref(), Some("http://nas/dav/SFX"));
        assert_eq!(roots[1].server_path, "/Volumes/Sound/Foley");
        assert_eq!(roots[0].state(), crate::RootState::Offline);
        assert_eq!(roots[0].file_count, Some(25));
        assert_eq!(roots[0].checked_at, Some(1704067200));
        assert!(
            roots[0]
                .status_message
                .as_deref()
                .unwrap()
                .contains("not mounted")
        );
        assert_eq!(roots[1].online, Some(true));
        assert!(!format!("{client:?}").contains("secret-token"));
    }

    #[test]
    fn library_waits_longer_than_other_json_requests() {
        assert!(LIBRARY_RESPONSE_TIMEOUT >= Duration::from_secs(60));
        assert!(LIBRARY_RESPONSE_TIMEOUT > RESPONSE_TIMEOUT);
        let client = RemoteClient::new("http://127.0.0.1:1", "t").unwrap();
        assert_eq!(client.library.response, LIBRARY_RESPONSE_TIMEOUT);
        assert_eq!(client.agent.response, RESPONSE_TIMEOUT);
    }

    #[test]
    fn unauthorized_and_unreachable_errors_are_classified() {
        let server = api_server();
        let client = RemoteClient::new(&server.url, "wrong").unwrap();
        assert!(client.health().is_ok());
        let error = client.library().unwrap_err();
        assert!(
            matches!(error, RemoteError::Unauthorized(ref message) if message.contains("invalid token"))
        );
        assert!(error.localized(true).contains("401"));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let offline = RemoteClient::new(&format!("http://127.0.0.1:{port}"), "x").unwrap();
        let error = offline.health().unwrap_err();
        assert!(
            matches!(error, RemoteError::Unreachable(_)),
            "a refused connection is offline, not slow: {error:?}"
        );
    }

    #[test]
    fn mdns_hint_only_for_a_401_that_is_not_the_servers_own() {
        let api = "http://example.local:8790/api/v1";
        let own = r#"{"error_code":"UNAUTHORIZED","message":"invalid token"}"#;
        // The server's own 401 (realm header and/or UNAUTHORIZED body): no hint.
        assert!(is_audiolib_bearer_realm(r#"Bearer realm="audiolib""#));
        assert!(is_audiolib_bearer_realm(
            r#"bearer realm=audiolib, error="x""#
        ));
        assert!(!is_audiolib_bearer_realm(r#"Basic realm="proxy""#));
        assert!(!is_audiolib_bearer_realm(r#"Bearer realm="other""#));
        assert_eq!(mdns_unauthorized_suffix(api, true, ""), None);
        assert_eq!(mdns_unauthorized_suffix(api, false, own), None);
        assert_eq!(mdns_unauthorized_suffix(api, true, own), None);
        // A bare 401 (proxy-blocked): hint.
        let bare = mdns_unauthorized_suffix(api, false, "Unauthorized").unwrap();
        let error = RemoteError::Unauthorized(format!("GET /library: HTTP 401{bare}"));
        assert!(error.localized(true).contains("局域网 IP"));
        assert!(error.localized(false).contains("LAN IP"));
        let server = RemoteError::Unauthorized("GET /library: invalid token".into());
        assert!(!server.localized(true).contains("fake-ip"));
        // Not a .local URL: never a hint.
        assert_eq!(
            mdns_unauthorized_suffix("http://192.0.2.1:8790/api/v1", false, ""),
            None
        );
        // Connect failures to a .local host keep the hint.
        let down = RemoteError::Unreachable(format!("{api}/health: connection failed"));
        assert!(down.localized(true).contains("局域网 IP"));
    }

    #[test]
    fn server_401_end_to_end_has_no_mdns_suffix_and_bare_401_is_classified() {
        let server = TestServer::start(|request| match request.path.as_str() {
            "/api/v1/library" => TestResponse::json(
                401,
                r#"{"error_code":"UNAUTHORIZED","message":"invalid token"}"#,
            )
            .with_header("WWW-Authenticate", r#"Bearer realm="audiolib""#),
            _ => TestResponse::json(401, "Unauthorized"),
        });
        let client = RemoteClient::new(&server.url, "wrong").unwrap();
        let own = client.library().unwrap_err();
        assert!(matches!(own, RemoteError::Unauthorized(_)), "{own:?}");
        assert!(!own.localized(true).contains("fake-ip"));
        let bare = client.roots().unwrap_err();
        assert!(matches!(bare, RemoteError::Unauthorized(_)), "{bare:?}");
    }

    #[test]
    fn auth_rate_limit_and_host_errors_are_classified_without_retrying() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = {
            let hits = hits.clone();
            TestServer::start(move |request| {
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match request.path.as_str() {
                    "/api/v1/library" => TestResponse::json(
                        429,
                        r#"{"error_code":"AUTH_RATE_LIMITED","message":"Too many failed authentication attempts"}"#,
                    )
                    .with_header("Retry-After", "60"),
                    "/api/v1/roots" => TestResponse::json(
                        403,
                        r#"{"error_code":"HOST_FORBIDDEN","message":"Host not allowed"}"#,
                    ),
                    _ => TestResponse::json(
                        403,
                        r#"{"error_code":"ADMIN_LAN_ONLY","message":"LAN only"}"#,
                    ),
                }
            })
        };
        let client = RemoteClient::new(&server.url, "wrong-token").unwrap();
        let limited = client.library().unwrap_err();
        assert!(
            matches!(
                limited,
                RemoteError::RateLimited {
                    retry_after: Some(60),
                    ..
                }
            ),
            "{limited:?}"
        );
        assert_eq!(limited.retry_after(), Some(60));
        assert!(limited.is_unauthorized() && !limited.is_offline());
        assert!(limited.localized(true).contains("60 秒"));
        assert!(limited.localized(false).contains("429"));
        // One request, no automatic retry; the poller then waits well past Retry-After.
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        let check = crate::RevisionCheck::classify(None, Err(limited), true);
        assert!(matches!(check, crate::RevisionCheck::Unauthorized(_)));
        let delay = crate::PollSchedule::new(30).next_delay(&check).unwrap();
        assert!(delay >= std::time::Duration::from_secs(60), "{delay:?}");

        let host = client.roots().unwrap_err();
        assert!(matches!(host, RemoteError::HostForbidden(_)), "{host:?}");
        assert!(host.localized(true).contains("局域网 IP"));
        assert!(host.localized(false).contains("HOST_FORBIDDEN"));
        assert!(
            host.localized(true)
                .contains("请检查 token，或改用服务器局域网 IP / 已配置的主机名")
        );
        assert!(!host.localized(true).contains("填入令牌"));
        assert!(
            host.localized(false)
                .contains("Check the access token, or use the server's LAN IP")
        );
        let admin = client.health().unwrap_err();
        assert!(matches!(admin, RemoteError::AdminLanOnly(_)), "{admin:?}");
        assert!(admin.localized(true).contains("ADMIN_LAN_ONLY"));
    }

    #[test]
    fn empty_token_sends_no_authorization_and_401_asks_for_a_token() {
        let headers = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let open = {
            let headers = headers.clone();
            TestServer::start(move |request| {
                headers
                    .lock()
                    .unwrap()
                    .push(request.header("authorization").map(str::to_owned));
                match request.path.as_str() {
                    "/api/v1/health" => TestResponse::json(200, r#"{"status":"ok","version":"1"}"#),
                    "/api/v1/library" => TestResponse::json(
                        200,
                        r#"{"library_id":"open","schema_version":3,"revision":1}"#,
                    ),
                    _ => TestResponse::json(404, "{}"),
                }
            })
        };
        let client = RemoteClient::new(&open.url, "  ").unwrap();
        assert_eq!(client.library().unwrap().library_id, "open");
        assert!(headers.lock().unwrap().iter().all(Option::is_none));
        assert_eq!(
            client.stream_source(3).auth,
            crate::stream::SourceAuth::None
        );

        // A server that does require a token: a clear "token required" hint, not "rejected".
        let secured = api_server();
        let error = RemoteClient::new(&secured.url, "")
            .unwrap()
            .library()
            .unwrap_err();
        assert!(matches!(error, RemoteError::TokenRequired(_)), "{error:?}");
        assert!(error.is_unauthorized() && !error.is_offline());
        assert!(error.localized(true).contains("服务器要求访问令牌"));
        assert!(error.localized(false).contains("requires an access token"));
        let stream = crate::stream::probe_stream(
            &RemoteClient::new(&secured.url, "")
                .unwrap()
                .stream_source(1),
        );
        assert!(
            matches!(stream, Err(RemoteError::TokenRequired(_))),
            "{stream:?}"
        );
        // A wrong token stays "rejected".
        let wrong = RemoteClient::new(&secured.url, "wrong")
            .unwrap()
            .library()
            .unwrap_err();
        assert!(matches!(wrong, RemoteError::Unauthorized(_)));
    }

    #[test]
    fn snapshot_cancellation_interrupts_headers_and_stalled_body_and_preserves_target() {
        for stalled_body in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let target = directory.path().join("snapshot.db");
            std::fs::write(&target, b"existing snapshot").unwrap();
            let (requested, waiting) = std::sync::mpsc::sync_channel(1);
            let server = TestServer::start(move |request| {
                assert_eq!(request.path, "/api/v1/library/snapshot");
                let _ = requested.send(());
                let response = TestResponse::bytes(200, vec![7; 300_000]);
                if stalled_body {
                    response.slow(1024, Duration::from_secs(2))
                } else {
                    response.delayed(Duration::from_secs(2))
                }
            });
            let client = RemoteClient::new(&server.url, "").unwrap();
            let cancel = Arc::new(AtomicBool::new(false));
            let flag = cancel.clone();
            let cancelled = std::thread::spawn(move || {
                waiting.recv_timeout(Duration::from_secs(3)).unwrap();
                std::thread::sleep(Duration::from_millis(100));
                flag.store(true, Ordering::Release);
                std::time::Instant::now()
            });
            let mut observed = Vec::new();
            let result =
                client.download_snapshot(&target, &mut |progress| observed.push(progress), &cancel);
            let returned = std::time::Instant::now();
            let cancelled_at = cancelled.join().unwrap();
            assert!(matches!(result, Err(RemoteError::Cancelled)), "{result:?}");
            assert!(
                returned.saturating_duration_since(cancelled_at) < Duration::from_secs(1),
                "stalled_body={stalled_body}, cancellation elapsed={:?}",
                returned.saturating_duration_since(cancelled_at)
            );
            assert_eq!(std::fs::read(&target).unwrap(), b"existing snapshot");
            assert_eq!(
                std::fs::read_dir(directory.path()).unwrap().count(),
                1,
                "partial removed before return"
            );
            assert!(observed.iter().all(|progress| matches!(progress,
                        SnapshotProgress::Downloading { received, total }
                            if *received < total.unwrap_or(u64::MAX))));
            if !stalled_body {
                assert!(
                    observed.is_empty(),
                    "no byte progress before delayed headers"
                );
            }
        }
    }

    #[test]
    fn snapshot_download_is_streamed_atomically_and_cancellable() {
        let server = api_server();
        let client = RemoteClient::new(&server.url, "secret-token").unwrap();
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("snap.db");
        let mut calls = 0;
        let meta = client
            .download_snapshot(
                &target,
                &mut |progress| {
                    calls += 1;
                    assert!(matches!(
                        progress,
                        SnapshotProgress::Downloading {
                            total: Some(300_000),
                            ..
                        }
                    ));
                },
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(meta.bytes, 300_000);
        assert_eq!(meta.library_id.as_deref(), Some("lib-1"));
        assert_eq!((meta.revision, meta.schema_version), (Some(8), Some(3)));
        assert!(calls >= 2);
        assert_eq!(std::fs::metadata(&target).unwrap().len(), 300_000);
        let cancelled = directory.path().join("cancelled.db");
        let error = client
            .download_snapshot(&cancelled, &mut |_| {}, &AtomicBool::new(true))
            .unwrap_err();
        assert!(matches!(error, RemoteError::Cancelled));
        assert!(!cancelled.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    /// Barely compressible bytes, so a cut body is really cut.
    fn database() -> Vec<u8> {
        let mut state = 0x1234_5678_u32;
        (0..700_000)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect()
    }

    /// `/library/snapshot` honouring `Accept-Encoding: zstd`, with the advertised
    /// uncompressed length `claimed` (or the real one) and the body cut after `cut`.
    fn zstd_server(claimed: Option<u64>, cut: Option<usize>) -> TestServer {
        TestServer::start(move |request| {
            let plain = database();
            if request.header("accept-encoding") != Some("zstd") {
                return TestResponse::bytes(200, plain);
            }
            let packed = zstd::encode_all(plain.as_slice(), 3).unwrap();
            let response = TestResponse::bytes(200, packed)
                .with_header("Content-Encoding", "zstd")
                .with_header(
                    "X-Uncompressed-Length",
                    &claimed.unwrap_or(plain.len() as u64).to_string(),
                )
                .with_header("X-Library-Revision", "9");
            match cut {
                Some(bytes) => response.cut_after(bytes),
                None => response,
            }
        })
    }

    #[test]
    fn zstd_snapshots_are_decompressed_and_verified() {
        let directory = tempfile::tempdir().unwrap();
        let server = zstd_server(None, None);
        let client = RemoteClient::new(&server.url, "").unwrap();
        let target = directory.path().join("snap.db");
        let mut totals = Vec::new();
        let meta = client
            .download_snapshot(
                &target,
                &mut |progress| totals.push(progress),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(meta.bytes, 700_000);
        assert_eq!(meta.revision, Some(9));
        assert_eq!(std::fs::read(&target).unwrap(), database());
        let packed_bytes = zstd::encode_all(database().as_slice(), 3).unwrap().len() as u64;
        let downloaded: Vec<_> = totals
            .iter()
            .filter_map(|&progress| match progress {
                SnapshotProgress::Downloading { received, total } => Some((received, total)),
                _ => None,
            })
            .collect();
        let decompressed: Vec<_> = totals
            .iter()
            .filter_map(|&progress| match progress {
                SnapshotProgress::Decompressing { written, total } => Some((written, total)),
                _ => None,
            })
            .collect();
        assert!(
            downloaded
                .iter()
                .all(|&(_, total)| total == Some(packed_bytes))
        );
        assert_eq!(downloaded.first(), Some(&(0, Some(packed_bytes))));
        assert_eq!(downloaded.last(), Some(&(packed_bytes, Some(packed_bytes))));
        assert!(
            decompressed
                .iter()
                .all(|&(_, total)| total == Some(700_000))
        );
        assert_eq!(decompressed.first(), Some(&(0, Some(700_000))));
        assert_eq!(decompressed.last(), Some(&(700_000, Some(700_000))));

        // A wrong uncompressed length, or a cut body, fails without a target.
        for (claimed, cut, label) in [
            (Some(700_001), None, "size mismatch"),
            (None, Some(100_000), "truncated"),
        ] {
            let server = zstd_server(claimed, cut);
            let client = RemoteClient::new(&server.url, "").unwrap();
            let target = directory.path().join(format!("{label}.db"));
            let error = client
                .download_snapshot(&target, &mut |_| {}, &AtomicBool::new(false))
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    RemoteError::Protocol(_) | RemoteError::Unreachable(_)
                ),
                "{label}: {error:?}"
            );
            assert!(!target.exists(), "{label}");
        }
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn snapshot_transfer_totals_are_independent_and_unknown_lengths_stay_unknown() {
        // A high compression ratio makes it impossible to confuse network bytes
        // with the database byte count while still letting decompression stream.
        let plain = vec![42_u8; 2 * 1024 * 1024];
        let packed = zstd::encode_all(plain.as_slice(), 3).unwrap();
        assert!(packed.len() < plain.len() / 100);
        for (compressed, know_wire, know_output) in [
            (false, false, false),
            (true, false, true),
            (true, true, false),
            (true, false, false),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let target = directory.path().join("snapshot.db");
            let body = if compressed { &packed } else { &plain }.clone();
            let wire_bytes = body.len() as u64;
            let plain_bytes = plain.len() as u64;
            let server = TestServer::start(move |_| {
                let mut response = TestResponse::bytes(200, body.clone());
                if compressed {
                    response = response.with_header("Content-Encoding", "zstd");
                    if know_output {
                        response =
                            response.with_header("X-Uncompressed-Length", &plain_bytes.to_string());
                    }
                }
                if !know_wire {
                    response = response.without_content_length();
                }
                response
            });
            let client = RemoteClient::new(&server.url, "").unwrap();
            let mut downloaded = Vec::new();
            let mut decompressed = Vec::new();
            let meta = client
                .download_snapshot(
                    &target,
                    &mut |progress| match progress {
                        SnapshotProgress::Downloading { received, total } => {
                            assert_eq!(total, know_wire.then_some(wire_bytes));
                            downloaded.push(received);
                        }
                        SnapshotProgress::Decompressing { written, total } => {
                            assert!(compressed);
                            assert_eq!(total, know_output.then_some(plain_bytes));
                            decompressed.push(written);
                        }
                    },
                    &AtomicBool::new(false),
                )
                .unwrap();
            assert_eq!(meta.bytes, plain_bytes);
            assert_eq!(std::fs::read(target).unwrap(), plain);
            assert_eq!(downloaded.first(), Some(&0));
            assert_eq!(downloaded.last(), Some(&wire_bytes));
            assert!(downloaded.windows(2).all(|pair| pair[0] <= pair[1]));
            if compressed {
                assert_eq!(decompressed.first(), Some(&0));
                assert_eq!(decompressed.last(), Some(&plain_bytes));
                assert!(decompressed.windows(2).all(|pair| pair[0] <= pair[1]));
            } else {
                assert!(decompressed.is_empty());
            }
        }
    }

    #[test]
    fn cancellation_during_decompression_preserves_existing_snapshot_and_cleans_partial() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("snapshot.db");
        std::fs::write(&target, b"existing snapshot").unwrap();
        let plain = vec![42_u8; 2 * 1024 * 1024];
        let packed = zstd::encode_all(plain.as_slice(), 3).unwrap();
        let server = TestServer::start(move |_| {
            TestResponse::bytes(200, packed.clone())
                .with_header("Content-Encoding", "zstd")
                .with_header("X-Uncompressed-Length", &plain.len().to_string())
        });
        let client = RemoteClient::new(&server.url, "").unwrap();
        let cancel = AtomicBool::new(false);
        let mut written = 0;
        let result = client.download_snapshot(
            &target,
            &mut |progress| {
                if let SnapshotProgress::Decompressing { written: bytes, .. } = progress
                    && bytes > 0
                {
                    written = bytes;
                    cancel.store(true, Ordering::Release);
                }
            },
            &cancel,
        );
        assert!(matches!(result, Err(RemoteError::Cancelled)), "{result:?}");
        assert!(written > 0);
        assert_eq!(std::fs::read(target).unwrap(), b"existing snapshot");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn waveforms_use_zstd_and_if_none_match() {
        let json = br#"{"sample_rate":96000,"channels":2,"frames":100,"samples_per_pixel":2,"peaks":[[],[]],"start_frame":0,"end_frame":100,"total_frames":100}"#;
        let server = TestServer::start(move |request| {
            assert!(
                request
                    .path
                    .starts_with("/api/v1/audio/7/waveform?buckets=64&colors=1"),
                "{}",
                request.path
            );
            if request.header("if-none-match") == Some("W/\"wf4-1\"") {
                return TestResponse::bytes(304, Vec::new()).with_header("ETag", "W/\"wf4-1\"");
            }
            if request.path.contains("end_frame=100") {
                return TestResponse::bytes(200, json.to_vec()).with_header("ETag", "W/\"wf4-1\"");
            }
            assert_eq!(request.header("accept-encoding"), Some("zstd"));
            TestResponse::bytes(200, zstd::encode_all(&json[..], 3).unwrap())
                .with_header("Content-Encoding", "zstd")
                .with_header("ETag", "W/\"wf4-1\"")
        });
        let client = RemoteClient::new(&server.url, "").unwrap();
        let cancel = AtomicBool::new(false);
        let query = WaveformQuery {
            buckets: 10,
            colors: true,
            start_frame: None,
            end_frame: None,
        };
        let expected = WaveformReply::Data {
            json: json.to_vec(),
            etag: Some("W/\"wf4-1\"".into()),
        };
        assert_eq!(client.waveform(7, &query, None, &cancel).unwrap(), expected);
        assert_eq!(
            client
                .waveform(7, &query, Some("W/\"wf4-1\""), &cancel)
                .unwrap(),
            WaveformReply::NotModified
        );
        // Identity encoding (no Content-Encoding) is accepted as is.
        let plain = WaveformQuery {
            start_frame: Some(0),
            end_frame: Some(100),
            ..query
        };
        assert_eq!(client.waveform(7, &plain, None, &cancel).unwrap(), expected);
    }
}
