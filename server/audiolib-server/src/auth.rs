// server/audiolib-server/src/auth.rs
//! Who may call the API: authentication modes, external-request detection, bearer tokens
//! generated on the admin page, and the admin settings file.
//!
//! * `off`: no endpoint needs a token.
//! * `external` (default): requests that came through a reverse proxy (any forwarding
//!   header) or from a socket peer outside the loopback / private LAN ranges need
//!   `Authorization: Bearer <token>`; LAN-direct requests with a trusted Host need none.
//! * `all`: every endpoint except `/health` needs a token (the old `auth = true`).
//!
//! Accepted tokens: the environment token (`AUDIOLIB_SERVER_TOKEN`, when set) and tokens
//! generated on the admin page. Only the SHA-256 hash of a generated token is stored
//! (`<library_db directory>/tokens.json`, mode 0600); settings changed on the admin page
//! live in `admin-settings.json` beside it and override `config.toml`, which is never
//! rewritten.
//!
//! Library management (`manage_mode`: scans, removing records and folders) is a second,
//! independent gate checked after authentication: `lan` (default) allows it only to
//! LAN-direct requests, `all` to every authorized request, `off` to nobody (403
//! `MANAGE_FORBIDDEN`).
use crate::{ApiError, ServerConfig, config::BearerToken};
use axum::http::{HeaderMap, StatusCode, header};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex, PoisonError, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

/// File name of the admin settings (public URL, auth mode, TLS) beside the library.
pub const SETTINGS_FILE: &str = "admin-settings.json";
/// Persistent lock sidecar for settings writes and root-journal reconciliation.
const SETTINGS_LOCK_FILE: &str = ".admin-settings.lock";
/// File name of the generated-token hashes beside the library.
pub const TOKENS_FILE: &str = "tokens.json";
/// `last_used_at` of a token is updated (and written) at most this often.
pub const LAST_USED_RESOLUTION_SECS: i64 = 60;
/// Upper bound for generated tokens.
pub const MAX_TOKENS: usize = 256;
/// Prefix of generated tokens (`al_` + 64 hex characters).
pub const TOKEN_PREFIX: &str = "al_";
/// Failed bearer attempts per socket peer IP in one minute; forwarded headers never
/// supply this identity. The bounded table cannot grow with arbitrary remote peers.
const AUTH_FAILURE_LIMIT: u32 = 60;
const AUTH_FAILURE_WINDOW: Duration = Duration::from_secs(60);
const MAX_AUTH_FAILURE_PEERS: usize = 1024;

#[derive(Default)]
struct AuthFailures(HashMap<IpAddr, (Instant, u32)>);

impl AuthFailures {
    fn limited(&mut self, peer: IpAddr, now: Instant) -> bool {
        self.0
            .retain(|_, (started, _)| now.duration_since(*started) < AUTH_FAILURE_WINDOW);
        self.0
            .get(&peer)
            .is_some_and(|(_, count)| *count >= AUTH_FAILURE_LIMIT)
    }

    fn record(&mut self, peer: IpAddr, now: Instant) {
        if self.0.len() >= MAX_AUTH_FAILURE_PEERS
            && !self.0.contains_key(&peer)
            && let Some(oldest) = self
                .0
                .iter()
                .min_by_key(|(_, (started, _))| *started)
                .map(|(peer, _)| *peer)
        {
            self.0.remove(&oldest);
        }
        self.0
            .entry(peer)
            .and_modify(|(_, count)| *count += 1)
            .or_insert((now, 1));
    }
}

/// Cross-instance exclusion for root edits, scan admission or one settings write.
/// The sidecar stays in place after unlocking to prevent file replacement races.
pub(crate) struct SettingsLease(Option<std::fs::File>);

impl Drop for SettingsLease {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            let _ = file.unlock();
        }
    }
}
/// Request headers that mark a request as forwarded by a proxy.
pub const FORWARDING_HEADERS: [&str; 5] = [
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
    "forwarded",
    "x-real-ip",
];

/// Authentication mode (`auth_mode` in `config.toml` / on the admin page).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// No endpoint needs a token.
    Off,
    /// Only external requests (forwarded or from a non-private peer) need a token.
    External,
    /// Every endpoint except `/health` needs a token.
    All,
}

impl AuthMode {
    /// Wire name (`off`, `external`, `all`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::External => "external",
            Self::All => "all",
        }
    }
}

impl fmt::Display for AuthMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for AuthMode {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "off" => Ok(Self::Off),
            "external" => Ok(Self::External),
            "all" => Ok(Self::All),
            other => Err(format!(
                "auth_mode must be off, external or all, got {other:?}"
            )),
        }
    }
}

/// Who may manage the library (`manage_mode` in `config.toml` / on the admin page): start
/// and control scans, remove records (`POST /audio/remove`) and folders
/// (`DELETE /folders/{id}`). Metadata writes and play counts are not affected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManageMode {
    /// Every authorized request may manage the library.
    All,
    /// Only LAN-direct requests (default; see [`classify`]); external ones get 403.
    Lan,
    /// Nobody; every management request gets 403.
    Off,
}

impl ManageMode {
    /// Wire name (`all`, `lan`, `off`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Lan => "lan",
            Self::Off => "off",
        }
    }

    /// Whether a request of `origin` may manage the library in this mode.
    pub fn allows(self, origin: RequestOrigin) -> bool {
        match self {
            Self::All => true,
            Self::Lan => origin == RequestOrigin::Lan,
            Self::Off => false,
        }
    }
}

impl fmt::Display for ManageMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ManageMode {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "all" => Ok(Self::All),
            "lan" => Ok(Self::Lan),
            "off" => Ok(Self::Off),
            other => Err(format!(
                "manage_mode must be all, lan or off, got {other:?}"
            )),
        }
    }
}

/// Which listener accepted a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listener {
    /// The plain HTTP listener (`bind`).
    Http,
    /// The optional native TLS listener.
    Tls,
}

/// Per-connection facts the accept loop attaches to every request as an extension.
/// Requests without it (in-process `oneshot` calls in tests) count as LAN-direct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// Socket peer address.
    pub peer: SocketAddr,
    /// Listener that accepted the connection.
    pub listener: Listener,
    /// Local socket address the connection was accepted on, when known. A peer whose IP
    /// equals it is this host itself (e.g. a same-host reverse proxy targeting the LAN IP).
    pub local: Option<SocketAddr>,
}

/// Where a request came from, for authentication and the admin guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestOrigin {
    /// Directly from a private LAN address of another machine, without forwarding
    /// headers. Loopback and same-host peers are never LAN: a reverse proxy on this
    /// host makes public traffic arrive from them.
    Lan,
    /// Through a reverse proxy (forwarding header) or from a non-private peer, including
    /// public peers of the native TLS listener.
    External,
}

/// Loopback or private LAN address: 127/8, 10/8, 172.16/12, 192.168/16, ::1, fc00::/7,
/// fe80::/10 (IPv4-mapped IPv6 addresses are judged as IPv4).
pub fn is_lan_ip(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

/// Whether the socket peer is this host itself: loopback (127/8, ::1, IPv4-mapped
/// loopback) or the connection's own local socket IP (this host reached via its LAN IP).
pub fn is_same_host_peer(info: &ConnectionInfo) -> bool {
    let peer = info.peer.ip().to_canonical();
    peer.is_loopback()
        || info
            .local
            .is_some_and(|local| local.ip().to_canonical() == peer)
}

/// Whether any of [`FORWARDING_HEADERS`] is present (even empty).
pub fn has_forwarding_headers(headers: &HeaderMap) -> bool {
    FORWARDING_HEADERS
        .iter()
        .any(|name| headers.contains_key(*name))
}

/// Classify a request; see [`RequestOrigin`].
pub fn classify(headers: &HeaderMap, connection: Option<&ConnectionInfo>) -> RequestOrigin {
    if has_forwarding_headers(headers) {
        return RequestOrigin::External;
    }
    match connection {
        Some(info) if !is_lan_ip(info.peer.ip()) || is_same_host_peer(info) => {
            RequestOrigin::External
        }
        _ => RequestOrigin::Lan,
    }
}

/// Whether a browser request's Origin exactly names this listener's scheme and Host
/// authority. Native requests without Origin pass. Forwarding headers are untrusted
/// for this check, so a proxy cannot claim that an HTTP connection used TLS.
pub(crate) fn same_request_origin(request: &axum::extract::Request) -> bool {
    let mut origins = request.headers().get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    let Some(origin) = origin
        .to_str()
        .ok()
        .and_then(|value| value.parse::<axum::http::Uri>().ok())
    else {
        return false;
    };
    let scheme = match request.extensions().get::<ConnectionInfo>() {
        Some(info) if info.listener == Listener::Tls => "https",
        Some(_) => "http",
        None => return false,
    };
    if origin.scheme_str() != Some(scheme)
        || origin
            .path_and_query()
            .is_some_and(|path| path.as_str() != "/")
    {
        return false;
    }
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = hosts
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<axum::http::uri::Authority>().ok());
    if hosts.next().is_some() {
        return false;
    }
    match (origin.authority(), host) {
        (Some(origin), Some(host))
            if !origin.as_str().contains('@') && !host.as_str().contains('@') =>
        {
            // Authority::port_u16 treats nonnumeric ports as absent. Parse the raw
            // authority strictly so malformed ports cannot match a default port.
            let Some((origin_host, origin_port)) = split_host_port(origin.as_str()) else {
                return false;
            };
            let Some((request_host, request_port)) = split_host_port(host.as_str()) else {
                return false;
            };
            let default_port = if scheme == "https" { 443 } else { 80 };
            origin_host.eq_ignore_ascii_case(request_host)
                && origin_port.unwrap_or(default_port) == request_port.unwrap_or(default_port)
        }
        _ => false,
    }
}

/// Validate and normalize a public base URL such as `https://audiolib.example.com:8443`
/// or `https://example.com/audiolib`: `http`/`https`, a host, an optional port and an
/// optional path prefix; no user info, query or fragment. Scheme and host are lowercased,
/// a trailing `/` and a trailing `/api/v1` are removed.
pub fn normalize_public_url(raw: &str) -> Result<String, String> {
    let text = raw.trim();
    let invalid = |why: &str| format!("Public URL {text:?} is invalid: {why}");
    if text.is_empty() {
        return Err(invalid("empty"));
    }
    if text.len() > 2048 || text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid("contains spaces or control characters"));
    }
    let (scheme, rest) = text
        .split_once("://")
        .ok_or_else(|| invalid("expected http:// or https://"))?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(invalid("expected http:// or https://"));
    }
    if rest.contains(['?', '#']) {
        return Err(invalid("query strings and fragments are not allowed"));
    }
    let (authority, path) = match rest.find('/') {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    };
    if authority.contains('@') {
        return Err(invalid("user names and passwords are not allowed"));
    }
    let (host, port) = split_host_port(authority).ok_or_else(|| invalid("bad host or port"))?;
    let host = host.to_ascii_lowercase();
    let host_ok = if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        inner.parse::<std::net::Ipv6Addr>().is_ok()
    } else {
        !host.is_empty()
            && host.len() <= 253
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    && !label.starts_with('-')
                    && !label.ends_with('-')
            })
    };
    if !host_ok {
        return Err(invalid("bad host name"));
    }
    let mut path = path.trim_end_matches('/').to_string();
    if let Some(stripped) = path.strip_suffix("/api/v1") {
        path = stripped.trim_end_matches('/').to_string();
    }
    if path.contains("//")
        || path
            .split('/')
            .any(|segment| segment == ".." || segment == ".")
    {
        return Err(invalid("bad path prefix"));
    }
    Ok(match port {
        Some(port) => format!("{scheme}://{host}:{port}{path}"),
        None => format!("{scheme}://{host}{path}"),
    })
}

/// Split `host[:port]` / `[v6][:port]`; `None` for an empty host or a bad port.
fn split_host_port(authority: &str) -> Option<(&str, Option<u16>)> {
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']')?;
        let (host, rest) = authority.split_at(end + 1);
        match rest {
            "" => (host, None),
            _ => (host, Some(rest.strip_prefix(':')?)),
        }
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port {
        None => None,
        Some(text) => {
            if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let port: u16 = text.parse().ok()?;
            if port == 0 {
                return None;
            }
            Some(port)
        }
    };
    Some((host, port))
}

/// Parse a Host authority without accepting userinfo, bad ports or invalid DNS labels.
fn host_name(authority: &str) -> Option<String> {
    let parsed: axum::http::uri::Authority = authority.parse().ok()?;
    if parsed.as_str().contains('@') {
        return None;
    }
    let (host, _) = split_host_port(authority)?;
    if let Some(ip) = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        return ip
            .parse::<std::net::Ipv6Addr>()
            .ok()
            .map(|ip| ip.to_string());
    }
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        return Some(ip.to_string());
    }
    let name = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    if name.len() > 253
        || !name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return None;
    }
    Some(name)
}

/// Valid IP literal or LAN hostname, including an optional valid TCP port.
pub(crate) fn is_lan_host(authority: &str) -> bool {
    let Some(name) = host_name(authority) else {
        return false;
    };
    name.parse::<IpAddr>().is_ok()
        || name == "localhost"
        || !name.contains('.')
        || [".local", ".lan", ".home.arpa", ".internal", ".localhost"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// Native TLS listener settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TlsSettings {
    /// PEM certificate chain (leaf first).
    pub cert_path: PathBuf,
    /// PEM private key (PKCS#8, PKCS#1 or SEC1).
    pub key_path: PathBuf,
    /// TCP port of the TLS listener (same address as `bind`).
    pub port: u16,
}

/// `admin-settings.json`: overrides of `config.toml` made on the admin page. A missing
/// field means "use the `config.toml` value".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminSettings {
    /// File format version (1).
    #[serde(default = "format_version")]
    pub version: u32,
    /// Public base URL clients use from outside the LAN.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    /// Authentication mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<AuthMode>,
    /// Who may manage the library (scans, removals).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manage_mode: Option<ManageMode>,
    /// LAN base URL shown to clients (detected from the network interfaces when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_url: Option<String>,
    /// Native TLS listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsSettings>,
    /// Library roots as changed on the admin page: when present this complete list
    /// replaces the `[[roots]]` of `config.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<Vec<crate::config::RootConfig>>,
    /// Pre-generate list thumbnail waveforms (doc/api/README.md §9.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pregenerate_list_waveforms: Option<bool>,
    /// List waveform generation rate in files per second, 1..=1000 (§9.5.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_waveform_rate: Option<u32>,
    /// Recovery journal for an atomic indexed-root relocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_relink: Option<crate::manage::RootRelinkJournal>,
    /// Recovery journal for one atomic root removal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_removal: Option<crate::manage::RootRemovalJournal>,
}

/// Built-in `list_waveform_rate` (files per second).
pub const DEFAULT_LIST_WAVEFORM_RATE: u32 = 10;

fn format_version() -> u32 {
    1
}

/// One generated token as listed on the admin page (never the secret itself).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRecord {
    /// Random identifier used to revoke the token.
    pub id: String,
    /// Name given when the token was generated.
    pub name: String,
    /// Lowercase hex SHA-256 of the token.
    pub sha256: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds of the last accepted request (updated at most once a minute).
    #[serde(default)]
    pub last_used_at: Option<i64>,
}

/// Public view of a [`TokenRecord`] (no hash).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenInfo {
    /// Identifier used to revoke the token.
    pub id: String,
    /// Name given when the token was generated.
    pub name: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds of the last accepted request, at minute resolution.
    pub last_used_at: Option<i64>,
}

#[derive(Default, Serialize, Deserialize)]
struct TokenFile {
    #[serde(default = "format_version")]
    version: u32,
    #[serde(default)]
    tokens: Vec<TokenRecord>,
}

struct StoredToken {
    record: TokenRecord,
    hash: [u8; 32],
}

/// Where a setting's effective value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Built-in default.
    Default,
    /// `config.toml`.
    Config,
    /// `admin-settings.json` (admin page).
    Admin,
}

/// State of the native TLS listener as reported on the admin page.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TlsStatus {
    /// `off`, `starting`, `listening` or `error`.
    pub state: String,
    /// Port being served (or that failed).
    pub port: Option<u16>,
    /// Error text for `error`.
    pub message: Option<String>,
    /// Incremented every time the TLS settings change.
    #[serde(skip)]
    pub generation: u64,
}

/// Values from `config.toml` that admin settings override.
#[derive(Clone, Debug)]
struct Defaults {
    auth_mode: AuthMode,
    auth_mode_source: Source,
    trust_lan_peers: bool,
    manage_mode: ManageMode,
    manage_mode_source: Source,
    pregenerate_list_waveforms: bool,
    pregenerate_list_waveforms_source: Source,
    list_waveform_rate: u32,
    list_waveform_rate_source: Source,
    public_url: Option<String>,
    tls: Option<TlsSettings>,
}

struct Mutable {
    settings: AdminSettings,
    tokens: Vec<StoredToken>,
}

/// Shared authentication and admin settings state.
pub struct Security {
    dir: Option<PathBuf>,
    defaults: Defaults,
    http_port: u16,
    env_token: Option<BearerToken>,
    state: RwLock<Mutable>,
    /// Serializes file writes so concurrent admin changes never lose an update.
    write_lock: Mutex<()>,
    tls_tx: watch::Sender<(u64, Option<TlsSettings>)>,
    tls_status: Mutex<TlsStatus>,
    warnings: Vec<String>,
    auth_failures: Mutex<AuthFailures>,
}

impl fmt::Debug for Security {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Security")
            .field("dir", &self.dir)
            .field("auth_mode", &self.auth_mode())
            .finish_non_exhaustive()
    }
}

impl Security {
    /// Lock the settings directory and reload its latest values before reconciliation.
    /// Root edits hold this lease across settings writes and the database commit.
    /// Scan admission holds it until the scanner acquires the database scan lease;
    /// always acquire this settings lease before a database scan/administration lease.
    pub(crate) fn settings_lease(&self) -> audiolib_core::Result<SettingsLease> {
        let Some(dir) = &self.dir else {
            return Ok(SettingsLease(None));
        };
        std::fs::create_dir_all(dir)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(dir.join(SETTINGS_LOCK_FILE))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(audiolib_core::Error::Busy(
                    "Library roots or settings are being changed, or a scan is starting (in this or another server instance); retry in a moment".into(),
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
        let lease = SettingsLease(Some(file));
        let _write = self
            .write_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let loaded = match std::fs::read(dir.join(SETTINGS_FILE)) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                audiolib_core::Error::Validation(format!(
                    "Cannot reload admin settings for root recovery: {error}"
                ))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => AdminSettings::default(),
            Err(error) => return Err(error.into()),
        };
        self.apply_settings(loaded);
        Ok(lease)
    }

    /// Security from `config.toml` plus the admin files beside the library database
    /// (created on the first admin change). `env_token` is the `token_env` token, if set.
    /// Unreadable admin files are renamed to `*.corrupt` and reported on the admin page.
    pub fn from_config(config: &ServerConfig, env_token: Option<BearerToken>) -> Arc<Self> {
        let dir = config.admin_dir();
        let mut warnings = Vec::new();
        let settings: AdminSettings =
            load_json(&dir.join(SETTINGS_FILE), &mut warnings).unwrap_or_default();
        let tokens: TokenFile =
            load_json(&dir.join(TOKENS_FILE), &mut warnings).unwrap_or_default();
        let mut stored = Vec::new();
        for record in tokens.tokens {
            match parse_hash(&record.sha256) {
                Some(hash) => stored.push(StoredToken { record, hash }),
                None => warnings.push(format!(
                    "Token {:?} in {TOKENS_FILE} has an invalid hash and is ignored",
                    record.name
                )),
            }
        }
        let (auth_mode, auth_mode_source) = match (config.auth, config.auth_mode) {
            (true, _) => (AuthMode::All, Source::Config),
            (false, Some(mode)) => (mode, Source::Config),
            (false, None) => (AuthMode::External, Source::Default),
        };
        let (manage_mode, manage_mode_source) = match config.manage_mode {
            Some(mode) => (mode, Source::Config),
            None => (ManageMode::Lan, Source::Default),
        };
        let (pregenerate_list_waveforms, pregenerate_list_waveforms_source) =
            match config.pregenerate_list_waveforms {
                Some(value) => (value, Source::Config),
                None => (false, Source::Default),
            };
        // `config.toml` always yields a value (default 10); report `config` only when it
        // differs from the built-in default.
        let list_waveform_rate_source = if config.list_waveform_rate == DEFAULT_LIST_WAVEFORM_RATE {
            Source::Default
        } else {
            Source::Config
        };
        let defaults = Defaults {
            auth_mode,
            auth_mode_source,
            trust_lan_peers: config.trust_lan_peers,
            manage_mode,
            manage_mode_source,
            pregenerate_list_waveforms,
            pregenerate_list_waveforms_source,
            list_waveform_rate: config.list_waveform_rate,
            list_waveform_rate_source,
            public_url: config.public_url.clone(),
            tls: config.tls(),
        };
        Self::build(
            Some(dir),
            defaults,
            config.bind.port(),
            env_token,
            settings,
            stored,
            warnings,
        )
    }

    /// In-memory security with a fixed mode and no admin files (embedding and tests):
    /// `all` with the token, `off` without one.
    pub fn fixed(env_token: Option<BearerToken>) -> Arc<Self> {
        let auth_mode = if env_token.is_some() {
            AuthMode::All
        } else {
            AuthMode::Off
        };
        let defaults = Defaults {
            auth_mode,
            auth_mode_source: Source::Config,
            trust_lan_peers: true,
            manage_mode: ManageMode::Lan,
            manage_mode_source: Source::Default,
            pregenerate_list_waveforms: false,
            pregenerate_list_waveforms_source: Source::Default,
            list_waveform_rate: DEFAULT_LIST_WAVEFORM_RATE,
            list_waveform_rate_source: Source::Default,
            public_url: None,
            tls: None,
        };
        Self::build(
            None,
            defaults,
            0,
            env_token,
            AdminSettings::default(),
            Vec::new(),
            Vec::new(),
        )
    }

    fn build(
        dir: Option<PathBuf>,
        defaults: Defaults,
        http_port: u16,
        env_token: Option<BearerToken>,
        settings: AdminSettings,
        tokens: Vec<StoredToken>,
        warnings: Vec<String>,
    ) -> Arc<Self> {
        let tls = settings.tls.clone().or_else(|| defaults.tls.clone());
        let (tls_tx, _) = watch::channel((0, tls));
        Arc::new(Self {
            dir,
            defaults,
            http_port,
            env_token,
            state: RwLock::new(Mutable { settings, tokens }),
            write_lock: Mutex::new(()),
            tls_tx,
            tls_status: Mutex::new(TlsStatus {
                state: "off".into(),
                ..TlsStatus::default()
            }),
            warnings,
            auth_failures: Mutex::new(AuthFailures::default()),
        })
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Mutable> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Mutable> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The environment token, if one was configured.
    pub fn env_token(&self) -> Option<&BearerToken> {
        self.env_token.as_ref()
    }

    /// Effective authentication mode and its source.
    pub fn auth_mode_with_source(&self) -> (AuthMode, Source) {
        match self.read().settings.auth_mode {
            Some(mode) => (mode, Source::Admin),
            None => (self.defaults.auth_mode, self.defaults.auth_mode_source),
        }
    }

    /// Effective authentication mode.
    pub fn auth_mode(&self) -> AuthMode {
        self.auth_mode_with_source().0
    }

    /// Effective library management mode and its source.
    pub fn manage_mode_with_source(&self) -> (ManageMode, Source) {
        match self.read().settings.manage_mode {
            Some(mode) => (mode, Source::Admin),
            None => (self.defaults.manage_mode, self.defaults.manage_mode_source),
        }
    }

    /// Effective `pregenerate_list_waveforms` and its source.
    pub fn pregenerate_list_waveforms_with_source(&self) -> (bool, Source) {
        match self.read().settings.pregenerate_list_waveforms {
            Some(value) => (value, Source::Admin),
            None => (
                self.defaults.pregenerate_list_waveforms,
                self.defaults.pregenerate_list_waveforms_source,
            ),
        }
    }

    /// Effective `pregenerate_list_waveforms`.
    pub fn pregenerate_list_waveforms(&self) -> bool {
        self.pregenerate_list_waveforms_with_source().0
    }

    /// Effective `list_waveform_rate` (files per second) and its source (§9.5.4).
    pub fn list_waveform_rate_with_source(&self) -> (u32, Source) {
        match self.read().settings.list_waveform_rate {
            Some(value) => (value.clamp(1, 1000), Source::Admin),
            None => (
                self.defaults.list_waveform_rate,
                self.defaults.list_waveform_rate_source,
            ),
        }
    }

    /// Effective `list_waveform_rate`.
    pub fn list_waveform_rate(&self) -> u32 {
        self.list_waveform_rate_with_source().0
    }

    /// Library roots set on the admin page (replacing `config.toml`'s), if any.
    pub fn roots_override(&self) -> Option<Vec<crate::config::RootConfig>> {
        self.read().settings.roots.clone()
    }

    /// Pending indexed-root relocation, absent after successful final persistence.
    pub(crate) fn root_relink(&self) -> Option<crate::manage::RootRelinkJournal> {
        self.read().settings.root_relink.clone()
    }

    /// Pending root removal, absent after successful final persistence.
    pub(crate) fn root_removal(&self) -> Option<crate::manage::RootRemovalJournal> {
        self.read().settings.root_removal.clone()
    }

    /// Effective library management mode.
    pub fn manage_mode(&self) -> ManageMode {
        self.manage_mode_with_source().0
    }

    /// Apply the startup trust policy before treating a private socket peer as LAN.
    /// Disabling this trust also closes the password-free admin page.
    pub fn classify(
        &self,
        headers: &HeaderMap,
        connection: Option<&ConnectionInfo>,
    ) -> RequestOrigin {
        if self.defaults.trust_lan_peers {
            classify(headers, connection)
        } else {
            RequestOrigin::External
        }
    }

    /// Check that an (already authorized) request may manage the library: scans, record
    /// and folder removal. 403 `MANAGE_FORBIDDEN` otherwise.
    pub fn authorize_manage(
        &self,
        headers: &HeaderMap,
        connection: Option<&ConnectionInfo>,
    ) -> Result<(), ApiError> {
        match self.manage_mode() {
            ManageMode::All => Ok(()),
            ManageMode::Lan if self.classify(headers, connection) == RequestOrigin::Lan => Ok(()),
            ManageMode::Lan => Err(ApiError::manage_forbidden(
                "Library management (scans, removing records or folders) is only allowed from the LAN on this server (manage_mode = lan)",
            )),
            ManageMode::Off => Err(ApiError::manage_forbidden(
                "Library management (scans, removing records or folders) is disabled on this server (manage_mode = off)",
            )),
        }
    }

    /// Effective public base URL and its source.
    pub fn public_url_with_source(&self) -> (Option<String>, Source) {
        match &self.read().settings.public_url {
            Some(url) => (Some(url.clone()), Source::Admin),
            None => match &self.defaults.public_url {
                Some(url) => (Some(url.clone()), Source::Config),
                None => (None, Source::Default),
            },
        }
    }

    /// Effective public base URL (`/health` `public_url`).
    pub fn public_url(&self) -> Option<String> {
        self.public_url_with_source().0
    }

    /// LAN base URL set on the admin page, if any.
    pub fn lan_url_override(&self) -> Option<String> {
        self.read().settings.lan_url.clone()
    }

    /// Port of the plain HTTP listener (`bind`).
    pub fn http_port(&self) -> u16 {
        self.http_port
    }

    /// Effective TLS settings and their source.
    pub fn tls_with_source(&self) -> (Option<TlsSettings>, Source) {
        match &self.read().settings.tls {
            Some(tls) => (Some(tls.clone()), Source::Admin),
            None => match &self.defaults.tls {
                Some(tls) => (Some(tls.clone()), Source::Config),
                None => (None, Source::Default),
            },
        }
    }

    /// Receiver of `(generation, effective TLS settings)` for the TLS listener manager.
    pub fn tls_watch(&self) -> watch::Receiver<(u64, Option<TlsSettings>)> {
        self.tls_tx.subscribe()
    }

    /// Current TLS listener status.
    pub fn tls_status(&self) -> TlsStatus {
        self.tls_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Record the TLS listener status (by the manager).
    pub fn set_tls_status(&self, status: TlsStatus) {
        *self
            .tls_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = status;
    }

    /// Problems found while loading the admin files.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Paths of the admin files (`None` for in-memory security).
    pub fn files(&self) -> Option<(PathBuf, PathBuf)> {
        self.dir
            .as_ref()
            .map(|dir| (dir.join(SETTINGS_FILE), dir.join(TOKENS_FILE)))
    }

    /// Number of generated tokens.
    pub fn token_count(&self) -> usize {
        self.read().tokens.len()
    }

    /// Check a request against the effective mode: `Ok` when no token is needed or a
    /// valid one is presented, else 401 `UNAUTHORIZED`. Token-free requests from peers
    /// that are not trusted direct LAN peers additionally require a trusted Host. Failed authentication is
    /// limited per socket IP; forwarding headers cannot change the limiter identity.
    pub fn authorize(
        self: &Arc<Self>,
        headers: &HeaderMap,
        connection: Option<&ConnectionInfo>,
    ) -> Result<(), ApiError> {
        let mode = self.auth_mode();
        let origin = self.classify(headers, connection);
        let required = match mode {
            AuthMode::Off => false,
            AuthMode::All => true,
            AuthMode::External => origin == RequestOrigin::External,
        };
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| token.trim())
            .filter(|token| !token.is_empty());
        let host_forbidden = || {
            ApiError::new(
                StatusCode::FORBIDDEN,
                "HOST_FORBIDDEN",
                "Token-free access requires an IP address, LAN hostname or configured server hostname",
            )
        };
        // A trusted direct LAN peer (no forwarding headers, private/link-local, not this host,
        // socket IP, `trust_lan_peers = true`) is admitted whatever its Host header says:
        // LAN clients may reach the server via a public domain (hairpin NAT, split DNS).
        // The Host check only guards token-free requests from other peers (`off` mode).
        let lan_peer = connection.is_some() && origin == RequestOrigin::Lan;
        if !required && (lan_peer || self.trusted_host(headers, connection)) {
            return Ok(());
        }
        if !required && presented.is_none() {
            return Err(host_forbidden());
        }
        if let Some(connection) = connection
            && self
                .auth_failures
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .limited(connection.peer.ip().to_canonical(), Instant::now())
        {
            // Generated tokens carry 256 random bits, so a limited peer cannot guess one;
            // other clients behind the same proxy or NAT keep working. The configured
            // environment token may be weaker and stays limited.
            if presented.is_some_and(|token| self.verify_generated(token)) {
                return Ok(());
            }
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "AUTH_RATE_LIMITED",
                "Too many failed authentication attempts from this socket peer; retry in one minute",
            ));
        }
        if presented.is_some_and(|token| self.verify(token)) {
            return Ok(());
        }
        if let Some(connection) = connection {
            self.auth_failures
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .record(connection.peer.ip().to_canonical(), Instant::now());
            tracing::warn!(peer = %connection.peer, "Bearer authentication failed");
        } else {
            tracing::warn!("Bearer authentication failed for an embedded request");
        }
        Err(if !required {
            host_forbidden()
        } else if presented.is_none() {
            match mode {
                AuthMode::External => ApiError::token_required(
                    "A bearer token is required: this request is not from a trusted direct LAN peer",
                ),
                _ => ApiError::unauthorized(),
            }
        } else {
            ApiError::token_invalid()
        })
    }

    fn trusted_host(&self, headers: &HeaderMap, connection: Option<&ConnectionInfo>) -> bool {
        let mut hosts = headers.get_all(header::HOST).iter();
        let Some(value) = hosts.next() else {
            // Embedders do not have a network authority; real connections always must.
            return connection.is_none();
        };
        if hosts.next().is_some() {
            return false;
        }
        let Ok(authority) = value.to_str() else {
            return false;
        };
        if is_lan_host(authority) {
            return true;
        }
        let Some(host) = host_name(authority) else {
            return false;
        };
        [self.public_url(), self.lan_url_override()]
            .into_iter()
            .flatten()
            .any(|url| {
                url.parse::<axum::http::Uri>()
                    .ok()
                    .and_then(|uri| {
                        uri.authority()
                            .and_then(|authority| host_name(authority.as_str()))
                    })
                    .is_some_and(|configured| configured == host)
            })
    }

    /// Whether `presented` is the environment token or a generated (not revoked) token.
    /// Every stored hash is compared in constant time.
    pub fn verify(self: &Arc<Self>, presented: &str) -> bool {
        let env_ok = self
            .env_token
            .as_ref()
            .is_some_and(|token| token.matches(presented));
        let generated = self.verify_generated(presented);
        env_ok || generated
    }

    /// Whether `presented` is a generated (not revoked) token, recording its use.
    fn verify_generated(self: &Arc<Self>, presented: &str) -> bool {
        let hash = sha256(presented.as_bytes());
        let matched = {
            let state = self.read();
            let mut found = None;
            for (index, token) in state.tokens.iter().enumerate() {
                if constant_time_eq(&token.hash, &hash) && found.is_none() {
                    found = Some((index, token.record.id.clone(), token.record.last_used_at));
                }
            }
            found
        };
        if let Some((_, id, last_used)) = &matched {
            let now = unix_now();
            if last_used.is_none_or(|last| now - last >= LAST_USED_RESOLUTION_SECS) {
                self.touch(id, now);
            }
        }
        matched.is_some()
    }

    /// Update `last_used_at` in memory and write the token file off the request path.
    fn touch(self: &Arc<Self>, id: &str, now: i64) {
        {
            let mut state = self.write();
            match state.tokens.iter_mut().find(|token| token.record.id == id) {
                Some(token)
                    if token
                        .record
                        .last_used_at
                        .is_none_or(|last| now - last >= LAST_USED_RESOLUTION_SECS) =>
                {
                    token.record.last_used_at = Some(now);
                }
                _ => return,
            }
        }
        let this = self.clone();
        let persist = move || {
            if let Err(error) = this.persist_tokens() {
                tracing::warn!(?error, "Cannot record token last use");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(persist);
            }
            Err(_) => persist(),
        }
    }

    /// Generated tokens, oldest first.
    pub fn tokens(&self) -> Vec<TokenInfo> {
        self.read()
            .tokens
            .iter()
            .map(|token| TokenInfo {
                id: token.record.id.clone(),
                name: token.record.name.clone(),
                created_at: token.record.created_at,
                last_used_at: token.record.last_used_at,
            })
            .collect()
    }

    /// Blocking. Generate a token named `name`; returns its info and the plaintext secret,
    /// which is not kept anywhere.
    pub fn create_token(&self, name: &str) -> Result<(TokenInfo, String), ApiError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
            return Err(ApiError::validation(
                "Token name must be 1..=64 characters without control characters",
            ));
        }
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if self.read().tokens.len() >= MAX_TOKENS {
            return Err(ApiError::validation(format!(
                "At most {MAX_TOKENS} tokens; revoke unused ones first"
            )));
        }
        let secret = format!("{TOKEN_PREFIX}{}", hex(&random_bytes::<32>()?));
        let hash = sha256(secret.as_bytes());
        let record = TokenRecord {
            id: hex(&random_bytes::<8>()?),
            name: name.to_string(),
            sha256: hex(&hash),
            created_at: unix_now(),
            last_used_at: None,
        };
        let info = TokenInfo {
            id: record.id.clone(),
            name: record.name.clone(),
            created_at: record.created_at,
            last_used_at: None,
        };
        self.write().tokens.push(StoredToken { record, hash });
        if let Err(error) = self.persist_tokens_locked() {
            let id = info.id.clone();
            self.write().tokens.retain(|token| token.record.id != id);
            return Err(error);
        }
        tracing::info!(name = %info.name, id = %info.id, "Token generated on the admin page");
        Ok((info, secret))
    }

    /// Blocking. Revoke (delete) a generated token; `false` for an unknown id.
    pub fn revoke_token(&self, id: &str) -> Result<bool, ApiError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let removed = {
            let mut state = self.write();
            let index = state.tokens.iter().position(|token| token.record.id == id);
            index.map(|index| (index, state.tokens.remove(index)))
        };
        let Some((index, token)) = removed else {
            return Ok(false);
        };
        if let Err(error) = self.persist_tokens_locked() {
            self.write().tokens.insert(index, token);
            return Err(error);
        }
        tracing::info!(name = %token.record.name, id, "Token revoked on the admin page");
        Ok(true)
    }

    /// Blocking. Apply `change` to the admin settings and write them; on a write failure
    /// nothing changes. TLS changes are passed to the listener manager.
    pub fn update_settings(
        &self,
        change: impl FnOnce(&mut AdminSettings),
    ) -> Result<AdminSettings, ApiError> {
        let lease = self.settings_lease()?;
        self.update_settings_locked(&lease, change)
    }

    /// Persist settings while reusing a root edit's held cross-instance lease.
    pub(crate) fn update_settings_locked(
        &self,
        _lease: &SettingsLease,
        change: impl FnOnce(&mut AdminSettings),
    ) -> Result<AdminSettings, ApiError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut next = self.read().settings.clone();
        change(&mut next);
        next.version = format_version();
        if let Some(dir) = &self.dir {
            write_private_json(&dir.join(SETTINGS_FILE), &next)?;
        }
        self.apply_settings(next.clone());
        Ok(next)
    }

    fn apply_settings(&self, next: AdminSettings) {
        let tls_changed = {
            let mut state = self.write();
            let changed = state.settings.tls != next.tls;
            state.settings = next.clone();
            changed
        };
        if tls_changed {
            let effective = self.tls_with_source().0;
            self.tls_tx.send_modify(|(generation, tls)| {
                *generation += 1;
                *tls = effective;
            });
        }
    }

    fn persist_tokens(&self) -> Result<(), ApiError> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.persist_tokens_locked()
    }

    fn persist_tokens_locked(&self) -> Result<(), ApiError> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };
        let file = TokenFile {
            version: format_version(),
            tokens: self
                .read()
                .tokens
                .iter()
                .map(|token| token.record.clone())
                .collect(),
        };
        write_private_json(&dir.join(TOKENS_FILE), &file)
    }
}

/// Load a JSON file; a missing file is `None`, an unreadable one is renamed to
/// `<name>.corrupt` (so the next write does not silently replace it) and reported.
fn load_json<T: serde::de::DeserializeOwned>(path: &Path, warnings: &mut Vec<String>) -> Option<T> {
    let text = match std::fs::read(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            warnings.push(format!("Cannot read {}: {error}", path.display()));
            tracing::error!(path = %path.display(), %error, "Cannot read admin file");
            return None;
        }
    };
    match serde_json::from_slice(&text) {
        Ok(value) => Some(value),
        Err(error) => {
            let aside = path.with_extension("json.corrupt");
            let moved = std::fs::rename(path, &aside).is_ok();
            let message = format!(
                "{} is not valid ({error}); {}",
                path.display(),
                if moved {
                    format!("moved to {}", aside.display())
                } else {
                    "it could not be moved aside".to_string()
                }
            );
            tracing::error!("{message}");
            warnings.push(message);
            None
        }
    }
}

/// Atomically replace `path` with pretty JSON, readable only by the owner (0600 on Unix).
fn write_private_json(path: &Path, value: &impl Serialize) -> Result<(), ApiError> {
    let fail = |error: &dyn fmt::Display| {
        ApiError::internal(format!("Cannot write {}: {error}", path.display()))
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| fail(&error))?;
    bytes.push(b'\n');
    // `tempfile` creates the file with mode 0600 on Unix; the rename keeps it.
    let mut file = tempfile::Builder::new()
        .prefix(".admin-")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(|error| fail(&error))?;
    use std::io::Write;
    file.write_all(&bytes).map_err(|error| fail(&error))?;
    file.as_file().sync_all().map_err(|error| fail(&error))?;
    file.persist(path).map_err(|error| fail(&error.error))?;
    Ok(())
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0_u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

fn random_bytes<const N: usize>() -> Result<[u8; N], ApiError> {
    use ring::rand::SecureRandom;
    let mut out = [0_u8; N];
    ring::rand::SystemRandom::new()
        .fill(&mut out)
        .map_err(|_| ApiError::internal("The system random generator failed"))?;
    Ok(out)
}

/// Compare two hashes without an early exit.
fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut difference = 0_u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn parse_hash(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0_u8; 32];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        out[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

/// Current Unix time in seconds.
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// IPv4 addresses of up, non-loopback interfaces in private ranges, `(interface, ip)`,
/// `en*` interfaces first. Empty where interfaces cannot be listed.
pub fn lan_ipv4_addresses() -> Vec<(String, std::net::Ipv4Addr)> {
    let mut found = interface_ipv4_addresses();
    found.retain(|(_, ip)| ip.is_private());
    found.sort_by_key(|(name, _)| (!name.starts_with("en"), name.clone()));
    found.dedup();
    found
}

#[cfg(unix)]
fn interface_ipv4_addresses() -> Vec<(String, std::net::Ipv4Addr)> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` fills `head` with a list that is released by `freeifaddrs`
    // below; nothing from the list is used after that.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return out;
    }
    let mut current = head;
    while !current.is_null() {
        // SAFETY: `current` is a node of the list returned by `getifaddrs`.
        let entry = unsafe { &*current };
        let up = entry.ifa_flags & (libc::IFF_UP as libc::c_uint) != 0;
        // SAFETY: `ifa_addr` is null or points to a `sockaddr` owned by the list; for
        // `AF_INET` it is a `sockaddr_in`. `ifa_name` is a NUL-terminated string.
        unsafe {
            if up
                && !entry.ifa_addr.is_null()
                && i32::from((*entry.ifa_addr).sa_family) == libc::AF_INET
            {
                let address = &*(entry.ifa_addr as *const libc::sockaddr_in);
                let ip = std::net::Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
                let name = std::ffi::CStr::from_ptr(entry.ifa_name)
                    .to_string_lossy()
                    .into_owned();
                out.push((name, ip));
            }
        }
        current = entry.ifa_next;
    }
    // SAFETY: `head` came from a successful `getifaddrs` call and is freed once.
    unsafe { libc::freeifaddrs(head) };
    out
}

#[cfg(not(unix))]
fn interface_ipv4_addresses() -> Vec<(String, std::net::Ipv4Addr)> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_origin_requires_one_valid_authority_and_the_actual_listener_scheme() {
        let request = |host: &str, origin: &str, listener: Option<Listener>| {
            let mut request = axum::http::Request::builder()
                .uri("/api/v1/audio/1/plays")
                .header(header::HOST, host)
                .header(header::ORIGIN, origin)
                .body(axum::body::Body::empty())
                .unwrap();
            if let Some(listener) = listener {
                request.extensions_mut().insert(ConnectionInfo {
                    peer: "127.0.0.1:1".parse().unwrap(),
                    listener,
                    local: None,
                });
            }
            request
        };
        assert!(same_request_origin(&request(
            "SERVER.lan:80",
            "http://server.lan",
            Some(Listener::Http)
        )));
        assert!(same_request_origin(&request(
            "[::1]:443",
            "https://[::1]",
            Some(Listener::Tls)
        )));
        for (host, origin, listener) in [
            ("server.lan", "http://server.lan", None),
            ("server.lan", "https://server.lan", Some(Listener::Http)),
            ("server.lan", "http://server.lan", Some(Listener::Tls)),
            ("server.lan:bad", "http://server.lan", Some(Listener::Http)),
            ("server.lan", "http://server.lan:bad", Some(Listener::Http)),
            ("server.lan", "http://user@server.lan", Some(Listener::Http)),
            ("server.lan", "http://server.lan?x", Some(Listener::Http)),
        ] {
            assert!(
                !same_request_origin(&request(host, origin, listener)),
                "{host} {origin}"
            );
        }
        let mut duplicate = request("server.lan", "http://server.lan", Some(Listener::Http));
        duplicate
            .headers_mut()
            .append(header::ORIGIN, "http://server.lan".parse().unwrap());
        assert!(!same_request_origin(&duplicate));
        let mut proxy_claim = request("server.lan", "https://server.lan", Some(Listener::Http));
        proxy_claim
            .headers_mut()
            .insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(!same_request_origin(&proxy_claim));
        let native = axum::http::Request::builder()
            .body(axum::body::Body::empty())
            .unwrap();
        assert!(same_request_origin(&native));
    }
    use axum::http::HeaderValue;

    fn conn(peer: &str, listener: Listener) -> ConnectionInfo {
        ConnectionInfo {
            peer: peer.parse().unwrap(),
            listener,
            local: None,
        }
    }

    #[test]
    fn lan_ranges_are_exactly_the_documented_ones() {
        for ip in [
            "127.0.0.1",
            "127.255.0.9",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.10",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "::ffff:192.168.1.2",
        ] {
            assert!(is_lan_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "172.32.0.1",
            "172.15.255.255",
            "192.169.0.1",
            "100.64.0.1",
            "169.254.1.1",
            "198.18.0.1",
            "2001:db8::1",
            "::ffff:8.8.8.8",
            "fec0::1",
        ] {
            assert!(!is_lan_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn forwarded_or_public_requests_are_external() {
        let empty = HeaderMap::new();
        assert_eq!(classify(&empty, None), RequestOrigin::Lan);
        let lan = conn("10.0.0.20:5000", Listener::Http);
        assert_eq!(classify(&empty, Some(&lan)), RequestOrigin::Lan);
        let lan_tls = conn("10.0.0.20:5000", Listener::Tls);
        assert_eq!(classify(&empty, Some(&lan_tls)), RequestOrigin::Lan);
        let public_tls = conn("1.2.3.4:5000", Listener::Tls);
        assert_eq!(classify(&empty, Some(&public_tls)), RequestOrigin::External);
        let public = conn("[2001:db8::1]:5000", Listener::Http);
        assert_eq!(classify(&empty, Some(&public)), RequestOrigin::External);
        for name in FORWARDING_HEADERS {
            let mut headers = HeaderMap::new();
            headers.insert(name, HeaderValue::from_static(""));
            assert_eq!(
                classify(&headers, Some(&lan)),
                RequestOrigin::External,
                "{name}"
            );
            assert_eq!(classify(&headers, None), RequestOrigin::External, "{name}");
        }
    }

    #[test]
    fn public_urls_are_validated_and_normalized() {
        for (raw, expected) in [
            (
                "https://audiolib.example.com:8443",
                "https://audiolib.example.com:8443",
            ),
            (
                " HTTPS://AudioLib.Example.com:8443/audiolib/ ",
                "https://audiolib.example.com:8443/audiolib",
            ),
            ("http://example.com/api/v1", "http://example.com"),
            ("https://example.com/x/api/v1/", "https://example.com/x"),
            ("http://192.0.2.10:8790", "http://192.0.2.10:8790"),
            ("https://[2001:db8::1]:8443", "https://[2001:db8::1]:8443"),
        ] {
            assert_eq!(normalize_public_url(raw).unwrap(), expected, "{raw}");
        }
        for raw in [
            "",
            "audiolib.example.com",
            "ftp://example.com",
            "https://",
            "https://:443",
            "https://example.com:0",
            "https://example.com:99999",
            "https://example.com:x",
            "https://user:pw@example.com",
            "https://example.com/?a=1",
            "https://example.com/#x",
            "https://exa mple.com",
            "https://-bad-.com",
            "https://example.com/a/../b",
            "https://[nope]:1",
        ] {
            assert!(normalize_public_url(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn tokens_are_hashed_listed_verified_and_revoked() {
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig::from_toml_str("library_db = \"lib.db\"\n", dir.path()).unwrap();
        let security = Security::from_config(&config, None);
        assert_eq!(
            security.auth_mode_with_source(),
            (AuthMode::External, Source::Default)
        );
        let (info, secret) = security.create_token("office laptop").unwrap();
        assert!(secret.starts_with(TOKEN_PREFIX) && secret.len() == 3 + 64);
        let text = std::fs::read_to_string(dir.path().join(TOKENS_FILE)).unwrap();
        assert!(!text.contains(&secret), "plaintext never stored");
        assert!(text.contains(&hex(&sha256(secret.as_bytes()))));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(TOKENS_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(security.verify(&secret));
        assert!(!security.verify("al_wrong"));
        let listed = security.tokens();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "office laptop");
        assert!(listed[0].last_used_at.is_some(), "first use is recorded");
        // A restart reads the same tokens back.
        let reloaded = Security::from_config(&config, None);
        assert!(reloaded.verify(&secret));
        assert_eq!(reloaded.tokens()[0].id, info.id);
        assert!(security.revoke_token(&info.id).unwrap());
        assert!(!security.revoke_token(&info.id).unwrap());
        assert!(!security.verify(&secret));
        assert!(Security::from_config(&config, None).tokens().is_empty());
        assert!(security.create_token("  ").is_err());
        assert!(security.create_token(&"x".repeat(65)).is_err());
    }

    #[test]
    fn admin_settings_override_config_and_corrupt_files_are_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig::from_toml_str(
            "library_db = \"lib.db\"\nauth_mode = \"off\"\npublic_url = \"https://a.example.com\"\n",
            dir.path(),
        )
        .unwrap();
        let security = Security::from_config(&config, None);
        assert_eq!(
            security.auth_mode_with_source(),
            (AuthMode::Off, Source::Config)
        );
        assert_eq!(
            security.public_url_with_source(),
            (Some("https://a.example.com".into()), Source::Config)
        );
        security
            .update_settings(|settings| {
                settings.auth_mode = Some(AuthMode::All);
                settings.public_url = Some("https://b.example.com:8443".into());
            })
            .unwrap();
        let reloaded = Security::from_config(&config, None);
        assert_eq!(
            reloaded.auth_mode_with_source(),
            (AuthMode::All, Source::Admin)
        );
        assert_eq!(
            reloaded.public_url().as_deref(),
            Some("https://b.example.com:8443")
        );
        security
            .update_settings(|settings| settings.public_url = None)
            .unwrap();
        assert_eq!(
            security.public_url().as_deref(),
            Some("https://a.example.com")
        );
        std::fs::write(dir.path().join(SETTINGS_FILE), b"{not json").unwrap();
        let broken = Security::from_config(&config, None);
        assert_eq!(broken.auth_mode(), AuthMode::Off);
        assert_eq!(broken.warnings().len(), 1, "{:?}", broken.warnings());
        assert!(dir.path().join("admin-settings.json.corrupt").exists());
    }

    #[test]
    fn manage_mode_defaults_to_lan_and_preserves_explicit_all() {
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig::from_toml_str("library_db = \"lib.db\"\n", dir.path()).unwrap();
        let security = Security::from_config(&config, None);
        assert_eq!(
            security.manage_mode_with_source(),
            (ManageMode::Lan, Source::Default)
        );
        let lan = conn("10.0.0.20:1", Listener::Http);
        let public = conn("8.8.8.8:1", Listener::Http);
        let mut forwarded = HeaderMap::new();
        forwarded.insert("x-forwarded-for", HeaderValue::from_static("1.2.3.4"));
        let empty = HeaderMap::new();
        let code = |result: Result<(), ApiError>| result.err().map(|error| error.code());
        let cases = [
            (&empty, Some(&lan)),
            (&empty, None),
            (&forwarded, Some(&lan)),
            (&empty, Some(&public)),
        ];
        for ((headers, connection), expected) in cases.into_iter().zip([
            None,
            None,
            Some("MANAGE_FORBIDDEN"),
            Some("MANAGE_FORBIDDEN"),
        ]) {
            assert_eq!(
                code(security.authorize_manage(headers, connection)),
                expected
            );
        }
        security
            .update_settings(|s| s.manage_mode = Some(ManageMode::All))
            .unwrap();
        let reloaded = Security::from_config(&config, None);
        assert_eq!(reloaded.manage_mode(), ManageMode::All);
        for (headers, connection) in cases {
            assert_eq!(code(reloaded.authorize_manage(headers, connection)), None);
        }
        security
            .update_settings(|s| s.manage_mode = Some(ManageMode::Off))
            .unwrap();
        for (headers, connection) in cases {
            let error = security.authorize_manage(headers, connection).unwrap_err();
            assert_eq!(
                (error.status().as_u16(), error.code()),
                (403, "MANAGE_FORBIDDEN")
            );
        }
        // Persisted and reloaded; clearing the override returns to config.toml / default.
        let reloaded = Security::from_config(&config, None);
        assert_eq!(
            reloaded.manage_mode_with_source(),
            (ManageMode::Off, Source::Admin)
        );
        security.update_settings(|s| s.manage_mode = None).unwrap();
        assert_eq!(security.manage_mode(), ManageMode::Lan);
        let configured = ServerConfig::from_toml_str(
            "library_db = \"lib.db\"\nmanage_mode = \"lan\"\n",
            dir.path(),
        )
        .unwrap();
        assert_eq!(
            Security::from_config(&configured, None).manage_mode_with_source(),
            (ManageMode::Lan, Source::Config)
        );
        assert_eq!("lan".parse::<ManageMode>(), Ok(ManageMode::Lan));
        assert!("some".parse::<ManageMode>().is_err());
    }

    #[test]
    fn modes_decide_who_needs_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig::from_toml_str("library_db = \"lib.db\"\n", dir.path()).unwrap();
        let env = BearerToken::new("env-token-0123456789abcdef0123456789abcdef").unwrap();
        let security = Security::from_config(&config, Some(env));
        let (_, secret) = security.create_token("t").unwrap();
        let public = conn("8.8.8.8:1", Listener::Http);
        let lan = conn("10.0.0.20:1", Listener::Http);
        let with = |token: Option<&str>| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, HeaderValue::from_static("localhost"));
            if let Some(token) = token {
                headers.insert(
                    header::AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
                );
            }
            headers
        };
        let code = |result: Result<(), ApiError>| result.err().map(|error| error.code());
        // external: LAN ignores any token, external needs a valid one.
        for token in [None, Some("garbage"), Some(secret.as_str())] {
            assert_eq!(code(security.authorize(&with(token), Some(&lan))), None);
        }
        assert_eq!(
            code(security.authorize(&with(None), Some(&public))),
            Some("UNAUTHORIZED")
        );
        assert_eq!(
            code(security.authorize(&with(Some("garbage")), Some(&public))),
            Some("UNAUTHORIZED")
        );
        assert_eq!(
            code(security.authorize(&with(Some(&secret)), Some(&public))),
            None
        );
        assert_eq!(
            code(security.authorize(
                &with(Some("env-token-0123456789abcdef0123456789abcdef")),
                Some(&public)
            )),
            None
        );
        security
            .update_settings(|s| s.auth_mode = Some(AuthMode::All))
            .unwrap();
        assert_eq!(
            code(security.authorize(&with(None), Some(&lan))),
            Some("UNAUTHORIZED")
        );
        assert_eq!(
            code(security.authorize(&with(Some(&secret)), Some(&lan))),
            None
        );
        security
            .update_settings(|s| s.auth_mode = Some(AuthMode::Off))
            .unwrap();
        assert_eq!(code(security.authorize(&with(None), Some(&public))), None);
    }

    #[test]
    fn a_limited_peer_keeps_generated_tokens_but_not_the_environment_token() {
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig::from_toml_str("library_db = \"lib.db\"\n", dir.path()).unwrap();
        let env = "env-token-0123456789abcdef0123456789abcdef";
        let security = Security::from_config(&config, Some(BearerToken::new(env).unwrap()));
        let (_, secret) = security.create_token("t").unwrap();
        // Clients behind one reverse proxy share its socket peer.
        let proxy = conn("8.8.8.8:1", Listener::Http);
        let with = |token: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, HeaderValue::from_static("localhost"));
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
            );
            headers
        };
        let code = |result: Result<(), ApiError>| result.err().map(|error| error.code());
        for _ in 0..AUTH_FAILURE_LIMIT {
            assert_eq!(
                code(security.authorize(&with("garbage"), Some(&proxy))),
                Some("UNAUTHORIZED")
            );
        }
        assert_eq!(
            code(security.authorize(&with("garbage"), Some(&proxy))),
            Some("AUTH_RATE_LIMITED")
        );
        assert_eq!(code(security.authorize(&with(&secret), Some(&proxy))), None);
        assert_eq!(
            code(security.authorize(&with(env), Some(&proxy))),
            Some("AUTH_RATE_LIMITED")
        );
        // Another peer is unaffected.
        let other = conn("8.8.4.4:1", Listener::Http);
        assert_eq!(code(security.authorize(&with(env), Some(&other))), None);
    }

    #[test]
    fn failure_windows_expire_and_the_peer_table_is_bounded() {
        let mut failures = AuthFailures::default();
        let now = Instant::now();
        let peer: IpAddr = "203.0.113.1".parse().unwrap();
        for _ in 0..AUTH_FAILURE_LIMIT {
            failures.record(peer, now);
        }
        assert!(failures.limited(peer, now));
        assert!(!failures.limited(peer, now + AUTH_FAILURE_WINDOW));
        for index in 0..MAX_AUTH_FAILURE_PEERS + 1 {
            failures.record(IpAddr::V4(std::net::Ipv4Addr::from(index as u32)), now);
        }
        assert_eq!(failures.0.len(), MAX_AUTH_FAILURE_PEERS);
    }
}
