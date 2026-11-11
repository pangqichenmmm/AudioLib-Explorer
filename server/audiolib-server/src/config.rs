// server/audiolib-server/src/config.rs
//! Server configuration loaded from TOML. The bearer token is read only from the
//! environment; it is never stored in the configuration file or written to logs. Settings
//! changed on the admin page (`admin-settings.json`, see `crate::auth`) override the
//! `auth_mode`, `manage_mode`, `public_url` and `tls_*` values here; this file is never
//! rewritten.
use crate::auth::{AuthMode, ManageMode, TlsSettings};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Environment variable naming the configuration file when `--config` is absent.
pub const CONFIG_ENV: &str = "AUDIOLIB_SERVER_CONFIG";
/// Default environment variable holding the bearer token.
pub const DEFAULT_TOKEN_ENV: &str = "AUDIOLIB_SERVER_TOKEN";
/// Default listen address: all interfaces, port 8790.
pub const DEFAULT_BIND: &str = "0.0.0.0:8790";
/// Default limit of concurrent `/audio/{id}/stream` bodies.
pub const DEFAULT_MAX_STREAMS: usize = 16;
/// Default ffmpeg executable (Homebrew on Apple silicon).
pub const DEFAULT_FFMPEG_PATH: &str = "/opt/homebrew/bin/ffmpeg";
/// Default limit of concurrent ffmpeg previews.
pub const DEFAULT_MAX_TRANSCODES: usize = 4;
/// Default limit of concurrent `/audio/{id}/preview` responses (both codecs).
pub const DEFAULT_MAX_PREVIEWS: usize = 8;
/// Minimum accepted bearer token length in characters.
pub const MIN_TOKEN_CHARS: usize = 32;

/// Parsed and validated server configuration. Relative paths are resolved against the
/// directory containing the configuration file.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Offer derived read-only schema-8 snapshots during the client-upgrade transition.
    #[serde(default = "default_legacy_snapshot_enabled")]
    pub legacy_snapshot_enabled: bool,
    /// Listen address, default [`DEFAULT_BIND`].
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    /// Existing AudioLib library database served by this process.
    pub library_db: PathBuf,
    /// Require `Authorization: Bearer <token>` on every endpoint except `/health` and the
    /// token variable at startup (same as `auth_mode = "all"`). Default `false`: the
    /// authentication mode is `auth_mode`.
    #[serde(default)]
    pub auth: bool,
    /// `off`, `external` (default) or `all`; see `crate::auth`. With `auth = true` only
    /// `all` is accepted. The admin page can override it.
    #[serde(default)]
    pub auth_mode: Option<AuthMode>,
    /// Trust private socket peers for token-free access and the password-free admin
    /// page (default true). Set false behind NAT or proxies that hide client addresses.
    /// This startup policy cannot be relaxed from the admin page.
    #[serde(default = "default_trust_lan_peers")]
    pub trust_lan_peers: bool,
    /// Who may manage the library (scans, record and folder removal): `lan` (default),
    /// `all` or `off`; see `crate::auth::ManageMode`. The admin page can override it.
    #[serde(default)]
    pub manage_mode: Option<ManageMode>,
    /// Public base URL clients use from outside the LAN (e.g. behind a reverse proxy),
    /// reported by `/health`; validated and normalized. The admin page can override it.
    #[serde(default)]
    pub public_url: Option<String>,
    /// PEM certificate chain of the optional native TLS listener (with `tls_key_path` and
    /// `tls_port`; all three or none). The admin page can override them.
    #[serde(default)]
    pub tls_cert_path: Option<PathBuf>,
    /// PEM private key of the native TLS listener.
    #[serde(default)]
    pub tls_key_path: Option<PathBuf>,
    /// Port of the native TLS listener (same address as `bind`; the HTTP port stays).
    #[serde(default)]
    pub tls_port: Option<u16>,
    /// Name of the environment variable holding the token, default [`DEFAULT_TOKEN_ENV`].
    #[serde(default)]
    pub token_env: Option<String>,
    /// Seconds between background library change checks (1..=3600, default 3).
    #[serde(default = "default_poll_secs")]
    pub revision_poll_secs: u64,
    /// Directory for temporary snapshots; `<library_db directory>/snapshots` when absent.
    #[serde(default)]
    pub snapshot_dir: Option<PathBuf>,
    /// Seconds `GET /library/snapshot` waits for a snapshot that has to be prepared before
    /// answering 503 `BUSY` with `Retry-After` (1..=3600, default 90, below the client's
    /// 120 s response-header timeout). The preparation continues in the background.
    #[serde(default = "default_snapshot_wait_secs")]
    pub snapshot_wait_secs: u64,
    /// Concurrent `/audio/{id}/stream` responses (1..=1024, default 16); more get 503 `BUSY`.
    #[serde(default = "default_max_streams")]
    pub max_streams: usize,
    /// Seconds between background checks of each root directory (60..=86400, default
    /// 300). A check is one `realpath` + `stat` of the root (no listing, no file reads).
    #[serde(default = "default_root_check_secs")]
    pub root_check_secs: u64,
    /// A root check that has not answered for this many seconds marks the root offline
    /// (10..=3600, default 120). Shorter waits are a waking disk, not an offline root.
    #[serde(default = "default_root_offline_secs")]
    pub root_offline_secs: u64,
    /// Waveform cache directory; `<library_db directory>/waveforms` when absent.
    #[serde(default)]
    pub waveform_cache_dir: Option<PathBuf>,
    /// Size limit of the waveform cache in MiB (16..=1048576, default 1024); the least
    /// recently used entries are removed beyond it.
    #[serde(default = "default_waveform_cache_mb")]
    pub waveform_cache_mb: u64,
    /// Concurrent waveform computations (1..=64, default 4); further requests wait.
    #[serde(default = "default_max_waveforms")]
    pub max_waveforms: usize,
    /// Pre-generate list thumbnail waveforms for the whole library in the background
    /// (default `false`); the admin page and `PATCH /settings` can override it.
    #[serde(default)]
    pub pregenerate_list_waveforms: Option<bool>,
    /// Size limit of the list thumbnail waveform cache in MiB (16..=1048576, default
    /// 640), separate from `waveform_cache_mb`.
    #[serde(default = "default_list_waveform_cache_mb")]
    pub list_waveform_cache_mb: u64,
    /// List waveform workers, i.e. `max_waveforms` slots they may hold at once (1..=64,
    /// default 3; never more than `max(1, max_waveforms - 1)`).
    #[serde(default = "default_list_waveform_workers")]
    pub list_waveform_workers: usize,
    /// Files pre-generation may start per second (1..=1000, default 10).
    #[serde(default = "default_list_waveform_rate")]
    pub list_waveform_rate: u32,
    /// Pause `POST /waveforms` metadata reads (in-request and background) for the 3 s
    /// prebuffer window after a stream or preview starts (default `false`).
    #[serde(default)]
    pub list_waveform_pause_stats_on_playback: bool,
    /// ffmpeg executable for `/audio/{id}/preview?codec=flac` (absolute path, default
    /// [`DEFAULT_FFMPEG_PATH`]). A missing ffmpeg only makes those requests answer 503
    /// `TRANSCODER_UNAVAILABLE`; the server still starts.
    #[serde(default = "default_ffmpeg_path")]
    pub ffmpeg_path: PathBuf,
    /// Concurrent ffmpeg previews (1..=64, default 4); further `codec=flac` requests wait
    /// up to 0.5 s for a slot, then get 503 `BUSY`.
    #[serde(default = "default_max_transcodes")]
    pub max_transcodes: usize,
    /// Concurrent `/audio/{id}/preview` responses of both codecs (1..=1024, default 8);
    /// more get 503 `BUSY`. Separate from `max_streams`, so busy `/stream` downloads never
    /// block previews (`codec=flac` is additionally limited by `max_transcodes`).
    #[serde(default = "default_max_previews")]
    pub max_previews: usize,
    /// Scan roots exposed to clients by name.
    #[serde(default)]
    pub roots: Vec<RootConfig>,
}

fn default_legacy_snapshot_enabled() -> bool {
    true
}

/// One configured scan root shared with clients.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RootConfig {
    /// Client-visible root name, e.g. `SFX_Main`; unique, no path separators.
    pub name: String,
    /// Absolute directory on the server.
    pub server_path: PathBuf,
    /// Optional WebDAV URL through which clients may reach the same directory.
    #[serde(default)]
    pub webdav_url: Option<String>,
}

/// A root name is 1..=128 characters without path separators or control characters.
pub fn check_root_name(name: &str) -> anyhow::Result<()> {
    if name.trim() != name
        || name.is_empty()
        || name.chars().count() > 128
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
    {
        bail!("Root name {name:?} must be 1..=128 characters without path separators");
    }
    Ok(())
}

fn default_trust_lan_peers() -> bool {
    true
}
fn default_bind() -> SocketAddr {
    DEFAULT_BIND.parse().expect("valid default bind address")
}
fn default_poll_secs() -> u64 {
    3
}
fn default_max_streams() -> usize {
    DEFAULT_MAX_STREAMS
}
fn default_root_check_secs() -> u64 {
    300
}
fn default_waveform_cache_mb() -> u64 {
    1024
}
fn default_snapshot_wait_secs() -> u64 {
    90
}
fn default_max_waveforms() -> usize {
    4
}
fn default_list_waveform_cache_mb() -> u64 {
    640
}
fn default_list_waveform_workers() -> usize {
    3
}
fn default_list_waveform_rate() -> u32 {
    crate::auth::DEFAULT_LIST_WAVEFORM_RATE
}
fn default_ffmpeg_path() -> PathBuf {
    PathBuf::from(DEFAULT_FFMPEG_PATH)
}
fn default_max_transcodes() -> usize {
    DEFAULT_MAX_TRANSCODES
}
fn default_max_previews() -> usize {
    DEFAULT_MAX_PREVIEWS
}
fn default_root_offline_secs() -> u64 {
    120
}

impl ServerConfig {
    /// Read, parse and validate a TOML configuration file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read server config {}", path.display()))?;
        let base = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        Self::from_toml_str(&text, base)
            .with_context(|| format!("Invalid server config {}", path.display()))
    }

    /// Parse and validate configuration text; relative paths resolve against `base_dir`.
    pub fn from_toml_str(text: &str, base_dir: &Path) -> anyhow::Result<Self> {
        let mut config: Self = toml::from_str(text)?;
        config.library_db = resolve(base_dir, &config.library_db);
        config.snapshot_dir = config.snapshot_dir.map(|dir| resolve(base_dir, &dir));
        config.waveform_cache_dir = config.waveform_cache_dir.map(|dir| resolve(base_dir, &dir));
        config.tls_cert_path = config.tls_cert_path.map(|path| resolve(base_dir, &path));
        config.tls_key_path = config.tls_key_path.map(|path| resolve(base_dir, &path));
        if let Some(url) = &config.public_url {
            config.public_url =
                Some(crate::auth::normalize_public_url(url).map_err(anyhow::Error::msg)?);
        }
        for root in &mut config.roots {
            root.server_path = resolve(base_dir, &root.server_path);
        }
        config.validate()?;
        Ok(config)
    }

    /// Root check timing derived from `root_check_secs` / `root_offline_secs`.
    pub fn root_policy(&self) -> crate::roots::RootPolicy {
        crate::roots::RootPolicy {
            interval: std::time::Duration::from_secs(self.root_check_secs),
            offline_after: std::time::Duration::from_secs(self.root_offline_secs),
            ..Default::default()
        }
    }

    /// Effective waveform cache directory (see [`ServerConfig::waveform_cache_dir`]).
    pub fn waveform_cache_path(&self) -> PathBuf {
        self.waveform_cache_dir.clone().unwrap_or_else(|| {
            self.library_db
                .parent()
                .unwrap_or(Path::new("."))
                .join("waveforms")
        })
    }

    /// List waveform workers actually started: `list_waveform_workers`, at most
    /// `max(1, max_waveforms - 1)` so detail waveforms always keep a slot.
    pub fn effective_list_waveform_workers(&self) -> usize {
        self.list_waveform_workers
            .min(self.max_waveforms.saturating_sub(1).max(1))
            .max(1)
    }

    /// Directory of the admin files (`admin-settings.json`, `tokens.json`): the directory
    /// of `library_db`.
    pub fn admin_dir(&self) -> PathBuf {
        self.library_db
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf()
    }

    /// Native TLS settings from `tls_cert_path` / `tls_key_path` / `tls_port`.
    pub fn tls(&self) -> Option<TlsSettings> {
        match (&self.tls_cert_path, &self.tls_key_path, self.tls_port) {
            (Some(cert), Some(key), Some(port)) => Some(TlsSettings {
                cert_path: cert.clone(),
                key_path: key.clone(),
                port,
            }),
            _ => None,
        }
    }

    /// Environment variable that must hold the bearer token.
    pub fn token_env_name(&self) -> &str {
        self.token_env.as_deref().unwrap_or(DEFAULT_TOKEN_ENV)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.library_db.as_os_str().is_empty() {
            bail!("library_db must not be empty");
        }
        let token_env = self.token_env_name();
        if token_env.is_empty()
            || !token_env
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            bail!("token_env must be a non-empty environment variable name ([A-Za-z0-9_])");
        }
        if !(1..=3600).contains(&self.revision_poll_secs) {
            bail!("revision_poll_secs must be 1..=3600");
        }
        if !(1..=1024).contains(&self.max_streams) {
            bail!("max_streams must be 1..=1024");
        }
        if !(60..=86_400).contains(&self.root_check_secs) {
            bail!("root_check_secs must be 60..=86400");
        }
        if !(10..=3600).contains(&self.root_offline_secs) {
            bail!("root_offline_secs must be 10..=3600");
        }
        if !(16..=1_048_576).contains(&self.waveform_cache_mb) {
            bail!("waveform_cache_mb must be 16..=1048576");
        }
        if !(1..=64).contains(&self.max_waveforms) {
            bail!("max_waveforms must be 1..=64");
        }
        if !(16..=1_048_576).contains(&self.list_waveform_cache_mb) {
            bail!("list_waveform_cache_mb must be 16..=1048576");
        }
        if !(1..=64).contains(&self.list_waveform_workers) {
            bail!("list_waveform_workers must be 1..=64");
        }
        if !(1..=1000).contains(&self.list_waveform_rate) {
            bail!("list_waveform_rate must be 1..=1000");
        }
        // `has_root` keeps the Unix default valid when the crate is tested on Windows.
        if !(self.ffmpeg_path.is_absolute() || self.ffmpeg_path.has_root()) {
            bail!("ffmpeg_path must be an absolute path");
        }
        if !(1..=64).contains(&self.max_transcodes) {
            bail!("max_transcodes must be 1..=64");
        }
        if !(1..=3600).contains(&self.snapshot_wait_secs) {
            bail!("snapshot_wait_secs must be 1..=3600");
        }
        if !(1..=1024).contains(&self.max_previews) {
            bail!("max_previews must be 1..=1024");
        }
        if self.auth && self.auth_mode.is_some_and(|mode| mode != AuthMode::All) {
            bail!("auth = true requires auth_mode = \"all\" (or no auth_mode)");
        }
        let tls_fields = [
            self.tls_cert_path.is_some(),
            self.tls_key_path.is_some(),
            self.tls_port.is_some(),
        ];
        if tls_fields.iter().any(|set| *set) && !tls_fields.iter().all(|set| *set) {
            bail!("tls_cert_path, tls_key_path and tls_port must be set together");
        }
        if let Some(port) = self.tls_port
            && (port == 0 || port == self.bind.port())
        {
            bail!("tls_port must be 1..=65535 and differ from the bind port");
        }
        let mut names = HashSet::new();
        for root in &self.roots {
            let name = root.name.as_str();
            check_root_name(name)?;
            if !names.insert(name.to_lowercase()) {
                bail!("Root name {name:?} is configured more than once");
            }
            if !root.server_path.is_absolute() {
                bail!("Root {name:?} server_path must be absolute");
            }
            if let Some(url) = &root.webdav_url
                && !(url.starts_with("http://") || url.starts_with("https://"))
            {
                bail!("Root {name:?} webdav_url must start with http:// or https://");
            }
        }
        Ok(())
    }
}

fn resolve(base_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() || path.as_os_str().is_empty() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

/// Shared secret for `Authorization: Bearer`. `Debug` output is redacted.
#[derive(Clone)]
pub struct BearerToken(Arc<str>);

impl BearerToken {
    /// Validate a token: at least [`MIN_TOKEN_CHARS`] visible ASCII characters.
    pub fn new(value: impl Into<String>) -> anyhow::Result<Self> {
        let value = value.into();
        if value.chars().count() < MIN_TOKEN_CHARS || !value.bytes().all(|b| b.is_ascii_graphic()) {
            bail!(
                "Bearer token must be at least {MIN_TOKEN_CHARS} visible ASCII characters without spaces"
            );
        }
        Ok(Self(value.into()))
    }

    /// Read the token from the environment variable configured by `token_env`.
    pub fn from_env(config: &ServerConfig) -> anyhow::Result<Self> {
        let name = config.token_env_name();
        let value = std::env::var(name)
            .with_context(|| format!("Bearer token missing: set environment variable {name}"))?;
        Self::new(value).with_context(|| format!("Invalid bearer token in {name}"))
    }

    /// Compare a presented token without early exit on the first differing byte.
    pub fn matches(&self, presented: &str) -> bool {
        let expected = self.0.as_bytes();
        let presented = presented.as_bytes();
        let mut difference = expected.len() ^ presented.len();
        for index in 0..expected.len().max(presented.len()) {
            let left = expected.get(index).copied().unwrap_or(0);
            let right = presented.get(index).copied().unwrap_or(0);
            difference |= usize::from(left ^ right);
        }
        difference == 0
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The example uses POSIX paths such as `/srv/...`, which are not absolute on Windows.
    #[cfg(unix)]
    #[test]
    fn example_config_parses_with_defaults_and_absolute_paths() {
        let config = ServerConfig::from_toml_str(
            include_str!("../config.example.toml"),
            Path::new("/srv/audiolib"),
        )
        .unwrap();
        assert_eq!(config.bind, default_bind());
        assert_eq!(config.token_env_name(), DEFAULT_TOKEN_ENV);
        assert!(!config.roots.is_empty());
        assert!(config.roots.iter().all(|r| r.server_path.is_absolute()));
    }

    #[test]
    fn relative_paths_resolve_against_config_directory() {
        let config = ServerConfig::from_toml_str(
            "bind = \"127.0.0.1:9000\"\nlibrary_db = \"data/library.db\"\nsnapshot_dir = \"tmp\"\n",
            Path::new("/srv/audiolib"),
        )
        .unwrap();
        assert_eq!(config.bind.port(), 9000);
        assert_eq!(
            config.library_db,
            Path::new("/srv/audiolib/data/library.db")
        );
        assert_eq!(
            config.snapshot_dir.as_deref(),
            Some(Path::new("/srv/audiolib/tmp"))
        );
        assert_eq!(config.revision_poll_secs, 3);
        assert_eq!(config.max_streams, DEFAULT_MAX_STREAMS);
        assert_eq!(config.ffmpeg_path, Path::new(DEFAULT_FFMPEG_PATH));
        assert_eq!(config.max_transcodes, DEFAULT_MAX_TRANSCODES);
        assert_eq!(config.max_previews, DEFAULT_MAX_PREVIEWS);
        assert_eq!(config.snapshot_wait_secs, 90);
        assert!(!config.auth, "authentication is off by default");
        assert_eq!(config.auth_mode, None, "external unless configured");
        assert_eq!(config.manage_mode, None, "all unless configured");
        assert_eq!(config.public_url, None);
        assert_eq!(config.tls(), None);
        assert_eq!(config.admin_dir(), Path::new("/srv/audiolib/data"));
        let config = ServerConfig::from_toml_str(
            "library_db = \"a.db\"\nauth_mode = \"all\"\npublic_url = \"HTTPS://X.Example.com:8443/\"\n\
             tls_cert_path = \"tls/c.pem\"\ntls_key_path = \"/etc/k.pem\"\ntls_port = 8791\n",
            Path::new("/srv"),
        )
        .unwrap();
        assert_eq!(config.auth_mode, Some(AuthMode::All));
        let managed = ServerConfig::from_toml_str(
            "library_db = \"a.db\"\nmanage_mode = \"off\"\n",
            Path::new("/srv"),
        )
        .unwrap();
        assert_eq!(managed.manage_mode, Some(ManageMode::Off));
        assert!(
            ServerConfig::from_toml_str(
                "library_db = \"a.db\"\nmanage_mode = \"some\"\n",
                Path::new("/srv")
            )
            .is_err()
        );
        assert_eq!(
            config.public_url.as_deref(),
            Some("https://x.example.com:8443")
        );
        let tls = config.tls().unwrap();
        assert_eq!(tls.cert_path, Path::new("/srv/tls/c.pem"));
        #[cfg(unix)]
        assert_eq!(tls.key_path, Path::new("/etc/k.pem"));
        assert_eq!(tls.port, 8791);
        let config =
            ServerConfig::from_toml_str("library_db = \"a.db\"\nauth = true\n", Path::new("/srv"))
                .unwrap();
        assert!(config.auth);
    }

    /// Error text of a configuration that must be rejected, including its context chain.
    fn rejection(text: &str) -> String {
        match ServerConfig::from_toml_str(text, Path::new("/srv")) {
            Ok(config) => panic!("accepted invalid configuration: {config:?}"),
            Err(error) => format!("{error:#}"),
        }
    }

    #[test]
    fn rejects_inline_tokens_duplicate_roots_and_bad_urls() {
        // Absolute on every platform (`/a` is not absolute on Windows), so each case below
        // reaches the check it targets instead of failing the absolute-path check first.
        // TOML literal strings keep Windows backslashes verbatim.
        let root = std::env::temp_dir();
        let a = root.join("audiolib-config-a");
        let b = root.join("audiolib-config-b");
        assert!(a.is_absolute() && !a.display().to_string().contains('\''));

        let error = rejection("library_db = \"a.db\"\ntoken = \"x\"\n");
        assert!(error.contains("unknown field `token`"), "{error}");
        let duplicate = format!(
            "library_db = \"a.db\"\n[[roots]]\nname = \"A\"\nserver_path = '{}'\n\
             [[roots]]\nname = \"a\"\nserver_path = '{}'\n",
            a.display(),
            b.display()
        );
        let error = rejection(&duplicate);
        assert!(error.contains("configured more than once"), "{error}");
        let bad_url = format!(
            "library_db = \"a.db\"\n[[roots]]\nname = \"A\"\nserver_path = '{}'\nwebdav_url = \"ftp://x\"\n",
            a.display()
        );
        let error = rejection(&bad_url);
        assert!(
            error.contains("webdav_url must start with http:// or https://"),
            "{error}"
        );
        let bad_name = format!(
            "library_db = \"a.db\"\n[[roots]]\nname = \"a/b\"\nserver_path = '{}'\n",
            a.display()
        );
        let error = rejection(&bad_name);
        assert!(error.contains("without path separators"), "{error}");
        let relative = "library_db = \"a.db\"\n[[roots]]\nname = \"A\"\nserver_path = \"\"\n";
        let error = rejection(relative);
        assert!(error.contains("must be absolute"), "{error}");
        let error = rejection("library_db = \"a.db\"\nrevision_poll_secs = 0\n");
        assert!(error.contains("revision_poll_secs"), "{error}");
        let error = rejection("library_db = \"a.db\"\nmax_streams = 0\n");
        assert!(error.contains("max_streams"), "{error}");
        let error = rejection("library_db = \"a.db\"\nmax_previews = 0\n");
        assert!(error.contains("max_previews"), "{error}");
        let error = rejection("library_db = \"a.db\"\nmax_transcodes = 0\n");
        assert!(error.contains("max_transcodes"), "{error}");
        let error = rejection("library_db = \"a.db\"\nauth = true\nauth_mode = \"external\"\n");
        assert!(error.contains("auth_mode"), "{error}");
        let error = rejection("library_db = \"a.db\"\nauth_mode = \"some\"\n");
        assert!(error.contains("auth_mode"), "{error}");
        let error = rejection("library_db = \"a.db\"\npublic_url = \"audiolib.example.com\"\n");
        assert!(error.contains("Public URL"), "{error}");
        let error = rejection("library_db = \"a.db\"\ntls_port = 8791\n");
        assert!(error.contains("set together"), "{error}");
        let error = rejection(
            "library_db = \"a.db\"\ntls_cert_path = \"c.pem\"\ntls_key_path = \"k.pem\"\ntls_port = 8790\n",
        );
        assert!(error.contains("differ from the bind port"), "{error}");
        let error = rejection("library_db = \"a.db\"\nffmpeg_path = \"ffmpeg\"\n");
        assert!(
            error.contains("ffmpeg_path must be an absolute path"),
            "{error}"
        );
    }

    #[test]
    fn token_is_validated_compared_and_redacted() {
        assert!(BearerToken::new("short").is_err());
        assert!(BearerToken::new("a".repeat(31)).is_err());
        assert!(BearerToken::new("a".repeat(32)).is_ok());
        assert!(BearerToken::new("has a space in the token").is_err());
        let token = BearerToken::new("0123456789abcdef0123456789abcdef-secret").unwrap();
        assert!(token.matches("0123456789abcdef0123456789abcdef-secret"));
        assert!(!token.matches("0123456789abcdef0123456789abcdef-secreT"));
        assert!(!token.matches("0123456789abcdef0123456789abcdef-secret-longer"));
        assert!(!token.matches(""));
        assert_eq!(format!("{token:?}"), "BearerToken(<redacted>)");
    }
}
