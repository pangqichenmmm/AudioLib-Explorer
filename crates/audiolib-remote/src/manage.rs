// crates/audiolib-remote/src/manage.rs
//! Server library management client (design `server-mgmt-gui.md` §6): capability probe,
//! server-side scans and database-only removals.
//!
//! Scans and history follow API §6.6/§6.7 (live): one scan at a time per library
//! (`409 BUSY`, also when the CLI or a local desktop scans), `503 ROOT_OFFLINE` when the
//! root is offline (`Retry-After: 5`), `409 SCAN_STATE` for a control the session's
//! state does not allow ([`RemoteError::ScanState`]). Poll [`RemoteClient::scan_status`]
//! about every 500 ms until [`ServerScan::active`] is false; on (re)connect call
//! [`RemoteClient::current_scan`] to attach to a running scan. The revision is frozen
//! during a scan and moves at most once at its end ([`ServerScan::revision`]); show scan
//! history from [`RemoteClient::scan_history`] rather than the snapshot.
//!
//! Removals (§6.8) only make the library forget records, never touch audio files: a
//! later scan re-adds files still on disk with **new ids** (annotations not restored).
//! Both are `409 BUSY` while a scan runs or is paused (even for unknown ids). A removal
//! that changed something bumps the revision by one: re-sync the snapshot and drop every
//! cached id (selection, cached playlists, waveform/preview keys); an unchanged revision
//! means nothing was removed.
//!
//! `manage_mode` (§6.9) gates scan control, database removals, directory browsing and
//! root management (Metadata edits and ordinary library reads are not gated);
//! refusals are `403 MANAGE_FORBIDDEN`
//! ([`RemoteError::Forbidden`], show the server's message). [`RemoteClient::capabilities`]
//! reports `can_manage` for this request's network path; a server without the endpoint
//! (`404`) offers no management ([`Capabilities::unknown`]). Server check order: 401 token
//! → 403 manage → 400 validation → 409 busy → 404 missing → 400 root protection. Servers
//! that predate an endpoint answer `404 NOT_FOUND "No such endpoint"` (or `405` where the
//! path collides with an existing route), reported as [`RemoteError::Unsupported`]. See
//! [`manage_error_text`] for the management wording.
use crate::client::RemoteClient;
use crate::error::{RemoteError, Result};
use audiolib_core::{ScanProgress, ScanSession};
use serde::{Deserialize, Serialize};
use std::time::Duration;

const MAX_JSON_BYTES: u64 = 16 * 1024 * 1024;
/// Most ids accepted by one `POST /audio/remove`.
pub const MAX_REMOVE_IDS: usize = 10_000;
/// Largest `workers` value accepted by `POST /scans`.
pub const MAX_SCAN_WORKERS: u32 = 16;
/// Message of the server's fallback route for unknown paths.
const NO_SUCH_ENDPOINT: &str = "No such endpoint";
/// Largest `limit` of `GET /history/*`.
pub const MAX_HISTORY_LIMIT: usize = 10_000;
/// `POST /scans` has no server time limit (it resolves the root first; a sleeping NAS
/// takes 20–30 s or more). Waiting longer than this gives up locally; the scan still
/// starts and [`RemoteClient::current_scan`] finds it.
const START_SCAN_TIMEOUT: Duration = Duration::from_secs(120);

/// Who may manage the library (scan control, roots and removals) through the API: the server's
/// `manage_mode` (§6.9).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManageMode {
    /// Any authenticated request (the server default).
    #[default]
    All,
    /// Only direct LAN requests.
    Lan,
    /// Nobody through the API.
    Off,
    /// Not reported (server without `/capabilities`) or a value this client does not know.
    #[serde(other)]
    Unknown,
}

/// `GET /capabilities` reply (§6.9; normal authentication).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Capabilities {
    /// Present on servers supporting mutual version and exact snapshot negotiation.
    #[serde(default)]
    pub compatibility: Option<audiolib_core::compatibility::CompatibilityManifest>,
    /// False when the server has no `/capabilities` endpoint (`404`).
    #[serde(skip, default = "known")]
    pub supported: bool,
    #[serde(default)]
    pub manage_mode: ManageMode,
    /// The server treats this request as external (§2.2), when reported.
    #[serde(default)]
    pub external: Option<bool>,
    /// Management endpoints would accept this request (same network path), when reported.
    #[serde(default)]
    pub can_manage: Option<bool>,
    /// This server supports authenticated directory browsing and root add/path/remove
    /// endpoints. Older servers omit it: keep these controls unavailable.
    #[serde(default)]
    pub root_management: bool,
    /// List thumbnail Waveforms (`POST /waveforms`, §9.1); `None` on older servers.
    #[serde(default)]
    pub list_waveforms: Option<crate::list_waveforms::ListWaveformsCapability>,
    /// The server's effective `pregenerate_list_waveforms` (read-only here, §9.2).
    #[serde(default)]
    pub pregenerate_list_waveforms: Option<bool>,
    /// List Waveform generation jobs and the rate setting (§9.5.7); `None` on older
    /// servers (only the §9.2 switch, status and pause/resume).
    #[serde(default)]
    pub list_waveform_jobs: Option<audiolib_core::waveform_cache::WaveformJobsCapability>,
    /// Library schema and served snapshot formats (search metadata v2); `None` on
    /// older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_schema: Option<audiolib_core::server_upgrade::LibrarySchemaInfo>,
    /// Schema migration state (search metadata v2); `None` on older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_migration: Option<audiolib_core::server_upgrade::SchemaMigrationInfo>,
    /// Vendor metadata import limits; `None` when the server does not offer it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_import: Option<audiolib_core::server_upgrade::VendorImportInfo>,
    /// Client upgrade advice (#214); absent means no prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_upgrade: Option<audiolib_core::server_upgrade::ClientUpgradeInfo>,
}

fn known() -> bool {
    true
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            compatibility: None,
            supported: true,
            manage_mode: ManageMode::All,
            external: None,
            can_manage: None,
            root_management: false,
            list_waveforms: None,
            pregenerate_list_waveforms: None,
            list_waveform_jobs: None,
            library_schema: None,
            schema_migration: None,
            vendor_import: None,
            client_upgrade: None,
        }
    }
}

impl Capabilities {
    /// An older server without `/capabilities`: read-only, no management offered.
    pub fn unknown() -> Self {
        Self {
            supported: false,
            manage_mode: ManageMode::Unknown,
            ..Self::default()
        }
    }

    /// Whether to enable scan and removal controls: the server's `can_manage` (the client
    /// never infers LAN/external itself). Only a hint: the settings can change at any
    /// time, so a `403 MANAGE_FORBIDDEN` is still handled. A reply without `can_manage`
    /// falls back to `manage_mode == all`; no endpoint means no management.
    pub fn may_manage(&self) -> bool {
        self.supported
            && self
                .can_manage
                .unwrap_or(self.manage_mode == ManageMode::All)
    }

    /// Whether to offer remote folder browsing and root management on this route.
    /// The server still checks its current policy for every request.
    pub fn may_manage_roots(&self) -> bool {
        self.root_management && self.may_manage()
    }

    /// Whether an optional marker feature may be used with this server: on marker
    /// servers both sides must declare it at the same revision; a missing or mismatched
    /// one only disables that function (Compatibility_Markers.md §4, §9.5). Servers
    /// without markers are refused before this matters; their legacy capability
    /// fields alone decide.
    pub fn feature_enabled(&self, name: &str) -> bool {
        match &self.compatibility {
            Some(server) if server.marker_revision.is_some() => {
                crate::compatibility::enabled_features(Some(server)).contains(name)
            }
            _ => true,
        }
    }

    /// Whether the list may show server thumbnail Waveforms (§9.1–§9.2): a known
    /// `list_waveforms` capability (version 1, 128 buckets, `i16le-minmax-base64`).
    pub fn list_waveforms_usable(&self) -> bool {
        self.supported
            && self.feature_enabled("list_waveforms")
            && self
                .list_waveforms
                .as_ref()
                .is_some_and(|capability| capability.usable())
    }
}

/// Latest scan session of a server root (`/roots` `last_scan`, API §6.4); `null` =
/// never scanned. Times are Unix seconds.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RootScan {
    /// Session id (usable with `GET /scans/{id}`).
    #[serde(alias = "session_id")]
    pub id: i64,
    /// Relative to the root; `""` = the whole root.
    pub subpath: String,
    /// `incremental` | `full` (kept as sent).
    #[serde(alias = "scan_type")]
    pub mode: String,
    /// `running` | `completed` | `cancelled` | `failed` as recorded in the library
    /// (`running` while this server is not scanning = interrupted or another process).
    #[serde(alias = "status")]
    pub state: String,
    pub started_at: Option<u64>,
    /// `None` while unfinished.
    pub finished_at: Option<u64>,
    pub scanned: u64,
    pub added: u64,
    pub updated: u64,
    pub removed: u64,
    pub unchanged: u64,
    pub errors: u64,
}

/// `mode` of `POST /scans`.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    /// Only new files and files whose size or modification time changed; never marks
    /// records missing.
    #[default]
    Incremental,
    /// Re-read every file; an error-free full scan marks unseen records missing.
    Full,
}

/// `POST /scans` body (API §6.6). `subpath` is relative to the root with `/`
/// separators; the client never sends server absolute paths.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct ScanRequest {
    pub root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
    pub mode: ScanMode,
    /// `1..=16` metadata threads; `None` or `0` = automatic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workers: Option<u32>,
}

impl ScanRequest {
    /// The request as sent: root trimmed, subpath without empty segments (the server
    /// ignores repeated and trailing `/`), `workers: 0` as automatic. Fails with the
    /// server's `400 VALIDATION` rules: a root name without separators; a relative
    /// subpath without `.`/`..` segments, backslashes or NUL and at most 4096 bytes;
    /// workers `0..=16`.
    pub fn normalized(&self) -> Result<Self> {
        let root = self.root.trim();
        if root.is_empty() || root.contains(['/', '\\', '\0']) || root.len() > 255 {
            return Err(RemoteError::Validation(format!(
                "invalid root name {:?}",
                self.root
            )));
        }
        let subpath = match self.subpath.as_deref().filter(|path| !path.is_empty()) {
            None => None,
            Some(path) => {
                let bad = path.starts_with('/')
                    || path.contains(['\\', '\0'])
                    || path.len() > 4096
                    || path
                        .split('/')
                        .any(|segment| segment == "." || segment == "..");
                if bad {
                    return Err(RemoteError::Validation(format!(
                        "invalid subpath {path:?}: use a relative path inside the root"
                    )));
                }
                let joined = path
                    .split('/')
                    .filter(|segment| !segment.is_empty())
                    .collect::<Vec<_>>()
                    .join("/");
                (!joined.is_empty()).then_some(joined)
            }
        };
        if self
            .workers
            .is_some_and(|workers| workers > MAX_SCAN_WORKERS)
        {
            return Err(RemoteError::Validation(format!(
                "workers must be 1..={MAX_SCAN_WORKERS} (0 = automatic)"
            )));
        }
        Ok(Self {
            root: root.to_owned(),
            subpath,
            mode: self.mode,
            workers: self.workers.filter(|workers| *workers > 0),
        })
    }
}

/// `ScanResponse` (API §6.6): the local [`ScanProgress`] fields plus the server's.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ServerScan {
    /// `session_id` (`0` while the server is still resolving the directory), `state`,
    /// `paused`, counters, `error_details`, `current_path` (server path), `message`.
    #[serde(flatten)]
    pub progress: ScanProgress,
    /// Root name; `None` for a history session outside every configured root.
    #[serde(default)]
    pub root: Option<String>,
    /// Normalised subpath (`""` = whole root).
    #[serde(default)]
    pub subpath: Option<String>,
    /// `incremental` | `full` (kept as sent).
    #[serde(default)]
    pub mode: Option<String>,
    /// This server process is running the session (running or paused): poll until
    /// false.
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub pause_requested: bool,
    #[serde(default)]
    pub cancel_requested: bool,
    /// Library revision after the scan ended (`None` while running and for history
    /// sessions). Differs from the cached revision → sync the snapshot.
    #[serde(default)]
    pub revision: Option<u64>,
}

impl ServerScan {
    /// Still running on the server (including paused): keep polling.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The scan finished and changed the library relative to `cached_revision`.
    pub fn needs_sync(&self, cached_revision: u64) -> bool {
        !self.active
            && self
                .revision
                .is_some_and(|revision| revision != cached_revision)
    }
}

/// `GET /scans/current` reply.
#[derive(Debug, Default, Deserialize)]
struct CurrentScan {
    #[serde(default)]
    scan: Option<ServerScan>,
}

/// `POST /scans/{id}/pause|resume|cancel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanControl {
    Pause,
    Resume,
    Cancel,
}

impl ScanControl {
    fn path(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Cancel => "cancel",
        }
    }
}

/// Reply to `POST /audio/remove` and `DELETE /folders/{id}` (§6.8): database records
/// removed on the server (files untouched) and the library revision before/after.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RemoveReport {
    /// Audio records removed.
    pub removed: u64,
    /// Requested ids that did not exist, in request order (`POST /audio/remove`).
    pub not_found: Vec<i64>,
    /// Folders removed including the folder itself (`DELETE /folders/{id}`).
    pub folders_removed: u64,
    pub revision_before: u64,
    pub revision: u64,
}

impl RemoveReport {
    /// The library changed (revision moved): re-sync and invalidate cached ids. A
    /// folder removal always changes it; an audio removal only if something existed.
    pub fn changed(&self) -> bool {
        self.revision != self.revision_before
    }
}

/// Compatibility catalog headers already sent on other peer reads (`GET /library`).
struct PeerCatalogHeaders {
    catalog: String,
    accepted: String,
    schema_max: String,
    search_storage_max: String,
    tokenization_max: String,
}

fn peer_catalog_headers() -> Result<PeerCatalogHeaders> {
    let manifest = audiolib_core::compatibility::compatibility_manifest();
    let catalog = serde_json::to_string(&manifest).map_err(|_| {
        RemoteError::Protocol("Cannot serialize client compatibility catalog".into())
    })?;
    Ok(PeerCatalogHeaders {
        accepted: manifest.snapshot_read_formats.join(", "),
        schema_max: audiolib_core::compatibility::SNAPSHOT_SCHEMA_MAX.to_string(),
        search_storage_max: audiolib_core::compatibility::SEARCH_STORAGE_MAX.to_string(),
        tokenization_max: audiolib_core::compatibility::TOKENIZATION_MAX.to_string(),
        catalog,
    })
}

/// A missing endpoint on an older server becomes [`RemoteError::Unsupported`].
fn endpoint_missing(error: RemoteError, context: &str) -> RemoteError {
    match error {
        RemoteError::NotFound(message) if message.contains(NO_SUCH_ENDPOINT) => {
            RemoteError::Unsupported(format!("{context}: the server does not offer this yet"))
        }
        RemoteError::Server { status: 405, .. } => {
            RemoteError::Unsupported(format!("{context}: the server does not offer this yet"))
        }
        other => other,
    }
}

impl RemoteClient {
    /// One management request; `None` for an empty (`204`) reply. Writes are never
    /// retried here.
    pub(crate) fn manage_call(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Option<String>> {
        self.manage_call_with(None, method, path, body)
    }

    fn manage_call_with(
        &self,
        agent: Option<&crate::abort::Timeouts>,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<Option<String>> {
        let url = format!("{}{path}", self.api);
        let context = format!("{method} {path}");
        let authorization = (!self.token.is_empty()).then(|| format!("Bearer {}", self.token));
        let library_id = (method != "GET")
            .then(|| self.write_library_id())
            .transpose()?;
        let compatibility = (method != "GET")
            .then(crate::compatibility::client_header)
            .transpose()?;
        let result = match method {
            // Reads may be resent once on a stale keep-alive connection; writes never are.
            // `GET /capabilities` is the exception: an unmarked peer is served the
            // schema-8 compatibility snapshot and a client-upgrade hint.
            "GET" => {
                let peer_catalog = (path == "/capabilities")
                    .then(peer_catalog_headers)
                    .transpose()?;
                crate::abort::call_fresh(|| {
                    let mut request = self.agent.get(&url);
                    if let Some(peer) = &peer_catalog {
                        request = request
                            .header(
                                audiolib_core::compatibility::CLIENT_COMPATIBILITY_HEADER,
                                &peer.catalog,
                            )
                            .header(
                                audiolib_core::compatibility::ACCEPT_FORMATS_HEADER,
                                &peer.accepted,
                            )
                            .header(
                                audiolib_core::compatibility::SNAPSHOT_SCHEMA_MAX_HEADER,
                                &peer.schema_max,
                            )
                            .header(
                                audiolib_core::compatibility::SEARCH_STORAGE_MAX_HEADER,
                                &peer.search_storage_max,
                            )
                            .header(
                                audiolib_core::compatibility::TOKENIZATION_MAX_HEADER,
                                &peer.tokenization_max,
                            );
                    }
                    match &authorization {
                        Some(value) => request.header("Authorization", value),
                        None => request,
                    }
                })
            }
            "DELETE" => {
                let mut request = agent
                    .unwrap_or(&self.writes)
                    .delete(&url)
                    .header(
                        crate::client::LIBRARY_ID_HEADER,
                        library_id.as_deref().unwrap_or_default(),
                    )
                    .header(
                        audiolib_core::compatibility::CLIENT_COMPATIBILITY_HEADER,
                        compatibility.as_deref().unwrap_or_default(),
                    );
                if let Some(value) = &authorization {
                    request = request.header("Authorization", value);
                }
                request.call()
            }
            _ => {
                let agent = agent.unwrap_or(&self.writes);
                let mut request = match method {
                    "PATCH" => agent.patch(&url),
                    _ => agent.post(&url),
                }
                .header(
                    crate::client::LIBRARY_ID_HEADER,
                    library_id.as_deref().unwrap_or_default(),
                )
                .header(
                    audiolib_core::compatibility::CLIENT_COMPATIBILITY_HEADER,
                    compatibility.as_deref().unwrap_or_default(),
                );
                if let Some(value) = &authorization {
                    request = request.header("Authorization", value);
                }
                match body {
                    Some(body) => {
                        let bytes = serde_json::to_vec(body).map_err(|error| {
                            RemoteError::Validation(format!("{context}: {error}"))
                        })?;
                        request
                            .header("Content-Type", "application/json")
                            .send(bytes.as_slice())
                    }
                    None => request.send_empty(),
                }
            }
        };
        let response = result.map_err(|error| RemoteError::from_transport(error, &url))?;
        let status = response.status().as_u16();
        let mut response = self
            .checked(response, &context)
            .map_err(|error| endpoint_missing(error, &context))?;
        let text = response
            .body_mut()
            .with_config()
            .limit(MAX_JSON_BYTES)
            .read_to_string()
            .map_err(|error| match RemoteError::from_transport(error, &url) {
                RemoteError::Timeout(detail) => RemoteError::Timeout(detail),
                other => RemoteError::Protocol(format!("{context}: {other}")),
            })?;
        let empty = text.trim().is_empty() || text.trim() == "null";
        Ok((status != 204 && !empty).then_some(text))
    }

    pub(crate) fn manage_json<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T> {
        self.manage_json_with(None, method, path, body)
    }

    /// A management JSON call with a specialized response/body budget. Mutations
    /// retain identity binding and are sent once, regardless of the budget.
    pub(crate) fn manage_json_with<T: serde::de::DeserializeOwned>(
        &self,
        agent: Option<&crate::abort::Timeouts>,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T> {
        let context = format!("{method} {path}");
        let text = self
            .manage_call_with(agent, method, path, body)?
            .ok_or_else(|| RemoteError::Protocol(format!("{context}: empty reply")))?;
        serde_json::from_str(&text)
            .map_err(|error| RemoteError::Protocol(format!("{context}: {error}")))
    }

    /// `GET /capabilities` (§6.9). A server without the endpoint (`404`) yields
    /// [`Capabilities::unknown`] (no management).
    pub fn capabilities(&self) -> Result<Capabilities> {
        match self.manage_json::<Capabilities>("GET", "/capabilities", None) {
            Ok(capabilities) => Ok(capabilities),
            Err(RemoteError::NotFound(_) | RemoteError::Unsupported(_)) => {
                Ok(Capabilities::unknown())
            }
            Err(error) => Err(error),
        }
    }

    /// `POST /scans`: start a scan of a root (or a subpath of it) on the server; returns
    /// once the session exists (`session_id`). `409 BUSY` while any scan runs (this server,
    /// CLI or a local desktop), `503 ROOT_OFFLINE`, `404` for an unknown root or missing
    /// subpath, `400` for an invalid subpath (also one resolving outside the root). A
    /// timeout or dropped connection leaves the outcome open: check
    /// [`RemoteClient::current_scan`].
    pub fn start_scan(&self, request: &ScanRequest) -> Result<ServerScan> {
        let request = request.normalized()?;
        let body = serde_json::to_value(&request)
            .map_err(|error| RemoteError::Validation(error.to_string()))?;
        let agent = crate::client::agent(START_SCAN_TIMEOUT, Duration::from_secs(30));
        let text = self
            .manage_call_with(Some(&agent), "POST", "/scans", Some(&body))?
            .ok_or_else(|| RemoteError::Protocol("POST /scans: empty reply".into()))?;
        serde_json::from_str(&text)
            .map_err(|error| RemoteError::Protocol(format!("POST /scans: {error}")))
    }

    /// `GET /scans/current`: the scan this server is running (or has paused), `None`
    /// when idle (`{"scan": null}`).
    pub fn current_scan(&self) -> Result<Option<ServerScan>> {
        let Some(text) = self.manage_call("GET", "/scans/current", None)? else {
            return Ok(None);
        };
        let current: CurrentScan = serde_json::from_str(&text)
            .map_err(|error| RemoteError::Protocol(format!("GET /scans/current: {error}")))?;
        Ok(current.scan)
    }

    /// `GET /scans/{id}`: progress, or the final state of a finished (or history) scan.
    pub fn scan_status(&self, session_id: i64) -> Result<ServerScan> {
        if session_id <= 0 {
            return Err(RemoteError::Validation(format!(
                "invalid scan id {session_id}"
            )));
        }
        self.manage_json("GET", &format!("/scans/{session_id}"), None)
    }

    /// `POST /scans/{id}/pause|resume|cancel` (idempotent). A pause is confirmed later
    /// (`paused` becomes true at a safe boundary). A state that does not allow the
    /// control (finished, cancelling, finalizing, not running here) is `409 SCAN_STATE`.
    pub fn control_scan(&self, session_id: i64, control: ScanControl) -> Result<ServerScan> {
        if session_id <= 0 {
            return Err(RemoteError::Validation(format!(
                "invalid scan id {session_id}"
            )));
        }
        self.manage_json(
            "POST",
            &format!("/scans/{session_id}/{}", control.path()),
            None,
        )
    }

    fn history<T: serde::de::DeserializeOwned>(
        &self,
        kind: &str,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<T>> {
        let path = format!(
            "/history/{kind}?limit={}&offset={offset}",
            limit.clamp(1, MAX_HISTORY_LIMIT)
        );
        let Some(text) = self.manage_call("GET", &path, None)? else {
            return Ok(Vec::new());
        };
        serde_json::from_str(&text)
            .map_err(|error| RemoteError::Protocol(format!("GET {path}: {error}")))
    }

    /// `GET /history/scans?limit=&offset=`: newest first; times are UTC text. Authoritative
    /// over the snapshot's copy (scans without changes do not move the revision).
    pub fn scan_history(&self, limit: usize, offset: usize) -> Result<Vec<ScanSession>> {
        self.history("scans", limit, offset)
    }

    /// `POST /audio/remove {ids}` (§6.8): remove audio records (with their annotations,
    /// playlist entries and play history) from the server database. Never deletes files.
    /// `1..=10000` positive ids, de-duplicated before sending; unknown ids come back in
    /// `not_found`. `409 BUSY` while a scan runs or is paused.
    pub fn remove_audio(&self, ids: &[i64]) -> Result<RemoveReport> {
        let mut seen = std::collections::HashSet::with_capacity(ids.len());
        let mut unique: Vec<i64> = Vec::with_capacity(ids.len());
        for &id in ids {
            if id <= 0 {
                return Err(RemoteError::Validation(format!("invalid audio id {id}")));
            }
            if seen.insert(id) {
                unique.push(id);
            }
        }
        if unique.is_empty() || unique.len() > MAX_REMOVE_IDS {
            return Err(RemoteError::Validation(format!(
                "remove 1..={MAX_REMOVE_IDS} records at a time"
            )));
        }
        self.manage_json(
            "POST",
            "/audio/remove",
            Some(&serde_json::json!({ "ids": unique })),
        )
    }

    /// `DELETE /folders/{id}` (§6.8): remove a folder's whole subtree (audio records and
    /// folder records) from the server database. Never deletes directories or files.
    /// `404` for an unknown folder; a root's top-level folder, or one containing a root,
    /// is refused (`400`); `409 BUSY` while a scan runs or is paused.
    pub fn remove_folder(&self, folder_id: i64) -> Result<RemoveReport> {
        if folder_id <= 0 {
            return Err(RemoteError::Validation(format!(
                "invalid folder id {folder_id}"
            )));
        }
        self.manage_json("DELETE", &format!("/folders/{folder_id}"), None)
    }
}

/// Text for a failed management request, in the desktop language: the agreed codes get
/// management wording (`403` reason, one scan at a time, root offline, older server);
/// anything else uses [`RemoteError::localized`].
pub fn manage_error_text(error: &RemoteError, chinese: bool) -> String {
    match (error, chinese) {
        (RemoteError::Forbidden(message), true) => format!(
            "服务器不允许从这里执行此管理操作（403）：{}。（{message}）",
            crate::error::forbidden_reason(message, true)
        ),
        (RemoteError::Forbidden(message), false) => format!(
            "The server does not allow this management operation from here (403): {}. ({message})",
            crate::error::forbidden_reason(message, false)
        ),
        (RemoteError::Busy { message, .. }, true) => format!(
            "服务器正忙（409）：有扫描正在运行或已暂停，或正在维护。同一时间只能运行一个扫描，扫描期间也不能从库中移除记录。请稍后重试，或查看当前扫描进度。（{message}）"
        ),
        (RemoteError::Busy { message, .. }, false) => format!(
            "The server is busy (409): a scan is running or paused, or maintenance is running. Only one scan runs at a time, and records cannot be removed during a scan. Retry later or follow the current scan. ({message})"
        ),
        (RemoteError::ScanState(message), true) => format!(
            "扫描当前的状态不允许此操作（409）：扫描可能已结束、正在取消或已进入最终对账。（{message}）"
        ),
        (RemoteError::ScanState(message), false) => format!(
            "The scan's current state does not allow this (409): it may have finished, be cancelling or be finalizing. ({message})"
        ),
        (RemoteError::RootOffline { message, .. }, true) => format!(
            "音效库根目录离线（503），操作未执行：请确认服务器上的磁盘或 NAS 共享已挂载，然后点“重查”。（{message}）"
        ),
        (RemoteError::RootOffline { message, .. }, false) => format!(
            "The library root is offline (503); nothing was done. Check that the disk or NAS share is mounted on the server, then re-check the root. ({message})"
        ),
        (RemoteError::Unsupported(message), true) => {
            format!("服务器版本不支持此操作，请更新服务器。（{message}）")
        }
        (RemoteError::Unsupported(message), false) => format!(
            "The server version does not support this operation; update the server. ({message})"
        ),
        (other, chinese) => other.localized(chinese),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_server::{TestResponse, TestServer};
    use std::sync::{Arc, Mutex};

    /// The §6.6 example reply, plus a field a later server might add.
    const STARTED: &str = r#"{"session_id":17,"state":"running","paused":false,"finalizing":false,"discovered":0,"processed":0,"added":0,"updated":0,"unchanged":0,"removed":0,"errors":0,"error_details":[],"errors_truncated":0,"current_path":"/srv/audiolib/library/ExampleLibrary/SFX/Foley/Doors","message":"","root":"SFX","subpath":"Foley/Doors","mode":"incremental","active":true,"pause_requested":false,"cancel_requested":false,"revision":null,"future":{"x":1}}"#;
    const FINISHED: &str = r#"{"session_id":17,"state":"completed","paused":false,"finalizing":false,"discovered":12,"processed":12,"added":1,"updated":0,"unchanged":11,"removed":0,"errors":0,"error_details":[],"errors_truncated":0,"current_path":"","message":"","root":"SFX","subpath":"Foley/Doors","mode":"incremental","active":false,"pause_requested":false,"cancel_requested":false,"revision":42}"#;

    #[allow(dead_code)]
    struct Seen {
        method: String,
        path: String,
        body: String,
        authorization: Option<String>,
    }

    fn server(
        reply: impl Fn(&str, &str) -> TestResponse + Send + Sync + 'static,
    ) -> (TestServer, Arc<Mutex<Vec<Seen>>>) {
        server_with_catalog(false, reply)
    }

    fn server_with_catalog(
        probe_catalog: bool,
        reply: impl Fn(&str, &str) -> TestResponse + Send + Sync + 'static,
    ) -> (TestServer, Arc<Mutex<Vec<Seen>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let server = TestServer::start(move |request| {
            if (request.path != "/api/v1/capabilities" || !probe_catalog)
                && let Some(response) = crate::test_server::marker_handshake(&request.path)
            {
                return response;
            }
            if request.method == "GET" && request.path == "/api/v1/library" {
                return TestResponse::json(
                    200,
                    r#"{"library_id":"lib-manage","identity_bound_writes":true,"schema_version":5,"revision":1}"#,
                );
            }
            if request.method != "GET" && !request.path.starts_with("/api/v1/roots/") {
                assert_eq!(request.header(crate::LIBRARY_ID_HEADER), Some("lib-manage"));
            }
            log.lock().unwrap().push(Seen {
                method: request.method.clone(),
                path: request.path.clone(),
                body: String::from_utf8_lossy(&request.body).into_owned(),
                authorization: request.header("authorization").map(str::to_owned),
            });
            reply(&request.method, &request.path)
        });
        (server, seen)
    }

    fn error(status: u16, code: &str, message: &str) -> TestResponse {
        TestResponse::json(
            status,
            &format!(r#"{{"error_code":"{code}","message":"{message}"}}"#),
        )
    }

    #[test]
    fn capabilities_drive_management_and_404_means_read_only() {
        let (current, seen) = server_with_catalog(true, |_, path| match path {
            "/api/v1/capabilities" => TestResponse::json(
                200,
                r#"{"manage_mode":"lan","external":true,"can_manage":false,"new":1}"#,
            ),
            _ => error(404, "NOT_FOUND", "No such endpoint"),
        });
        let client = RemoteClient::new(&current.url, "tok").unwrap();
        let caps = client.capabilities().unwrap();
        assert!(caps.supported);
        assert_eq!(
            (caps.manage_mode, caps.external, caps.can_manage),
            (ManageMode::Lan, Some(true), Some(false))
        );
        assert!(!caps.may_manage());
        assert_eq!(
            seen.lock().unwrap()[0].authorization.as_deref(),
            Some("Bearer tok")
        );
        // `can_manage` decides, whatever the mode says.
        let lan: Capabilities =
            serde_json::from_str(r#"{"manage_mode":"lan","external":false,"can_manage":true}"#)
                .unwrap();
        assert!(lan.may_manage());
        let off: Capabilities =
            serde_json::from_str(r#"{"manage_mode":"off","external":false,"can_manage":false}"#)
                .unwrap();
        assert!(!off.may_manage());
        // Without `can_manage`, only `all` (the default) allows management.
        let bare: Capabilities = serde_json::from_str("{}").unwrap();
        assert!(bare.may_manage() && bare.manage_mode == ManageMode::All);
        let odd: Capabilities = serde_json::from_str(r#"{"manage_mode":"weekends"}"#).unwrap();
        assert!(!odd.may_manage());
        // A server without /capabilities: read-only, no management offered.
        let (old, _) =
            server_with_catalog(true, |_, _| error(404, "NOT_FOUND", "No such endpoint"));
        let caps = RemoteClient::new(&old.url, "")
            .unwrap()
            .capabilities()
            .unwrap();
        assert!(!caps.supported && !caps.may_manage());
        assert_eq!(caps.manage_mode, ManageMode::Unknown);
        // A token problem is still an error, not "unknown".
        let (secured, _) = server_with_catalog(true, |_, _| {
            error(401, "UNAUTHORIZED", "Missing or invalid token")
        });
        assert!(matches!(
            RemoteClient::new(&secured.url, "").unwrap().capabilities(),
            Err(RemoteError::TokenRequired(_))
        ));
    }

    #[test]
    fn capabilities_get_sends_the_client_compatibility_catalog() {
        use audiolib_core::compatibility::{
            ACCEPT_FORMATS_HEADER, CLIENT_COMPATIBILITY_HEADER, PLAN_B_V9_FORMAT,
            SEARCH_STORAGE_MAX, SEARCH_STORAGE_MAX_HEADER, SNAPSHOT_SCHEMA_MAX,
            SNAPSHOT_SCHEMA_MAX_HEADER, TOKENIZATION_MAX, TOKENIZATION_MAX_HEADER,
            compatibility_manifest,
        };
        let seen = Arc::new(Mutex::new(None));
        let log = seen.clone();
        let server = TestServer::start(move |request| {
            if request.method == "GET" && request.path == "/api/v1/capabilities" {
                *log.lock().unwrap() = Some(request.headers.clone());
                return TestResponse::json(200, r#"{"manage_mode":"all","can_manage":true}"#);
            }
            TestResponse::json(
                404,
                r#"{"error_code":"NOT_FOUND","message":"No such endpoint"}"#,
            )
        });
        let caps = RemoteClient::new(&server.url, "tok")
            .unwrap()
            .capabilities()
            .unwrap();
        assert!(caps.supported);
        let headers = seen.lock().unwrap().clone().expect("capabilities request");
        let header = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        let catalog: audiolib_core::compatibility::CompatibilityManifest =
            serde_json::from_str(header(CLIENT_COMPATIBILITY_HEADER).expect("catalog")).unwrap();
        assert_eq!(catalog, compatibility_manifest());
        assert!(
            header(ACCEPT_FORMATS_HEADER)
                .unwrap()
                .split(',')
                .any(|id| id.trim() == PLAN_B_V9_FORMAT)
        );
        assert_eq!(
            header(SNAPSHOT_SCHEMA_MAX_HEADER).unwrap(),
            SNAPSHOT_SCHEMA_MAX.to_string()
        );
        assert_eq!(
            header(SEARCH_STORAGE_MAX_HEADER).unwrap(),
            SEARCH_STORAGE_MAX.to_string()
        );
        assert_eq!(
            header(TOKENIZATION_MAX_HEADER).unwrap(),
            TOKENIZATION_MAX.to_string()
        );
    }

    #[test]
    fn roots_carry_missing_count_last_scan_and_scanning() {
        // Synthetic §6.4 entries: a completed scan, an active scan and an older server.
        let (server, _) = server(|method, path| match (method, path) {
            ("GET", "/api/v1/roots") => TestResponse::json(
                200,
                r#"[{"name":"SFX","server_path":"/srv/audiolib/library/ExampleLibrary/SFX","webdav_url":null,"file_count":22,"missing_count":0,"online":true,"status":"online","status_message":null,"checked_at":1704067200,"scanning":false,"last_scan":{"id":2,"subpath":"","mode":"incremental","state":"completed","started_at":1704067260,"finished_at":1704067380,"scanned":25,"added":20,"updated":0,"removed":0,"unchanged":2,"errors":3}},{"name":"Foley","server_path":"/x/Foley","file_count":null,"missing_count":0,"online":true,"status":"waking","scanning":true,"last_scan":{"id":5,"subpath":"Doors","mode":"full","state":"running","started_at":1704067800,"finished_at":null,"scanned":10}},{"name":"Ambience","path":"/x/Ambience","available":true}]"#,
            ),
            ("POST", "/api/v1/roots/SFX/check") => TestResponse::json(
                200,
                r#"{"name":"SFX","server_path":"/x/SFX","online":false,"status":"offline","status_message":"Root directory not found","missing_count":3,"scanning":false,"last_scan":null}"#,
            ),
            _ => error(404, "NOT_FOUND", "No such endpoint"),
        });
        let client = RemoteClient::new(&server.url, "").unwrap();
        let roots = client.roots().unwrap();
        assert_eq!(roots.len(), 3);
        let sfx = &roots[0];
        assert_eq!((sfx.missing_count, sfx.scanning), (Some(0), Some(false)));
        let scan = sfx.last_scan.as_ref().unwrap();
        assert_eq!(
            (
                scan.id,
                scan.subpath.as_str(),
                scan.mode.as_str(),
                scan.state.as_str()
            ),
            (2, "", "incremental", "completed")
        );
        assert_eq!(
            (scan.started_at, scan.finished_at),
            (Some(1704067260), Some(1704067380))
        );
        assert_eq!(
            (scan.scanned, scan.added, scan.unchanged, scan.errors),
            (25, 20, 2, 3)
        );
        let foley = &roots[1];
        assert_eq!(foley.scanning, Some(true));
        let running = foley.last_scan.as_ref().unwrap();
        assert_eq!(
            (running.state.as_str(), running.finished_at),
            ("running", None)
        );
        assert_eq!(
            (running.updated, running.errors),
            (0, 0),
            "missing counts default"
        );
        let older = &roots[2];
        assert!(older.last_scan.is_none() && older.missing_count.is_none());
        assert!(older.scanning.is_none());
        // The existing re-check endpoint carries them too.
        let checked = client.check_root("SFX").unwrap();
        assert_eq!(
            (checked.missing_count, checked.scanning),
            (Some(3), Some(false))
        );
        assert_eq!(checked.state(), crate::RootState::Offline);
        // Cached roots of earlier versions (no new fields) still load and re-serialise
        // without them.
        let old: crate::RemoteRoot =
            serde_json::from_str(r#"{"name":"A","server_path":"/a"}"#).unwrap();
        let json = serde_json::to_string(&old).unwrap();
        assert!(!json.contains("last_scan") && !json.contains("missing_count"));
    }

    #[test]
    fn scans_start_poll_control_and_attach() {
        let polls = Arc::new(Mutex::new(0));
        let counter = polls.clone();
        let (server, seen) = server(move |method, path| match (method, path) {
            ("POST", "/api/v1/scans") => TestResponse::json(200, STARTED),
            ("GET", "/api/v1/scans/current") => {
                TestResponse::json(200, &format!(r#"{{"scan":{STARTED}}}"#))
            }
            ("GET", "/api/v1/scans/17") => {
                let mut polls = counter.lock().unwrap();
                *polls += 1;
                TestResponse::json(200, if *polls < 3 { STARTED } else { FINISHED })
            }
            ("POST", "/api/v1/scans/17/pause") => TestResponse::json(
                200,
                &STARTED.replace(r#""pause_requested":false"#, r#""pause_requested":true"#),
            ),
            ("POST", "/api/v1/scans/17/cancel") => {
                error(409, "SCAN_STATE", "Scan session 17 has already finished")
            }
            _ => error(404, "NOT_FOUND", "Unknown scan session"),
        });
        let client = RemoteClient::new(&server.url, "").unwrap();
        let started = client
            .start_scan(&ScanRequest {
                root: " SFX ".into(),
                subpath: Some("Foley//Doors/".into()),
                mode: ScanMode::Incremental,
                workers: Some(0),
            })
            .unwrap();
        assert_eq!(started.progress.session_id, 17);
        assert!(started.is_active() && !started.pause_requested);
        assert_eq!(started.root.as_deref(), Some("SFX"));
        assert_eq!(started.subpath.as_deref(), Some("Foley/Doors"));
        assert_eq!(started.mode.as_deref(), Some("incremental"));
        assert_eq!(started.revision, None);
        {
            let seen = seen.lock().unwrap();
            let body: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
            assert_eq!(
                body,
                serde_json::json!({"root":"SFX","subpath":"Foley/Doors","mode":"incremental"})
            );
            assert!(seen[0].authorization.is_none(), "LAN without token");
        }
        // Attach on (re)connect, then poll until inactive.
        let attached = client.current_scan().unwrap().unwrap();
        assert_eq!(attached.progress.session_id, 17);
        let paused = client.control_scan(17, ScanControl::Pause).unwrap();
        assert!(paused.pause_requested && !paused.progress.paused);
        let finished = loop {
            let scan = client.scan_status(17).unwrap();
            if !scan.is_active() {
                break scan;
            }
        };
        assert_eq!(*polls.lock().unwrap(), 3);
        assert_eq!(finished.progress.state, audiolib_core::ScanState::Completed);
        assert_eq!((finished.progress.added, finished.revision), (1, Some(42)));
        assert!(finished.needs_sync(41) && !finished.needs_sync(42));
        assert!(!started.needs_sync(1), "running scans never ask for a sync");
        // A control the state does not allow: its own error, still a "busy"-like 409.
        let state = client.control_scan(17, ScanControl::Cancel).unwrap_err();
        assert!(matches!(state, RemoteError::ScanState(_)), "{state:?}");
        assert!(manage_error_text(&state, true).contains("扫描当前的状态不允许"));
        assert!(matches!(
            audiolib_core::Error::from(state),
            audiolib_core::Error::Busy(_)
        ));
        assert!(matches!(
            client.scan_status(99),
            Err(RemoteError::NotFound(_))
        ));
        // Rejected locally: absolute or escaping subpaths, bad roots, too many workers.
        for request in [
            ScanRequest {
                root: "SFX".into(),
                subpath: Some("/Volumes/x".into()),
                ..ScanRequest::default()
            },
            ScanRequest {
                root: "SFX".into(),
                subpath: Some("a/../b".into()),
                ..ScanRequest::default()
            },
            ScanRequest {
                root: "SFX".into(),
                subpath: Some("a/./b".into()),
                ..ScanRequest::default()
            },
            ScanRequest {
                root: "SFX".into(),
                subpath: Some("a\\b".into()),
                ..ScanRequest::default()
            },
            ScanRequest {
                root: "a/b".into(),
                ..ScanRequest::default()
            },
            ScanRequest {
                root: "SFX".into(),
                workers: Some(17),
                ..ScanRequest::default()
            },
        ] {
            assert!(matches!(
                client.start_scan(&request),
                Err(RemoteError::Validation(_))
            ));
        }
        assert!(client.scan_status(0).is_err());
        let whole = ScanRequest {
            root: "SFX".into(),
            subpath: Some("/".repeat(0)),
            mode: ScanMode::Full,
            workers: Some(4),
        }
        .normalized()
        .unwrap();
        assert_eq!(
            serde_json::to_value(&whole).unwrap(),
            serde_json::json!({"root":"SFX","mode":"full","workers":4})
        );
    }

    #[test]
    fn idle_server_reports_no_current_scan() {
        let (idle, _) = server(|_, path| match path {
            "/api/v1/scans/current" => TestResponse::json(200, r#"{"scan":null}"#),
            _ => error(404, "NOT_FOUND", "No such endpoint"),
        });
        let client = RemoteClient::new(&idle.url, "").unwrap();
        assert!(client.current_scan().unwrap().is_none());
        // An older server without the endpoint says so instead of "idle".
        let (old, _) = server(|_, _| error(404, "NOT_FOUND", "No such endpoint"));
        assert!(matches!(
            RemoteClient::new(&old.url, "").unwrap().current_scan(),
            Err(RemoteError::Unsupported(_))
        ));
    }

    #[test]
    fn agreed_error_codes_are_typed_with_chinese_messages() {
        let (current, _) = server(|method, path| match (method, path) {
            ("POST", "/api/v1/scans") => error(409, "BUSY", "Scan session 17 is running"),
            ("GET", "/api/v1/scans/9") => {
                error(503, "ROOT_OFFLINE", "Library root \\\"Foley\\\" is offline")
            }
            ("GET", "/api/v1/scans/10") => error(503, "UNAVAILABLE", "The server is shutting down"),
            ("POST", "/api/v1/audio/remove") => error(
                403,
                "MANAGE_FORBIDDEN",
                "Library management (scans, removing records or folders) is only allowed from the LAN on this server (manage_mode = lan)",
            ),
            ("DELETE", "/api/v1/folders/4") => error(
                403,
                "MANAGE_FORBIDDEN",
                "Library management (scans, removing records or folders) is disabled on this server (manage_mode = off)",
            ),
            _ => error(404, "NOT_FOUND", "No such endpoint"),
        });
        let client = RemoteClient::new(&current.url, "tok").unwrap();
        let busy = client
            .start_scan(&ScanRequest {
                root: "SFX".into(),
                ..ScanRequest::default()
            })
            .unwrap_err();
        assert!(matches!(busy, RemoteError::Busy { .. }), "{busy:?}");
        assert!(manage_error_text(&busy, true).contains("只能运行一个扫描"));
        let offline = client.scan_status(9).unwrap_err();
        assert!(
            matches!(offline, RemoteError::RootOffline { .. }),
            "{offline:?}"
        );
        assert!(manage_error_text(&offline, true).contains("根目录离线"));
        let stopping = client.scan_status(10).unwrap_err();
        assert!(
            matches!(stopping, RemoteError::Starting { .. }),
            "{stopping:?}"
        );
        let forbidden = client.remove_audio(&[3]).unwrap_err();
        assert!(
            matches!(forbidden, RemoteError::Forbidden(_)),
            "{forbidden:?}"
        );
        assert!(!forbidden.is_unauthorized());
        let text = manage_error_text(&forbidden, true);
        assert!(
            text.contains("403") && text.contains("仅限局域网"),
            "{text}"
        );
        assert!(manage_error_text(&forbidden, false).contains("local network"));
        assert!(
            text.contains("manage_mode = lan"),
            "the server's message is shown"
        );
        let off = client.remove_folder(4).unwrap_err();
        let text = manage_error_text(&off, true);
        assert!(
            text.contains("已关闭") && text.contains("manage_mode = off"),
            "{text}"
        );
        // An older server without the endpoint: unsupported, not "not found".
        let missing = client.remove_folder(5).unwrap_err();
        assert!(
            matches!(missing, RemoteError::Unsupported(_)),
            "{missing:?}"
        );
        assert!(manage_error_text(&missing, true).contains("服务器版本不支持"));
        // A real 404 (unknown root or subpath) stays NotFound.
        let (newer, _) = server(|_, _| error(404, "NOT_FOUND", "Unknown library root"));
        assert!(matches!(
            RemoteClient::new(&newer.url, "")
                .unwrap()
                .start_scan(&ScanRequest {
                    root: "nope".into(),
                    ..ScanRequest::default()
                }),
            Err(RemoteError::NotFound(_))
        ));
        // 405 where the path collides with an existing route (POST /audio/{id}).
        let (routed, _) = server(|_, _| {
            error(
                405,
                "METHOD_NOT_ALLOWED",
                "Method not allowed for this endpoint",
            )
        });
        assert!(matches!(
            RemoteClient::new(&routed.url, "")
                .unwrap()
                .remove_audio(&[1]),
            Err(RemoteError::Unsupported(_))
        ));
    }

    #[test]
    fn removals_send_unique_ids_and_return_revisions() {
        // The §6.8 replies and errors of the test instance.
        let (server, seen) = server(|method, path| match (method, path) {
            ("POST", "/api/v1/audio/remove") => TestResponse::json(
                200,
                r#"{"removed":2,"not_found":[999999999],"revision_before":2,"revision":3}"#,
            ),
            ("DELETE", "/api/v1/folders/412") => TestResponse::json(
                200,
                r#"{"removed":2,"folders_removed":2,"revision_before":3,"revision":4,"extra":"x"}"#,
            ),
            ("DELETE", "/api/v1/folders/1") => error(
                400,
                "VALIDATION",
                "Folder 1 is the top-level folder of library root \\\"Scratch\\\" (or contains it) and cannot be removed through the API",
            ),
            ("DELETE", "/api/v1/folders/999999999") => {
                error(404, "NOT_FOUND", "Not found: Folder 999999999")
            }
            ("DELETE", "/api/v1/folders/77") => error(
                409,
                "BUSY",
                "Busy: A scan is running for this library (session 2); retry when it has finished",
            ),
            _ => error(404, "NOT_FOUND", "No such endpoint"),
        });
        let client = RemoteClient::new(&server.url, "tok").unwrap();
        let report = client.remove_audio(&[5, 999999999, 5, 6]).unwrap();
        assert_eq!(
            (report.removed, report.not_found.as_slice()),
            (2, &[999999999][..])
        );
        assert_eq!((report.revision_before, report.revision), (2, 3));
        assert!(report.changed());
        let folder = client.remove_folder(412).unwrap();
        assert_eq!((folder.removed, folder.folders_removed), (2, 2));
        assert_eq!((folder.revision_before, folder.revision), (3, 4));
        assert!(matches!(
            client.remove_folder(1),
            Err(RemoteError::Validation(_))
        ));
        // A missing folder is a real 404, not an older server.
        assert!(matches!(
            client.remove_folder(999999999),
            Err(RemoteError::NotFound(_))
        ));
        assert!(matches!(
            client.remove_folder(77),
            Err(RemoteError::Busy { .. })
        ));
        let nothing: RemoveReport = serde_json::from_str(
            r#"{"removed":0,"not_found":[5,999999999],"revision_before":3,"revision":3}"#,
        )
        .unwrap();
        assert!(!nothing.changed());
        let log = seen.lock().unwrap();
        assert_eq!(log[0].body, r#"{"ids":[5,999999999,6]}"#);
        assert_eq!(log[1].method, "DELETE");
        assert_eq!(log[1].authorization.as_deref(), Some("Bearer tok"));
        let requests = log.len();
        drop(log);
        // Rejected locally, before any request.
        assert!(client.remove_audio(&[]).is_err());
        assert!(client.remove_audio(&[0]).is_err());
        assert!(client.remove_audio(&[-3]).is_err());
        let too_many: Vec<i64> = (1..=(MAX_REMOVE_IDS as i64 + 1)).collect();
        assert!(matches!(
            client.remove_audio(&too_many),
            Err(RemoteError::Validation(_))
        ));
        assert!(client.remove_folder(-1).is_err());
        assert!(client.remove_folder(0).is_err());
        assert_eq!(
            seen.lock().unwrap().len(),
            requests,
            "rejected before sending"
        );
    }
}
