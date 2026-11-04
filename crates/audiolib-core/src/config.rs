// crates/audiolib-core/src/config.rs
//! Portable defaults and one-time, read-only interpretation of legacy YAML settings.
use crate::{Error, Result, SortOrder};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[path = "local_libraries.rs"]
mod local_libraries;
pub use local_libraries::{DEFAULT_LOCAL_LIBRARY_ID, LocalLibrary};

pub use crate::profiles::{
    ACTIVE_LOCAL, ActiveSource, DEFAULT_PROFILE_ID, ProfileKind, ProfileMigration, ServerProfile,
    TestSummary, default_transcode,
};

/// Where the desktop reads library data from. Old settings files default to `Local`.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DataSourceMode {
    /// The local SQLite library and local asset paths (the original behaviour).
    #[default]
    Local,
    /// A read-only cached snapshot of an AudioLib server library, audio over WebDAV.
    Server,
}

/// Non-secret WebDAV preferences. The password is stored separately by audiolib-remote.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebDavSettings {
    /// Quick fallback: `<base_url>/<root name>/<relative path>` when neither a user
    /// override nor a server-provided URL exists for a scan root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub username: String,
    /// User WebDAV URL per server scan root name. An entry overrides the server's
    /// suggested `webdav_url`; a missing or empty entry uses the server default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub root_urls: BTreeMap<String, String>,
}

/// Default seconds between background server revision checks.
pub const DEFAULT_POLL_INTERVAL_SECS: u32 = 60;
/// Smallest accepted non-zero revision check interval.
pub const MIN_POLL_INTERVAL_SECS: u32 = 10;
/// Largest accepted revision check interval (one day).
pub const MAX_POLL_INTERVAL_SECS: u32 = 86_400;

fn default_poll_interval() -> u32 {
    DEFAULT_POLL_INTERVAL_SECS
}

/// Default memory for server-mode audition streams (MiB).
pub const DEFAULT_AUDITION_MEMORY_MB: u32 = 512;
/// Accepted range of [`DataSourceSettings::audition_memory_mb`].
pub const MIN_AUDITION_MEMORY_MB: u32 = 32;
pub const MAX_AUDITION_MEMORY_MB: u32 = 8192;

fn default_audition_memory() -> u32 {
    DEFAULT_AUDITION_MEMORY_MB
}

/// Default maximum client-local playback events retained separately for each library.
pub const PLAY_HISTORY_LIMIT_PER_LIBRARY: u32 = 5_000;

fn default_play_history_limit() -> u32 {
    PLAY_HISTORY_LIMIT_PER_LIBRARY
}

/// Data source preferences persisted in settings.json. Secrets (API token and WebDAV
/// password) are deliberately absent: they live in a protected sidecar file.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataSourceSettings {
    #[serde(default)]
    pub mode: DataSourceMode,
    /// Server base URL such as `http://server.example.lan:8790` or
    /// `https://audiolib.example.com:8443/audiolib`; `/api/v1` is appended by clients
    /// (see [`crate::server_url`]).
    #[serde(default)]
    pub server_url: String,
    #[serde(default)]
    pub webdav: WebDavSettings,
    /// Seconds between background `GET /library` revision checks in server mode;
    /// `0` disables periodic checks (manual refresh still checks).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u32,
    /// Download and open a new snapshot automatically when the server revision or
    /// library identity changes, instead of only showing a notice with a Sync action.
    #[serde(default)]
    pub auto_sync: bool,
    /// Memory (MiB) for server-mode audition: streams are held in memory, least recently
    /// used first out, never written to disk.
    #[serde(default = "default_audition_memory")]
    pub audition_memory_mb: u32,
    /// Legacy "always audition the original" switch of earlier builds. Superseded by the
    /// per-profile [`crate::profiles::ServerProfile::transcode`]: read once when profiles
    /// are migrated (`true` → transcode off), afterwards written as the inverse of the
    /// active profile's choice so older builds behave the same.
    #[serde(default)]
    pub audition_original: bool,
    /// Active data source: [`ACTIVE_LOCAL`] or a profile id; `None` in settings written
    /// before profiles existed (see [`DataSourceSettings::migrate_profiles`]). `mode` and
    /// `server_url` above mirror it for older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// Saved server connections (LAN and external addresses are separate profiles).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ServerProfile>,
    /// Registered local databases. Removing an entry never removes its database or WAVs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_libraries: Vec<LocalLibrary>,
    /// Last selected local database, retained while a server source is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_local_library: Option<String>,
}

impl Default for DataSourceSettings {
    fn default() -> Self {
        Self {
            mode: DataSourceMode::default(),
            server_url: String::new(),
            webdav: WebDavSettings::default(),
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            auto_sync: false,
            audition_memory_mb: DEFAULT_AUDITION_MEMORY_MB,
            audition_original: false,
            active: None,
            profiles: Vec::new(),
            local_libraries: Vec::new(),
            active_local_library: None,
        }
    }
}

impl DataSourceSettings {
    /// Validate server-mode fields before they are saved or used for a connection.
    pub fn validate(&self) -> Result<()> {
        if self.mode == DataSourceMode::Server {
            crate::server_url::ServerAddress::parse(&self.server_url)?;
        }
        if self.poll_interval_secs != 0
            && !(MIN_POLL_INTERVAL_SECS..=MAX_POLL_INTERVAL_SECS).contains(&self.poll_interval_secs)
        {
            return Err(Error::Validation(format!(
                "Revision check interval must be 0 (off) or {MIN_POLL_INTERVAL_SECS}..={MAX_POLL_INTERVAL_SECS} seconds"
            )));
        }
        if !(MIN_AUDITION_MEMORY_MB..=MAX_AUDITION_MEMORY_MB).contains(&self.audition_memory_mb) {
            return Err(Error::Validation(format!(
                "Audition memory must be {MIN_AUDITION_MEMORY_MB}..={MAX_AUDITION_MEMORY_MB} MiB"
            )));
        }
        if let Some(base) = self
            .webdav
            .base_url
            .as_deref()
            .filter(|url| !url.is_empty())
        {
            validate_http_url(base, "WebDAV base URL")?;
        }
        for (name, url) in &self.webdav.root_urls {
            if !url.trim().is_empty() {
                validate_http_url(url, &format!("WebDAV URL for {name}"))?;
            }
        }
        if self.webdav.username.len() > 256 || self.webdav.username.contains(['\r', '\n']) {
            return Err(Error::Validation("WebDAV username is invalid".into()));
        }
        self.validate_profiles()?;
        self.validate_local_libraries()?;
        Ok(())
    }

    /// Why server mode cannot start from these settings, or `None` when it can: server
    /// mode needs `active` to name an existing profile whose `server_url` is non-empty
    /// and parses as a server address. Local mode always passes.
    pub fn server_mode_problem(&self) -> Option<String> {
        if self.mode != DataSourceMode::Server {
            return None;
        }
        let Some(active) = self.active.as_deref() else {
            return Some("no active server profile is selected".into());
        };
        let Some(profile) = self.active_profile() else {
            return Some(format!("active server profile {active:?} does not exist"));
        };
        let url = profile.server_url.trim();
        if url.is_empty() {
            return Some(format!("server profile {:?} has no server URL", profile.id));
        }
        crate::server_url::ServerAddress::parse(url)
            .err()
            .map(|error| {
                format!(
                    "server profile {:?} has an invalid URL: {error}",
                    profile.id
                )
            })
    }

    /// Startup guard: switch an unusable server selection to local mode in memory and
    /// return the reason for the caller to log. Settings on disk and saved profiles are
    /// not changed here.
    pub fn guard_server_mode(&mut self) -> Option<String> {
        let problem = self.server_mode_problem()?;
        self.mode = DataSourceMode::Local;
        Some(problem)
    }
}

/// Validate one set of WebDAV preferences (a profile override has the global shape).
pub(crate) fn validate_webdav(webdav: &WebDavSettings) -> Result<()> {
    if let Some(base) = webdav.base_url.as_deref().filter(|url| !url.is_empty()) {
        validate_http_url(base, "WebDAV base URL")?;
    }
    for (name, url) in &webdav.root_urls {
        if !url.trim().is_empty() {
            validate_http_url(url, &format!("WebDAV URL for {name}"))?;
        }
    }
    if webdav.username.len() > 256 || webdav.username.contains(['\r', '\n']) {
        return Err(Error::Validation("WebDAV username is invalid".into()));
    }
    Ok(())
}

/// Accept only absolute http(s) URLs with a host and no embedded credentials.
pub fn validate_http_url(url: &str, label: &str) -> Result<()> {
    let trimmed = url.trim();
    let rest = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"))
        .ok_or_else(|| Error::Validation(format!("{label} must start with http:// or https://")))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty()
        || authority.contains('@')
        || trimmed.len() > 2048
        || trimmed.chars().any(char::is_whitespace)
    {
        return Err(Error::Validation(format!(
            "{label} requires a host and must not contain spaces or credentials"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppPaths {
    pub database: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
    pub settings: PathBuf,
    pub legacy_database: Option<PathBuf>,
    pub audio_terms: Option<PathBuf>,
}

/// Audio result table widths in logical pixels and the library-wide search sort.
/// Missing preferences retain the default layout, while all widths stay within safe bounds.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AudioTablePreferences {
    /// File name column width, at least 120 logical pixels.
    #[serde(deserialize_with = "deserialize_name_width")]
    pub name_width: f32,
    /// Duration column width, at least 60 logical pixels.
    #[serde(deserialize_with = "deserialize_duration_width")]
    pub duration_width: f32,
    /// Sample rate and bit depth column width, at least 82 logical pixels.
    #[serde(deserialize_with = "deserialize_format_width")]
    pub format_width: f32,
    /// Channel count column width, at least 48 logical pixels.
    #[serde(deserialize_with = "deserialize_channels_width")]
    pub channels_width: f32,
    /// Tags column width, at least 80 logical pixels.
    #[serde(deserialize_with = "deserialize_tags_width")]
    pub tags_width: f32,
    /// Search ordering; this includes the existing Modified ordering.
    pub order_by: SortOrder,
    /// Reverse the selected ordering when true.
    pub descending: bool,
}

impl Default for AudioTablePreferences {
    fn default() -> Self {
        Self {
            name_width: 360.0,
            duration_width: 80.0,
            format_width: 100.0,
            channels_width: 64.0,
            tags_width: 160.0,
            order_by: SortOrder::Relevance,
            descending: false,
        }
    }
}

impl AudioTablePreferences {
    /// Return safe widths for UI use or persistence: finite values are clamped to
    /// each column's minimum and 2000 pixels, and non-finite values use its default.
    pub fn normalized(&self) -> Self {
        let defaults = Self::default();
        Self {
            name_width: normalize_table_width(self.name_width, 120.0, defaults.name_width),
            duration_width: normalize_table_width(
                self.duration_width,
                60.0,
                defaults.duration_width,
            ),
            format_width: normalize_table_width(self.format_width, 82.0, defaults.format_width),
            channels_width: normalize_table_width(
                self.channels_width,
                48.0,
                defaults.channels_width,
            ),
            tags_width: normalize_table_width(self.tags_width, 80.0, defaults.tags_width),
            order_by: self.order_by,
            descending: self.descending,
        }
    }
}

fn normalize_table_width(width: f32, minimum: f32, default: f32) -> f32 {
    if width.is_finite() {
        width.clamp(minimum, 2000.0)
    } else {
        default
    }
}

fn deserialize_table_width<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
    minimum: f32,
    default: f32,
) -> std::result::Result<f32, D::Error> {
    // Recover malformed UI-only widths without discarding otherwise valid library preferences.
    let value = serde_json::Value::deserialize(deserializer)?;
    let width = value.as_f64().map(|value| value as f32).unwrap_or(default);
    Ok(normalize_table_width(width, minimum, default))
}

fn deserialize_name_width<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<f32, D::Error> {
    deserialize_table_width(deserializer, 120.0, 360.0)
}

fn deserialize_duration_width<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<f32, D::Error> {
    deserialize_table_width(deserializer, 60.0, 80.0)
}

fn deserialize_format_width<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<f32, D::Error> {
    deserialize_table_width(deserializer, 82.0, 100.0)
}

fn deserialize_channels_width<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<f32, D::Error> {
    deserialize_table_width(deserializer, 48.0, 64.0)
}

fn deserialize_tags_width<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<f32, D::Error> {
    deserialize_table_width(deserializer, 80.0, 160.0)
}

/// Persistent spectral-color thresholds; old settings keep the original defaults.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SpectralPeakPreferences {
    /// Noise threshold multiplier, matching the display slider's 0.1..=2 range.
    #[serde(deserialize_with = "deserialize_peak_noise")]
    pub noise: f32,
    /// Frequency variance sensitivity, matching the display slider's 0.05..=1 range.
    #[serde(deserialize_with = "deserialize_peak_variance")]
    pub variance: f32,
}

impl Default for SpectralPeakPreferences {
    fn default() -> Self {
        Self {
            noise: 1.0,
            variance: 0.38,
        }
    }
}

impl SpectralPeakPreferences {
    /// Clamp display-only values without discarding unrelated library preferences.
    pub fn normalized(self) -> Self {
        Self {
            noise: normalize_peak_value(self.noise, 0.1, 2.0, 1.0),
            variance: normalize_peak_value(self.variance, 0.05, 1.0, 0.38),
        }
    }
}

fn normalize_peak_value(value: f32, minimum: f32, maximum: f32, default: f32) -> f32 {
    if value.is_finite() {
        value.clamp(minimum, maximum)
    } else {
        default
    }
}

fn deserialize_peak_noise<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<f32, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(normalize_peak_value(
        value.as_f64().map_or(1.0, |value| value as f32),
        0.1,
        2.0,
        1.0,
    ))
}

fn deserialize_peak_variance<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<f32, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(normalize_peak_value(
        value.as_f64().map_or(0.38, |value| value as f32),
        0.05,
        1.0,
        0.38,
    ))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
    pub paths: AppPaths,
    pub language: String,
    pub playback: serde_json::Value,
    /// Start preview playback when an audio list item is selected by a single click.
    /// Enabled by default (new installs and settings files without the field); an
    /// explicitly saved value, including `false`, is kept as-is.
    #[serde(default = "default_true")]
    pub audition_play_on_single_click: bool,
    /// Client-local playback event limit per library; zero disables new records.
    /// History lives under the user data root, independently of libraries and caches.
    #[serde(default = "default_play_history_limit")]
    pub play_history_limit: u32,
    /// Show embedded album artwork in local and server mode. Images stay outside libraries.
    #[serde(default = "default_true")]
    pub show_album_artwork: bool,
    /// Local mode: pre-generate list thumbnail Waveforms in the background after scans
    /// (doc/api/README.md §9.3). New installs default on; an existing settings.json that
    /// omits the key stays off (`#[serde(default)]` on the saved field is false).
    #[serde(default)]
    pub pregenerate_list_waveforms: bool,
    /// Server mode: hold new `POST /api/v1/waveforms` batches until 3 s after the latest
    /// audition start (deferred, not dropped). Off by default.
    #[serde(default)]
    pub hold_list_waveform_batches_after_audition: bool,
    /// Maximum open search tasks, set in Appearance and bounded to 1..=30.
    #[serde(default = "default_max_search_tasks")]
    pub max_search_tasks: u32,
    /// Interface scale in percent: 100-200 in steps of 25, or 0 to follow the system.
    /// Following the system keeps the content scale at 1.0 because the OS display scale
    /// already applies to the window. Other values are normalized to 0.
    #[serde(default)]
    pub ui_scale_percent: u32,
    /// Appearance theme identifier from [`THEME_IDS`]; unknown values use [`DEFAULT_THEME`].
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Interface text size in pixels before the interface scale, clamped to
    /// [`FONT_SIZE_MIN`]..=[`FONT_SIZE_MAX`]; [`DEFAULT_FONT_SIZE`] keeps the design sizes.
    #[serde(default = "default_font_size")]
    pub ui_font_size: u32,
    /// Result list text size in pixels before the interface scale, same range as
    /// [`AppConfig::ui_font_size`].
    #[serde(default = "default_font_size")]
    pub list_font_size: u32,
    /// Persistent table layout and sorting, applied to every search page.
    #[serde(default)]
    pub audio_table: AudioTablePreferences,
    /// Spectral color thresholds restored on the next launch.
    #[serde(default)]
    pub spectral_peaks: SpectralPeakPreferences,
    /// Desktop window geometry and pane layout restored on the next launch.
    #[serde(default)]
    pub window_layout: crate::window_layout::WindowLayoutPreferences,
    #[serde(default)]
    pub allowed_scan_roots: Vec<PathBuf>,
    #[serde(default)]
    pub data_source: DataSourceSettings,
    /// Set by [`AppConfig::guard_server_mode`] when an unusable server selection was
    /// opened as local for this session only. Never persisted; see [`AppConfig::save`].
    #[serde(skip)]
    pub data_source_fallback: Option<DataSourceFallback>,
}

/// The persisted data-source selection replaced in memory by the startup guard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataSourceFallback {
    pub mode: DataSourceMode,
    pub active: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SavedPreferences {
    #[serde(default)]
    active_database: Option<PathBuf>,
    #[serde(default = "english")]
    language: String,
    #[serde(default = "default_playback")]
    playback: serde_json::Value,
    #[serde(default = "default_true")]
    audition_play_on_single_click: bool,
    #[serde(default = "default_play_history_limit")]
    play_history_limit: u32,
    #[serde(default = "default_true")]
    show_album_artwork: bool,
    #[serde(default)]
    pregenerate_list_waveforms: bool,
    #[serde(default)]
    hold_list_waveform_batches_after_audition: bool,
    #[serde(
        default = "default_max_search_tasks",
        deserialize_with = "lenient_max_search_tasks"
    )]
    max_search_tasks: u32,
    #[serde(default)]
    ui_scale_percent: u32,
    #[serde(default = "default_theme", deserialize_with = "lenient_theme")]
    theme: String,
    #[serde(default = "default_font_size", deserialize_with = "lenient_font_size")]
    ui_font_size: u32,
    #[serde(default = "default_font_size", deserialize_with = "lenient_font_size")]
    list_font_size: u32,
    #[serde(default)]
    audio_table: AudioTablePreferences,
    #[serde(default)]
    spectral_peaks: SpectralPeakPreferences,
    #[serde(default)]
    window_layout: crate::window_layout::WindowLayoutPreferences,
    #[serde(default)]
    allowed_scan_roots: Vec<PathBuf>,
    #[serde(default)]
    data_source: DataSourceSettings,
}
/// Default open-task capacity, matching the hard workspace limit.
pub const DEFAULT_MAX_SEARCH_TASKS: u32 = crate::search_workspace::MAX_SEARCH_TASKS as u32;

/// Clamp a task capacity without deleting any already persisted tasks.
pub fn normalize_max_search_tasks(value: u32) -> u32 {
    value.clamp(1, DEFAULT_MAX_SEARCH_TASKS)
}

fn default_max_search_tasks() -> u32 {
    DEFAULT_MAX_SEARCH_TASKS
}

fn lenient_max_search_tasks<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u32, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::Number(number) => number
            .as_f64()
            .filter(|value| value.is_finite())
            .map(|value| {
                value
                    .round()
                    .clamp(1.0, f64::from(DEFAULT_MAX_SEARCH_TASKS)) as u32
            })
            .unwrap_or(DEFAULT_MAX_SEARCH_TASKS),
        _ => DEFAULT_MAX_SEARCH_TASKS,
    })
}
/// Interface scale choices offered in the Appearance settings, in percent.
pub const UI_SCALE_PERCENTS: [u32; 5] = [100, 125, 150, 175, 200];

/// Keep a supported interface scale; anything else (including 0) follows the system.
pub fn normalize_ui_scale_percent(percent: u32) -> u32 {
    if UI_SCALE_PERCENTS.contains(&percent) {
        percent
    } else {
        0
    }
}

/// Identifier of the built-in default theme (the original palette).
pub const DEFAULT_THEME: &str = "default";
/// Identifier of the opt-in Studio theme, with the redesigned audition controls.
pub const STUDIO_THEME: &str = "studio";

/// Audition presentation selected by a theme; playback state remains shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerStyle {
    /// Original beta audition layout and controls.
    Classic,
    /// Redesigned audition controls, overview and motion.
    Studio,
}

/// Built-in appearance definition shared by settings validation and the desktop.
pub struct BuiltinTheme {
    /// Stable settings identifier; existing identifiers must retain their appearance.
    pub id: &'static str,
    /// English name offered in Appearance settings.
    pub name: &'static str,
    /// Chinese name offered in Appearance settings.
    pub name_cn: &'static str,
    /// Audition controls and layout belonging to this theme.
    pub player_style: PlayerStyle,
}

/// Single registry of shipped themes, with the original default first.
pub const BUILTIN_THEMES: [BuiltinTheme; 2] = [
    BuiltinTheme {
        id: DEFAULT_THEME,
        name: "Default",
        name_cn: "默认",
        player_style: PlayerStyle::Classic,
    },
    BuiltinTheme {
        id: STUDIO_THEME,
        name: "Studio",
        name_cn: "工作室",
        player_style: PlayerStyle::Studio,
    },
];

/// Theme identifiers offered in the Appearance settings, in display order.
pub const THEME_IDS: [&str; BUILTIN_THEMES.len()] = {
    let mut ids = [""; BUILTIN_THEMES.len()];
    let mut index = 0;
    while index < BUILTIN_THEMES.len() {
        ids[index] = BUILTIN_THEMES[index].id;
        index += 1;
    }
    ids
};

/// Keep a known theme identifier; anything else uses the default theme.
pub fn normalize_theme(theme: &str) -> String {
    if THEME_IDS.contains(&theme) {
        theme.to_owned()
    } else {
        DEFAULT_THEME.to_owned()
    }
}

/// Smallest interface and list font size in pixels.
pub const FONT_SIZE_MIN: u32 = 10;
/// Largest interface and list font size in pixels.
pub const FONT_SIZE_MAX: u32 = 24;
/// Default interface and list font size in pixels.
pub const DEFAULT_FONT_SIZE: u32 = 13;

/// Clamp a font size setting into [`FONT_SIZE_MIN`]..=[`FONT_SIZE_MAX`].
pub fn clamp_font_size(size: i64) -> u32 {
    size.clamp(i64::from(FONT_SIZE_MIN), i64::from(FONT_SIZE_MAX)) as u32
}

fn default_theme() -> String {
    DEFAULT_THEME.to_owned()
}
/// Accept any JSON value for the theme; non-strings fall back to [`DEFAULT_THEME`]
/// so one bad field never discards the other preferences.
fn lenient_theme<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(theme) => theme,
        _ => default_theme(),
    })
}

fn default_font_size() -> u32 {
    DEFAULT_FONT_SIZE
}
/// Out-of-range numbers clamp (fractions round); a non-numeric value uses the default
/// so one bad field never discards the other preferences.
fn lenient_font_size<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u32, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::Number(number) => number
            .as_i64()
            .or_else(|| {
                number
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .map(|value| value.round().clamp(i64::MIN as f64, i64::MAX as f64) as i64)
            })
            .map_or(DEFAULT_FONT_SIZE, clamp_font_size),
        _ => DEFAULT_FONT_SIZE,
    })
}

fn default_true() -> bool {
    true
}
fn english() -> String {
    "en".into()
}
fn default_playback() -> serde_json::Value {
    serde_json::json!({"backend":"wasapi","device_id":null,"buffer_frames":0,
        "output_pair":0,"source_pair":0,"volume":0.8,"rate_policy":"device_default"})
}

impl AppConfig {
    /// Load user preferences, finding legacy settings without modifying the old library.
    pub fn load_default() -> Result<Self> {
        Self::load_in(Self::default_root()?)
    }

    /// Resolve the per-user root, preserving an explicit `AUDIOLIB_DATA_DIR`.
    /// macOS uses `~/Library/Application Support/AudioLibExplorer/v1`, or the
    /// sibling `beta` directory for a validated beta/rc package. A beta package
    /// also respects `AUDIOLIB_BETA_DATA_DIR`. Windows retains
    /// `%LOCALAPPDATA%/AudioLibExplorer/v1`. Unpublished macOS development
    /// data under `.audiolib-user` is never moved or deleted; select it explicitly
    /// with `AUDIOLIB_DATA_DIR` to continue using that directory.
    pub fn default_root() -> Result<PathBuf> {
        let override_root = std::env::var_os("AUDIOLIB_DATA_DIR").map(PathBuf::from);
        #[cfg(target_os = "macos")]
        if override_root.is_none() {
            let executable = std::env::current_exe()?;
            return macos_root_for_executable(
                std::env::var_os("HOME").map(PathBuf::from),
                std::env::var_os("AUDIOLIB_BETA_DATA_DIR").map(PathBuf::from),
                &executable,
                env!("CARGO_PKG_VERSION"),
            );
        }
        default_root_with_paths(
            override_root,
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
            cfg!(target_os = "macos"),
            || Ok(std::env::current_dir()?),
        )
    }

    /// Directory holding settings.json and all per-user runtime state.
    pub fn data_root(&self) -> PathBuf {
        self.paths
            .settings
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Load settings from an explicit user-data root; useful for isolated validation.
    pub fn load_in(root: PathBuf) -> Result<Self> {
        let mut roots = vec![std::env::current_dir()?];
        if let Ok(exe) = std::env::current_exe() {
            roots.extend(exe.ancestors().skip(1).take(5).map(PathBuf::from));
        }
        Self::load_with_roots(root, &roots)
    }

    fn load_with_roots(root: PathBuf, roots: &[PathBuf]) -> Result<Self> {
        let legacy_root = roots
            .iter()
            .find(|path| path.join("config/audiolib_config.yaml").is_file())
            .cloned();
        let settings = root.join("settings.json");
        let mut config = Self {
            paths: AppPaths {
                database: root.join("library.db"),
                cache: root.join("cache"),
                logs: root.join("logs"),
                settings,
                legacy_database: None,
                audio_terms: roots
                    .iter()
                    .map(|path| path.join("resources/audio_terms.txt"))
                    .find(|path| path.is_file()),
            },
            language: english(),
            playback: default_playback(),
            audition_play_on_single_click: true,
            play_history_limit: PLAY_HISTORY_LIMIT_PER_LIBRARY,
            show_album_artwork: true,
            pregenerate_list_waveforms: true,
            hold_list_waveform_batches_after_audition: false,
            max_search_tasks: DEFAULT_MAX_SEARCH_TASKS,
            ui_scale_percent: 0,
            theme: default_theme(),
            ui_font_size: DEFAULT_FONT_SIZE,
            list_font_size: DEFAULT_FONT_SIZE,
            audio_table: AudioTablePreferences::default(),
            spectral_peaks: SpectralPeakPreferences::default(),
            window_layout: Default::default(),
            allowed_scan_roots: Vec::new(),
            data_source: DataSourceSettings::default(),
            data_source_fallback: None,
        };
        // Established Rust state is independent of legacy YAML validity or presence.
        if !config.paths.settings.exists()
            && !config.paths.database.exists()
            && let Some(legacy_root) = legacy_root
        {
            let yaml_path = legacy_root.join("config/audiolib_config.yaml");
            let yaml: serde_yaml::Value = serde_yaml::from_slice(&std::fs::read(yaml_path)?)
                .map_err(|error| Error::Validation(format!("Legacy config: {error}")))?;
            let old_path = yaml["database"]["path"]
                .as_str()
                .unwrap_or("data/audiolib.db");
            let path = PathBuf::from(old_path);
            let path = if path.is_absolute() {
                path
            } else {
                legacy_root.join(path)
            };
            if path.is_file() && path != config.paths.database {
                config.paths.legacy_database = Some(path);
            }
            if let Some(language) = yaml["application"]["language"].as_str() {
                config.language = language.into();
            }
            if let Some(volume) = yaml["playback"]["default_volume"].as_f64()
                && volume.is_finite()
                && (0.0..=1.0).contains(&volume)
            {
                config.playback["volume"] = serde_json::json!(volume);
            }
            if let Some(roots) = yaml["security"]["allowed_scan_roots"].as_sequence() {
                config.allowed_scan_roots = roots
                    .iter()
                    .filter_map(|value| value.as_str().map(PathBuf::from))
                    .collect();
            }
        }
        if config.paths.settings.exists() {
            let saved: SavedPreferences =
                serde_json::from_slice(&std::fs::read(&config.paths.settings)?)
                    .map_err(|error| Error::Validation(format!("User settings: {error}")))?;
            config.language = saved.language;
            config.playback = saved.playback;
            config.audition_play_on_single_click = saved.audition_play_on_single_click;
            config.play_history_limit = saved.play_history_limit;
            config.show_album_artwork = saved.show_album_artwork;
            config.pregenerate_list_waveforms = saved.pregenerate_list_waveforms;
            config.hold_list_waveform_batches_after_audition =
                saved.hold_list_waveform_batches_after_audition;
            config.max_search_tasks = normalize_max_search_tasks(saved.max_search_tasks);
            config.ui_scale_percent = normalize_ui_scale_percent(saved.ui_scale_percent);
            config.theme = normalize_theme(&saved.theme);
            config.ui_font_size = clamp_font_size(i64::from(saved.ui_font_size));
            config.list_font_size = clamp_font_size(i64::from(saved.list_font_size));
            config.audio_table = saved.audio_table.normalized();
            config.spectral_peaks = saved.spectral_peaks.normalized();
            config.window_layout = saved.window_layout.normalized();
            config.allowed_scan_roots = saved.allowed_scan_roots;
            config.data_source = saved.data_source;
            // Single-server settings become one profile; older-client edits of the old
            // fields are folded into the profiles. Persisted by the next save; the first
            // migration keeps a one-time copy of the original file.
            if config.data_source.migrate_profiles() == ProfileMigration::Created {
                let backup = config.paths.settings.with_extension("json.pre-profiles");
                if !backup.exists() {
                    let _ = std::fs::copy(&config.paths.settings, &backup);
                }
            }
            if let Some(path) = saved.active_database {
                config.paths.database = if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                };
                config.paths.legacy_database = None;
            } else if let Some(library) = config.selected_local_library().cloned() {
                config.paths.database = library.database;
                config.pregenerate_list_waveforms = library.pregenerate_list_waveforms;
            }
        }
        config.reconcile_local_libraries()?;
        if config.language != "cn" && config.language != "en" {
            config.language = english();
        }
        for directory in [&root, &config.paths.cache, &config.paths.logs] {
            std::fs::create_dir_all(directory)?;
        }
        Ok(config)
    }

    /// Atomically persist new preferences separately from legacy YAML.
    pub fn save(&self) -> Result<()> {
        let saved = SavedPreferences {
            active_database: Some(if self.paths.database.is_absolute() {
                self.paths.database.clone()
            } else {
                std::env::current_dir()?.join(&self.paths.database)
            }),
            language: self.language.clone(),
            playback: self.playback.clone(),
            audition_play_on_single_click: self.audition_play_on_single_click,
            play_history_limit: self.play_history_limit,
            show_album_artwork: self.show_album_artwork,
            pregenerate_list_waveforms: self.pregenerate_list_waveforms,
            hold_list_waveform_batches_after_audition: self
                .hold_list_waveform_batches_after_audition,
            max_search_tasks: normalize_max_search_tasks(self.max_search_tasks),
            ui_scale_percent: normalize_ui_scale_percent(self.ui_scale_percent),
            theme: normalize_theme(&self.theme),
            ui_font_size: clamp_font_size(i64::from(self.ui_font_size)),
            list_font_size: clamp_font_size(i64::from(self.list_font_size)),
            audio_table: self.audio_table.normalized(),
            spectral_peaks: self.spectral_peaks.normalized(),
            window_layout: self.window_layout.normalized(),
            allowed_scan_roots: self.allowed_scan_roots.clone(),
            data_source: {
                // Code that edits `mode` / `server_url` directly keeps working: the
                // profiles follow those fields before they are written.
                let mut data_source = self.persisted_data_source();
                data_source.migrate_profiles();
                data_source.reconcile_local_libraries(
                    &self.paths.database,
                    self.pregenerate_list_waveforms,
                )?;
                data_source
            },
        };
        let data = serde_json::to_vec_pretty(&saved)
            .map_err(|error| Error::Validation(error.to_string()))?;
        let temporary = self
            .paths
            .settings
            .with_extension(format!("tmp-{}", std::process::id()));
        let mut file = std::fs::File::create(&temporary)?;
        use std::io::Write;
        file.write_all(&data)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &self.paths.settings)?;
        Ok(())
    }

    /// Startup guard: open an unusable server selection as local for this session (see
    /// [`DataSourceSettings::server_mode_problem`]) and return the reason to log. The
    /// original `mode` / `active` are remembered so [`AppConfig::save`] keeps them on disk.
    pub fn guard_server_mode(&mut self) -> Option<String> {
        let fallback = DataSourceFallback {
            mode: self.data_source.mode,
            active: self.data_source.active.clone(),
        };
        let problem = self.data_source.guard_server_mode()?;
        self.data_source_fallback = Some(fallback);
        Some(problem)
    }

    /// The data source to write: while the startup fallback is in effect and the user has
    /// not selected a source in this session (still local with the original `active`), the
    /// original `mode` / `active` are written instead of the in-memory local fallback.
    fn persisted_data_source(&self) -> DataSourceSettings {
        let mut data_source = self.data_source.clone();
        if let Some(fallback) = &self.data_source_fallback
            && data_source.mode == DataSourceMode::Local
            && data_source.active == fallback.active
        {
            data_source.mode = fallback.mode;
            data_source.active = fallback.active.clone();
        }
        data_source
    }

    /// Select an existing, already validated library; saving remains an explicit step.
    pub fn select_database(&mut self, path: &Path) -> Result<()> {
        if !path.is_file() {
            return Err(Error::NotFound(format!(
                "Library database {}",
                path.display()
            )));
        }
        let database = path.canonicalize()?;
        let mut updated = self.clone();
        updated.reconcile_local_libraries()?;
        if let Some(id) = updated
            .data_source
            .local_libraries
            .iter()
            .find(|entry| {
                entry
                    .database
                    .canonicalize()
                    .unwrap_or_else(|_| entry.database.clone())
                    == database
            })
            .map(|entry| entry.id.clone())
        {
            updated.select_local_library(&id)?;
        } else {
            updated.paths.database = database;
            updated.paths.legacy_database = None;
            updated.reconcile_local_libraries()?;
        }
        *self = updated;
        Ok(())
    }
}

fn default_root_with_paths(
    override_root: Option<PathBuf>,
    local_root: Option<PathBuf>,
    home: Option<PathBuf>,
    macos: bool,
    current_dir: impl FnOnce() -> Result<PathBuf>,
) -> Result<PathBuf> {
    if let Some(root) = override_root {
        return Ok(root);
    }
    if macos {
        return Ok(macos_user_root(home)?.join("v1"));
    }
    if let Some(root) = local_root {
        return Ok(root.join("AudioLibExplorer").join("v1"));
    }
    Ok(current_dir()?.join(".audiolib-user"))
}

fn macos_user_root(home: Option<PathBuf>) -> Result<PathBuf> {
    let home = home.filter(|path| path.is_absolute()).ok_or_else(|| {
        Error::Validation("macOS requires an absolute HOME or AUDIOLIB_DATA_DIR".into())
    })?;
    Ok(home
        .join("Library")
        .join("Application Support")
        .join("AudioLibExplorer"))
}

#[cfg(any(target_os = "macos", test))]
fn read_macos_package_marker(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(any(target_os = "macos", test))]
fn macos_root_for_executable(
    home: Option<PathBuf>,
    beta_override: Option<PathBuf>,
    executable: &Path,
    binary_version: &str,
) -> Result<PathBuf> {
    let directory = executable
        .parent()
        .ok_or_else(|| Error::Validation("Cannot locate macOS package metadata".into()))?;
    let contents = directory.parent().filter(|contents| {
        directory
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("MacOS"))
            && contents
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("Contents"))
            && contents
                .parent()
                .and_then(Path::extension)
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
    });
    // Only executable code belongs in Contents/MacOS. Resources holds the
    // signed bundle's paired data markers; adjacent stray files cannot select
    // its channel. Standalone CLI/development binaries retain adjacent markers.
    let marker_directory =
        contents.map_or_else(|| directory.to_path_buf(), |p| p.join("Resources"));
    let channel = read_macos_package_marker(&marker_directory.join("update-channel.txt"))?;
    let version = read_macos_package_marker(&marker_directory.join("update-version.txt"))?;
    if contents.is_some() && channel.is_none() && version.is_none() {
        return Err(Error::Validation(
            "macOS application bundle is missing channel/version metadata".into(),
        ));
    }
    macos_root_with_metadata(
        home,
        beta_override,
        channel.as_deref(),
        version.as_deref(),
        binary_version,
    )
}

#[cfg(any(target_os = "macos", test))]
fn macos_root_with_metadata(
    home: Option<PathBuf>,
    beta_override: Option<PathBuf>,
    channel: Option<&str>,
    installed_version: Option<&str>,
    binary_version: &str,
) -> Result<PathBuf> {
    let beta = macos_package_uses_beta(channel, installed_version, binary_version)?;
    if beta && let Some(root) = beta_override {
        return Ok(root);
    }
    Ok(macos_user_root(home)?.join(if beta { "beta" } else { "v1" }))
}

#[cfg(any(target_os = "macos", test))]
fn macos_package_uses_beta(
    channel: Option<&str>,
    installed_version: Option<&str>,
    binary_version: &str,
) -> Result<bool> {
    let invalid =
        || Error::Validation("Invalid or mismatched macOS package channel/version metadata".into());
    let (channel, installed_version) = match (channel, installed_version) {
        (None, None) => return Ok(false),
        (Some(channel), Some(version)) => (channel.trim(), version.trim()),
        _ => return Err(invalid()),
    };
    let installed = semver::Version::parse(installed_version).map_err(|_| invalid())?;
    let binary = semver::Version::parse(binary_version).map_err(|_| invalid())?;
    if (installed.major, installed.minor, installed.patch)
        != (binary.major, binary.minor, binary.patch)
        || !installed.build.is_empty()
    {
        return Err(invalid());
    }
    let prerelease = installed.pre.as_str();
    let candidate_number = prerelease
        .strip_prefix("beta.")
        .or_else(|| prerelease.strip_prefix("rc."));
    match channel {
        "stable" if prerelease.is_empty() => Ok(false),
        "beta"
            if candidate_number.is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
            }) =>
        {
            Ok(true)
        }
        _ => Err(invalid()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spectral_peak_thresholds_round_trip_minima_without_changing_library_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("userdata");
        let mut config = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        let database = config.paths.database.clone();
        config.language = "cn".into();
        config.playback = serde_json::json!({"volume":0.25,"device_id":"existing-device"});
        config.allowed_scan_roots = vec![directory.path().join("sounds")];
        config.spectral_peaks = SpectralPeakPreferences {
            noise: 0.1,
            variance: 0.05,
        };
        config.save().unwrap();
        let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        assert_eq!(loaded.spectral_peaks, config.spectral_peaks);
        assert_eq!(loaded.paths.database, database);
        assert_eq!(loaded.playback, config.playback);
        assert_eq!(loaded.allowed_scan_roots, config.allowed_scan_roots);
        assert_eq!(loaded.language, "cn");

        config.spectral_peaks = SpectralPeakPreferences {
            noise: f32::NAN,
            variance: f32::INFINITY,
        };
        config.save().unwrap();
        let loaded = AppConfig::load_with_roots(root, &[]).unwrap();
        assert_eq!(loaded.spectral_peaks, SpectralPeakPreferences::default());
        assert_eq!(loaded.paths.database, database);
        assert_eq!(loaded.playback, config.playback);
    }

    #[test]
    fn old_settings_and_invalid_spectral_values_retain_other_preferences() {
        let directory = tempfile::tempdir().unwrap();
        for (input, expected) in [
            ("{}", SpectralPeakPreferences::default()),
            (
                r#"{"noise":"bad","variance":null}"#,
                SpectralPeakPreferences::default(),
            ),
            (
                r#"{"noise":-10,"variance":-10}"#,
                SpectralPeakPreferences {
                    noise: 0.1,
                    variance: 0.05,
                },
            ),
            (
                r#"{"noise":100,"variance":100}"#,
                SpectralPeakPreferences {
                    noise: 2.0,
                    variance: 1.0,
                },
            ),
        ] {
            std::fs::write(directory.path().join("settings.json"), format!(
                "{{\"language\":\"cn\",\"playback\":{{\"volume\":0.35}},\"spectral_peaks\":{input}}}"
            )).unwrap();
            let loaded = AppConfig::load_with_roots(directory.path().to_owned(), &[]).unwrap();
            assert_eq!(loaded.spectral_peaks, expected);
            assert_eq!(loaded.language, "cn");
            assert_eq!(loaded.playback["volume"], serde_json::json!(0.35));
        }
    }

    #[test]
    fn explicit_data_root_wins_without_resolving_home_or_working_directory() {
        let explicit = PathBuf::from("existing data ");
        for macos in [false, true] {
            assert_eq!(
                default_root_with_paths(
                    Some(explicit.clone()),
                    Some(PathBuf::from("other")),
                    None,
                    macos,
                    || panic!("explicit data root must not resolve cwd"),
                )
                .unwrap(),
                explicit
            );
        }
    }

    #[test]
    fn macos_data_root_is_per_user_and_independent_of_windows_or_working_directory() {
        let home = std::env::temp_dir().join("owned-home");
        let resolved = default_root_with_paths(
            None,
            Some(home.join("windows-local")),
            Some(home.clone()),
            true,
            || panic!("macOS data must not depend on cwd"),
        )
        .unwrap();
        assert_eq!(
            resolved,
            home.join("Library/Application Support/AudioLibExplorer/v1")
        );
        for invalid_home in [None, Some(PathBuf::from("relative-home"))] {
            assert!(
                default_root_with_paths(None, None, invalid_home, true, || {
                    panic!("missing HOME must not select cwd")
                })
                .is_err()
            );
        }
    }

    #[test]
    fn other_platform_data_roots_retain_localappdata_and_cwd_fallback() {
        let root = PathBuf::from("existing-local");
        assert_eq!(
            default_root_with_paths(None, Some(root.clone()), None, false, || {
                panic!("LOCALAPPDATA must not resolve cwd")
            })
            .unwrap(),
            root.join("AudioLibExplorer/v1")
        );
        assert_eq!(
            default_root_with_paths(None, None, None, false, || Ok(root.clone())).unwrap(),
            root.join(".audiolib-user")
        );
    }

    #[test]
    fn macos_package_metadata_selects_only_matching_stable_beta_or_rc_channels() {
        for (channel, version, beta) in [
            (None, None, false),
            (Some("stable\n"), Some("0.8.3\n"), false),
            (Some("beta\r\n"), Some("0.8.3-beta.1\r\n"), true),
            (Some("beta"), Some("0.8.3-beta.0"), true),
            (Some("beta"), Some("0.8.3-rc.12"), true),
        ] {
            assert_eq!(
                macos_package_uses_beta(channel, version, "0.8.3").unwrap(),
                beta
            );
        }
    }

    #[test]
    fn macos_package_metadata_rejects_partial_malformed_and_mismatched_markers() {
        for (channel, version) in [
            (Some("beta"), None),
            (None, Some("0.8.3-beta.1")),
            (Some(""), Some("0.8.3")),
            (Some("beta"), Some("")),
            (Some("Beta"), Some("0.8.3-beta.1")),
            (Some("stable"), Some("0.8.3-beta.1")),
            (Some("stable"), Some("0.8.3-rc.1")),
            (Some("beta"), Some("0.8.3")),
            (Some("beta"), Some("0.8.3-0")),
            (Some("beta"), Some("0.8.3-alpha.1")),
            (Some("beta"), Some("0.8.3-beta.01")),
            (Some("beta"), Some("0.8.3-rc.1.extra")),
            (Some("beta"), Some("0.8.3-beta.1+other")),
            (Some("stable"), Some("0.8.3+other")),
            (Some("beta"), Some("v0.8.3-beta.1")),
            (Some("beta"), Some("0.8.2-beta.1")),
            (Some("stable"), Some("0.8.4")),
            (Some("beta\nstable"), Some("0.8.3-beta.1")),
            (Some("stable"), Some("not-a-version")),
        ] {
            assert!(
                macos_package_uses_beta(channel, version, "0.8.3").is_err(),
                "accepted {channel:?} / {version:?}"
            );
        }
    }

    #[test]
    fn macos_beta_and_rc_data_roots_are_isolated_from_stable() {
        let home = std::env::temp_dir().join("owned-home");
        let application = home.join("Library/Application Support/AudioLibExplorer");
        for version in ["0.8.3-beta.1", "0.8.3-rc.2"] {
            assert_eq!(
                macos_root_with_metadata(
                    Some(home.clone()),
                    None,
                    Some("beta"),
                    Some(version),
                    "0.8.3",
                )
                .unwrap(),
                application.join("beta")
            );
        }
        let beta_override = PathBuf::from("existing-beta-data ");
        assert_eq!(
            macos_root_with_metadata(
                None,
                Some(beta_override.clone()),
                Some("beta"),
                Some("0.8.3-rc.2"),
                "0.8.3",
            )
            .unwrap(),
            beta_override
        );
        for (channel, version) in [(None, None), (Some("stable"), Some("0.8.3"))] {
            assert_eq!(
                macos_root_with_metadata(
                    Some(home.clone()),
                    Some(beta_override.clone()),
                    channel,
                    version,
                    "0.8.3",
                )
                .unwrap(),
                application.join("v1")
            );
        }
    }

    #[test]
    fn macos_invalid_package_metadata_cannot_select_even_an_explicit_beta_root() {
        assert!(
            macos_root_with_metadata(
                None,
                Some(PathBuf::from("existing-beta-data")),
                Some("beta"),
                Some("0.8.2-beta.1"),
                "0.8.3",
            )
            .is_err()
        );
    }

    #[test]
    fn macos_bundle_reads_resource_markers_and_ignores_adjacent_strays() {
        let temporary = tempfile::tempdir().unwrap();
        let contents = temporary.path().join("AudioLib Explorer.APP/Contents");
        let executable = contents.join("MacOS/AudioLibCli");
        let resources = contents.join("Resources");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::write(resources.join("update-channel.txt"), "beta\n").unwrap();
        std::fs::write(resources.join("update-version.txt"), "0.8.3-beta.1\n").unwrap();
        std::fs::write(
            executable.parent().unwrap().join("update-channel.txt"),
            "stable\n",
        )
        .unwrap();
        std::fs::write(
            executable.parent().unwrap().join("update-version.txt"),
            "0.8.3\n",
        )
        .unwrap();
        let home = temporary.path().join("home");
        assert_eq!(
            macos_root_for_executable(Some(home.clone()), None, &executable, "0.8.3").unwrap(),
            home.join("Library/Application Support/AudioLibExplorer/beta")
        );
        std::fs::write(resources.join("update-version.txt"), "0.8.2-beta.1\n").unwrap();
        assert!(macos_root_for_executable(Some(home), None, &executable, "0.8.3").is_err());
    }

    #[test]
    fn macos_bundle_cannot_fall_back_to_stable_when_resource_markers_are_missing() {
        let temporary = tempfile::tempdir().unwrap();
        let contents = temporary.path().join("AudioLib Explorer.app/Contents");
        let executable = contents.join("MacOS/AudioLibExplorer");
        let resources = contents.join("Resources");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::write(
            executable.parent().unwrap().join("update-channel.txt"),
            "stable\n",
        )
        .unwrap();
        std::fs::write(
            executable.parent().unwrap().join("update-version.txt"),
            "0.8.3\n",
        )
        .unwrap();
        let home = temporary.path().join("home");
        assert!(macos_root_for_executable(Some(home.clone()), None, &executable, "0.8.3").is_err());
        std::fs::write(resources.join("update-channel.txt"), "beta\n").unwrap();
        assert!(macos_root_for_executable(Some(home), None, &executable, "0.8.3").is_err());
    }

    #[test]
    fn macos_unbundled_development_uses_optional_adjacent_markers() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("standalone");
        std::fs::create_dir_all(&directory).unwrap();
        let executable = directory.join("AudioLibCli");
        let home = temporary.path().join("home");
        assert_eq!(
            macos_root_for_executable(Some(home.clone()), None, &executable, "0.8.3").unwrap(),
            home.join("Library/Application Support/AudioLibExplorer/v1")
        );
        std::fs::write(directory.join("update-channel.txt"), "beta\n").unwrap();
        std::fs::write(directory.join("update-version.txt"), "0.8.3-rc.2\n").unwrap();
        assert_eq!(
            macos_root_for_executable(Some(home), None, &executable, "0.8.3").unwrap(),
            temporary
                .path()
                .join("home/Library/Application Support/AudioLibExplorer/beta")
        );
    }

    #[test]
    fn artwork_preference_defaults_for_old_settings_and_round_trips_explicit_choices() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("preferences");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("settings.json"),
            r#"{"language":"cn","playback":{"volume":0.35},"allowed_scan_roots":[]}"#,
        )
        .unwrap();
        let mut config = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        assert!(config.show_album_artwork);
        for enabled in [false, true] {
            config.show_album_artwork = enabled;
            config.save().unwrap();
            let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
            assert_eq!(loaded.show_album_artwork, enabled);
            assert_eq!(loaded.language, "cn");
            assert_eq!(loaded.playback["volume"], serde_json::json!(0.35));
            assert_eq!(loaded.data_root(), root);
        }
    }

    #[test]
    fn client_local_history_limit_defaults_and_round_trips_without_moving_data_root() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("preferences");
        let mut config = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        assert_eq!(config.play_history_limit, PLAY_HISTORY_LIMIT_PER_LIBRARY);
        let alternate = directory.path().join("elsewhere.db");
        std::fs::write(&alternate, []).unwrap();
        config.select_database(&alternate).unwrap();
        for limit in [0, 12_345] {
            config.play_history_limit = limit;
            config.save().unwrap();
            let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
            assert_eq!(loaded.play_history_limit, limit);
            assert_eq!(loaded.data_root(), root);
            assert_eq!(loaded.paths.database, alternate.canonicalize().unwrap());
        }
        std::fs::write(root.join("settings.json"), r#"{"language":"cn"}"#).unwrap();
        assert_eq!(
            AppConfig::load_with_roots(root, &[])
                .unwrap()
                .play_history_limit,
            PLAY_HISTORY_LIMIT_PER_LIBRARY
        );
    }

    #[test]
    fn old_settings_without_field_enable_single_click_audition() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("settings.json"),
            r#"{"language":"cn","playback":{"volume":0.35},"allowed_scan_roots":[]}"#,
        )
        .unwrap();
        let config = AppConfig::load_with_roots(directory.path().to_owned(), &[]).unwrap();
        assert!(config.audition_play_on_single_click);
        // Existing settings.json without the key keeps serde's false default (do not
        // flip upgrades that never wrote the field).
        assert!(!config.pregenerate_list_waveforms);
        assert!(!config.hold_list_waveform_batches_after_audition);
        assert_eq!(config.audio_table, AudioTablePreferences::default());
        assert_eq!(config.spectral_peaks, SpectralPeakPreferences::default());
        assert_eq!(config.language, "cn");
        assert_eq!(config.playback["volume"], serde_json::json!(0.35));
        config.save().unwrap();
        let loaded = AppConfig::load_with_roots(directory.path().to_owned(), &[]).unwrap();
        assert_eq!(loaded.audio_table, AudioTablePreferences::default());
        assert_eq!(loaded.language, config.language);
        assert_eq!(loaded.playback, config.playback);
    }

    #[test]
    fn fresh_install_without_settings_enables_list_waveform_pregeneration() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("userdata");
        assert!(!root.join("settings.json").exists());
        let config = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        assert!(
            config.pregenerate_list_waveforms,
            "brand-new data root defaults pre-generation on"
        );
        assert!(!config.hold_list_waveform_batches_after_audition);
        // First save persists the construction default so later loads stay on.
        config.save().unwrap();
        let loaded = AppConfig::load_with_roots(root, &[]).unwrap();
        assert!(loaded.pregenerate_list_waveforms);
    }

    #[test]
    fn existing_settings_explicit_false_keeps_list_waveform_pregeneration_off() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("settings.json"),
            r#"{"language":"cn","pregenerate_list_waveforms":false}"#,
        )
        .unwrap();
        let config = AppConfig::load_with_roots(directory.path().to_owned(), &[]).unwrap();
        assert!(!config.pregenerate_list_waveforms);
    }

    #[test]
    fn table_preferences_round_trip_without_changing_existing_settings() {
        let directory = tempfile::tempdir().unwrap();
        let library = directory.path().join("selected.db");
        std::fs::write(&library, []).unwrap();
        let root = directory.path().join("userdata");
        let mut config = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        config.select_database(&library).unwrap();
        config.language = "cn".into();
        config.playback = serde_json::json!({"volume":0.35,"device_id":"custom-device"});
        config.audition_play_on_single_click = true;
        config.pregenerate_list_waveforms = true;
        config.hold_list_waveform_batches_after_audition = true;
        config.allowed_scan_roots = vec![directory.path().join("sounds")];
        config.data_source.webdav.username = "existing-user".into();
        config.audio_table = AudioTablePreferences {
            name_width: 452.5,
            duration_width: 90.0,
            format_width: 115.5,
            channels_width: 72.0,
            tags_width: 245.0,
            order_by: SortOrder::Modified,
            descending: true,
        };
        config.save().unwrap();
        let loaded = AppConfig::load_with_roots(root, &[]).unwrap();
        assert_eq!(loaded.audio_table, config.audio_table);
        assert_eq!(loaded.paths.database, library.canonicalize().unwrap());
        assert_eq!(loaded.language, config.language);
        assert_eq!(loaded.playback, config.playback);
        assert_eq!(
            loaded.audition_play_on_single_click,
            config.audition_play_on_single_click
        );
        assert!(loaded.pregenerate_list_waveforms);
        assert!(loaded.hold_list_waveform_batches_after_audition);
        assert_eq!(loaded.allowed_scan_roots, config.allowed_scan_roots);
        let mut expected_sources = config.clone();
        expected_sources.reconcile_local_libraries().unwrap();
        assert_eq!(loaded.data_source, expected_sources.data_source);
    }

    #[test]
    fn search_task_capacity_defaults_round_trips_and_recovers_bad_fields() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("userdata");
        let mut config = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        assert_eq!(config.max_search_tasks, 30);
        for value in [1, 6, 7, 29, 30] {
            config.max_search_tasks = value;
            config.save().unwrap();
            assert_eq!(
                AppConfig::load_with_roots(root.clone(), &[])
                    .unwrap()
                    .max_search_tasks,
                value
            );
        }
        for (stored, expected) in [
            (serde_json::json!(0), 1),
            (serde_json::json!(-9), 1),
            (serde_json::json!(31), 30),
            (serde_json::json!(u32::MAX), 30),
            (serde_json::json!(29.6), 30),
            (serde_json::json!("bad"), 30),
            (serde_json::Value::Null, 30),
        ] {
            std::fs::write(
                &config.paths.settings,
                serde_json::json!({"language":"cn","max_search_tasks":stored}).to_string(),
            )
            .unwrap();
            let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
            assert_eq!(loaded.max_search_tasks, expected);
            assert_eq!(loaded.language, "cn");
        }
        std::fs::write(&config.paths.settings, r#"{"language":"cn"}"#).unwrap();
        assert_eq!(
            AppConfig::load_with_roots(root, &[])
                .unwrap()
                .max_search_tasks,
            30
        );
        for (value, expected) in [(0, 1), (6, 6), (7, 7), (30, 30), (31, 30), (u32::MAX, 30)] {
            assert_eq!(normalize_max_search_tasks(value), expected);
        }
    }

    #[test]
    fn ui_scale_round_trips_and_unsupported_values_follow_the_system() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("userdata");
        let mut config = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        assert_eq!(config.ui_scale_percent, 0);
        for percent in UI_SCALE_PERCENTS {
            config.ui_scale_percent = percent;
            config.save().unwrap();
            let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
            assert_eq!(loaded.ui_scale_percent, percent);
        }
        for unsupported in [1, 99, 110, 201, u32::MAX] {
            config.ui_scale_percent = unsupported;
            config.save().unwrap();
            let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
            assert_eq!(loaded.ui_scale_percent, 0, "{unsupported}");
        }
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config.paths.settings).unwrap()).unwrap();
        raw["ui_scale_percent"] = serde_json::json!(130);
        std::fs::write(&config.paths.settings, raw.to_string()).unwrap();
        assert_eq!(
            AppConfig::load_with_roots(root, &[])
                .unwrap()
                .ui_scale_percent,
            0
        );
    }

    #[test]
    fn window_layout_round_trips_and_bad_values_keep_other_preferences() {
        use crate::window_layout::WindowLayoutPreferences;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("preferences");
        let mut config = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        assert_eq!(config.window_layout, WindowLayoutPreferences::default());
        config.window_layout = WindowLayoutPreferences {
            width: Some(1366.0),
            height: Some(768.0),
            x: Some(-1900),
            y: Some(24),
            maximized: true,
            sidebar_width: Some(250.0),
            inspector_width: Some(380.0),
            inspector_visible: Some(false),
            filters_visible: Some(true),
            folders_section_expanded: Some(false),
            libraries_section_expanded: Some(true),
            folders_section_ratio: Some(0.7),
        };
        config.save().unwrap();
        let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        assert_eq!(loaded.window_layout, config.window_layout);
        // A malformed layout resets only itself, like a malformed theme.
        std::fs::write(
            root.join("settings.json"),
            r#"{"language":"cn","list_font_size":15,"window_layout":"broken"}"#,
        )
        .unwrap();
        let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        assert_eq!(loaded.window_layout, WindowLayoutPreferences::default());
        assert_eq!(
            (loaded.language.as_str(), loaded.list_font_size),
            ("cn", 15)
        );
        std::fs::write(
            root.join("settings.json"),
            r#"{"language":"cn","window_layout":{"width":-5,"height":"x","x":3,"y":4,"maximized":null}}"#,
        )
        .unwrap();
        let loaded = AppConfig::load_with_roots(root, &[]).unwrap();
        assert_eq!(loaded.language, "cn");
        assert_eq!(
            loaded.window_layout.width,
            Some(crate::window_layout::WINDOW_MIN_WIDTH)
        );
        assert_eq!(loaded.window_layout.height, None);
        assert_eq!(
            (loaded.window_layout.x, loaded.window_layout.y),
            (Some(3), Some(4))
        );
        assert!(!loaded.window_layout.maximized);
    }

    #[test]
    fn builtin_themes_round_trip_without_changing_other_preferences() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("userdata");
        let mut config = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        config.language = "cn".into();
        config.ui_font_size = 17;
        config.list_font_size = 19;
        config.ui_scale_percent = 125;
        assert_eq!(BUILTIN_THEMES[0].id, DEFAULT_THEME);
        for (index, entry) in BUILTIN_THEMES.iter().enumerate() {
            assert!(!entry.id.is_empty());
            assert!(!entry.name.is_empty() && !entry.name_cn.is_empty());
            assert!(
                BUILTIN_THEMES[..index]
                    .iter()
                    .all(|other| other.id != entry.id)
            );
            config.theme = entry.id.into();
            config.save().unwrap();
            let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
            assert_eq!(loaded.theme, entry.id);
            assert_eq!(loaded.language, "cn");
            assert_eq!(loaded.ui_font_size, 17);
            assert_eq!(loaded.list_font_size, 19);
            assert_eq!(loaded.ui_scale_percent, 125);
        }
        config.theme = "missing-theme".into();
        config.save().unwrap();
        let loaded = AppConfig::load_with_roots(root, &[]).unwrap();
        assert_eq!(loaded.theme, DEFAULT_THEME);
        assert_eq!(loaded.ui_font_size, 17);
    }

    #[test]
    fn theme_and_font_sizes_default_round_trip_and_clamp() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("userdata");
        let mut config = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        assert_eq!(config.theme, DEFAULT_THEME);
        assert_eq!(config.ui_font_size, 13);
        assert_eq!(config.list_font_size, 13);
        // Settings written before these fields existed load with the defaults.
        config.save().unwrap();
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config.paths.settings).unwrap()).unwrap();
        for key in ["theme", "ui_font_size", "list_font_size"] {
            raw.as_object_mut().unwrap().remove(key);
        }
        raw["language"] = serde_json::json!("cn");
        std::fs::write(&config.paths.settings, raw.to_string()).unwrap();
        let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        assert_eq!(
            (
                loaded.theme.as_str(),
                loaded.ui_font_size,
                loaded.list_font_size
            ),
            (DEFAULT_THEME, 13, 13)
        );
        assert_eq!(loaded.language, "cn");
        // Every supported size round-trips, independently for the two settings.
        for size in FONT_SIZE_MIN..=FONT_SIZE_MAX {
            config.ui_font_size = size;
            config.list_font_size = FONT_SIZE_MAX + FONT_SIZE_MIN - size;
            config.save().unwrap();
            let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
            assert_eq!(loaded.ui_font_size, size);
            assert_eq!(loaded.list_font_size, FONT_SIZE_MAX + FONT_SIZE_MIN - size);
        }
        // Out-of-range in-memory values clamp when saved.
        config.ui_font_size = 0;
        config.list_font_size = 99;
        config.theme = "neon".into();
        config.save().unwrap();
        let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        assert_eq!(
            (
                loaded.theme.as_str(),
                loaded.ui_font_size,
                loaded.list_font_size
            ),
            (DEFAULT_THEME, 10, 24)
        );
        // Out-of-range or malformed stored values clamp or fall back on load.
        for (stored, expected) in [
            (serde_json::json!(-5), 10),
            (serde_json::json!(9), 10),
            (serde_json::json!(25), 24),
            (serde_json::json!(u64::MAX), 24),
            (serde_json::json!(14.6), 15),
            (serde_json::json!("large"), 13),
            (serde_json::json!(null), 13),
        ] {
            let mut raw: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&config.paths.settings).unwrap()).unwrap();
            raw["ui_font_size"] = stored.clone();
            raw["list_font_size"] = stored.clone();
            raw["theme"] = serde_json::json!("neon");
            std::fs::write(&config.paths.settings, raw.to_string()).unwrap();
            let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
            assert_eq!(loaded.ui_font_size, expected, "{stored}");
            assert_eq!(loaded.list_font_size, expected, "{stored}");
            assert_eq!(loaded.theme, DEFAULT_THEME);
        }

        for stored in [
            serde_json::json!(7),
            serde_json::json!(true),
            serde_json::json!(null),
            serde_json::json!({"id": "dark"}),
        ] {
            let mut raw: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&config.paths.settings).unwrap()).unwrap();
            raw["ui_font_size"] = serde_json::json!(16);
            raw["list_font_size"] = serde_json::json!(12);
            raw["theme"] = stored.clone();
            std::fs::write(&config.paths.settings, raw.to_string()).unwrap();
            let loaded = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
            assert_eq!(loaded.theme, DEFAULT_THEME, "{stored}");
            assert_eq!(loaded.ui_font_size, 16, "{stored}");
            assert_eq!(loaded.list_font_size, 12, "{stored}");
        }
    }

    #[test]
    fn malformed_and_extreme_table_widths_recover_without_losing_settings() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("settings.json"),
            r#"{"language":"cn","playback":{"volume":0.35},"audio_table":{
                "name_width":"wide","duration_width":null,"format_width":-15,
                "channels_width":10000000000,"tags_width":{},
                "order_by":"modified","descending":true}}"#,
        )
        .unwrap();
        let config = AppConfig::load_with_roots(directory.path().to_owned(), &[]).unwrap();
        assert_eq!(
            config.audio_table,
            AudioTablePreferences {
                name_width: 360.0,
                duration_width: 80.0,
                format_width: 82.0,
                channels_width: 2000.0,
                tags_width: 160.0,
                order_by: SortOrder::Modified,
                descending: true,
            }
        );
        assert_eq!(config.language, "cn");
        assert_eq!(config.playback["volume"], serde_json::json!(0.35));
        config.save().unwrap();
        let reloaded = AppConfig::load_with_roots(directory.path().to_owned(), &[]).unwrap();
        assert_eq!(reloaded.audio_table, config.audio_table);
    }

    #[test]
    fn table_width_normalization_caps_all_columns_and_nonfinite_values_before_save() {
        let undersized = AudioTablePreferences {
            name_width: -1.0,
            duration_width: -1.0,
            format_width: -1.0,
            channels_width: -1.0,
            tags_width: -1.0,
            ..Default::default()
        }
        .normalized();
        assert_eq!(undersized.name_width, 120.0);
        assert_eq!(undersized.duration_width, 60.0);
        assert_eq!(undersized.format_width, 82.0);
        assert_eq!(undersized.channels_width, 48.0);
        assert_eq!(undersized.tags_width, 80.0);
        let oversized = AudioTablePreferences {
            name_width: f32::MAX,
            duration_width: f32::MAX,
            format_width: f32::MAX,
            channels_width: f32::MAX,
            tags_width: f32::MAX,
            ..Default::default()
        }
        .normalized();
        assert_eq!(oversized.name_width, 2000.0);
        assert_eq!(oversized.duration_width, 2000.0);
        assert_eq!(oversized.format_width, 2000.0);
        assert_eq!(oversized.channels_width, 2000.0);
        assert_eq!(oversized.tags_width, 2000.0);
        let directory = tempfile::tempdir().unwrap();
        let mut config = AppConfig::load_with_roots(directory.path().to_owned(), &[]).unwrap();
        config.audio_table = AudioTablePreferences {
            name_width: f32::NAN,
            duration_width: f32::INFINITY,
            format_width: f32::NEG_INFINITY,
            channels_width: f32::NAN,
            tags_width: f32::INFINITY,
            ..Default::default()
        };
        config.save().unwrap();
        let loaded = AppConfig::load_with_roots(directory.path().to_owned(), &[]).unwrap();
        assert_eq!(loaded.audio_table, AudioTablePreferences::default());
    }

    #[test]
    fn explicitly_saved_single_click_disabled_is_respected() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("settings.json"),
            r#"{"language":"en","playback":{"volume":0.5},"audition_play_on_single_click":false}"#,
        )
        .unwrap();
        let config = AppConfig::load_with_roots(directory.path().to_owned(), &[]).unwrap();
        assert!(!config.audition_play_on_single_click);
    }

    #[test]
    fn single_click_audition_preference_survives_enabling_and_disabling() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_owned();
        let mut config = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
        assert!(config.audition_play_on_single_click);
        config.language = "cn".into();
        config.playback["volume"] = serde_json::json!(0.35);
        for enabled in [false, true, false] {
            config.audition_play_on_single_click = enabled;
            config.save().unwrap();
            config.save().unwrap();
            let saved: serde_json::Value = serde_json::from_slice(
                &std::fs::read(directory.path().join("settings.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(saved["audition_play_on_single_click"], enabled);
            config = AppConfig::load_with_roots(root.clone(), &[]).unwrap();
            assert_eq!(config.audition_play_on_single_click, enabled);
            assert_eq!(config.language, "cn");
            assert_eq!(config.playback["volume"], serde_json::json!(0.35));
        }
    }

    #[test]
    fn selected_library_is_persisted_independently_of_working_directory() {
        let directory = tempfile::tempdir().unwrap();
        let library = directory.path().join("another.db");
        std::fs::write(&library, []).unwrap();
        let mut config = AppConfig::load_in(directory.path().join("userdata")).unwrap();
        config.select_database(&library).unwrap();
        config.save().unwrap();
        let loaded = AppConfig::load_in(directory.path().join("userdata")).unwrap();
        assert_eq!(loaded.paths.database, library.canonicalize().unwrap());
        assert!(loaded.paths.legacy_database.is_none());
    }
    #[test]
    fn old_settings_load_as_local_mode_and_data_source_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("settings.json"),
            r#"{"language":"cn","active_database":null,"allowed_scan_roots":[]}"#,
        )
        .unwrap();
        let mut config = AppConfig::load_in(directory.path().to_owned()).unwrap();
        let mut server_settings = config.data_source.clone();
        server_settings.local_libraries.clear();
        server_settings.active_local_library = None;
        assert_eq!(server_settings, DataSourceSettings::default());
        assert_eq!(config.data_source.mode, DataSourceMode::Local);
        assert_eq!(
            config.data_source.poll_interval_secs,
            DEFAULT_POLL_INTERVAL_SECS
        );
        assert!(!config.data_source.auto_sync);
        config.data_source.poll_interval_secs = 120;
        config.data_source.auto_sync = true;
        config.data_source.mode = DataSourceMode::Server;
        config.data_source.server_url = "http://server.example.lan:8790".into();
        config.data_source.webdav.username = "sound".into();
        config
            .data_source
            .webdav
            .root_urls
            .insert("SFX".into(), "http://nas/dav/SFX".into());
        config.data_source.validate().unwrap();
        config.save().unwrap();
        let saved = std::fs::read_to_string(directory.path().join("settings.json")).unwrap();
        assert!(saved.contains("\"mode\": \"server\""));
        assert!(!saved.contains("token") && !saved.contains("password"));
        let loaded = AppConfig::load_in(directory.path().to_owned()).unwrap();
        // The server set through the old fields was saved as the active profile.
        let mut expected = config.data_source.clone();
        assert_eq!(expected.migrate_profiles(), ProfileMigration::Created);
        assert_eq!(loaded.data_source, expected);
        assert_eq!(
            loaded.data_source.active.as_deref(),
            Some(DEFAULT_PROFILE_ID)
        );
        assert_eq!(
            loaded.data_source.profiles[0].server_url,
            "http://server.example.lan:8790"
        );
        assert_eq!(loaded.language, "cn");
    }

    #[test]
    fn data_source_urls_are_validated() {
        let mut settings = DataSourceSettings {
            mode: DataSourceMode::Server,
            server_url: "ftp://host".into(),
            ..Default::default()
        };
        assert!(settings.validate().is_err());
        settings.server_url = "http://user:pw@host:8790".into();
        assert!(settings.validate().is_err());
        settings.server_url = "https://host:8790/".into();
        assert!(settings.validate().is_ok());
        settings.webdav.base_url = Some("http:// spaced".into());
        assert!(settings.validate().is_err());
        settings.webdav.base_url = None;
        settings.webdav.root_urls.insert("A".into(), String::new());
        assert!(settings.validate().is_ok());
        settings.mode = DataSourceMode::Local;
        settings.server_url.clear();
        assert!(settings.validate().is_ok());
        for (interval, valid) in [
            (0, true),
            (5, false),
            (10, true),
            (86_400, true),
            (86_401, false),
        ] {
            settings.poll_interval_secs = interval;
            assert_eq!(settings.validate().is_ok(), valid, "interval {interval}");
        }
        // A data_source block written by the previous release has no poll fields.
        let old: DataSourceSettings =
            serde_json::from_str(r#"{"mode":"server","server_url":"http://h:8790"}"#).unwrap();
        assert_eq!(old.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
        assert!(!old.auto_sync);
        assert_eq!(old.audition_memory_mb, DEFAULT_AUDITION_MEMORY_MB);
        assert!(!old.audition_original);
        settings.poll_interval_secs = 0;
        for (megabytes, valid) in [
            (16, false),
            (32, true),
            (512, true),
            (8192, true),
            (8193, false),
        ] {
            settings.audition_memory_mb = megabytes;
            assert_eq!(settings.validate().is_ok(), valid, "{megabytes} MiB");
        }
        // Saved URLs of earlier releases stay valid and are kept verbatim (no rewrite).
        let mut legacy = old.clone();
        assert!(legacy.validate().is_ok());
        assert_eq!(
            serde_json::to_value(&legacy).unwrap()["server_url"],
            "http://h:8790"
        );
        legacy.server_url = "https://audiolib.example.com:8443/audiolib".into();
        assert!(legacy.validate().is_ok());
        legacy.server_url = "https://host:99999".into();
        assert!(legacy.validate().is_err());
    }

    #[test]
    fn user_settings_survive_repeated_save_and_override_legacy() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = AppConfig::load_in(directory.path().to_owned()).unwrap();
        config.language = "cn".into();
        config.playback["volume"] = serde_json::json!(0.3);
        config.save().unwrap();
        config.save().unwrap();
        let loaded = AppConfig::load_in(directory.path().to_owned()).unwrap();
        assert_eq!(loaded.language, "cn");
        assert_eq!(loaded.playback["volume"], serde_json::json!(0.3));
    }

    #[test]
    fn saved_rust_settings_and_existing_library_ignore_broken_legacy_yaml() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("legacy");
        std::fs::create_dir_all(legacy.join("config")).unwrap();
        std::fs::write(legacy.join("config/audiolib_config.yaml"), "broken: [").unwrap();
        let established = temp.path().join("saved");
        let mut config = AppConfig::load_with_roots(established.clone(), &[]).unwrap();
        config.language = "cn".into();
        config.playback["volume"] = serde_json::json!(0.4);
        config.save().unwrap();
        let loaded =
            AppConfig::load_with_roots(established, std::slice::from_ref(&legacy)).unwrap();
        assert_eq!(loaded.language, "cn");
        assert_eq!(loaded.playback["volume"], serde_json::json!(0.4));
        assert!(loaded.paths.legacy_database.is_none());
        let existing = temp.path().join("existing");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("library.db"), []).unwrap();
        assert!(AppConfig::load_with_roots(existing, std::slice::from_ref(&legacy)).is_ok());
        assert!(AppConfig::load_with_roots(temp.path().join("first-run"), &[legacy]).is_err());
    }

    #[test]
    fn first_run_still_imports_valid_legacy_paths_and_preferences() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("legacy");
        std::fs::create_dir_all(legacy.join("config")).unwrap();
        std::fs::create_dir_all(legacy.join("data")).unwrap();
        std::fs::write(legacy.join("data/old.db"), []).unwrap();
        std::fs::write(legacy.join("config/audiolib_config.yaml"),"database:\n  path: data/old.db\napplication:\n  language: cn\nplayback:\n  default_volume: 0.35\nsecurity:\n  allowed_scan_roots: []\n").unwrap();
        let config = AppConfig::load_with_roots(
            temp.path().join("first-run"),
            std::slice::from_ref(&legacy),
        )
        .unwrap();
        assert_eq!(
            config.paths.legacy_database,
            Some(legacy.join("data/old.db"))
        );
        assert_eq!(config.language, "cn");
        assert_eq!(config.playback["volume"], serde_json::json!(0.35));
        config.save().unwrap();
        std::fs::write(legacy.join("config/audiolib_config.yaml"), "broken: [").unwrap();
        let loaded = AppConfig::load_with_roots(
            config.paths.settings.parent().unwrap().to_path_buf(),
            &[legacy],
        )
        .unwrap();
        assert_eq!(loaded.playback["volume"], serde_json::json!(0.35));
    }

    #[test]
    fn server_mode_guard_requires_existing_profile_with_valid_url() {
        let mut settings = DataSourceSettings {
            mode: DataSourceMode::Server,
            server_url: "http://192.0.2.10:8790".into(),
            ..Default::default()
        };
        // Never migrated: no active profile at all.
        assert!(settings.clone().guard_server_mode().is_some());
        settings.migrate_profiles();
        assert_eq!(settings.server_mode_problem(), None);
        let mut usable = settings.clone();
        assert_eq!(usable.guard_server_mode(), None);
        assert_eq!(usable.mode, DataSourceMode::Server);

        let mut dangling = settings.clone();
        dangling.active = Some("missing".into());
        let reason = dangling.guard_server_mode().unwrap();
        assert!(reason.contains("does not exist"), "{reason}");
        assert_eq!(dangling.mode, DataSourceMode::Local);
        assert_eq!(dangling.profiles, settings.profiles, "profiles are kept");

        let mut empty = settings.clone();
        empty.profiles[0].server_url = "  ".into();
        assert!(empty.guard_server_mode().unwrap().contains("no server URL"));
        assert_eq!(empty.mode, DataSourceMode::Local);

        let mut invalid = settings.clone();
        invalid.profiles[0].server_url = "htp:/oops".into();
        assert!(invalid.guard_server_mode().unwrap().contains("invalid URL"));
        assert_eq!(invalid.mode, DataSourceMode::Local);

        let mut local = DataSourceSettings::default();
        assert_eq!(local.guard_server_mode(), None);
        assert_eq!(local.mode, DataSourceMode::Local);
    }

    #[test]
    fn guarded_server_mode_is_not_persisted_as_local() {
        let root = std::env::temp_dir().join(format!(
            "audiolib-guard-save-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut config = AppConfig::load_in(root.clone()).unwrap();
        config.data_source.mode = DataSourceMode::Server;
        config.data_source.server_url = "http://192.0.2.10:8790".into();
        config.save().unwrap();
        // Corrupt the active profile's URL on disk, as an older client or hand edit might.
        let mut on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&config.paths.settings).unwrap()).unwrap();
        on_disk["data_source"]["profiles"][0]["server_url"] = "htp:/broken".into();
        on_disk["data_source"]["server_url"] = "htp:/broken".into();
        std::fs::write(
            &config.paths.settings,
            serde_json::to_vec(&on_disk).unwrap(),
        )
        .unwrap();
        let original = AppConfig::load_in(root.clone()).unwrap().data_source;
        assert_eq!(original.mode, DataSourceMode::Server);

        let mut guarded = AppConfig::load_in(root.clone()).unwrap();
        assert!(guarded.guard_server_mode().is_some());
        assert_eq!(guarded.data_source.mode, DataSourceMode::Local);
        // Unrelated preference saves (language, playback, close) keep the server selection.
        guarded.language = "cn".into();
        guarded.save().unwrap();
        let reloaded = AppConfig::load_in(root.clone()).unwrap();
        assert_eq!(reloaded.language, "cn");
        assert_eq!(reloaded.data_source.mode, original.mode);
        assert_eq!(reloaded.data_source.active, original.active);
        assert_eq!(reloaded.data_source.profiles, original.profiles);

        // An explicit source choice in this session is written as chosen.
        guarded
            .data_source
            .set_active(&ActiveSource::Local)
            .unwrap();
        guarded.save().unwrap();
        let chosen = AppConfig::load_in(root.clone()).unwrap();
        assert_eq!(chosen.data_source.mode, DataSourceMode::Local);
        assert_eq!(chosen.data_source.active.as_deref(), Some(ACTIVE_LOCAL));
        assert_eq!(chosen.data_source.profiles, original.profiles);
        if root.parent() == Some(std::env::temp_dir().as_path()) {
            std::fs::remove_dir_all(&root).unwrap();
        }
    }
}
