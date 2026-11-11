//! Client view of a server that is newer, older or migrating its library schema
//! (DESIGN-SEARCH-METADATA-V2 §5.3, §6, §7). Pure decisions only: no I/O, the remote
//! crate gathers the inputs (`/capabilities`, `/health`, response headers).
//!
//! The client never downloads or installs anything here; it only reports which
//! notices to show and whether path search may be offered.
use crate::compatibility;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Response header a newer server adds for an older client (§6, non-blocking).
pub const UPGRADE_AVAILABLE_HEADER: &str = "x-audiolib-upgrade-available";
/// Longest header value accepted; longer values are ignored.
pub const MAX_UPGRADE_HEADER_BYTES: usize = 512;
/// The schema 9 Plan B snapshot format this client recognizes (§3, §6).
/// Re-export of the schema-9 format this client reads (`sd1`).
pub use compatibility::{PLAN_B_V9_AUDIO_FORMAT, PLAN_B_V9_FORMAT};
/// Path search rules this client knows (`sd1`, `search_path: 1`).
pub const SEARCH_PATH_REVISION: u32 = 1;
/// Compatibility feature names introduced by the design (§6).
pub const SEARCH_PATH_FEATURE: &str = "search_path";
pub const VORBIS_TAGS_FEATURE: &str = "vorbis_tags";
pub const VENDOR_METADATA_FEATURE: &str = "vendor_metadata";
/// Ordinary responses during a migration carry the migration state here
/// (agreed name, 11-03).
pub const MIGRATION_HEADER: &str = "x-audiolib-migration";
/// Read-only compatibility alias; accepted only when the canonical header is absent.
pub const SCHEMA_MIGRATION_HEADER: &str = "x-audiolib-schema-migration";
/// Header names read for the migration state, canonical first.
pub const MIGRATION_HEADER_NAMES: [&str; 2] = [MIGRATION_HEADER, SCHEMA_MIGRATION_HEADER];

/// Look up the migration header (canonical first, then the alias) and parse it.
pub fn migration_header_from<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
) -> Option<SchemaMigrationHeader> {
    MIGRATION_HEADER_NAMES
        .iter()
        .find_map(|name| get(name))
        .and_then(parse_schema_migration_header)
}
/// `unavailable` on an "all" scope search answered from the main table only.
pub const PATH_SEARCH_HEADER: &str = "x-audiolib-path-search";
/// Vendor metadata import API root (#214 / #208); subpaths hang off this.
pub const VENDOR_IMPORT_API: &str = "/api/vendor-import";
/// Agreed vendor import limits (#208 / #214): 50 MB / 200k rows / 256 KiB per line.
pub const VENDOR_IMPORT_MAX_FILE_BYTES: u64 = 52_428_800;
pub const VENDOR_IMPORT_MAX_ROWS: u64 = 200_000;
pub const VENDOR_IMPORT_MAX_ROW_TEXT_BYTES: u64 = 262_144;
/// Admin migration status (read by every client that may reach it).
pub const MIGRATION_STATUS_PATH: &str = "/admin/api/migration/status";
/// Admin migration controls (`POST /admin/api/migration/{action}`), manage only.
pub const MIGRATION_ACTIONS: [&str; 5] = ["start", "pause", "resume", "cancel", "rollback"];
/// `503` while a path scope search waits for the migration (`Retry-After: 30`).
pub const MIGRATION_IN_PROGRESS: &str = "migration_in_progress";
/// `503` for writes during the switch (`Retry-After: 30`); nothing was written.
pub const MIGRATION_SWITCHING: &str = "migration_switching";
/// Search scope request parameter: `scope=main|all|path|filename`; omitted means `main`
/// (same behaviour as s8).
pub const SEARCH_SCOPE_PARAM: &str = "scope";
/// 只搜主表 (default; s8 ordering).
pub const SEARCH_SCOPE_MAIN: &str = "main";
/// 「路径」: path-only search (`503 migration_in_progress` while migrating).
pub const SEARCH_SCOPE_PATH: &str = "path";
/// 「全部」: main table plus paths (`x-audiolib-path-search: unavailable` while migrating).
pub const SEARCH_SCOPE_ALL: &str = "all";
/// File-name columns only; excludes notes, tags and embedded/source Metadata.
pub const SEARCH_SCOPE_FILENAME: &str = "filename";

/// Search scope chosen in the UI; `Main` is the default and is sent as no parameter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchScope {
    #[default]
    Main,
    All,
    Path,
    /// Restrict every term to file-name columns.
    Filename,
}

impl SearchScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => SEARCH_SCOPE_MAIN,
            Self::All => SEARCH_SCOPE_ALL,
            Self::Path => SEARCH_SCOPE_PATH,
            Self::Filename => SEARCH_SCOPE_FILENAME,
        }
    }

    /// Query pair to send; `None` for `Main` so s8 servers see an unchanged request.
    pub fn query(self) -> Option<(&'static str, &'static str)> {
        (self != Self::Main).then(|| (SEARCH_SCOPE_PARAM, self.as_str()))
    }

    /// Parses a scope value; missing or empty means `Main`, unknown values are `None`.
    pub fn parse(value: Option<&str>) -> Option<Self> {
        match value.map(str::trim) {
            None | Some("") | Some(SEARCH_SCOPE_MAIN) => Some(Self::Main),
            Some(SEARCH_SCOPE_ALL) => Some(Self::All),
            Some(SEARCH_SCOPE_PATH) => Some(Self::Path),
            Some(SEARCH_SCOPE_FILENAME) => Some(Self::Filename),
            Some(_) => None,
        }
    }
}

/// Server search paging: send `cursor` (opaque, echoed verbatim), read `next_cursor`
/// (`null` on the last page). `offset`/`page` stay accepted; with both, `cursor` wins.
pub const SEARCH_CURSOR_PARAM: &str = "cursor";

/// One page of a server search reply (`{ids, next_cursor}`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchCursorPage {
    #[serde(default)]
    pub ids: Vec<i64>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

impl SearchCursorPage {
    /// Query pair for the next page, `None` after the last page. The cursor is never
    /// parsed or rebuilt; callers URL-encode it.
    pub fn next_query(&self) -> Option<(&'static str, &str)> {
        self.next_cursor
            .as_deref()
            .map(|cursor| (SEARCH_CURSOR_PARAM, cursor))
    }
}

/// Fixed `phases` keys of the status reply.
pub const MIGRATION_PHASES: [&str; 4] = ["backup", "path_build", "optimize", "switch"];

/// Controls offered for the migration: only with manage permission.
pub fn migration_actions(can_manage: bool) -> &'static [&'static str] {
    if can_manage { &MIGRATION_ACTIONS } else { &[] }
}

/// A `503` the server answers because of a migration, from its `error_code`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationRefusal {
    /// Path scope search: show "temporarily unavailable" and retry after the delay.
    PathSearchInProgress,
    /// A write during the switch: retry after the delay; never treat as lost.
    Switching,
}

impl MigrationRefusal {
    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            MIGRATION_IN_PROGRESS => Some(Self::PathSearchInProgress),
            MIGRATION_SWITCHING => Some(Self::Switching),
            _ => None,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::PathSearchInProgress => MIGRATION_IN_PROGRESS,
            Self::Switching => MIGRATION_SWITCHING,
        }
    }

    /// Retry delay when the server sent none (the agreed defaults).
    pub fn default_retry_after(self) -> u64 {
        30
    }

    /// Honor a longer server delay and keep switching retries at least 30 seconds
    /// apart, including peers that still advertise the earlier five-second delay.
    pub fn retry_after(self, seconds: Option<u64>) -> u64 {
        let seconds = seconds.filter(|value| *value > 0).unwrap_or(30);
        match self {
            Self::Switching => seconds.max(30),
            Self::PathSearchInProgress => seconds,
        }
    }

    pub fn text(self, chinese: bool) -> &'static str {
        match (self, chinese) {
            (Self::PathSearchInProgress, true) => {
                "服务器正在升级音效库，路径搜索暂不可用，稍后自动重试。"
            }
            (Self::PathSearchInProgress, false) => {
                "The server is upgrading its sound library; path search is temporarily unavailable and will retry."
            }
            (Self::Switching, true) => "服务器正在切换到新版音效库，修改未写入，稍后自动重试。",
            (Self::Switching, false) => {
                "The server is switching to the upgraded sound library; the change was not written and will retry."
            }
        }
    }
}

/// Highest schema and the tokenization this client understands.
pub const MAX_KNOWN_SCHEMA: u32 = 9;

/// Upgrade advice level from `/capabilities.client_upgrade` or the upgrade header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientUpgradeLevel {
    #[default]
    None,
    Recommended,
    Required,
    #[serde(other)]
    Unknown,
}

impl ClientUpgradeLevel {
    pub fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "" => Self::None,
            "recommended" => Self::Recommended,
            "required" => Self::Required,
            _ => Self::Unknown,
        }
    }

    pub fn needs_notice(self) -> bool {
        matches!(self, Self::Recommended | Self::Required | Self::Unknown)
    }

    pub fn blocks_library(self) -> bool {
        matches!(self, Self::Required)
    }
}

/// `/capabilities.client_upgrade` (#214); absent means no prompt needed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientUpgradeInfo {
    #[serde(default)]
    pub level: ClientUpgradeLevel,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub missing_features: Vec<String>,
    #[serde(default)]
    pub min_client_version: Option<String>,
    #[serde(default)]
    pub served_snapshot_format: Option<String>,
}

/// A parsed `x-audiolib-upgrade-available` header (#214).
/// Forms: `recommended; min=0.9.0; features="a,b"` or a bare version `0.9.0`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeAvailable {
    pub level: ClientUpgradeLevel,
    /// `min=` or a bare version value.
    pub version: Option<String>,
    pub features: Vec<String>,
}

/// Parse semicolon-separated tokens; first bare token is the level or a bare version.
pub fn parse_upgrade_available(value: &str) -> Option<UpgradeAvailable> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_UPGRADE_HEADER_BYTES {
        return None;
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_graphic() || c == ' ' || c == '"')
    {
        return None;
    }
    let mut level = ClientUpgradeLevel::Recommended;
    let mut version = None;
    let mut features = Vec::new();
    let mut saw_token = false;
    for (index, raw) in value.split(';').enumerate() {
        let token = raw.trim().trim_matches('"');
        if token.is_empty() {
            continue;
        }
        saw_token = true;
        if let Some((key, val)) = token.split_once('=') {
            let key = key.trim().to_ascii_lowercase();
            let val = val.trim().trim_matches('"').trim();
            match key.as_str() {
                "min" | "version" => {
                    if !val.is_empty() {
                        version = Some(val.trim_start_matches(['v', 'V']).to_string());
                    }
                }
                "features" => {
                    features = val
                        .split(',')
                        .map(str::trim)
                        .filter(|part| !part.is_empty())
                        .map(str::to_string)
                        .collect();
                }
                "level" => level = ClientUpgradeLevel::parse(val),
                _ => {}
            }
        } else if index == 0 {
            match token.to_ascii_lowercase().as_str() {
                "none" | "recommended" | "required" => {
                    level = ClientUpgradeLevel::parse(token);
                }
                _ => {
                    let version_like = token.trim_start_matches(['v', 'V']);
                    let is_version = version_like.starts_with(|c: char| c.is_ascii_digit())
                        && version_like
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
                    if is_version {
                        version = Some(version_like.to_string());
                    }
                    level = ClientUpgradeLevel::Recommended;
                }
            }
        }
    }
    saw_token.then_some(UpgradeAvailable {
        level,
        version,
        features,
    })
}

/// A parsed `x-audiolib-migration` (or `x-audiolib-schema-migration` alias) header value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaMigrationHeader {
    pub state: MigrationState,
    pub state_label: String,
    pub target: Option<u32>,
    pub done: Option<u64>,
    pub total: Option<u64>,
    pub error: Option<String>,
}

/// Parse `deferred; target=9` / `running; done=1; total=4` / `paused; error=disk_full`.
pub fn parse_schema_migration_header(value: &str) -> Option<SchemaMigrationHeader> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_UPGRADE_HEADER_BYTES {
        return None;
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_graphic() || c == ' ' || c == '"')
    {
        return None;
    }
    let mut state_label = String::new();
    let mut target = None;
    let mut done = None;
    let mut total = None;
    let mut error = None;
    for (index, raw) in value.split(';').enumerate() {
        let token = raw.trim().trim_matches('"');
        if token.is_empty() {
            continue;
        }
        if let Some((key, val)) = token.split_once('=') {
            let key = key.trim().to_ascii_lowercase();
            let val = val.trim().trim_matches('"').trim();
            match key.as_str() {
                "target" => target = val.parse().ok(),
                "done" | "completed" => done = val.parse().ok(),
                "total" => total = val.parse().ok(),
                "error" | "error_code" => {
                    if !val.is_empty() {
                        error = Some(val.to_string());
                    }
                }
                "state" if state_label.is_empty() => state_label = val.to_string(),
                _ => {}
            }
        } else if index == 0 {
            state_label = token.to_string();
        }
    }
    if state_label.is_empty() {
        return None;
    }
    Some(SchemaMigrationHeader {
        state: MigrationState::parse(&state_label),
        state_label,
        target,
        done,
        total,
        error,
    })
}

/// Segments of a snapshot format ID such as `ale-s9-cld1-t3-m2-p1-a1-sd1`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatId {
    pub schema: u32,
    pub tokenization: u32,
    /// `sd{n}` (directory-segment) or legacy `sp{n}`; absent before schema 9.
    pub search_path: Option<u32>,
}

/// Parse the segments this client reasons about; `None` for anything malformed.
pub fn parse_format_id(id: &str) -> Option<FormatId> {
    let mut parts = id.split('-');
    if parts.next()? != "ale" {
        return None;
    }
    let number = |part: &str, prefix: &str| -> Option<u32> {
        let digits = part.strip_prefix(prefix)?;
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .then(|| digits.parse().ok())
            .flatten()
    };
    let schema = number(parts.next()?, "s")?;
    let mut tokenization = None;
    let mut search_path = None;
    for part in parts {
        if part.is_empty() {
            return None;
        }
        if let Some(value) = number(part, "sd").or_else(|| number(part, "sp")) {
            if search_path.replace(value).is_some() {
                return None;
            }
        } else if let Some(value) = number(part, "t") {
            if tokenization.replace(value).is_some() {
                return None;
            }
        } else if !part.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
    }
    Some(FormatId {
        schema,
        tokenization: tokenization?,
        search_path,
    })
}

/// How this client relates to one snapshot format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatSupport {
    /// A format this build reads today.
    Readable,
    /// The schema 9 `sd1` format: recognized, path search rules known.
    KnownPathSearch,
    /// Main tables known but newer path search rules (`sd2`+): never query them with
    /// `sd1` rules.
    NewerPathRules,
    /// Unknown schema/tokenization major or malformed: incompatible.
    Incompatible,
}

pub fn format_support(id: &str) -> FormatSupport {
    if matches!(id, PLAN_B_V9_FORMAT | PLAN_B_V9_AUDIO_FORMAT) {
        return FormatSupport::KnownPathSearch;
    }
    let manifest = compatibility::compatibility_manifest();
    if manifest
        .snapshot_read_formats
        .iter()
        .any(|known| known == id)
    {
        return FormatSupport::Readable;
    }
    match parse_format_id(id) {
        Some(format)
            if format.schema == MAX_KNOWN_SCHEMA
                && format.tokenization == compatibility::TOKENIZATION_REVISION
                && format
                    .search_path
                    .is_some_and(|sp| sp > SEARCH_PATH_REVISION) =>
        {
            FormatSupport::NewerPathRules
        }
        _ => FormatSupport::Incompatible,
    }
}

/// `/capabilities.library_schema` (§6).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibrarySchemaInfo {
    #[serde(default)]
    pub current: Option<u32>,
    #[serde(default)]
    pub source_search_rev: Option<u32>,
    #[serde(default)]
    pub snapshot_formats_served: Vec<String>,
}

/// Migration state values (agreed list); unknown values are kept as `Unknown`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationState {
    #[default]
    Idle,
    Deferred,
    Preflight,
    BackingUp,
    Running,
    Paused,
    Switching,
    Done,
    Failed,
    Cancelled,
    RolledBack,
    #[serde(other)]
    Unknown,
}

impl MigrationState {
    /// Show progress (unknown values too: tolerant).
    pub fn in_progress(self) -> bool {
        matches!(
            self,
            Self::Preflight
                | Self::BackingUp
                | Self::Running
                | Self::Paused
                | Self::Switching
                | Self::Unknown
        )
    }

    /// Tolerant parse of the state text.
    pub fn parse(value: &str) -> Self {
        match value.trim() {
            "idle" | "" => Self::Idle,
            "deferred" => Self::Deferred,
            "preflight" => Self::Preflight,
            "backing_up" => Self::BackingUp,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "switching" => Self::Switching,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "rolled_back" => Self::RolledBack,
            _ => Self::Unknown,
        }
    }

    /// Ended without switching: the previous library stays in use.
    pub fn failed(self) -> bool {
        matches!(self, Self::Failed | Self::Cancelled | Self::RolledBack)
    }
}

/// User-facing name of a state text; unknown values are shown as sent.
pub fn migration_state_text(state: &str, chinese: bool) -> String {
    let known = match (MigrationState::parse(state), chinese) {
        (MigrationState::Unknown, _) => None,
        (MigrationState::Idle, true) => Some("未开始"),
        (MigrationState::Deferred, true) => Some("已推迟"),
        (MigrationState::Preflight, true) => Some("预检中"),
        (MigrationState::BackingUp, true) => Some("正在备份"),
        (MigrationState::Running, true) => Some("正在迁移"),
        (MigrationState::Paused, true) => Some("已暂停"),
        (MigrationState::Switching, true) => Some("正在切换"),
        (MigrationState::Done, true) => Some("已完成"),
        (MigrationState::Failed, true) => Some("失败"),
        (MigrationState::Cancelled, true) => Some("已取消"),
        (MigrationState::RolledBack, true) => Some("已回滚"),
        (MigrationState::Idle, false) => Some("not started"),
        (MigrationState::Deferred, false) => Some("deferred"),
        (MigrationState::Preflight, false) => Some("preflight checks"),
        (MigrationState::BackingUp, false) => Some("backing up"),
        (MigrationState::Running, false) => Some("migrating"),
        (MigrationState::Paused, false) => Some("paused"),
        (MigrationState::Switching, false) => Some("switching"),
        (MigrationState::Done, false) => Some("done"),
        (MigrationState::Failed, false) => Some("failed"),
        (MigrationState::Cancelled, false) => Some("cancelled"),
        (MigrationState::RolledBack, false) => Some("rolled back"),
    };
    known.map_or_else(|| state.trim().to_string(), str::to_string)
}

/// Hint for a migration `error_code` (agreed list); unknown codes are shown as sent.
pub fn migration_error_text(code: &str, chinese: bool) -> String {
    let known = match (code.trim(), chinese) {
        ("insufficient_space", true) => "磁盘空间不足：迁移需要约音效库 1.3 倍的可用空间。",
        ("locked", true) => "音效库正被其他程序占用，请关闭后重试。",
        ("already_running", true) => "迁移已在进行中。",
        ("backup_failed", true) => "备份失败，原音效库未改动。",
        ("integrity_failed", true) => "完整性校验失败，已保留原音效库。",
        ("io_error", true) => "读写出错，请检查磁盘或网络存储。",
        ("not_migrated", true) => "音效库尚未迁移。",
        ("disk_full", true) => {
            "磁盘已满，迁移已暂停并保留检查点。请腾出空间后点「继续」从断点接着迁移。"
        }
        ("read_only", true) => {
            "音效库不可写，迁移在预检阶段失败，原音效库未改动。请检查权限或只读挂载后重新开始。"
        }
        ("insufficient_space", false) => {
            "Not enough disk space: the migration needs about 1.3 times the library size free."
        }
        ("locked", false) => "The library is in use by another program; close it and retry.",
        ("already_running", false) => "A migration is already running.",
        ("backup_failed", false) => "The backup failed; the library was not changed.",
        ("integrity_failed", false) => "The integrity check failed; the previous library is kept.",
        ("io_error", false) => "A read or write failed; check the disk or network storage.",
        ("not_migrated", false) => "The library has not been migrated yet.",
        ("disk_full", false) => {
            "The disk is full; the migration paused and kept its checkpoint. Free space, then choose Resume to continue."
        }
        ("read_only", false) => {
            "The library is not writable; the migration failed at preflight and the library was not changed. Check permissions or a read-only mount, then start again."
        }
        (other, true) => return format!("错误代码：{other}"),
        (other, false) => return format!("error code: {other}"),
    };
    known.to_string()
}

/// `/capabilities.schema_migration` (§6).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaMigrationInfo {
    #[serde(default)]
    pub state: MigrationState,
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// `/capabilities.vendor_import` (§6); limits are the server's, never assumed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VendorImportInfo {
    /// Same-origin absolute route prefix. The import handlers are not implemented
    /// by server PR #225, so deployments may advertise their actual route here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub revision: u32,
    pub max_file_bytes: u64,
    pub max_rows: u64,
    /// Max bytes of one row's text; omitted on older servers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_row_text_bytes: Option<u64>,
}

/// `/health.upgrade`: `{phase, completed?, total?}` mirrored for every client.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthUpgrade {
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub completed: Option<u64>,
    #[serde(default)]
    pub total: Option<u64>,
}

/// `progress` of the migration status reply.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationCounts {
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub total: u64,
}

/// One committed batch of the migration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MigrationBatch {
    #[serde(default)]
    pub rows: u64,
    #[serde(default)]
    pub ms: f64,
}

/// `switch` of the status reply (`source_search_rev` 2 before, 3 after).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationSwitch {
    #[serde(default)]
    pub source_search_rev: Option<u32>,
    #[serde(default)]
    pub user_version: Option<u32>,
    #[serde(default)]
    pub features: Vec<String>,
}

/// `GET /admin/api/migration/status` (same shape as the smv2 harness report).
/// `state` is kept as text: its value set is not final.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MigrationStatus {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub progress: Option<MigrationCounts>,
    /// Milliseconds per phase (`backup`, `path_build`, `optimize`, `switch`).
    #[serde(default)]
    pub phases: BTreeMap<String, f64>,
    #[serde(default)]
    pub switch: Option<MigrationSwitch>,
    #[serde(default)]
    pub batches: Vec<MigrationBatch>,
    #[serde(default)]
    pub eta_s: Option<f64>,
}

impl MigrationStatus {
    pub fn kind(&self) -> MigrationState {
        MigrationState::parse(&self.state)
    }

    /// Current phase: from the state, else the last phase with a recorded duration
    /// (all four `phases` keys are always present, zero until reached).
    pub fn phase(&self) -> Option<String> {
        let from_state = match self.kind() {
            MigrationState::BackingUp => Some("backup"),
            MigrationState::Running => Some("path_build"),
            MigrationState::Switching => Some("switch"),
            _ => None,
        };
        from_state
            .or_else(|| {
                MIGRATION_PHASES
                    .iter()
                    .rev()
                    .find(|phase| self.phases.get(**phase).is_some_and(|ms| *ms > 0.0))
                    .copied()
            })
            .map(str::to_string)
    }
}

/// Everything the evaluation looks at, gathered by the caller.
#[derive(Clone, Debug, Default)]
pub struct ServerUpgradeInput {
    pub server_version: Option<String>,
    /// `/capabilities` was missing or unreadable: treat as an older server (VN-04).
    pub capabilities_known: bool,
    pub features: BTreeMap<String, u32>,
    pub library_schema: Option<LibrarySchemaInfo>,
    pub schema_migration: Option<SchemaMigrationInfo>,
    pub vendor_import: Option<VendorImportInfo>,
    pub health_upgrade: Option<HealthUpgrade>,
    pub admin_migration: Option<MigrationStatus>,
    /// Last schema-migration / legacy migration header seen on a response.
    pub migration_header: Option<SchemaMigrationHeader>,
    /// `/capabilities.client_upgrade`, when the server asks for a client update.
    pub client_upgrade: Option<ClientUpgradeInfo>,
    /// An "all" scope search came back with `x-audiolib-path-search: unavailable`.
    pub path_search_header_unavailable: bool,
    /// Format IDs actually observed for the snapshot in use (library descriptor and
    /// snapshot response header); disagreement disables path search (VN-12).
    pub observed_formats: Vec<String>,
    pub upgrade_available: Option<UpgradeAvailable>,
    pub can_manage: bool,
}

/// Whether path search may be offered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PathSearch {
    Available,
    /// Older or migrating server: hidden silently (§6, §7.2 "only when clicked").
    NotOffered,
    /// The server uses newer path rules than this client: hidden, prompt upgrade.
    NeedsClientUpgrade,
    /// The server is migrating: path scope searches answer `503`, retry later.
    PausedForMigration,
}

/// A migration the status bar shows (§7.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationProgress {
    pub state: MigrationState,
    /// The raw state text (shown for values this client does not know).
    pub state_label: String,
    /// `error_code` of the status reply (e.g. `disk_full` while paused).
    pub error_code: Option<String>,
    pub phase: Option<String>,
    /// `completed / total`, when both are known.
    pub percent: Option<u8>,
}

/// Notices, each shown at most once per session (keyed by [`Notice::key`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Notice {
    /// §7.6: the server advertised a newer client.
    ClientUpgradeAvailable {
        version: Option<String>,
        level: ClientUpgradeLevel,
    },
    /// VN-12: newer path rules; main search keeps working.
    PathSearchNeedsUpgrade,
    /// §7.3: migration in progress.
    Migrating { percent: Option<u8> },
    /// Paused with a reason the user can act on (e.g. `disk_full`: free space, resume).
    MigrationPaused { error_code: String },
    /// §7.2: nothing the server offers is readable.
    Incompatible { server_version: Option<String> },
    /// The migration failed or was rolled back; the old library stays in use.
    MigrationFailed {
        state: String,
        error_code: Option<String>,
    },
}

/// True when `current` parses as a SemVer at or above `minimum`.
/// An unparsable side does not count as meeting the minimum.
fn client_version_meets(current: &str, minimum: &str) -> bool {
    match (
        semver::Version::parse(current.trim()),
        semver::Version::parse(minimum.trim()),
    ) {
        (Ok(current), Ok(minimum)) => current >= minimum,
        _ => false,
    }
}

impl Notice {
    /// Session de-duplication key (progress changes do not re-show a notice).
    pub fn key(&self) -> &'static str {
        match self {
            Self::ClientUpgradeAvailable { .. } => "client-upgrade",
            Self::PathSearchNeedsUpgrade => "path-search-upgrade",
            Self::Migrating { .. } => "migrating",
            Self::MigrationPaused { .. } => "migration-paused",
            Self::Incompatible { .. } => "incompatible",
            Self::MigrationFailed { .. } => "migration-failed",
        }
    }

    /// Offer the existing "check for updates" action (never auto-install).
    pub fn offers_update_check(&self) -> bool {
        !matches!(
            self,
            Self::Migrating { .. } | Self::MigrationPaused { .. } | Self::MigrationFailed { .. }
        )
    }

    /// Same as [`Self::offers_update_check`], except a client already at or newer
    /// than an advertised `min_client_version` is not prompted.
    pub fn offers_update_check_for(&self, current_version: &str) -> bool {
        if !self.offers_update_check() {
            return false;
        }
        match self {
            Self::ClientUpgradeAvailable {
                version: Some(minimum),
                ..
            } => !client_version_meets(current_version, minimum),
            _ => true,
        }
    }

    pub fn text(&self, chinese: bool) -> String {
        match (self, chinese) {
            (Self::ClientUpgradeAvailable { level, .. }, true) if level.blocks_library() => {
                "服务器版本较新，当前客户端无法继续使用该音效库。请更新客户端。".into()
            }
            (Self::ClientUpgradeAvailable { .. }, true) => {
                "服务器已升级，更新客户端可使用路径搜索和厂商资料".into()
            }
            (Self::ClientUpgradeAvailable { level, .. }, false) if level.blocks_library() => {
                "The server is newer; this client can no longer use its sound library. Please update the client."
                    .into()
            }
            (Self::ClientUpgradeAvailable { version, .. }, false) => match version {
                Some(version) => format!(
                    "The server was upgraded; update the client ({version}) to use path search and vendor metadata"
                ),
                None => {
                    "The server was upgraded; update the client to use path search and vendor metadata"
                        .into()
                }
            },
            (Self::PathSearchNeedsUpgrade, true) => {
                "服务器的路径搜索规则较新，当前客户端已关闭路径搜索，主搜索不受影响。请更新客户端。".into()
            }
            (Self::PathSearchNeedsUpgrade, false) => {
                "The server uses newer path search rules; path search is off and main search still works. Please update the client.".into()
            }
            (Self::Migrating { percent }, true) => format!(
                "服务器正在升级音效库（{}），暂时使用本地缓存。",
                percent.map_or("准备中".into(), |p| format!("{p}%"))
            ),
            (Self::Migrating { percent }, false) => format!(
                "The server is upgrading its sound library ({}); using the local cache for now.",
                percent.map_or("preparing".into(), |p| format!("{p}%"))
            ),
            (Self::Incompatible { server_version }, true) => format!(
                "服务器版本较新（{}），当前客户端无法读取它的音效库。请更新客户端。",
                server_version.as_deref().unwrap_or("未知")
            ),
            (Self::MigrationPaused { error_code }, chinese) => format!(
                "{}{}",
                if chinese { "服务器音效库升级已暂停：" } else { "The server's library upgrade paused: " },
                migration_error_text(error_code, chinese)
            ),
            (Self::MigrationFailed { state, error_code }, true) => format!(
                "服务器音效库升级{}，仍在使用原音效库。{}",
                migration_state_text(state, true),
                error_code
                    .as_deref()
                    .map(|code| migration_error_text(code, true))
                    .unwrap_or_default()
            ),
            (Self::MigrationFailed { state, error_code }, false) => format!(
                "The server's library upgrade {}; the previous library stays in use. {}",
                migration_state_text(state, false),
                error_code
                    .as_deref()
                    .map(|code| migration_error_text(code, false))
                    .unwrap_or_default()
            ),
            (Self::Incompatible { server_version }, false) => format!(
                "The server is newer ({}); this client cannot read its sound library. Please update the client.",
                server_version.as_deref().unwrap_or("unknown")
            ),
        }
    }
}

/// §7.2 explanation shown only when the user clicks a hidden feature on an old server.
pub fn old_server_explanation(chinese: bool) -> &'static str {
    if chinese {
        "服务器版本较旧，部分新功能（路径搜索、厂商资料导入）不可用。"
    } else {
        "The server is older; some new features (path search, vendor metadata import) are unavailable."
    }
}

/// Outcome for one server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerUpgradeStatus {
    pub path_search: PathSearch,
    /// Main search stays available unless the server is incompatible.
    pub main_search: bool,
    /// Vendor import limits, only when offered and this request may manage.
    pub vendor_import: Option<VendorImportInfo>,
    pub migration: Option<MigrationProgress>,
    /// An "all" scope result lacks path matches (mark it in the results).
    pub path_results_absent: bool,
    pub notices: Vec<Notice>,
}

impl ServerUpgradeStatus {
    pub fn incompatible(&self) -> bool {
        self.notices
            .iter()
            .any(|notice| matches!(notice, Notice::Incompatible { .. }))
    }
}

fn percent(completed: Option<u64>, total: Option<u64>) -> Option<u8> {
    match (completed, total) {
        (Some(completed), Some(total)) if total > 0 => {
            Some((completed.min(total).saturating_mul(100) / total) as u8)
        }
        _ => None,
    }
}

/// Decide notices and feature availability (§6, §7, VN-04/06/10/12).
pub fn evaluate(input: &ServerUpgradeInput) -> ServerUpgradeStatus {
    let mut notices = Vec::new();
    let served: Vec<&str> = input
        .library_schema
        .iter()
        .flat_map(|schema| schema.snapshot_formats_served.iter())
        .map(|id| id.trim_start_matches("...").trim())
        .filter(|id| !id.is_empty())
        .collect();
    let supports: Vec<FormatSupport> = served.iter().map(|id| format_support(id)).collect();
    let upgrade_required = input
        .client_upgrade
        .as_ref()
        .is_some_and(|info| info.level.blocks_library())
        || input
            .upgrade_available
            .as_ref()
            .is_some_and(|upgrade| upgrade.level.blocks_library());
    let incompatible = upgrade_required
        || (!supports.is_empty()
            && supports
                .iter()
                .all(|support| *support == FormatSupport::Incompatible))
        || input
            .observed_formats
            .iter()
            .any(|id| format_support(id) == FormatSupport::Incompatible);

    let feature = input.features.get(SEARCH_PATH_FEATURE).copied();
    let newer_rules = feature.is_some_and(|rev| rev > SEARCH_PATH_REVISION)
        || input
            .observed_formats
            .iter()
            .any(|id| format_support(id) == FormatSupport::NewerPathRules)
        || (!supports.is_empty()
            && supports.contains(&FormatSupport::NewerPathRules)
            && !supports.contains(&FormatSupport::KnownPathSearch));
    let consistent_v9 = input.observed_formats.first().is_some_and(|first| {
        matches!(first.as_str(), PLAN_B_V9_FORMAT | PLAN_B_V9_AUDIO_FORMAT)
            && input.observed_formats.iter().all(|id| id == first)
    });
    let path_search = if incompatible {
        PathSearch::NotOffered
    } else if newer_rules {
        PathSearch::NeedsClientUpgrade
    } else if input.capabilities_known && feature == Some(SEARCH_PATH_REVISION) && consistent_v9 {
        PathSearch::Available
    } else {
        PathSearch::NotOffered
    };

    let migration = {
        let capability = input
            .schema_migration
            .as_ref()
            .map(|migration| migration.state);
        let interesting =
            |state: MigrationState| state.in_progress() || state == MigrationState::Deferred;
        let header = input
            .migration_header
            .as_ref()
            .filter(|header| interesting(header.state));
        if let Some(admin) = input
            .admin_migration
            .as_ref()
            .filter(|m| interesting(m.kind()))
        {
            Some(MigrationProgress {
                state: admin.kind(),
                state_label: admin.state.clone(),
                error_code: admin.error_code.clone(),
                phase: admin.phase(),
                percent: admin
                    .progress
                    .as_ref()
                    .and_then(|p| percent(Some(p.done), Some(p.total))),
            })
        } else if header.is_some()
            || capability.is_some_and(interesting)
            || input.health_upgrade.is_some()
        {
            let health = input.health_upgrade.clone().unwrap_or_default();
            if let Some(header) = header {
                Some(MigrationProgress {
                    state: header.state,
                    state_label: header.state_label.clone(),
                    error_code: header.error.clone(),
                    phase: health.phase.clone(),
                    percent: percent(
                        header.done.or(health.completed),
                        header.total.or(health.total),
                    ),
                })
            } else {
                let state = capability.unwrap_or(MigrationState::Running);
                let label = serde_json::to_value(state)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                Some(MigrationProgress {
                    state,
                    state_label: label,
                    error_code: None,
                    phase: health.phase,
                    percent: percent(health.completed, health.total),
                })
            }
        } else {
            None
        }
    };
    let path_search = if path_search == PathSearch::Available
        && (migration.is_some() || input.path_search_header_unavailable)
    {
        PathSearch::PausedForMigration
    } else {
        path_search
    };
    let failed = input
        .admin_migration
        .as_ref()
        .filter(|status| status.kind().failed());

    if incompatible {
        notices.push(Notice::Incompatible {
            server_version: input.server_version.clone(),
        });
    } else {
        if let Some(migration) = &migration {
            if migration.state.in_progress() {
                notices.push(Notice::Migrating {
                    percent: migration.percent,
                });
            }
            if let Some(code) = migration
                .error_code
                .clone()
                .filter(|_| migration.state == MigrationState::Paused)
            {
                notices.push(Notice::MigrationPaused { error_code: code });
            }
        } else if let Some(failed) = failed {
            notices.push(Notice::MigrationFailed {
                state: failed.state.clone(),
                error_code: failed.error_code.clone(),
            });
        }
        if path_search == PathSearch::NeedsClientUpgrade {
            notices.push(Notice::PathSearchNeedsUpgrade);
        }
        let from_capabilities = input
            .client_upgrade
            .as_ref()
            .filter(|info| info.level.needs_notice());
        let from_header = input
            .upgrade_available
            .as_ref()
            .filter(|upgrade| upgrade.level.needs_notice());
        if let Some(info) = from_capabilities {
            if info.level.blocks_library() {
                if !notices
                    .iter()
                    .any(|existing| matches!(existing, Notice::Incompatible { .. }))
                {
                    notices.push(Notice::Incompatible {
                        server_version: info.min_client_version.clone(),
                    });
                }
            } else {
                notices.push(Notice::ClientUpgradeAvailable {
                    version: info.min_client_version.clone(),
                    level: info.level,
                });
            }
        } else if let Some(upgrade) = from_header {
            if upgrade.level.blocks_library() {
                if !notices
                    .iter()
                    .any(|existing| matches!(existing, Notice::Incompatible { .. }))
                {
                    notices.push(Notice::Incompatible {
                        server_version: upgrade.version.clone(),
                    });
                }
            } else {
                notices.push(Notice::ClientUpgradeAvailable {
                    version: upgrade.version.clone(),
                    level: upgrade.level,
                });
            }
        }
    }
    let vendor_import = input
        .vendor_import
        .clone()
        .filter(|_| input.can_manage && !incompatible)
        .filter(|_| input.features.get(VENDOR_METADATA_FEATURE) == Some(&1));
    ServerUpgradeStatus {
        path_search,
        main_search: !incompatible,
        vendor_import,
        migration,
        path_results_absent: input.path_search_header_unavailable,
        notices,
    }
}

/// Session memory: each notice key is shown once per session (VN-06).
#[derive(Clone, Debug, Default)]
pub struct NoticeSession {
    shown: std::collections::BTreeSet<&'static str>,
}

impl NoticeSession {
    /// Notices not shown yet this session; marks them shown. Migration progress is
    /// always returned so the status bar can update its percentage.
    pub fn take<'a>(&mut self, status: &'a ServerUpgradeStatus) -> Vec<&'a Notice> {
        status
            .notices
            .iter()
            .filter(|notice| {
                matches!(notice, Notice::Migrating { .. }) || self.shown.insert(notice.key())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v9_input(served: &[&str], observed: &[&str]) -> ServerUpgradeInput {
        ServerUpgradeInput {
            server_version: Some("0.9.0".into()),
            capabilities_known: true,
            features: BTreeMap::from([(SEARCH_PATH_FEATURE.into(), 1)]),
            library_schema: Some(LibrarySchemaInfo {
                current: Some(9),
                source_search_rev: Some(1),
                snapshot_formats_served: served.iter().map(|s| s.to_string()).collect(),
            }),
            observed_formats: observed.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn search_cursor_is_opaque_and_null_ends_paging() {
        let page: SearchCursorPage =
            serde_json::from_str(r#"{"ids":[3,1],"next_cursor":"b64:+/=?&x"}"#).unwrap();
        assert_eq!(page.ids, vec![3, 1]);
        assert_eq!(page.next_query(), Some(("cursor", "b64:+/=?&x")));
        let last: SearchCursorPage =
            serde_json::from_str(r#"{"ids":[],"next_cursor":null}"#).unwrap();
        assert_eq!(last.next_query(), None);
    }

    #[test]
    fn upgrade_header_parsing() {
        assert_eq!(
            parse_upgrade_available(" 0.9.2 "),
            Some(UpgradeAvailable {
                level: ClientUpgradeLevel::Recommended,
                version: Some("0.9.2".into()),
                features: vec![],
            })
        );
        assert_eq!(
            parse_upgrade_available("v1.0.0-beta.1")
                .unwrap()
                .version
                .as_deref(),
            Some("1.0.0-beta.1")
        );
        let structured = parse_upgrade_available(
            r#"recommended; min=0.9.0; features="search_path,vendor_metadata,vorbis_tags""#,
        )
        .unwrap();
        assert_eq!(structured.level, ClientUpgradeLevel::Recommended);
        assert_eq!(structured.version.as_deref(), Some("0.9.0"));
        assert_eq!(
            structured.features,
            vec!["search_path", "vendor_metadata", "vorbis_tags"]
        );
        assert_eq!(
            parse_upgrade_available("required; min=0.9.0")
                .unwrap()
                .level,
            ClientUpgradeLevel::Required
        );
        assert_eq!(parse_upgrade_available("yes").unwrap().version, None);
        assert_eq!(parse_upgrade_available(""), None);
        assert_eq!(parse_upgrade_available(&"9".repeat(513)), None);
    }

    #[test]
    fn migration_header_canonical_and_alias_names() {
        assert_eq!(MIGRATION_HEADER, "x-audiolib-migration");
        assert_eq!(SCHEMA_MIGRATION_HEADER, "x-audiolib-schema-migration");
        let lookup = |pairs: &'static [(&'static str, &'static str)]| {
            migration_header_from(move |name| {
                pairs
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| *v)
            })
        };
        let canonical = lookup(&[("x-audiolib-migration", "running; done=1; total=4")]).unwrap();
        assert_eq!(canonical.state, MigrationState::Running);
        let alias = lookup(&[("x-audiolib-schema-migration", "deferred; target=9")]).unwrap();
        assert_eq!(alias.state, MigrationState::Deferred);
        assert_eq!(alias.target, Some(9));
        let both = lookup(&[
            ("x-audiolib-schema-migration", "deferred"),
            ("x-audiolib-migration", "switching"),
        ])
        .unwrap();
        assert_eq!(both.state_label, "switching");
        assert_eq!(lookup(&[("x-other", "running")]), None);
    }

    #[test]
    fn schema_migration_header_parsing() {
        let deferred = parse_schema_migration_header("deferred; target=9").unwrap();
        assert_eq!(deferred.state, MigrationState::Deferred);
        assert_eq!(deferred.target, Some(9));
        let running =
            parse_schema_migration_header("running; target=9; done=12000; total=20248").unwrap();
        assert_eq!(running.state, MigrationState::Running);
        assert_eq!(running.done, Some(12_000));
        assert_eq!(running.total, Some(20_248));
        assert_eq!(percent(running.done, running.total), Some(59));
        let paused = parse_schema_migration_header("paused; error=disk_full").unwrap();
        assert_eq!(paused.state, MigrationState::Paused);
        assert_eq!(paused.error.as_deref(), Some("disk_full"));
        let failed = parse_schema_migration_header("failed; error=read_only").unwrap();
        assert_eq!(failed.state, MigrationState::Failed);
        assert_eq!(failed.error.as_deref(), Some("read_only"));
        assert_eq!(parse_schema_migration_header(""), None);
    }

    #[test]
    fn current_client_is_not_offered_an_update_at_or_above_min_client_version() {
        let current = "0.9.1";
        let at_minimum = Notice::ClientUpgradeAvailable {
            version: Some(current.into()),
            level: ClientUpgradeLevel::Recommended,
        };
        assert!(at_minimum.offers_update_check());
        assert!(!at_minimum.offers_update_check_for(current));
        assert!(!at_minimum.offers_update_check_for("0.9.2"));
        assert!(!at_minimum.offers_update_check_for("0.9.1+local.1"));
        assert!(at_minimum.offers_update_check_for("0.9.0"));
        assert!(at_minimum.offers_update_check_for("0.9.1-beta.1"));
        let newer = Notice::ClientUpgradeAvailable {
            version: Some("0.9.2".into()),
            level: ClientUpgradeLevel::Recommended,
        };
        assert!(newer.offers_update_check_for(current));
        let unmarked = Notice::ClientUpgradeAvailable {
            version: None,
            level: ClientUpgradeLevel::Recommended,
        };
        assert!(unmarked.offers_update_check_for(current));
        let path = Notice::PathSearchNeedsUpgrade;
        assert!(path.offers_update_check_for(current));
        assert!(!Notice::Migrating { percent: None }.offers_update_check_for(current));
    }

    #[test]
    fn client_upgrade_capabilities_map_to_notices() {
        let mut input = v9_input(
            &[compatibility::PLAN_B_FORMAT],
            &[compatibility::PLAN_B_FORMAT],
        );
        input.client_upgrade = Some(ClientUpgradeInfo {
            level: ClientUpgradeLevel::Recommended,
            reason: Some("snapshot_format_downgraded".into()),
            missing_features: vec!["search_path".into()],
            min_client_version: Some("0.9.0".into()),
            served_snapshot_format: Some(compatibility::PLAN_B_FORMAT.into()),
        });
        let status = evaluate(&input);
        assert_eq!(
            status.notices,
            vec![Notice::ClientUpgradeAvailable {
                version: Some("0.9.0".into()),
                level: ClientUpgradeLevel::Recommended,
            }]
        );
        input.client_upgrade.as_mut().unwrap().level = ClientUpgradeLevel::Required;
        let status = evaluate(&input);
        assert!(status.incompatible());
        assert!(!status.main_search);
        assert!(
            status
                .notices
                .iter()
                .any(|n| matches!(n, Notice::Incompatible { .. }))
        );
    }

    #[test]
    fn deferred_migration_header_is_surfaced_without_progress_notice() {
        let mut input = v9_input(&[PLAN_B_V9_FORMAT], &[PLAN_B_V9_FORMAT]);
        input.migration_header = parse_schema_migration_header("deferred; target=9");
        let status = evaluate(&input);
        let migration = status.migration.as_ref().unwrap();
        assert_eq!(migration.state, MigrationState::Deferred);
        assert!(
            !status
                .notices
                .iter()
                .any(|n| matches!(n, Notice::Migrating { .. }))
        );
        assert_eq!(status.path_search, PathSearch::PausedForMigration);
    }

    #[test]
    fn vendor_import_limits_match_agreed_contract() {
        assert_eq!(VENDOR_IMPORT_MAX_FILE_BYTES, 52_428_800);
        assert_eq!(VENDOR_IMPORT_MAX_ROWS, 200_000);
        assert_eq!(VENDOR_IMPORT_MAX_ROW_TEXT_BYTES, 262_144);
        assert_eq!(VENDOR_IMPORT_API, "/api/vendor-import");
        let info: VendorImportInfo = serde_json::from_str(
            r#"{"revision":1,"max_file_bytes":52428800,"max_rows":200000,"max_row_text_bytes":262144}"#,
        )
        .unwrap();
        assert_eq!(
            info.max_row_text_bytes,
            Some(VENDOR_IMPORT_MAX_ROW_TEXT_BYTES)
        );
        let legacy: VendorImportInfo =
            serde_json::from_str(r#"{"revision":1,"max_file_bytes":52428800,"max_rows":200000}"#)
                .unwrap();
        assert_eq!(legacy.max_row_text_bytes, None);
    }

    #[test]
    fn format_ids_are_classified() {
        assert_eq!(
            parse_format_id(PLAN_B_V9_FORMAT),
            Some(FormatId {
                schema: 9,
                tokenization: 3,
                search_path: Some(1)
            })
        );
        assert_eq!(
            format_support(compatibility::PLAN_B_FORMAT),
            FormatSupport::Readable
        );
        assert_eq!(
            format_support(PLAN_B_V9_FORMAT),
            FormatSupport::KnownPathSearch
        );
        assert_eq!(
            format_support(PLAN_B_V9_AUDIO_FORMAT),
            FormatSupport::KnownPathSearch
        );
        assert_eq!(
            format_support(compatibility::PLAN_B_AUDIO_FORMAT),
            FormatSupport::Readable
        );
        assert_eq!(
            format_support("ale-s9-cld1-t3-m2-p1-a1-sd2"),
            FormatSupport::NewerPathRules
        );
        for unknown in [
            "ale-s10-cld1-t3-m2-p1-a1-sd1",
            "ale-s9-cld1-t4-m2-p1-a1-sd1",
            "ale-s9-cld1-t3-m2-p1-a1-sd1-sd2",
            "ale-s9-cld1-t3-m2-p1-a1-sd1-af2",
            "ale-s8-cld1-t3-m2-p1-a1-af2",
            "xyz-s9",
            "ale-s9--t3",
        ] {
            assert_eq!(
                format_support(unknown),
                FormatSupport::Incompatible,
                "{unknown}"
            );
        }
    }

    #[test]
    fn sd1_server_offers_path_search() {
        let status = evaluate(&v9_input(
            &[PLAN_B_V9_FORMAT, compatibility::PLAN_B_FORMAT],
            &[PLAN_B_V9_FORMAT],
        ));
        assert_eq!(status.path_search, PathSearch::Available);
        assert!(status.main_search);
        assert!(status.notices.is_empty());
    }

    #[test]
    fn extended_source_format_keeps_path_search_only_with_matching_observed_formats() {
        let status = evaluate(&v9_input(
            &[PLAN_B_V9_AUDIO_FORMAT],
            &[PLAN_B_V9_AUDIO_FORMAT],
        ));
        assert_eq!(status.path_search, PathSearch::Available);
        let mixed = evaluate(&v9_input(
            &[PLAN_B_V9_AUDIO_FORMAT],
            &[PLAN_B_V9_FORMAT, PLAN_B_V9_AUDIO_FORMAT],
        ));
        assert_eq!(mixed.path_search, PathSearch::NotOffered);
    }

    #[test]
    fn sd2_disables_path_search_but_keeps_main_search() {
        let sd2 = "ale-s9-cld1-t3-m2-p1-a1-sd2";
        let status = evaluate(&v9_input(&[sd2, compatibility::PLAN_B_FORMAT], &[]));
        assert_eq!(status.path_search, PathSearch::NeedsClientUpgrade);
        assert!(status.main_search);
        assert_eq!(status.notices, vec![Notice::PathSearchNeedsUpgrade]);
        assert!(status.notices[0].offers_update_check());

        // Header says sd1 but the content descriptor says sd2 (VN-12 mismatch).
        let status = evaluate(&v9_input(&[PLAN_B_V9_FORMAT], &[PLAN_B_V9_FORMAT, sd2]));
        assert_eq!(status.path_search, PathSearch::NeedsClientUpgrade);
        assert!(status.main_search);

        // A newer `search_path` feature revision alone also disables it.
        let mut input = v9_input(&[PLAN_B_V9_FORMAT], &[PLAN_B_V9_FORMAT]);
        input.features.insert(SEARCH_PATH_FEATURE.into(), 2);
        assert_eq!(evaluate(&input).path_search, PathSearch::NeedsClientUpgrade);
    }

    #[test]
    fn unknown_major_is_incompatible() {
        let status = evaluate(&v9_input(&["ale-s10-cld1-t4-m2-p1-a1-sd1"], &[]));
        assert!(status.incompatible());
        assert!(!status.main_search);
        assert_eq!(status.path_search, PathSearch::NotOffered);
        assert_eq!(
            status.notices[0].text(true),
            "服务器版本较新（0.9.0），当前客户端无法读取它的音效库。请更新客户端。"
        );
        // An observed tampered format ID is incompatible as well.
        let status = evaluate(&v9_input(
            &[compatibility::PLAN_B_FORMAT],
            &["ale-s9-cld1-t3-m2-p1-a1-sdX"],
        ));
        assert!(status.incompatible());
    }

    #[test]
    fn old_server_hides_features_without_prompt() {
        let status = evaluate(&ServerUpgradeInput::default());
        assert_eq!(status.path_search, PathSearch::NotOffered);
        assert!(status.main_search);
        assert!(status.notices.is_empty());
        assert!(status.vendor_import.is_none());
        let status = evaluate(&v9_input(
            &[compatibility::PLAN_B_FORMAT],
            &[compatibility::PLAN_B_FORMAT],
        ));
        assert_eq!(status.path_search, PathSearch::NotOffered);
        assert!(status.notices.is_empty());
    }

    #[test]
    fn migrating_server_reports_progress() {
        let mut input = v9_input(
            &[compatibility::PLAN_B_FORMAT],
            &[compatibility::PLAN_B_FORMAT],
        );
        input.schema_migration = Some(SchemaMigrationInfo {
            state: MigrationState::Running,
            endpoint: Some("/admin/api/migration".into()),
        });
        input.health_upgrade = Some(HealthUpgrade {
            phase: Some("backfill".into()),
            completed: Some(250),
            total: Some(1000),
        });
        let status = evaluate(&input);
        assert!(status.main_search);
        assert_eq!(status.path_search, PathSearch::NotOffered);
        assert_eq!(status.migration.as_ref().unwrap().percent, Some(25));
        assert_eq!(
            status.notices,
            vec![Notice::Migrating { percent: Some(25) }]
        );
        assert_eq!(
            status.notices[0].text(true),
            "服务器正在升级音效库（25%），暂时使用本地缓存。"
        );
        assert!(!status.notices[0].offers_update_check());

        input.admin_migration = Some(
            serde_json::from_str(
                r#"{"state":"paused","error_code":null,"progress":{"done":9,"total":10},
                "phases":{"backup":120,"path_build":4000,"optimize":0,"switch":0},"switch":null,
                "batches":[{"rows":500,"ms":30}],"eta_s":12}"#,
            )
            .unwrap(),
        );
        let status = evaluate(&input);
        let migration = status.migration.as_ref().unwrap();
        assert_eq!(migration.percent, Some(90));
        assert_eq!(migration.phase.as_deref(), Some("path_build"));
        assert_eq!(migration.state, MigrationState::Paused);

        input.schema_migration.as_mut().unwrap().state = MigrationState::Done;
        input.health_upgrade = None;
        input.admin_migration = None;
        assert!(evaluate(&input).migration.is_none());
    }

    #[test]
    fn unknown_migration_state_is_tolerated_and_shown() {
        let mut input = v9_input(&[PLAN_B_V9_FORMAT], &[PLAN_B_V9_FORMAT]);
        input.migration_header = parse_schema_migration_header("rebuilding_index");
        let status = evaluate(&input);
        let migration = status.migration.as_ref().unwrap();
        assert_eq!(migration.state, MigrationState::Unknown);
        assert_eq!(migration.state_label, "rebuilding_index");
        assert_eq!(status.path_search, PathSearch::PausedForMigration);
        assert!(status.main_search);
        input.migration_header = parse_schema_migration_header("done");
        let status = evaluate(&input);
        assert!(status.migration.is_none());
        assert_eq!(status.path_search, PathSearch::Available);
    }

    #[test]
    fn all_scope_without_path_results_is_marked() {
        let mut input = v9_input(&[PLAN_B_V9_FORMAT], &[PLAN_B_V9_FORMAT]);
        input.path_search_header_unavailable = true;
        let status = evaluate(&input);
        assert!(status.path_results_absent);
        assert_eq!(status.path_search, PathSearch::PausedForMigration);
        assert!(status.main_search);
    }

    #[test]
    fn failed_or_rolled_back_migration_reports_error_code() {
        let mut input = v9_input(&[compatibility::PLAN_B_FORMAT], &[]);
        input.admin_migration = Some(MigrationStatus {
            state: "rolled_back".into(),
            error_code: Some("insufficient_space".into()),
            ..Default::default()
        });
        let status = evaluate(&input);
        assert!(status.migration.is_none());
        assert_eq!(
            status.notices,
            vec![Notice::MigrationFailed {
                state: "rolled_back".into(),
                error_code: Some("insufficient_space".into())
            }]
        );
        let text = status.notices[0].text(true);
        assert!(text.contains("已回滚") && text.contains("1.3 倍"), "{text}");

        input.admin_migration = Some(MigrationStatus {
            state: "failed".into(),
            error_code: Some("read_only".into()),
            ..Default::default()
        });
        assert!(evaluate(&input).notices[0].text(true).contains("不可写"));
    }

    #[test]
    fn disk_full_pauses_and_asks_to_free_space() {
        let mut input = v9_input(&[compatibility::PLAN_B_FORMAT], &[]);
        input.admin_migration = Some(MigrationStatus {
            state: "paused".into(),
            error_code: Some("disk_full".into()),
            progress: Some(MigrationCounts { done: 1, total: 2 }),
            ..Default::default()
        });
        let status = evaluate(&input);
        assert_eq!(
            status.migration.as_ref().unwrap().state,
            MigrationState::Paused
        );
        assert!(status.main_search);
        let paused = status
            .notices
            .iter()
            .find(|n| matches!(n, Notice::MigrationPaused { .. }))
            .unwrap();
        assert!(paused.text(true).contains("腾出空间后点「继续」"));
    }

    #[test]
    fn every_agreed_state_and_error_code_has_chinese_text() {
        for state in [
            "idle",
            "deferred",
            "preflight",
            "backing_up",
            "running",
            "paused",
            "switching",
            "done",
            "failed",
            "cancelled",
            "rolled_back",
        ] {
            assert_ne!(
                MigrationState::parse(state),
                MigrationState::Unknown,
                "{state}"
            );
            let text = migration_state_text(state, true);
            assert!(!text.is_ascii(), "{state}: {text}");
        }
        for code in [
            "insufficient_space",
            "locked",
            "already_running",
            "backup_failed",
            "integrity_failed",
            "io_error",
            "not_migrated",
            "disk_full",
            "read_only",
        ] {
            let text = migration_error_text(code, true);
            assert!(!text.contains("错误代码"), "{code}: {text}");
        }
        assert_eq!(migration_state_text("warming_up", true), "warming_up");
        assert_eq!(migration_error_text("quota_x", true), "错误代码：quota_x");
        for active in ["preflight", "backing_up", "switching", "warming_up"] {
            assert!(MigrationState::parse(active).in_progress(), "{active}");
        }
        for ended in ["failed", "cancelled", "rolled_back"] {
            assert!(MigrationState::parse(ended).failed(), "{ended}");
        }
    }

    #[test]
    fn migration_refusals_and_controls() {
        assert_eq!(
            MigrationRefusal::from_code("migration_in_progress"),
            Some(MigrationRefusal::PathSearchInProgress)
        );
        assert_eq!(
            MigrationRefusal::from_code("migration_switching").map(|r| r.default_retry_after()),
            Some(30)
        );
        assert_eq!(MigrationRefusal::from_code("BUSY"), None);
        assert!(migration_actions(false).is_empty());
        assert_eq!(migration_actions(true), &MIGRATION_ACTIONS);
    }

    #[test]
    fn upgrade_header_prompts_once_per_session() {
        let mut input = v9_input(&[compatibility::PLAN_B_FORMAT], &[]);
        input.upgrade_available = parse_upgrade_available("0.9.0");
        let status = evaluate(&input);
        assert_eq!(
            status.notices[0].text(true),
            "服务器已升级，更新客户端可使用路径搜索和厂商资料"
        );
        assert!(status.notices[0].offers_update_check());
        let mut session = NoticeSession::default();
        assert_eq!(session.take(&status).len(), 1);
        assert!(session.take(&status).is_empty());
    }

    #[test]
    fn vendor_import_needs_feature_and_manage() {
        let limits = VendorImportInfo {
            endpoint: None,
            revision: 1,
            max_file_bytes: 52_428_800,
            max_rows: 200_000,
            max_row_text_bytes: Some(262_144),
        };
        let mut input = v9_input(&[PLAN_B_V9_FORMAT], &[PLAN_B_V9_FORMAT]);
        input.vendor_import = Some(limits.clone());
        assert!(evaluate(&input).vendor_import.is_none());
        input.can_manage = true;
        assert!(evaluate(&input).vendor_import.is_none());
        input.features.insert(VENDOR_METADATA_FEATURE.into(), 1);
        assert_eq!(evaluate(&input).vendor_import, Some(limits));
    }

    #[test]
    fn capability_fields_deserialize_with_unknown_values() {
        let migration: SchemaMigrationInfo =
            serde_json::from_str(r#"{"state":"rebuilding","endpoint":"/admin/api/migration"}"#)
                .unwrap();
        assert_eq!(migration.state, MigrationState::Unknown);
        let health: HealthUpgrade =
            serde_json::from_str(r#"{"phase":"copying","completed":1,"total":4}"#).unwrap();
        assert_eq!(percent(health.completed, health.total), Some(25));
    }

    #[test]
    fn zero_total_progress_reads_preparing_without_dividing() {
        assert_eq!(percent(Some(0), Some(0)), None);
        assert_eq!(percent(Some(5), Some(0)), None);
        assert_eq!(percent(None, None), None);
        let notice = Notice::Migrating {
            percent: percent(Some(0), Some(0)),
        };
        assert!(
            notice.text(true).contains("准备中"),
            "{}",
            notice.text(true)
        );
        assert!(notice.text(false).contains("preparing"));
        assert!(!notice.text(true).contains('%'));
        assert!(
            Notice::Migrating { percent: Some(0) }
                .text(true)
                .contains("0%")
        );
    }

    #[test]
    fn search_scope_defaults_to_main_and_omits_the_parameter() {
        assert_eq!(SearchScope::default(), SearchScope::Main);
        assert_eq!(SearchScope::Main.query(), None);
        assert_eq!(SearchScope::All.query(), Some(("scope", "all")));
        assert_eq!(SearchScope::Path.query(), Some(("scope", "path")));
        assert_eq!(SearchScope::Filename.query(), Some(("scope", "filename")));
        assert_eq!(SearchScope::parse(None), Some(SearchScope::Main));
        assert_eq!(SearchScope::parse(Some("main")), Some(SearchScope::Main));
        assert_eq!(SearchScope::parse(Some("")), Some(SearchScope::Main));
        assert_eq!(SearchScope::parse(Some("path")), Some(SearchScope::Path));
        assert_eq!(SearchScope::parse(Some("all")), Some(SearchScope::All));
        assert_eq!(SearchScope::parse(Some("folder")), None);
        for scope in [
            SearchScope::Main,
            SearchScope::All,
            SearchScope::Path,
            SearchScope::Filename,
        ] {
            assert_eq!(SearchScope::parse(Some(scope.as_str())), Some(scope));
        }
    }
}
