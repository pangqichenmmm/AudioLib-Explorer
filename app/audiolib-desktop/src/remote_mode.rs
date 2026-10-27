// app/audiolib-desktop/src/remote_mode.rs
//! Server data source for the desktop: settings dialog, connection test, snapshot sync,
//! read-only provider swap and Range-streamed playback (server stream, WebDAV fallback).
//!
//! Server mode never touches the local working library: it opens a validated snapshot
//! from `<data dir>/remote/<library_id>/` with the ordinary read-only `SqliteProvider`,
//! so search and browse keep local-mode performance. Swaps reuse the library operation
//! machinery (generation bump, Metadata drain, draft reset). All network work runs on
//! worker threads; results return through the Slint event loop and stale generations
//! are discarded.
use super::{Controller, LibraryOperation, post};
#[path = "remote_sync_progress.rs"]
mod remote_sync_progress;
use crate::AppWindow;
use audiolib_audio::{WaveformData, WaveformRequest};
use audiolib_core::AudioFile;
use audiolib_core::config::{AppConfig, DataSourceMode, DataSourceSettings};
use audiolib_core::server_url::{Scheme, ServerAddress, ServerUrlError};
use audiolib_db::SqliteProvider;
use audiolib_remote::{
    AudioRoute, AutoSyncGate, CacheState, CachedIdentity, DEFAULT_DOWNLOAD_CACHE_BYTES,
    DownloadCache, DownloadItem, DownloadProgress, ListWaveformStore, ListWaveformsCapability,
    MemoryCache, MemoryStream, PendingSync, PollSchedule, RemoteCache, RemoteClient, RemoteError,
    RemoteListWaveforms, RemoteProvider, RemoteRoot, RemoteSecrets, RemoteWriter, RevisionCheck,
    RootState, RouteOptions, SourceAuth, StreamSource, SyncControl, SyncOptions, SyncProgress,
    SyncRunOutcome, UrlSource, WAKING_HINT_AFTER, WaveformQuery, WaveformReply, WebDavAuth,
    WriteEvent, WriteListener, bandwidth, check_revision_with_roots, choose_route, effective_roots,
    load_profile_secrets, map_server_path, root_for_path, sync_library_with_options,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

/// The full server waveform waits at most this long for the audio prebuffer.
const WAVEFORM_AFTER_AUDIO: Duration = Duration::from_secs(3);
/// Server mode: a selection starts its network audio only after resting this long, so
/// arrowing through rows does not open a request per row.
pub(super) const SELECTION_SETTLE: Duration = Duration::from_millis(180);
/// Every new in-memory registration owns its path, including server/WebDAV
/// representations of the same record and route. Reused streams keep their path.
static STREAM_REGISTRATION_ID: AtomicU64 = AtomicU64::new(1);

/// One sync worker owns the busy flag until all of its cache work has drained.
/// Releasing it cannot depend on UI delivery or the request generation: cancellation
/// invalidates that generation while the same worker still owns its cache work.
struct RemoteSyncBusyGuard(Option<Arc<Controller>>);

impl Drop for RemoteSyncBusyGuard {
    fn drop(&mut self) {
        if let Some(controller) = self.0.take() {
            controller.remote_busy.store(false, Ordering::Release);
        }
    }
}

/// Play admission and worker completion share one predicate lock, so a source
/// gate followed by a drain cannot miss a concurrently registering worker.
#[derive(Default)]
pub(super) struct RemotePlayState {
    workers: usize,
    publication_gates: usize,
}

#[allow(dead_code)] // plays are no longer sent to the server
/// Cache promotion finishes on the sync worker before its UI/provider publication.
/// Keep new play writes out of that interval without making close wait for UI delivery.
struct RemotePlayPublicationGate(Arc<Controller>);

impl Drop for RemotePlayPublicationGate {
    fn drop(&mut self) {
        self.0.remote_plays.lock().unwrap().publication_gates -= 1;
    }
}

#[path = "server_removal.rs"]
mod server_removal;
#[cfg(test)]
#[path = "source_consistency_tests.rs"]
mod source_consistency_tests;
#[path = "source_manager.rs"]
mod source_manager;
pub(super) use source_manager::RootOperation;
#[cfg(test)]
#[path = "snapshot_refresh_tests.rs"]
mod snapshot_refresh_tests;
#[cfg(test)]
#[path = "source_transition_tests.rs"]
mod source_transition_tests;
#[path = "remote_sources.rs"]
mod sources;
#[cfg(test)]
#[path = "sync_resume_tests.rs"]
mod sync_resume_tests;

/// Provider chosen before the window opens when settings select server mode.
pub struct RemoteStartup {
    pub provider: Arc<SqliteProvider>,
    /// Cached snapshot state, or `None` when the empty placeholder is shown.
    pub state: Option<CacheState>,
}

/// Bootstrap the GUI without opening/upgrading a large cache. The manager's single
/// sync pipeline selects, prepares and opens the actual snapshot after the UI is visible.
pub fn open_remote_placeholder(config: &AppConfig) -> anyhow::Result<RemoteStartup> {
    apply_audition_memory(&config.data_source);
    let cache = RemoteCache::new(&config.data_root());
    cache.cleanup_stale_temporaries();
    let path = cache.placeholder_database()?;
    Ok(RemoteStartup {
        provider: Arc::new(SqliteProvider::open_read_only(&path)?),
        state: None,
    })
}

/// Open the last synced snapshot (offline-capable) or an empty placeholder. The local
/// working library is not opened or modified in server mode.
#[cfg(test)]
fn open_remote_startup(config: &AppConfig) -> anyhow::Result<RemoteStartup> {
    open_remote_startup_with_progress(config, &mut |_| {})
}

/// Open the cached server source, reporting offline preparation on the caller's worker.
pub fn open_remote_startup_with_progress(
    config: &AppConfig,
    progress: &mut dyn FnMut(crate::startup::StartupProgress),
) -> anyhow::Result<RemoteStartup> {
    use crate::startup::StartupProgress;
    progress(StartupProgress::Remote(
        audiolib_remote::CachePreparation::Checking,
    ));
    let cache = RemoteCache::new(&config.data_root());
    apply_audition_memory(&config.data_source);
    // Audition streams into memory now: the disk audition cache of earlier versions goes
    // (drag-out downloads in `<data dir>/cache/` stay).
    let removed = cache.remove_audition_caches();
    if removed > 0 {
        tracing::info!(removed, "old disk audition caches removed");
    }
    // Reclaim only abandoned top-level generated files. Leases protect live work;
    // durable transfers and protected migration jobs retain their recovery state.
    cache.cleanup_stale_temporaries();
    if let Some(state) = cache
        .active()
        .filter(|state| cached_source_matches(&state.server_url, &config.data_source.server_url))
    {
        let opened = cache
            .prepare_cached_state_with_progress(&state, &mut |stage| {
                progress(StartupProgress::Remote(stage));
            })
            .and_then(|state| {
                progress(StartupProgress::Opening);
                SqliteProvider::open_read_only(&cache.database_path(&state))
                    .map(|provider| (provider, state))
                    .map_err(|error| match error {
                        error @ audiolib_core::Error::NewerDatabase { .. } => error.into(),
                        error => RemoteError::Snapshot(error.to_string()),
                    })
            });
        match opened {
            Ok((provider, state)) => {
                return Ok(RemoteStartup {
                    provider: Arc::new(provider),
                    state: Some(state),
                });
            }
            Err(error) => tracing::warn!(%error, "Cached server snapshot unusable"),
        }
    }
    progress(StartupProgress::Placeholder);
    let placeholder = cache.placeholder_database()?;
    Ok(RemoteStartup {
        provider: Arc::new(SqliteProvider::open_read_only(&placeholder)?),
        state: None,
    })
}

/// After a startup that opened the main window: open 音效库管理 when the startup sync
/// failed (unreachable, unauthorized, snapshot invalid or a failed rebuild) or the
/// server's library_id changed; a newer revision alone does not.
fn startup_sync_opens_manager(result: &Result<audiolib_remote::SyncOutcome, RemoteError>) -> bool {
    match result {
        Ok(outcome) => {
            outcome.library_changed
                || outcome
                    .previous
                    .as_ref()
                    .is_some_and(|previous| previous.library_id != outcome.state.library_id)
        }
        Err(RemoteError::Cancelled) => false,
        Err(_) => true,
    }
}

/// Whether startup must open 音效库管理: only without a cached snapshot of the
/// configured server. A newer server revision just syncs in the background.
fn startup_needs_manager(cached: Option<&CacheState>, configured_url: &str) -> bool {
    configured_url.trim().is_empty()
        || !cached.is_some_and(|state| cached_source_matches(&state.server_url, configured_url))
}

/// Cached IDs are valid only for the configured endpoint. Ignore user spelling of
/// hosts/default ports and the accepted trailing API suffix when comparing addresses.
fn cached_source_matches(cached: &str, configured: &str) -> bool {
    let (Ok(cached), Ok(configured)) = (
        ServerAddress::parse(cached),
        ServerAddress::parse(configured),
    ) else {
        return false;
    };
    cached.scheme == configured.scheme
        && cached.host.eq_ignore_ascii_case(&configured.host)
        && cached.port.unwrap_or(cached.scheme.default_port())
            == configured.port.unwrap_or(configured.scheme.default_port())
        && cached.prefix == configured.prefix
}

fn pending_sync_text(pending: &PendingSync, chinese: bool) -> String {
    let stage = match (pending.manual_paused, chinese) {
        (true, true) => "同步已暂停，已保留",
        (true, false) => "Sync paused; retained",
        (false, true) => "同步未完成，已保留",
        (false, false) => "Sync incomplete; retained",
    };
    let received = pending.downloaded_bytes as f64 / 1_048_576.0;
    match pending.total_bytes {
        Some(total) => format!(
            "{stage} {received:.1} / {:.1} MiB",
            total as f64 / 1_048_576.0
        ),
        None => format!("{stage} {received:.1} MiB"),
    }
}

/// Target of a data-source provider swap, executed as a library operation.
#[derive(Clone)]
pub(super) enum DataSourceSwitch {
    Local,
    Server {
        database: PathBuf,
        state: Option<Box<CacheState>>,
    },
}

/// One revision poll, bound to the connection and snapshot used for its requests.
struct RevisionPoll {
    source_generation: u64,
    checked: Option<CachedIdentity>,
    check: RevisionCheck,
    roots: Option<Vec<RemoteRoot>>,
    next_check: Option<u64>,
    auto_due: bool,
}

/// The owned server destination captured together with a drag's database provider.
pub(super) struct RemoteDragSource {
    state: CacheState,
    client: RemoteClient,
    cache_directory: PathBuf,
}

/// Mutable server-mode state shared by the dialog, sync and playback.
#[derive(Default)]
pub(super) struct RemoteRuntime {
    /// Per-profile tokens; the top-level token is the active (or last used) profile's.
    secrets: RemoteSecrets,
    secrets_loaded: bool,
    /// Snapshot currently opened by the provider (server mode only).
    pub(super) state: Option<CacheState>,
    /// Roots shown in the dialog and the text currently in each row's URL field.
    last_error: Option<String>,
    /// Latest background revision check (or sync failure) for the open snapshot.
    check: Option<RevisionCheck>,
    /// A known replacement blocks writes until its snapshot is actually opened.
    /// Later offline/check failures must never re-enable stale cached record IDs.
    library_changed: bool,
    /// The startup sync is still running: its failure or a library_id change opens
    /// 音效库管理 (a plain revision change does not).
    startup_check: bool,
    /// Unix seconds of the next automatic revision check, when scheduled.
    next_check: Option<u64>,
    /// A new revision is known but automatic sync waits for the server to settle
    /// (see [`AutoSyncGate`]).
    auto_deferred: bool,
    /// Latest owned transfer state. A manual pause blocks all automatic starts,
    /// including startup and write-triggered resyncs, until an explicit user action.
    pending_sync: Option<PendingSync>,
    /// Root status (§6.4) from the latest successful `/roots` (revision poll or sync);
    /// `None` while the server is unreachable, so no stale status is shown.
    live_roots: Option<Vec<RemoteRoot>>,
    /// Notice text the user dismissed; hidden until the notice changes.
    dismissed_notice: Option<String>,
    /// Earliest automatic re-check requested outside the schedule (a sync that failed
    /// because the server was still starting).
    retry_hint: Option<Instant>,
    /// One write-triggered resync waits for playback to finish per source generation.
    resync_pending: Option<u64>,
    /// Record id of each playback-cache path handed to the engine, so a finished play of
    /// a cached copy is counted for the right server record.
    audio_ids: HashMap<PathBuf, i64>,
    /// Original file name of the record behind each of those paths: the player shows it
    /// instead of the stream's virtual name (`<id>-<route>.wav`).
    audio_names: HashMap<PathBuf, String>,
    /// Original source format and record identity behind each virtual audition path.
    audio_files: HashMap<PathBuf, AudioFile>,
    /// Old connection retained until a source change successfully swaps its provider.
    source_rollback: Option<(DataSourceSettings, RemoteSecrets)>,
    /// Per record: `codec=pcm` refused (415), `codec=flac` refused.
    preview_refused: HashMap<i64, (bool, bool)>,
    /// The server has no transcoder (503 TRANSCODER_UNAVAILABLE): no FLAC previews.
    flac_unavailable: bool,
    /// Server waveforms by library, revision and request (see `remote_waveform`).
    waveforms: HashMap<String, WaveformData>,
    /// The unified 「数据源与音效库」 window.
    manager: source_manager::SourceManager,
    /// Records the server removed that the open snapshot still lists (hidden until the
    /// resync swaps the snapshot).
    hidden: server_removal::HiddenRecords,
    /// Whether the active server lets this PC remove records (menus and context menus).
    manage: server_removal::ServerManage,
    /// List Waveform provider of the open snapshot (`POST /waveforms`), keyed by source
    /// generation, library and snapshot file.
    list_waveforms: Option<(ListWaveformKey, Arc<RemoteProvider>)>,
}

type ListWaveformKey = (u64, String, String, Option<ListWaveformsCapability>);

/// Info line shown above the connection report when no access token is entered.
fn tokenless_note(chinese: bool) -> String {
    cn_or(
        chinese,
        "ⓘ 未填写访问令牌：请求不带 Authorization（局域网直连不需要；外网地址需要令牌，服务器要求时会提示）\n",
        "ⓘ No access token: requests carry no Authorization header (not needed on the LAN; external addresses need one and you are told if the server requires it)\n",
    )
}

/// Closing line of a successful connection report; failed optional checks (WebDAV) are
/// called out without turning the result into a failure.
fn connection_verdict(warnings: usize, chinese: bool) -> String {
    match (warnings, chinese) {
        (0, true) => "\n全部检查通过。".into(),
        (0, false) => "\nAll checks passed.".into(),
        (count, true) => format!("\n连接正常；{count} 项可选检查未通过（!），不影响服务器流试听。"),
        (count, false) => format!(
            "\nConnection OK; {count} optional check(s) failed (!), server-stream playback is unaffected."
        ),
    }
}

/// Apply the configured audition memory budget to the process-wide stream cache, and
/// keep a spare keep-alive connection after a cancelled audition (a remote server pays
/// ~0.5 s per new TLS connection).
pub(super) fn apply_audition_memory(settings: &DataSourceSettings) {
    MemoryCache::global().set_budget(u64::from(settings.audition_memory_mb) * 1024 * 1024);
    audiolib_remote::set_connection_warming(true);
}

/// Kind of the non-blocking server notice: 0 none, 1 update available (Sync),
/// 2 offline/starting/failed (Retry), 3 token rejected (Settings).
pub(super) const NOTICE_NONE: i32 = 0;
pub(super) const NOTICE_UPDATE: i32 = 1;
pub(super) const NOTICE_RETRY: i32 = 2;
pub(super) const NOTICE_SETTINGS: i32 = 3;

/// How an available update is handled: shown only, synced now, or synced automatically
/// once the server stops changing ([`AutoSyncGate`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AutoSync {
    Off,
    Now,
    Deferred,
}

impl AutoSync {
    fn from_settings(auto_sync: bool, deferred: bool) -> Self {
        match (auto_sync, deferred) {
            (false, _) => Self::Off,
            (true, false) => Self::Now,
            (true, true) => Self::Deferred,
        }
    }
}

/// Notice text and kind for a revision check; `None` hides the notice. Pure so the
/// decision of what the user sees is unit tested.
pub(super) fn revision_notice(
    check: &RevisionCheck,
    chinese: bool,
    auto: AutoSync,
    retry_at: Option<&str>,
) -> Option<(String, i32)> {
    let retry = |text: String| match retry_at {
        Some(time) if chinese => format!("{text}；将于 {time} 自动重试"),
        Some(time) => format!("{text}; retrying automatically at {time}"),
        None => text,
    };
    let update = |text: String| match auto {
        AutoSync::Off => text,
        AutoSync::Now if chinese => format!("{text}，正在自动同步…"),
        AutoSync::Now => format!("{text}; syncing automatically…"),
        AutoSync::Deferred if chinese => {
            format!("{text}；等待服务器更新稳定及当前试听结束后自动同步，也可点击“同步”立即更新")
        }
        AutoSync::Deferred => format!(
            "{text}; syncing automatically once the server settles and the current audition finishes, or Sync now"
        ),
    };
    Some(match check {
        RevisionCheck::UpToDate { .. } => return None,
        RevisionCheck::NewRevision { cached, server } => (
            update(if chinese {
                format!("服务器素材库有更新：r{server}（当前缓存 r{cached}）")
            } else {
                format!("Server library updated: revision {server} (cached {cached})")
            }),
            NOTICE_UPDATE,
        ),
        RevisionCheck::LibraryChanged {
            cached_id,
            server_id,
            ..
        } => (
            update(if chinese {
                format!(
                    "服务器素材库身份已变化（{} → {}），同步后将清空选择与草稿",
                    short_id(cached_id),
                    short_id(server_id)
                )
            } else {
                format!(
                    "The server library identity changed ({} → {}); syncing clears selection and drafts",
                    short_id(cached_id),
                    short_id(server_id)
                )
            }),
            NOTICE_UPDATE,
        ),
        RevisionCheck::NotSynced {
            server_revision, ..
        } => (
            update(if chinese {
                format!("服务器已可连接（r{server_revision}），尚未同步素材库")
            } else {
                format!(
                    "Server reachable (revision {server_revision}); the library is not synced yet"
                )
            }),
            NOTICE_UPDATE,
        ),
        RevisionCheck::Offline(_) => (
            retry(cn_or(
                chinese,
                "服务器离线：继续使用本地缓存（只读）",
                "Server offline: using the local cache (read-only)",
            )),
            NOTICE_RETRY,
        ),
        // Connected but slow (a large library): not offline, the cache stays usable.
        RevisionCheck::Slow(_) => (
            match (retry_at, chinese) {
                (Some(time), true) => format!("服务器响应慢，将于 {time} 重试：继续使用本地缓存"),
                (None, true) => "服务器响应慢，正在重试：继续使用本地缓存".to_owned(),
                (Some(time), false) => {
                    format!(
                        "Server is responding slowly, retrying at {time}: using the local cache"
                    )
                }
                (None, false) => {
                    "Server is responding slowly, retrying: using the local cache".to_owned()
                }
            },
            NOTICE_RETRY,
        ),
        RevisionCheck::Starting { .. } => (
            retry(cn_or(
                chinese,
                "服务器正在启动（正在打开素材库），继续使用本地缓存",
                "The server is starting (opening its library); using the local cache",
            )),
            NOTICE_RETRY,
        ),
        // The classified message tells a rejected token from a missing one.
        RevisionCheck::Unauthorized(message) if !message.trim().is_empty() => {
            (message.clone(), NOTICE_SETTINGS)
        }
        RevisionCheck::Unauthorized(_) => (
            cn_or(
                chinese,
                "服务器拒绝了访问令牌（401）：请在局域网内打开服务器网页管理页面（http://<服务器局域网地址>:8790/admin）生成令牌，并在数据源设置中更新",
                "The server rejected the token (401): generate one on the server's web admin page (http://<LAN server address>:8790/admin, from the local network) and update it in Data source settings",
            ),
            NOTICE_SETTINGS,
        ),
        RevisionCheck::Failed(message) => (
            retry(if chinese {
                format!("检查服务器版本失败：{message}")
            } else {
                format!("Server revision check failed: {message}")
            }),
            NOTICE_RETRY,
        ),
    })
}

fn cn_or(chinese: bool, cn: &str, en: &str) -> String {
    if chinese { cn.into() } else { en.into() }
}

fn format_time(unix: u64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(unix as i64, 0)
        .single()
        .map(|time| time.format("%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

fn format_clock(unix: u64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(unix as i64, 0)
        .single()
        .map(|time| time.format("%H:%M:%S").to_string())
        .unwrap_or_default()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// `ds-address-form` values.
const ADDRESS_FORM_URL: i32 = 0;
const ADDRESS_FORM_PARTS: i32 = 1;

/// The dialog's server address: a full URL, or scheme + domain + port + path prefix.
#[derive(Clone, Debug, Default)]
struct AddressForm {
    parts: bool,
    url: String,
    /// `ds-scheme`: 0 https (default), 1 http.
    scheme: i32,
    host: String,
    port: String,
    prefix: String,
}

impl AddressForm {
    fn read(ui: &AppWindow) -> Self {
        Self {
            parts: ui.get_ds_address_form() == ADDRESS_FORM_PARTS,
            url: ui.get_ds_server_url().to_string(),
            scheme: ui.get_ds_scheme(),
            host: ui.get_ds_host().to_string(),
            port: ui.get_ds_port().to_string(),
            prefix: ui.get_ds_prefix().to_string(),
        }
    }

    fn scheme(&self) -> Scheme {
        if self.scheme == 1 {
            Scheme::Http
        } else {
            Scheme::Https
        }
    }

    /// Input without `scheme://` in the URL form means HTTPS.
    fn address(&self) -> Result<ServerAddress, ServerUrlError> {
        if self.parts {
            ServerAddress::from_parts(self.scheme(), &self.host, &self.port, &self.prefix)
        } else {
            ServerAddress::parse_input(&self.url, Scheme::Https)
        }
    }
}

fn set_address_parts(ui: &AppWindow, address: Option<&ServerAddress>) {
    let Some(address) = address else {
        ui.set_ds_scheme(0);
        ui.set_ds_host("".into());
        ui.set_ds_port("".into());
        ui.set_ds_prefix("".into());
        return;
    };
    ui.set_ds_scheme(if address.scheme == Scheme::Http { 1 } else { 0 });
    ui.set_ds_host(address.host.clone().into());
    ui.set_ds_port(
        address
            .port
            .map(|port| port.to_string())
            .unwrap_or_default()
            .into(),
    );
    ui.set_ds_prefix(address.prefix.clone().into());
}

/// Hint line under the address fields and whether it is an error. Empty input shows the
/// accepted forms instead of an error.
fn address_hint(address: &Result<ServerAddress, ServerUrlError>, chinese: bool) -> (String, bool) {
    match address {
        Ok(address) => {
            let api = address.api_url();
            let text = match (address.scheme, chinese) {
                (Scheme::Https, true) => format!("将连接 {api}（HTTPS，校验公共受信任证书）"),
                (Scheme::Https, false) => {
                    format!("Connects to {api} (HTTPS, publicly trusted certificate required)")
                }
                (Scheme::Http, true) => {
                    format!("将连接 {api}（http:// 不加密，仅建议在局域网使用）")
                }
                (Scheme::Http, false) => {
                    format!("Connects to {api} (http:// is unencrypted; use it on the LAN only)")
                }
            };
            (text, false)
        }
        Err(ServerUrlError::Empty) => (
            cn_or(
                chinese,
                "例如 https://audiolib.example.com:8443（外网）或 http://server.example.lan:8790（局域网）；不写协议时默认 https://，/api/v1 会自动补全",
                "e.g. https://audiolib.example.com:8443 (internet) or http://server.example.lan:8790 (LAN); https:// is assumed without a scheme and /api/v1 is added automatically",
            ),
            false,
        ),
        Err(error) => (error.message(chinese), true),
    }
}

fn short_id(id: &str) -> String {
    if id.chars().count() > 12 {
        format!("{}…", id.chars().take(8).collect::<String>())
    } else {
        id.to_owned()
    }
}

impl Controller {
    /// Annotation writes are unavailable while the known server state is read-only.
    /// Slow responses remain retryable; cached playback and browsing stay available.
    pub(super) fn require_metadata_writable(&self) -> Result<(), String> {
        self.require_remote_write_identity()?;
        if self.remote_metadata_blocked() {
            Err(cn_or(
                self.chinese.load(Ordering::Acquire),
                "服务器暂时无法保存标注：本地缓存只读。请重新连接后重试；已有草稿仍然保留。",
                "The server cannot currently save annotations: the local cache is read-only. Reconnect and retry; existing drafts are retained.",
            ))
        } else {
            Ok(())
        }
    }

    fn remote_metadata_blocked(&self) -> bool {
        self.remote_read_only()
            && matches!(
                self.remote.lock().unwrap().check,
                Some(
                    RevisionCheck::Offline(_)
                        | RevisionCheck::Unauthorized(_)
                        | RevisionCheck::Starting { .. }
                )
            )
    }

    /// True while the provider is a server snapshot (structural writes refused; annotation
    /// writes and plays go to the server).
    pub(super) fn remote_read_only(&self) -> bool {
        self.remote_mode.load(Ordering::Acquire)
    }

    /// Refuse a structural library write in server mode with a clear message; returns true
    /// if refused. Annotation edits are not refused: they go to the server.
    pub(super) fn refuse_remote_write(&self) -> bool {
        if !self.remote_read_only() {
            return false;
        }
        self.status(cn_or(
            self.chinese.load(Ordering::Acquire),
            "服务器模式：扫描、维护、恢复、修复引用和导入请在本地模式或服务器端执行（标注编辑会直接保存到服务器；服务器允许时可从编辑菜单移除记录，扫描在「服务器音效库」页）",
            "Server mode: scan, maintain, restore, repair or import in local mode or on the server (annotation edits are saved to the server; when the server allows it, Edit removes records and the Server library page runs scans)",
        ));
        true
    }

    /// Server-mode `DataProvider` for annotation writes and plays: reads come from the
    /// open snapshot, writes go to the server API and are applied to the snapshot by the
    /// revision rule (otherwise a full sync is started).
    pub(super) fn remote_provider(self: &Arc<Self>) -> Result<Arc<RemoteProvider>, String> {
        let chinese = self.chinese.load(Ordering::Acquire);
        self.require_remote_write_identity()?;
        let state = self.remote.lock().unwrap().state.clone().ok_or_else(|| {
            cn_or(
                chinese,
                "服务器素材库尚未同步，暂时无法保存到服务器",
                "The server library has not been synced yet; changes cannot be saved to the server",
            )
        })?;
        let settings = self.config.lock().unwrap().data_source.clone();
        let secrets = self.secrets();
        let client = RemoteClient::new(&settings.server_url, &secrets.token)
            .map_err(|error| error.localized(chinese))?;
        let writer = RemoteWriter::new(
            client,
            RemoteCache::new(&self.data_root()),
            state.library_id.clone(),
        );
        let controller = Arc::downgrade(self);
        let library_id = state.library_id;
        let database_file = state.database_file;
        let listener: WriteListener = Arc::new(move |event| {
            if let Some(controller) = controller.upgrade() {
                controller.remote_write_event(&library_id, &database_file, event);
            }
        });
        Ok(Arc::new(RemoteProvider::new(
            self.provider(),
            writer,
            chinese,
            Some(listener),
        )))
    }

    /// Server-mode provider for the list Waveform worker: reads from the open snapshot,
    /// thumbnails only from `POST /waveforms` (never the snapshot's `waveform_cache` rows,
    /// §9.3) with the client disk cache under `<cache>/waveforms/server/`. Rebuilt when the
    /// source or snapshot changes; cheap otherwise. Called on the worker thread.
    pub(super) fn remote_list_waveform_provider(&self) -> Option<Arc<RemoteProvider>> {
        let (settings, cache_root) = {
            let config = self.config.lock().unwrap();
            (config.data_source.clone(), config.paths.cache.clone())
        };
        // The capability comes from the `/capabilities` read after each sync (the menu
        // gate): no request of our own, and no column until it is known.
        let (state, capabilities) = {
            let remote = self.remote.lock().unwrap();
            (
                remote.state.clone()?,
                remote
                    .manage
                    .capabilities_for(&settings.server_url)?
                    .clone(),
            )
        };
        let key: ListWaveformKey = (
            self.source_generation.load(Ordering::Acquire),
            state.library_id.clone(),
            state.database_file.clone(),
            capabilities.list_waveforms.clone(),
        );
        if let Some((cached, provider)) = &self.remote.lock().unwrap().list_waveforms
            && *cached == key
        {
            return Some(provider.clone());
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        let secrets = self.secrets();
        let client = RemoteClient::new(&settings.server_url, &secrets.token).ok()?;
        let mut service = RemoteListWaveforms::new(
            client.clone(),
            state.library_id.clone(),
            ListWaveformStore::for_library(&cache_root, &state.library_id),
        )
        .with_capabilities(&capabilities);
        // Compact i8 items are requested only when the server lists that encoding.
        if capabilities
            .list_waveforms
            .as_ref()
            .is_some_and(|capability| capability.offers_i8())
        {
            let path = RemoteListWaveforms::i8_store_path(&cache_root, &state.library_id);
            match audiolib_db::WaveformsDb::open(&path) {
                Ok(store) => service = service.with_i8_store(store),
                Err(error) => tracing::warn!(%error, "waveforms.db unavailable; using int16"),
            }
        }
        let service = Arc::new(service);
        spawn_waveform_bundle_import(&service);
        let writer = RemoteWriter::new(
            client,
            RemoteCache::new(&self.data_root()),
            state.library_id.clone(),
        );
        let provider = Arc::new(
            RemoteProvider::new(self.provider(), writer, chinese, None)
                .with_list_waveforms(service),
        );
        self.remote.lock().unwrap().list_waveforms = Some((key, provider.clone()));
        Some(provider)
    }

    /// Cache effect of a server write (called on the writing thread).
    fn remote_write_event(
        self: &Arc<Self>,
        library_id: &str,
        database_file: &str,
        event: WriteEvent,
    ) {
        match event {
            WriteEvent::Applied { revision } => {
                let mut remote = self.remote.lock().unwrap();
                let current = remote.state.as_mut().filter(|state| {
                    state.library_id == library_id && state.database_file == database_file
                });
                let Some(state) = current else {
                    // The snapshot was replaced meanwhile: let a sync settle it.
                    drop(remote);
                    self.post_remote_resync("snapshot replaced during a write");
                    return;
                };
                // The writer releases its request/cache lock before invoking this
                // listener. A later reply can therefore publish first; an older
                // callback must not move the open snapshot's revision backwards.
                state.revision = state.revision.max(revision);
                let revision = state.revision;
                // Our own write is not "a newer revision on the server".
                if matches!(
                    remote.check,
                    Some(RevisionCheck::NewRevision { server, .. }) if server <= revision
                ) {
                    remote.check = Some(RevisionCheck::UpToDate { revision });
                }
                drop(remote);
                let c = self.clone();
                post(self.ui.clone(), move |ui| c.publish_data_source(&ui));
            }
            WriteEvent::Resync { reason } => {
                let current = self
                    .remote
                    .lock()
                    .unwrap()
                    .state
                    .as_ref()
                    .is_some_and(|state| {
                        state.library_id == library_id && state.database_file == database_file
                    });
                if current && self.remote_read_only() {
                    self.post_remote_resync(&reason);
                }
            }
            WriteEvent::LibraryChanged { reason } => {
                let mut remote = self.remote.lock().unwrap();
                if !self.remote_read_only()
                    || !remote.state.as_ref().is_some_and(|state| {
                        state.library_id == library_id && state.database_file == database_file
                    })
                {
                    return;
                }
                remote.library_changed = true;
                drop(remote);
                let c = self.clone();
                post(self.ui.clone(), move |ui| {
                    if c.require_remote_write_identity().is_ok() {
                        return;
                    }
                    c.publish_data_source(&ui);
                    // Close recovery owns the status line after a rejected close.
                    if c.closing.load(Ordering::Acquire)
                        || c.close_retains_status.load(Ordering::Acquire)
                        || ui.get_closing()
                    {
                        return;
                    }
                    ui.set_status(reason.into());
                });
                self.post_remote_resync("server library identity changed");
            }
        }
    }

    /// A displayed snapshot's IDs can only be sent to the same server library.
    pub(super) fn require_remote_write_identity(&self) -> Result<(), String> {
        if self.remote_read_only() && self.remote.lock().unwrap().library_changed {
            return Err(cn_or(
                self.chinese.load(Ordering::Acquire),
                "服务器素材库已更换，请同步新素材库后再编辑或管理",
                "The server library was replaced; sync the new library before editing or managing it",
            ));
        }
        Ok(())
    }

    /// Snapshot owning edits that cannot be retried after a server library replacement.
    /// The caller must drain Metadata writes before using this for recovery.
    pub(super) fn remote_recovery_snapshot(&self) -> Option<CacheState> {
        if !self.remote_read_only() {
            return None;
        }
        let remote = self.remote.lock().unwrap();
        remote
            .library_changed
            .then(|| remote.state.clone())
            .flatten()
    }

    /// A rejected bound management request is also a known identity replacement.
    pub(super) fn note_remote_identity_error(
        &self,
        ui: &AppWindow,
        snapshot: &CacheState,
        error: &RemoteError,
    ) {
        let mut remote = self.remote.lock().unwrap();
        if matches!(error, RemoteError::LibraryChanged(_))
            && self.remote_read_only()
            && remote.state.as_ref().is_some_and(|current| {
                current.library_id == snapshot.library_id
                    && current.database_file == snapshot.database_file
            })
        {
            remote.library_changed = true;
            drop(remote);
            self.publish_data_source(ui);
        }
    }

    /// Background management results belong to the snapshot that created their client.
    pub(super) fn remote_snapshot_current(&self, snapshot: &CacheState) -> bool {
        self.remote_read_only()
            && self
                .remote
                .lock()
                .unwrap()
                .state
                .as_ref()
                .is_some_and(|current| {
                    current.library_id == snapshot.library_id
                        && current.database_file == snapshot.database_file
                })
    }

    /// Start a normal snapshot sync once the library is idle (a batch edit is itself a
    /// library operation, and a busy library cannot swap snapshots). A play-count write
    /// must not stop the audition which triggered it merely to refresh its snapshot.
    fn post_remote_resync(self: &Arc<Self>, reason: &str) {
        crate::diagnostics::record(crate::diagnostics::Event::RemoteResyncRequested);
        let category = if reason.starts_with("cached revision ") {
            Some(crate::diagnostics::Event::CacheRevisionMismatch)
        } else if reason.starts_with("could not update the cached snapshot:") {
            Some(crate::diagnostics::Event::CachePatchFailed)
        } else if reason == "no cached snapshot for this library" {
            Some(crate::diagnostics::Event::CacheMissing)
        } else if reason.starts_with("could not record the new revision:") {
            Some(crate::diagnostics::Event::CacheRevisionSaveFailed)
        } else if reason.starts_with("write outcome uncertain:") {
            Some(crate::diagnostics::Event::RemoteWriteOutcomeUnknown)
        } else {
            None
        };
        if let Some(category) = category {
            crate::diagnostics::record(category);
        }
        let source_generation = self.source_generation.load(Ordering::Acquire);
        {
            let mut remote = self.remote.lock().unwrap();
            if remote.resync_pending == Some(source_generation) {
                return;
            }
            remote.resync_pending = Some(source_generation);
        }
        tracing::info!(%reason, "server write: running a full snapshot sync");
        let c = self.clone();
        std::thread::spawn(move || {
            while !c.shutdown.load(Ordering::Acquire)
                && !c.closing.load(Ordering::Acquire)
                && c.remote_read_only()
                && c.source_generation.load(Ordering::Acquire) == source_generation
                && (!c.available() || c.snapshot_refresh_interrupts_playback())
            {
                std::thread::sleep(Duration::from_millis(100));
            }
            let c2 = c.clone();
            post(c.ui.clone(), move |_| {
                {
                    let mut remote = c2.remote.lock().unwrap();
                    if remote.resync_pending != Some(source_generation) {
                        return;
                    }
                    remote.resync_pending = None;
                }
                if c2.remote_read_only()
                    && !c2.shutdown.load(Ordering::Acquire)
                    && !c2.closing.load(Ordering::Acquire)
                    && c2.source_generation.load(Ordering::Acquire) == source_generation
                {
                    if c2.snapshot_refresh_interrupts_playback() || !c2.available() {
                        // Playback or a library operation can begin after worker admission.
                        c2.post_remote_resync("waiting for the current audition to finish");
                    } else {
                        c2.start_sync(false, false);
                    }
                }
            });
        });
    }

    /// Only a refresh of the same endpoint and library may retain navigation IDs.
    pub(super) fn is_same_library_snapshot_refresh(&self, target: &DataSourceSwitch) -> bool {
        let DataSourceSwitch::Server {
            state: Some(next), ..
        } = target
        else {
            return false;
        };
        self.remote_read_only()
            && !self.source_changing.load(Ordering::Acquire)
            && self
                .remote
                .lock()
                .unwrap()
                .state
                .as_ref()
                .is_some_and(|current| {
                    current.library_id == next.library_id
                        && cached_source_matches(&current.server_url, &next.server_url)
                })
    }

    /// Automatic refresh waits for an active, paused or still-loading audition.
    /// Stop/Ended ends that audition and allows releasing its snapshot stream.
    fn snapshot_refresh_interrupts_playback(&self) -> bool {
        matches!(
            self.audio.status().state,
            audiolib_audio::PlaybackState::Loading
                | audiolib_audio::PlaybackState::Playing
                | audiolib_audio::PlaybackState::Paused
        )
    }

    /// Server record id of a playback-cache path handed to the engine.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn remote_audio_id(&self, path: &Path) -> Option<i64> {
        self.remote.lock().unwrap().audio_ids.get(path).copied()
    }

    pub(super) fn remote_wave_file(&self, path: &Path) -> Option<AudioFile> {
        self.remote.lock().unwrap().audio_files.get(path).cloned()
    }

    /// Set the live root status for isolated playback transition fixtures.
    #[cfg(test)]
    pub(super) fn set_remote_live_roots_for_test(&self, roots: Vec<RemoteRoot>) {
        self.remote.lock().unwrap().live_roots = Some(roots);
    }

    /// Associate an owned playback fixture with its original server record.
    #[cfg(test)]
    pub(super) fn set_remote_wave_file_for_test(&self, path: PathBuf, file: AudioFile) {
        self.remote.lock().unwrap().audio_files.insert(path, file);
    }

    /// Registration identities retained for isolated playback cleanup assertions.
    #[cfg(test)]
    pub(super) fn remote_playback_paths_for_test(&self) -> Vec<PathBuf> {
        self.remote
            .lock()
            .unwrap()
            .audio_files
            .keys()
            .cloned()
            .collect()
    }

    /// Name shown for the file the engine has loaded: the record's original file name for
    /// a server-mode playback-cache copy, otherwise the path's own file name (local mode).
    pub(super) fn playback_display_name(&self, path: &Path) -> String {
        let remote = self.remote.lock().unwrap();
        playback_name(path, remote.audio_names.get(path).map(String::as_str))
    }

    /// Wait off the UI thread after source/library/close admission has been closed.
    /// Sent plays finish their reply and settlement; queued unsent plays cancel.
    pub(super) fn wait_remote_plays(&self) -> Result<(), String> {
        let mut plays = self.remote_plays.lock().unwrap();
        while plays.workers != 0 {
            if self.shutdown.load(Ordering::Acquire) {
                return Err("Desktop is shutting down".into());
            }
            plays = self
                .remote_plays_wake
                .wait_timeout(plays, Duration::from_millis(100))
                .unwrap()
                .0;
        }
        Ok(())
    }

    fn data_root(&self) -> PathBuf {
        self.config.lock().unwrap().data_root()
    }

    fn secrets(&self) -> RemoteSecrets {
        let mut remote = self.remote.lock().unwrap();
        if !remote.secrets_loaded {
            let config = self.config.lock().unwrap();
            let (root, settings) = (config.data_root(), config.data_source.clone());
            drop(config);
            match load_profile_secrets(&root, &settings) {
                Ok(secrets) => remote.secrets = secrets,
                Err(error) => remote.last_error = Some(error.to_string()),
            }
            remote.secrets_loaded = true;
        }
        remote.secrets.clone()
    }

    /// Initialize server-mode state after construction; starts the startup sync.
    pub(super) fn init_remote(self: &Arc<Self>, startup: Option<RemoteStartup>) {
        self.start_revision_poller();
        if let Some(startup) = startup {
            let deferred = startup.state.is_none();
            self.remote_mode.store(true, Ordering::Release);
            self.remote.lock().unwrap().state = startup.state;
            if let Some(ui) = self.ui.upgrade() {
                self.publish_data_source(&ui);
                // The GUI always starts on the placeholder and lets the sync pipeline
                // prepare the cache, so `deferred` alone is no reason to show the
                // manager. Open it only when there is no usable cached snapshot for
                // the configured server (first run or a changed address); revision
                // drift or an unreachable server are reported by the sync itself.
                if deferred {
                    let (root, url) = {
                        let config = self.config.lock().unwrap();
                        (config.data_root(), config.data_source.server_url.clone())
                    };
                    let cached = RemoteCache::new(&root).active();
                    if startup_needs_manager(cached.as_ref(), &url) {
                        ui.invoke_open_data_source();
                    } else {
                        self.remote.lock().unwrap().startup_check = true;
                    }
                }
            }
            self.start_sync(false, deferred);
        } else if let Some(ui) = self.ui.upgrade() {
            self.publish_data_source(&ui);
        }
    }

    /// Bind the data source callbacks (the unified window's in `source_manager`).
    pub(super) fn bind_remote(self: &Arc<Self>, ui: &AppWindow) {
        self.bind_source_manager(ui);
        let c = self.clone();
        ui.on_remote_write_refused(move || {
            c.refuse_remote_write();
        });
        let c = self.clone();
        ui.on_data_source_address_edited(move || {
            if let Some(ui) = c.ui.upgrade() {
                c.refresh_address_hint(&ui);
            }
        });
        let c = self.clone();
        ui.on_data_source_address_form_changed(move || {
            if let Some(ui) = c.ui.upgrade() {
                c.switch_address_form(&ui);
            }
        });
        let c = self.clone();
        ui.on_data_source_sync_requested(move || {
            if c.config.lock().unwrap().data_source.mode != DataSourceMode::Server
                || !c.remote_read_only()
            {
                if let Some(ui) = c.ui.upgrade() {
                    ui.set_ds_message(
                        cn_or(
                            c.chinese.load(Ordering::Acquire),
                            "请先「切换到此数据源」，再同步。",
                            "Switch to this source before syncing.",
                        )
                        .into(),
                    );
                }
                return;
            }
            // Sync now follows the revision rule; the CLI offers --force re-downloads.
            c.start_sync_requested(false, false, false);
        });
        let c = self.clone();
        ui.on_data_source_sync_pause_requested(move || {
            if c.remote_busy.load(Ordering::Acquire)
                && !c.closing.load(Ordering::Acquire)
                && let Some(ui) = c.ui.upgrade()
                && ui.get_ds_sync_can_pause()
                && !ui.get_ds_sync_pausing()
            {
                c.remote_cancel.lock().unwrap().pause();
                ui.set_ds_sync_pausing(true);
                ui.set_ds_sync_info(
                    cn_or(
                        c.chinese.load(Ordering::Acquire),
                        "正在暂停并保存下载进度…",
                        "Pausing and saving download progress…",
                    )
                    .into(),
                );
            }
        });
        let c = self.clone();
        ui.on_data_source_sync_restart_requested(move || {
            if c.remote_read_only() && c.available() {
                c.start_sync_requested(true, false, true);
            }
        });
        let c = self.clone();
        ui.on_remote_notice_action(move || {
            let Some(ui) = c.ui.upgrade() else { return };
            match ui.get_remote_notice_kind() {
                NOTICE_UPDATE => {
                    if c.remote_read_only() && c.available() {
                        c.start_sync_requested(false, false, false);
                    }
                }
                NOTICE_RETRY => c.retry_remote_notice(),
                NOTICE_SETTINGS if !ui.get_library_visible() => {
                    ui.invoke_open_data_source();
                }
                _ => {}
            }
        });
        let c = self.clone();
        ui.on_remote_notice_dismissed(move || {
            let Some(ui) = c.ui.upgrade() else { return };
            let key = c.notice_key();
            c.remote.lock().unwrap().dismissed_notice = key;
            c.publish_remote_notice(&ui);
        });
    }

    /// Retry on the server notice. A revision notice (server offline, starting…) re-checks
    /// the server now; the offline-root notice asks the server to re-check each offline
    /// root (`POST /roots/{name}/check`, about 2 s) and keeps the notice while a root stays
    /// offline. Servers without that endpoint (404) are probed with a `HEAD` of the
    /// selected record's stream when it lies in the root. Never blocks the event loop.
    fn retry_remote_notice(self: &Arc<Self>) {
        if !self.available() || !self.remote_read_only() {
            return;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        let auto_sync = self.config.lock().unwrap().data_source.auto_sync;
        let names: Vec<String> = {
            let remote = self.remote.lock().unwrap();
            let auto = AutoSync::from_settings(auto_sync, remote.auto_deferred);
            let revision_pending = remote
                .check
                .as_ref()
                .and_then(|check| revision_notice(check, chinese, auto, None))
                .is_some();
            if revision_pending {
                Vec::new()
            } else {
                remote
                    .live_roots
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .filter(|root| root.state() == RootState::Offline)
                    .map(|root| root.name.clone())
                    .collect()
            }
        };
        if names.is_empty() {
            self.wake_revision_poller();
            return;
        }
        let settings = self.config.lock().unwrap().data_source.clone();
        let secrets = self.secrets();
        let selected = self.selected.lock().unwrap().clone();
        let source_generation = self.source_generation.load(Ordering::Acquire);
        self.status(if chinese {
            format!("正在重新检查音效库根目录：{}", names.join("、"))
        } else {
            format!("Rechecking library roots: {}", names.join(", "))
        });
        let c = self.clone();
        std::thread::spawn(move || {
            let client = match RemoteClient::new(&settings.server_url, &secrets.token) {
                Ok(client) => client,
                Err(_) => {
                    c.wake_revision_poller();
                    return;
                }
            };
            let known = c.known_roots();
            let mut updated = Vec::new();
            let mut still_offline = Vec::new();
            for name in names {
                match client.check_root(&name) {
                    Ok(root) => {
                        if root.state() == RootState::Offline {
                            still_offline.push(name);
                        }
                        updated.push(root);
                    }
                    // An older server without the endpoint: a stream HEAD of the selected
                    // record makes the server look at the root (it rechecks inline).
                    Err(RemoteError::NotFound(_)) => {
                        let root = known.iter().find(|root| root.name == name);
                        let probe = selected.as_ref().filter(|file| {
                            root.is_some_and(|root| {
                                root_for_path(&file.file_path, std::slice::from_ref(root)).is_some()
                            })
                        });
                        let online = probe.is_some_and(|file| {
                            audiolib_remote::probe_download(
                                &client
                                    .stream_source(file.id)
                                    .with_first_byte_timeout(Duration::from_secs(5)),
                            )
                            .is_ok()
                        });
                        match root {
                            Some(root) if online => updated.push(RemoteRoot {
                                online: Some(true),
                                status: Some("online".into()),
                                status_message: None,
                                ..root.clone()
                            }),
                            _ => still_offline.push(name),
                        }
                    }
                    Err(error) => {
                        tracing::info!(%error, root = %name, "root recheck failed");
                        still_offline.push(name);
                    }
                }
            }
            let message = root_retry_message(&still_offline, chinese);
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                if c2.source_generation.load(Ordering::Acquire) != source_generation
                    || !c2.remote_read_only()
                    || !c2.available()
                {
                    return;
                }
                if let Some(live) = c2.remote.lock().unwrap().live_roots.as_mut() {
                    merge_roots(live, &updated);
                }
                c2.list_waveforms.roots_online(&online_root_names(&updated));
                c2.publish_remote_notice(&ui);
                c2.publish_data_source(&ui);
                ui.set_status(message.into());
            });
        });
    }

    /// Check the server revision now (Retry, settings saved), outside the schedule.
    pub(super) fn wake_revision_poller(&self) {
        *self.poll_signal.lock().unwrap() += 1;
        self.poll_wake.notify_all();
    }

    /// Background revision poller: one thread for the controller lifetime, idle outside
    /// server mode. Network checks never run on the event loop; results are posted back
    /// and discarded when the open snapshot changed meanwhile.
    fn start_revision_poller(self: &Arc<Self>) {
        let c = self.clone();
        let spawned = std::thread::Builder::new()
            .name("audiolib-remote-poll".into())
            .spawn(move || c.revision_poll_loop());
        if let Err(error) = spawned {
            tracing::warn!(%error, "Revision poller unavailable");
        }
    }

    fn revision_poll_loop(self: Arc<Self>) {
        let mut seen = *self.poll_signal.lock().unwrap();
        let mut interval = self.config.lock().unwrap().data_source.poll_interval_secs;
        let mut schedule = PollSchedule::new(interval);
        let mut gate = AutoSyncGate::new(schedule.interval());
        // The startup sync performs the first check; poll one interval later.
        let mut deadline = schedule
            .enabled()
            .then(|| Instant::now() + schedule.interval());
        loop {
            {
                let mut signal = self.poll_signal.lock().unwrap();
                loop {
                    if self.shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    if *signal != seen {
                        seen = *signal;
                        break;
                    }
                    if let Some(hint) = self.remote.lock().unwrap().retry_hint.take() {
                        deadline = Some(deadline.map_or(hint, |deadline| deadline.min(hint)));
                    }
                    let now = Instant::now();
                    if deadline.is_some_and(|deadline| now >= deadline) {
                        break;
                    }
                    let wait = deadline
                        .map_or(Duration::from_secs(1), |deadline| deadline - now)
                        .min(Duration::from_secs(1));
                    signal = self.poll_wake.wait_timeout(signal, wait).unwrap().0;
                }
            }
            let source_generation = self.source_generation.load(Ordering::Acquire);
            let settings = self.config.lock().unwrap().data_source.clone();
            if settings.poll_interval_secs != interval {
                interval = settings.poll_interval_secs;
                schedule = PollSchedule::new(interval);
                gate.set_quiet(schedule.interval());
            }
            let next_interval = || {
                schedule
                    .enabled()
                    .then(|| Instant::now() + schedule.interval())
            };
            if settings.mode != DataSourceMode::Server || !self.remote_read_only() {
                deadline = next_interval();
                continue;
            }
            if self.remote_busy.load(Ordering::Acquire) || !self.available() {
                // A running sync is itself a revision check.
                deadline = next_interval();
                continue;
            }
            let chinese = self.chinese.load(Ordering::Acquire);
            let open = self.remote.lock().unwrap().state.clone();
            // A replaced snapshot stays open until the last reader of the previous provider
            // is gone; finish removing it here. `cleanup_after_swap` does nothing while a
            // newer sync waits for its swap and always keeps the snapshot cache.json names.
            if let Some(state) = &open {
                RemoteCache::new(&self.data_root()).cleanup_after_swap(state);
            }
            let checked = open.as_ref().map(|state| CachedIdentity {
                library_id: state.library_id.clone(),
                revision: state.revision,
            });
            let secrets = self.secrets();
            if self.source_generation.load(Ordering::Acquire) != source_generation
                || !self.available()
            {
                deadline = next_interval();
                continue;
            }
            // Root status (§6.4) rides on the same cadence; `/roots` never touches the NAS.
            let (check, roots) = match RemoteClient::new(&settings.server_url, &secrets.token) {
                // `/roots` is fetched even when `/library` is slow.
                Ok(client) => check_revision_with_roots(&client, checked.as_ref(), chinese),
                Err(error) => (RevisionCheck::Failed(error.localized(chinese)), None),
            };
            let delay = schedule.next_delay(&check);
            let auto_due = gate.observe(&check, Instant::now());
            deadline = delay.map(|delay| Instant::now() + delay);
            let next_check = delay.map(|delay| unix_now() + delay.as_secs());
            tracing::debug!(
                ?check,
                failures = schedule.failures(),
                "Server revision check"
            );
            let c = self.clone();
            post(self.ui.clone(), move |ui| {
                c.apply_revision_poll(
                    &ui,
                    RevisionPoll {
                        source_generation,
                        checked,
                        check,
                        roots,
                        next_check,
                        auto_due,
                    },
                );
            });
        }
    }

    /// Publish roots only after the same connection and snapshot checks as its revision.
    /// A delayed old server response must never change the current audition route.
    fn apply_revision_poll(self: &Arc<Self>, ui: &AppWindow, poll: RevisionPoll) {
        if self.source_generation.load(Ordering::Acquire) != poll.source_generation
            || !self.remote_read_only()
            || !self.available()
        {
            return;
        }
        let roots_changed = {
            let mut remote = self.remote.lock().unwrap();
            let current = remote.state.as_ref().map(|state| CachedIdentity {
                library_id: state.library_id.clone(),
                revision: state.revision,
            });
            if current != poll.checked {
                return;
            }
            let roots_changed = poll.roots.as_deref().is_some_and(|roots| {
                remote
                    .live_roots
                    .as_deref()
                    .or_else(|| remote.state.as_ref().map(|state| state.roots.as_slice()))
                    .is_some_and(|previous| {
                        source_manager::root_configuration_changed(previous, roots)
                    })
            });
            if let Some(roots) = poll.roots.as_deref() {
                self.list_waveforms.roots_online(&online_root_names(roots));
            }
            remote.live_roots = poll.roots;
            roots_changed
        };
        if roots_changed {
            self.refresh_source_manager_roots(ui);
        }
        self.apply_revision_check(ui, poll.checked, poll.check, poll.next_check, poll.auto_due);
    }

    /// Event-loop side of a revision check: record it, auto-sync when configured and due
    /// (`auto_due`, from [`AutoSyncGate`]) and refresh the notice. Results for a snapshot
    /// that has since been replaced are stale.
    fn apply_revision_check(
        self: &Arc<Self>,
        ui: &AppWindow,
        checked: Option<CachedIdentity>,
        check: RevisionCheck,
        next_check: Option<u64>,
        auto_due: bool,
    ) {
        if !self.remote_read_only() || self.shutdown.load(Ordering::Acquire) {
            return;
        }
        let current = self
            .remote
            .lock()
            .unwrap()
            .state
            .as_ref()
            .map(|state| CachedIdentity {
                library_id: state.library_id.clone(),
                revision: state.revision,
            });
        if current != checked {
            return;
        }
        let auto_sync = self.config.lock().unwrap().data_source.auto_sync;
        let update = check.update_available();
        let audition_active = self.snapshot_refresh_interrupts_playback();
        {
            let mut remote = self.remote.lock().unwrap();
            if matches!(check, RevisionCheck::LibraryChanged { .. }) {
                remote.library_changed = true;
            }
            remote.check = Some(check);
            remote.next_check = next_check;
            remote.auto_deferred = update && auto_sync && (!auto_due || audition_active);
        }
        if update
            && auto_sync
            && auto_due
            && !audition_active
            && self.available()
            && !self.remote_busy.load(Ordering::Acquire)
        {
            self.start_sync(false, false);
        }
        self.publish_data_source(ui);
    }

    /// Identity of the current notice without its retry time, so a dismissed notice
    /// stays hidden across retries until the situation changes.
    fn notice_key(&self) -> Option<String> {
        let chinese = self.chinese.load(Ordering::Acquire);
        let auto_sync = self.config.lock().unwrap().data_source.auto_sync;
        let remote = self.remote.lock().unwrap();
        if let Some(pending) = remote.pending_sync.as_ref().filter(|p| p.manual_paused) {
            return Some(pending_sync_text(pending, chinese));
        }
        let auto = AutoSync::from_settings(auto_sync, remote.auto_deferred);
        remote
            .check
            .as_ref()
            .and_then(|check| revision_notice(check, chinese, auto, None))
            .map(|(text, _)| text)
            .or_else(|| {
                remote
                    .live_roots
                    .as_deref()
                    .and_then(|roots| root_notice(roots, chinese))
            })
    }

    /// Publish the non-blocking server notice (status area under the header).
    pub(super) fn publish_remote_notice(&self, ui: &AppWindow) {
        let chinese = self.chinese.load(Ordering::Acquire);
        let auto_sync = self.config.lock().unwrap().data_source.auto_sync;
        let key = self.notice_key();
        let remote = self.remote.lock().unwrap();
        let visible = self.remote_read_only() && key.is_some() && remote.dismissed_notice != key;
        let notice = visible
            .then(|| {
                if let Some(pending) = remote.pending_sync.as_ref().filter(|p| p.manual_paused) {
                    return Some((pending_sync_text(pending, chinese), NOTICE_UPDATE));
                }
                remote
                    .check
                    .as_ref()
                    .and_then(|check| {
                        let retry_at = remote.next_check.map(format_clock);
                        let auto = AutoSync::from_settings(auto_sync, remote.auto_deferred);
                        revision_notice(check, chinese, auto, retry_at.as_deref())
                    })
                    // Offline roots, when nothing about the library itself is pending;
                    // Retry re-checks the server (and its roots) at once.
                    .or_else(|| {
                        remote
                            .live_roots
                            .as_deref()
                            .and_then(|roots| root_notice(roots, chinese))
                            .map(|text| (text, NOTICE_RETRY))
                    })
            })
            .flatten();
        let (text, kind) = notice.unwrap_or((String::new(), NOTICE_NONE));
        ui.set_remote_notice(text.into());
        ui.set_remote_notice_kind(kind);
    }

    /// Header/status identity: current mode, server and library revision.
    pub(super) fn publish_data_source(&self, ui: &AppWindow) {
        self.publish_manage_busy(ui);
        let chinese = self.chinese.load(Ordering::Acquire);
        let remote = self.remote_read_only();
        ui.set_remote_mode(remote);
        ui.set_remote_writes_blocked(remote && self.remote.lock().unwrap().library_changed);
        ui.set_metadata_writes_blocked(
            self.remote_metadata_blocked()
                || (remote && self.remote.lock().unwrap().library_changed),
        );
        self.publish_server_manage(ui);
        let label = if !remote {
            cn_or(chinese, "本地模式", "Local mode")
        } else {
            let server = host_of(&self.config.lock().unwrap().data_source.server_url);
            let remote_state = self.remote.lock().unwrap();
            // Freshness of the open snapshot from the latest background check.
            let freshness = match &remote_state.check {
                Some(RevisionCheck::Offline(_)) => cn_or(chinese, " · 离线", " · offline"),
                Some(RevisionCheck::Slow(_)) => cn_or(chinese, " · 响应慢", " · slow"),
                Some(RevisionCheck::Starting { .. }) => {
                    cn_or(chinese, " · 服务器启动中", " · server starting")
                }
                Some(RevisionCheck::NewRevision { server, .. }) => {
                    if chinese {
                        format!(" · 有更新 r{server}")
                    } else {
                        format!(" · update r{server}")
                    }
                }
                Some(RevisionCheck::LibraryChanged { .. }) => {
                    cn_or(chinese, " · 库已更换", " · library replaced")
                }
                _ => String::new(),
            };
            let freshness = freshness
                + &remote_state
                    .live_roots
                    .as_deref()
                    .map(|roots| root_summary(roots, chinese))
                    .unwrap_or_default();
            match &remote_state.state {
                Some(state) => {
                    let synced = format_time(state.checked_at.max(state.downloaded_at));
                    if chinese {
                        format!(
                            "服务器模式 · {server} · 库 {} · r{} · 同步于 {synced}{freshness}",
                            short_id(&state.library_id),
                            state.revision
                        )
                    } else {
                        format!(
                            "Server mode · {server} · library {} · r{} · synced {synced}{freshness}",
                            short_id(&state.library_id),
                            state.revision
                        )
                    }
                }
                None => {
                    if chinese {
                        format!("服务器模式 · {server} · 尚未同步")
                    } else {
                        format!("Server mode · {server} · not synced yet")
                    }
                }
            }
        };
        ui.set_data_source_label(label.into());
        if !ui.get_ds_syncing() {
            let pending = self.remote.lock().unwrap().pending_sync.clone();
            ui.set_ds_sync_paused(pending.as_ref().is_some_and(|p| p.manual_paused));
            ui.set_ds_sync_pending(pending.is_some());
            if let Some(pending) = pending {
                ui.set_ds_sync_info(pending_sync_text(&pending, chinese).into());
                ui.set_ds_progress(
                    pending
                        .total_bytes
                        .filter(|total| *total > 0)
                        .map_or(-1.0, |total| {
                            (pending.downloaded_bytes as f64 / total as f64).clamp(0.0, 1.0) as f32
                        }),
                );
            } else {
                ui.set_ds_sync_info(self.sync_info(chinese).into());
            }
        }
        self.publish_remote_notice(ui);
        if ui.get_library_visible() {
            self.publish_sources(ui);
        }
    }

    fn sync_info(&self, chinese: bool) -> String {
        let remote = self.remote.lock().unwrap();
        match (&remote.state, self.remote_read_only()) {
            (Some(state), true) => {
                if chinese {
                    format!(
                        "当前缓存：库 {} · 版本 r{} · {} 个音效 · 快照 {:.1} MiB · 下载于 {} · 最近检查 {}",
                        state.library_id,
                        state.revision,
                        state.audio_files,
                        state.snapshot_bytes as f64 / 1_048_576.0,
                        format_time(state.downloaded_at),
                        format_time(state.checked_at)
                    )
                } else {
                    format!(
                        "Cache: library {} · revision {} · {} sounds · snapshot {:.1} MiB · downloaded {} · checked {}",
                        state.library_id,
                        state.revision,
                        state.audio_files,
                        state.snapshot_bytes as f64 / 1_048_576.0,
                        format_time(state.downloaded_at),
                        format_time(state.checked_at)
                    )
                }
            }
            (None, true) => cn_or(
                chinese,
                "尚未同步服务器素材库。",
                "The server library has not been synced yet.",
            ),
            _ => cn_or(chinese, "当前使用本地素材库。", "Using the local library."),
        }
    }

    fn known_roots(&self) -> Vec<RemoteRoot> {
        let remote = self.remote.lock().unwrap();
        if let Some(roots) = &remote.live_roots {
            return roots.clone();
        }
        if let Some(state) = &remote.state {
            return without_status(&state.roots);
        }
        drop(remote);
        RemoteCache::new(&self.data_root())
            .active()
            .map(|state| without_status(&state.roots))
            .unwrap_or_default()
    }

    /// Show the normalised API address (or why the input is invalid) under the fields.
    fn refresh_address_hint(&self, ui: &AppWindow) {
        let chinese = self.chinese.load(Ordering::Acquire);
        let address = AddressForm::read(ui).address();
        let (hint, error) = address_hint(&address, chinese);
        ui.set_ds_address_hint(hint.into());
        ui.set_ds_address_error(error);
        // Until the user changes it, the "transcode auditions" box shows what saving this
        // address would store (the profile's own choice, or the address class default).
        if !ui.get_ds_transcode_touched() {
            let url = address
                .map(|address| address.base_url())
                .unwrap_or_default();
            ui.set_ds_transcode(self.form_transcode(&url));
        }
    }

    /// Carry the address over when switching between the full URL and domain + port
    /// forms (invalid input stays where it was typed).
    fn switch_address_form(&self, ui: &AppWindow) {
        let form = AddressForm::read(ui);
        if form.parts {
            if let Ok(address) = ServerAddress::parse_input(&form.url, Scheme::Https) {
                set_address_parts(ui, Some(&address));
            }
        } else {
            let parts = AddressForm {
                parts: true,
                ..form
            };
            if let Ok(address) = parts.address() {
                ui.set_ds_server_url(address.base_url().into());
            }
        }
        self.refresh_address_hint(ui);
    }

    /// Check the server revision and download/validate a new snapshot when needed, then
    /// swap the provider. `apply` also switches from local mode, falling back to the last
    /// cache (or the empty placeholder) with a clear error when the server is unavailable.
    pub(super) fn start_sync(self: &Arc<Self>, force: bool, apply: bool) {
        self.start_sync_with_request(
            SyncOptions {
                force,
                restart: false,
            },
            apply,
            false,
        );
    }

    /// An explicit continue, restart or source selection releases a persisted
    /// manual pause. Background checks and write recovery use `start_sync` instead.
    pub(super) fn start_sync_requested(self: &Arc<Self>, force: bool, apply: bool, restart: bool) {
        self.start_sync_with_request(SyncOptions { force, restart }, apply, true);
    }

    fn start_sync_with_request(
        self: &Arc<Self>,
        options: SyncOptions,
        apply: bool,
        requested: bool,
    ) {
        if self.shutdown.load(Ordering::Acquire)
            || self.closing.load(Ordering::Acquire)
            || self.config.lock().unwrap().data_source.mode != DataSourceMode::Server
        {
            return;
        }
        if !requested
            && self
                .remote
                .lock()
                .unwrap()
                .pending_sync
                .as_ref()
                .is_some_and(|p| p.manual_paused)
        {
            return;
        }
        if apply
            && self.source_changing.load(Ordering::Acquire)
            && let Some(ui) = self.ui.upgrade()
        {
            // Settings are now saved; local escape is safe until provider opening.
            ui.set_ds_source_syncing(true);
        }
        let already_busy = {
            let plays = self.remote_plays.lock().unwrap();
            // Owned cache work releases `remote_busy` before its event-loop
            // publication so close can drain without UI delivery. Until that
            // publication settles, another sync must not supersede its generation.
            if self.remote_busy.load(Ordering::Acquire) || plays.publication_gates != 0 {
                true
            } else {
                self.remote_busy.store(true, Ordering::Release);
                false
            }
        };
        if already_busy {
            if apply || requested {
                // A running sync would be superseded; restart after it notices cancel.
                let generation = self.remote_generation.load(Ordering::Acquire);
                let source_generation = self.source_generation.load(Ordering::Acquire);
                let c = self.clone();
                std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while c.remote_sync_pending() && Instant::now() < deadline {
                        if c.remote_generation.load(Ordering::Acquire) != generation
                            || c.source_generation.load(Ordering::Acquire) != source_generation
                        {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    let c2 = c.clone();
                    post(c.ui.clone(), move |_| {
                        if c2.remote_generation.load(Ordering::Acquire) == generation
                            && c2.source_generation.load(Ordering::Acquire) == source_generation
                        {
                            c2.start_sync_with_request(options, apply, requested);
                        }
                    });
                });
            }
            return;
        }
        // Establish ownership before settings preparation or spawning can fail.
        let busy_guard = RemoteSyncBusyGuard(Some(self.clone()));
        let play_publication_gate = {
            let mut plays = self.remote_plays.lock().unwrap();
            plays.publication_gates += 1;
            RemotePlayPublicationGate(self.clone())
        };
        let chinese = self.chinese.load(Ordering::Acquire);
        let generation = self.remote_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let control = SyncControl::new();
        *self.remote_cancel.lock().unwrap() = control.clone();
        let settings = self.config.lock().unwrap().data_source.clone();
        let secrets = self.secrets();
        let data_root = self.data_root();
        if requested && let Some(pending) = self.remote.lock().unwrap().pending_sync.as_mut() {
            pending.manual_paused = false;
        }
        if let Some(ui) = self.ui.upgrade() {
            ui.set_ds_syncing(true);
            ui.set_ds_sync_pausing(false);
            ui.set_ds_sync_paused(false);
            ui.set_ds_sync_pending(false);
            ui.set_ds_sync_can_pause(true);
            ui.set_ds_progress(-1.0);
            ui.set_ds_decompression_info("".into());
            ui.set_ds_decompression_progress(-1.0);
            ui.set_ds_sync_info(
                cn_or(chinese, "正在检查服务器版本…", "Checking server revision…").into(),
            );
            self.publish_remote_notice(&ui);
        }
        let c = self.clone();
        std::thread::spawn(move || {
            // On unwind, later cache locals drain before this guard releases busy.
            let sync_busy_guard = busy_guard;
            let cache = RemoteCache::new(&data_root);
            let mut progress_reporter = remote_sync_progress::ProgressReporter::default();
            let cached_preparation_incomplete = std::cell::Cell::new(false);
            let mut report = |progress| {
                match progress {
                    SyncProgress::Preparing(audiolib_remote::CachePreparation::Checking) => {
                        cached_preparation_incomplete.set(true);
                    }
                    // Reaching publication means cache preparation succeeded, even
                    // if saving the refreshed server/check metadata then fails.
                    SyncProgress::Saving => cached_preparation_incomplete.set(false),
                    _ => {}
                }
                let Some(display) = progress_reporter.report(progress, chinese, Instant::now())
                else {
                    return;
                };
                let c2 = c.clone();
                post(c.ui.clone(), move |ui| {
                    if c2.remote_generation.load(Ordering::Acquire) == generation
                        && !ui.get_ds_sync_pausing()
                    {
                        ui.set_ds_sync_can_pause(!matches!(progress, SyncProgress::Saving));
                        ui.set_ds_progress(display.fraction);
                        ui.set_ds_sync_info(display.text.clone().into());
                        ui.set_ds_decompression_progress(display.decompression_fraction);
                        ui.set_ds_decompression_info(display.decompression_text.clone().into());
                        let status = if display.decompression_text.is_empty() {
                            display.text
                        } else {
                            format!("{} · {}", display.text, display.decompression_text)
                        };
                        ui.set_status(status.into());
                    }
                });
            };
            let result = c
                .wait_remote_plays()
                .map_err(|_| RemoteError::Cancelled)
                .and_then(|_| RemoteClient::new(&settings.server_url, &secrets.token))
                .and_then(|client| {
                    if !requested
                        && let Some(pending) = cache.pending_sync(client.api_url())?
                        && pending.manual_paused
                    {
                        return Ok(SyncRunOutcome::Paused(pending));
                    }
                    sync_library_with_options(&client, &cache, &mut report, &control, options)
                });
            let pending = match &result {
                Ok(SyncRunOutcome::Paused(pending)) => Some(pending.clone()),
                _ => audiolib_remote::api_base_url(&settings.server_url)
                    .ok()
                    .and_then(|api| cache.pending_sync(&api).ok().flatten()),
            };
            // A deliberate pause commits the requested source to a verified cache
            // or the owned placeholder. Never leave its old provider attached to
            // newly saved credentials, nor hold the source gate while paused.
            let paused_target = if matches!(result, Ok(SyncRunOutcome::Paused(_)))
                && apply
                && !c.closing.load(Ordering::Acquire)
                && !c.shutdown.load(Ordering::Acquire)
            {
                let cached = pending
                    .as_ref()
                    .and_then(|p| p.library_id.as_deref())
                    .and_then(|id| cache.state(id))
                    .or_else(|| cache.active())
                    .filter(|state| cached_source_matches(&state.server_url, &settings.server_url));
                let prepared = cached.and_then(|state| {
                    cache
                        .prepare_cached_state_with_progress_cancellable(
                            &state,
                            &mut |stage| report(SyncProgress::Preparing(stage)),
                            &c.closing,
                        )
                        .map(|state| (cache.database_path(&state), Some(state)))
                        .ok()
                });
                prepared.or_else(|| {
                    cache
                        .placeholder_database()
                        .ok()
                        .map(|database| (database, None))
                })
            } else {
                None
            };
            let fallback = if result
                .as_ref()
                .is_err_and(|error| !matches!(error, RemoteError::Cancelled))
                && !cached_preparation_incomplete.get()
                && apply
                && !control.cancellation().load(Ordering::Acquire)
            {
                // Offline startup uses the same preparation stages as sync. Never
                // open another endpoint's cache or retry its failed preparation.
                // A rejected *new* snapshot must still allow the valid old cache.
                cache
                    .active()
                    .filter(|state| cached_source_matches(&state.server_url, &settings.server_url))
                    .and_then(|state| {
                        cache
                            .prepare_cached_state_with_progress_cancellable(
                                &state,
                                &mut |stage| report(SyncProgress::Preparing(stage)),
                                control.cancellation(),
                            )
                            .map(|state| (cache.database_path(&state), Some(state)))
                            .map_err(
                                |error| tracing::warn!(%error, "Offline cache preparation failed"),
                            )
                            .ok()
                    })
            } else {
                None
            };
            // Cache preparation and any owned fallback are finished. Close may now
            // drain even if posting this result is rejected or never delivered.
            drop(sync_busy_guard);
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                let _play_publication_gate = play_publication_gate;
                if c2.remote_generation.load(Ordering::Acquire) != generation
                    || c2.shutdown.load(Ordering::Acquire)
                    || c2.closing.load(Ordering::Acquire)
                {
                    return;
                }
                ui.set_ds_source_syncing(false);
                ui.set_ds_syncing(false);
                ui.set_ds_sync_pausing(false);
                ui.set_ds_sync_can_pause(false);
                ui.set_ds_progress(0.0);
                ui.set_ds_decompression_info("".into());
                ui.set_ds_decompression_progress(-1.0);
                c2.remote.lock().unwrap().pending_sync = pending;
                match result {
                    Ok(SyncRunOutcome::Completed(outcome)) => {
                        c2.complete_sync(&ui, Ok(*outcome), fallback, &cache);
                    }
                    Ok(SyncRunOutcome::Paused(pending)) => {
                        c2.complete_paused_sync(&ui, pending, paused_target);
                    }
                    Err(error) => c2.complete_sync(&ui, Err(error), fallback, &cache),
                }
            });
        });
    }

    /// Both worker ownership and its queued UI publication exclude another sync.
    /// Closing deliberately drains only worker ownership, retaining bounded exit.
    fn remote_sync_pending(&self) -> bool {
        let plays = self.remote_plays.lock().unwrap();
        self.remote_busy.load(Ordering::Acquire) || plays.publication_gates != 0
    }

    fn complete_paused_sync(
        self: &Arc<Self>,
        ui: &AppWindow,
        pending: PendingSync,
        target: Option<(PathBuf, Option<CacheState>)>,
    ) {
        let message = pending_sync_text(&pending, self.chinese.load(Ordering::Acquire));
        {
            let mut remote = self.remote.lock().unwrap();
            remote.last_error = None;
            // A persisted or requested pause is a normal startup outcome; a later
            // explicit resume must not inherit the startup-only manager policy.
            remote.startup_check = false;
        }
        ui.set_ds_message("".into());
        ui.set_status(message.into());
        if let Some((database, state)) = target {
            self.swap_to(
                ui,
                DataSourceSwitch::Server {
                    database,
                    state: state.map(Box::new),
                },
            );
        } else if self.source_changing.load(Ordering::Acquire) {
            // Even an inability to create an owned placeholder must restore the
            // previous credential/provider pair through the ordinary rollback.
            self.finish_source_change(ui, false);
        }
        self.publish_data_source(ui);
    }

    fn complete_sync(
        self: &Arc<Self>,
        ui: &AppWindow,
        result: Result<audiolib_remote::SyncOutcome, RemoteError>,
        fallback: Option<(PathBuf, Option<CacheState>)>,
        cache: &RemoteCache,
    ) {
        let chinese = self.chinese.load(Ordering::Acquire);
        let startup_check = std::mem::take(&mut self.remote.lock().unwrap().startup_check);
        if self.config.lock().unwrap().data_source.mode != DataSourceMode::Server {
            return;
        }
        if startup_check && startup_sync_opens_manager(&result) && !ui.get_library_visible() {
            // Unreachable server, rejected token, unusable snapshot or a new
            // library_id: show the reason in 音效库管理 rather than the main window.
            ui.invoke_open_data_source();
        }
        match result {
            Ok(outcome) => {
                let roots_changed = {
                    let mut remote = self.remote.lock().unwrap();
                    let roots_changed = remote
                        .live_roots
                        .as_deref()
                        .or_else(|| remote.state.as_ref().map(|state| state.roots.as_slice()))
                        .is_some_and(|previous| {
                            source_manager::root_configuration_changed(
                                previous,
                                &outcome.state.roots,
                            )
                        });
                    remote.last_error = None;
                    // The sync was a successful revision check: clear stale notices.
                    remote.check = None;
                    remote.dismissed_notice = None;
                    remote.live_roots = Some(outcome.state.roots.clone());
                    roots_changed
                };
                ui.set_ds_message("".into());
                // The menus' removal gate follows the server's current `/capabilities`.
                self.refresh_server_manage();
                let database = cache.database_path(&outcome.state);
                let current = self.remote.lock().unwrap().state.clone();
                let same_snapshot = self.remote_read_only()
                    && current.as_ref().is_some_and(|state| {
                        state.library_id == outcome.state.library_id
                            && state.database_file == outcome.state.database_file
                    });
                if same_snapshot {
                    let resume_reads = self.source_changing.load(Ordering::Acquire);
                    self.finish_source_change(ui, true);
                    self.remote.lock().unwrap().state = Some(outcome.state.clone());
                    // The provider already has this exact file open. Acknowledge any
                    // deferred wire cleanup without performing disk I/O on the UI loop.
                    let cache = cache.clone();
                    let opened = outcome.state.clone();
                    std::thread::spawn(move || {
                        if let Err(error) = cache.finish_sync_transfer(&opened) {
                            tracing::warn!(%error, "Opened snapshot retained; transfer cleanup deferred");
                        }
                    });
                    if roots_changed {
                        self.refresh_source_manager_roots(ui);
                    }
                    let hidden_settled = self.settle_hidden();
                    if resume_reads {
                        self.resume_source_reads(ui, true);
                    } else if hidden_settled {
                        self.reload_tree();
                        self.request_search(ui, true, false);
                    }
                    let message = if chinese {
                        format!("服务器素材库已是最新（r{}）", outcome.state.revision)
                    } else {
                        format!(
                            "Server library is up to date (revision {})",
                            outcome.state.revision
                        )
                    };
                    ui.set_status(message.into());
                    self.publish_data_source(ui);
                    self.publish_metadata_recovery_notice(ui);
                    if self.close_after_operation.swap(false, Ordering::AcqRel) {
                        self.request_close(ui);
                    }
                    return;
                }
                let changed = current
                    .as_ref()
                    .is_some_and(|state| state.library_id != outcome.state.library_id)
                    || outcome.library_changed;
                if changed {
                    let previous = current
                        .map(|state| state.library_id)
                        .or_else(|| outcome.previous.map(|state| state.library_id))
                        .unwrap_or_default();
                    let warning = if chinese {
                        format!(
                            "注意：服务器素材库身份已变化（library_id {previous} → {}），已重新同步并清空选择与草稿。",
                            outcome.state.library_id
                        )
                    } else {
                        format!(
                            "Warning: the server library identity changed (library_id {previous} → {}); resynced and cleared selection and drafts.",
                            outcome.state.library_id
                        )
                    };
                    self.remote.lock().unwrap().last_error = Some(warning.clone());
                    ui.set_ds_message(warning.into());
                }
                self.swap_to(
                    ui,
                    DataSourceSwitch::Server {
                        database,
                        state: Some(Box::new(outcome.state)),
                    },
                );
            }
            Err(error) => {
                let mut message = error.localized(chinese);
                if self.source_changing.load(Ordering::Acquire) {
                    ui.set_ds_message(message.clone().into());
                    ui.set_status(message.into());
                    self.finish_source_change(ui, false);
                    self.publish_metadata_recovery_notice(ui);
                    return;
                }
                // Reflect the failure in the server notice / header (offline, starting…).
                let check = match &error {
                    // A timeout on a live connection is slow, not offline.
                    RemoteError::Timeout(_) => Some(RevisionCheck::Slow(message.clone())),
                    error if error.is_offline() => Some(RevisionCheck::Offline(message.clone())),
                    RemoteError::Starting { retry_after, .. } => Some(RevisionCheck::Starting {
                        message: message.clone(),
                        retry_after: *retry_after,
                    }),
                    error if error.is_unauthorized() => {
                        Some(RevisionCheck::Unauthorized(message.clone()))
                    }
                    RemoteError::Cancelled => None,
                    _ => Some(RevisionCheck::Failed(message.clone())),
                };
                if let Some(check) = check {
                    let mut remote = self.remote.lock().unwrap();
                    if let RevisionCheck::Starting { retry_after, .. } = &check {
                        // Re-check soon, never earlier than the server's Retry-After.
                        let delay = audiolib_remote::STARTING_RETRY
                            .max(Duration::from_secs(retry_after.unwrap_or(0)));
                        remote.retry_hint = Some(Instant::now() + delay);
                        remote.next_check = Some(unix_now() + delay.as_secs());
                    } else if matches!(check, RevisionCheck::Slow(_)) {
                        // Quick first retry; the poller schedules the following ones.
                        let delay = Duration::from_secs(audiolib_remote::SLOW_RETRY_STEPS[0]);
                        remote.retry_hint = Some(Instant::now() + delay);
                        remote.next_check = Some(unix_now() + delay.as_secs());
                    } else {
                        remote.next_check = None;
                    }
                    remote.check = Some(check);
                }
                // `sync_library` stored the roots it fetched while `/library` was slow.
                let slow_roots = if matches!(error, RemoteError::Timeout(_)) {
                    cache
                        .active()
                        .map(|state| state.roots)
                        .filter(|roots| !roots.is_empty())
                } else {
                    None
                };
                if slow_roots.is_some() {
                    self.remote.lock().unwrap().live_roots = slow_roots.clone();
                }
                if let Some((database, state)) =
                    fallback.filter(|(path, _)| !path.as_os_str().is_empty())
                {
                    if !self.remote_read_only() || self.remote.lock().unwrap().state.is_none() {
                        // The server could not be reached: no live root status (the roots
                        // saved with the cache are used without their old status).
                        self.remote.lock().unwrap().live_roots = slow_roots.clone();
                        message.push_str(&if state.is_some() {
                            cn_or(chinese, "\n已切换到上次同步的离线缓存（只读）。", "\nSwitched to the last synced offline cache (read-only).")
                        } else {
                            cn_or(chinese, "\n尚无本地缓存：显示空素材库，连接恢复后请点击“立即同步”。", "\nNo cache yet: showing an empty library; use Sync now once the server is reachable.")
                        });
                        self.swap_to(
                            ui,
                            DataSourceSwitch::Server {
                                database,
                                state: state.map(Box::new),
                            },
                        );
                    }
                } else if self.remote_read_only() && self.remote.lock().unwrap().state.is_some() {
                    message.push_str(&cn_or(
                        chinese,
                        "\n继续使用上次同步的离线缓存。",
                        "\nContinuing with the last synced offline cache.",
                    ));
                }
                self.remote.lock().unwrap().last_error = Some(message.clone());
                ui.set_ds_message(message.clone().into());
                ui.set_status(message.replace('\n', " ").into());
                self.publish_data_source(ui);
            }
        }
        self.publish_metadata_recovery_notice(ui);
    }

    fn swap_to(self: &Arc<Self>, ui: &AppWindow, target: DataSourceSwitch) {
        // Keep the source gate and rollback until the provider actually opens: an
        // invalid snapshot must not leave the old provider using new credentials.
        if !self.available() && !self.source_changing.load(Ordering::Acquire) {
            ui.set_ds_message(
                cn_or(
                    self.chinese.load(Ordering::Acquire),
                    "素材库正忙，暂时无法切换数据源，请稍后点击“立即同步”。",
                    "The library is busy; switch later with Sync now.",
                )
                .into(),
            );
            return;
        }
        self.run_library_operation(LibraryOperation::DataSource(target));
    }

    /// Worker-side provider swap; runs inside `run_library_operation` after the
    /// Metadata FIFO and scans have drained and readers were invalidated.
    pub(super) fn switch_data_source(&self, target: &DataSourceSwitch) -> Result<String, String> {
        let chinese = self.chinese.load(Ordering::Acquire);
        // Playback streams belong to the snapshot being replaced.
        self.stop_remote_stream();
        match target {
            DataSourceSwitch::Local => {
                let config = self.config.lock().unwrap().clone();
                let path = config.paths.database.clone();
                let default_database = config.data_root().join("library.db");
                if !path.exists() && path != default_database {
                    return Err(format!(
                        "The selected local library no longer exists: {}",
                        path.display()
                    ));
                }
                let next = if path.exists() {
                    SqliteProvider::open_existing_local_with_progress_cancellable(
                        &path,
                        &config.data_root(),
                        config.play_history_limit,
                        &AtomicBool::new(false),
                        |_| {},
                    )
                } else {
                    SqliteProvider::open_local_with_progress_cancellable(
                        &path,
                        &config.data_root(),
                        config.play_history_limit,
                        &AtomicBool::new(false),
                        |_| {},
                    )
                }
                .map_err(|error| error.local_display_text(chinese))?;
                crate::startup::prepare_local_search(&next);
                *self.sqlite.write().unwrap() = Arc::new(next);
                self.remote_mode.store(false, Ordering::Release);
                let mut remote = self.remote.lock().unwrap();
                remote.state = None;
                remote.library_changed = false;
                remote.manager.invalidate_library();
                // Root status belongs to the server left behind.
                remote.live_roots = None;
                drop(remote);
                Ok(if chinese {
                    format!("已切换到本地模式：{}", path.display())
                } else {
                    format!("Switched to local mode: {}", path.display())
                })
            }
            DataSourceSwitch::Server { database, state } => {
                let next = SqliteProvider::open_read_only(database)
                    .map_err(|error| error.display_text(chinese))?;
                *self.sqlite.write().unwrap() = Arc::new(next);
                self.remote_mode.store(true, Ordering::Release);
                {
                    let mut remote = self.remote.lock().unwrap();
                    remote.state = state.as_deref().cloned();
                    remote.library_changed = false;
                    remote.manager.invalidate_library();
                }
                if let Some(state) = state {
                    // Only now, with the provider on the new file: older snapshots and a
                    // library the server replaced (its snapshots and audio cache) go.
                    let cache = RemoteCache::new(&self.data_root());
                    cache.cleanup_after_swap(state);
                    cache.remove_audition_caches();
                }
                Ok(match state {
                    Some(state) if chinese => format!(
                        "已切换到服务器模式：库 {} · 版本 r{} · {} 个音效",
                        state.library_id, state.revision, state.audio_files
                    ),
                    Some(state) => format!(
                        "Switched to server mode: library {} · revision {} · {} sounds",
                        state.library_id, state.revision, state.audio_files
                    ),
                    None => cn_or(
                        chinese,
                        "服务器模式：尚未同步，显示空素材库",
                        "Server mode: not synced yet, showing an empty library",
                    ),
                })
            }
        }
    }

    /// Cancel any in-flight server sync (shutdown or mode change).
    pub(super) fn cancel_remote(&self) {
        self.remote_generation.fetch_add(1, Ordering::AcqRel);
        self.remote_cancel.lock().unwrap().cancel();
    }

    /// Path the audio engine plays.
    ///
    /// Local mode plays the library file. Server mode streams into memory (no file on
    /// disk): the server's `/audio/{id}/stream` or a `/preview` chosen from the record's
    /// format and the measured bandwidth ([`choose_route`]), falling back to WebDAV when the
    /// user configured a WebDAV address for the record's root. The stream is registered
    /// under a virtual path `<data dir>/remote/<library>/stream/<id>-<route>-<registration>.wav` together
    /// with its buffer state, so the decoder (on the engine worker, never the output
    /// callback) reads it while it arrives and the engine pauses ("buffering") instead of
    /// waiting on the network.
    pub(super) fn playable_path(
        self: &Arc<Self>,
        file: &AudioFile,
        cancel: &AtomicBool,
        generation: u64,
    ) -> Result<PathBuf, String> {
        let source_pair = self.playback.lock().unwrap().source_pair;
        self.playable_path_for_pair(file, cancel, generation, source_pair, false)
    }

    /// Prepare a representation for a specific output source pair. Reconfiguration
    /// retains the current stream until the engine acknowledges its replacement.
    pub(super) fn playable_path_for_pair(
        self: &Arc<Self>,
        file: &AudioFile,
        cancel: &AtomicBool,
        generation: u64,
        source_pair: u16,
        retain_current: bool,
    ) -> Result<PathBuf, String> {
        if !self.remote_read_only() {
            return Ok(PathBuf::from(&file.file_path));
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        let state = self.remote.lock().unwrap().state.clone().ok_or_else(|| {
            cn_or(
                chinese,
                "服务器素材库尚未同步",
                "The server library has not been synced",
            )
        })?;
        let settings = self.config.lock().unwrap().data_source.clone();
        let secrets = self.secrets();
        let client = RemoteClient::new(&settings.server_url, &secrets.token)
            .map_err(|error| error.localized(chinese))?;
        let library_dir = RemoteCache::new(&self.data_root()).library_dir(&state.library_id);
        let (pcm_refused, flac_refused) = {
            let remote = self.remote.lock().unwrap();
            let refused = remote
                .preview_refused
                .get(&file.id)
                .copied()
                .unwrap_or_default();
            (refused.0, refused.1 || remote.flac_unavailable)
        };
        let options = RouteOptions {
            source_pair,
            // The active profile's "transcode auditions" choice; off = always /stream.
            transcode: settings.audition_transcode(),
            pcm_unsupported: pcm_refused,
            flac_unavailable: flac_refused,
        };
        let mut route = choose_route(file, bandwidth().estimate_bps(), options);
        let playback_path = |route: &AudioRoute| {
            library_dir.join("stream").join(format!(
                "{}-{}-{}.wav",
                file.id,
                route.tag(),
                STREAM_REGISTRATION_ID.fetch_add(1, Ordering::Relaxed),
            ))
        };
        let memory_key = |route: &AudioRoute, source: &str| {
            format!(
                "{}/{}/{}/{}/{}/{}",
                state.library_id,
                file.id,
                file.file_size,
                route.tag(),
                source,
                // Incremental annotation/play writes retain this snapshot filename;
                // a rescan sync opens a new one even for a same-size replacement WAV.
                state.database_file,
            )
        };
        {
            let mut current = self.remote_stream.lock().unwrap();
            if let Some(playback) = current.as_ref()
                && playback.file_id == file.id
                && playback.stream.key() == memory_key(&route, "server")
                && playback.stream.source() == &client.audio_source(file, &route)
                && !playback.stream.is_failed()
                && (playback.stream.is_active() || playback.stream.is_complete())
            {
                return Ok(playback.path.clone());
            }
            if !retain_current && let Some(playback) = current.take() {
                // Another record: stop the old request first (its connection closes), so a
                // decoder still waiting on it fails at once instead of holding the audio
                // worker. What arrived stays in the memory cache for a later selection.
                playback.stream.cancel();
                audiolib_audio::unregister_stream_source(&playback.path);
            }
        }
        // Always ask the server: for an offline root it answers 503 at once without
        // touching the disk, and a cached status may be stale.
        let roots = self
            .remote
            .lock()
            .unwrap()
            .live_roots
            .clone()
            .unwrap_or_else(|| without_status(&state.roots));
        let root = root_for_path(&file.file_path, &roots);
        let webdav = configured_webdav_source(file, &state.roots, &settings, &secrets);
        let waking = root.is_some_and(|root| root.state() == RootState::Waking);
        if waking {
            self.status(if chinese {
                format!("正在唤醒音效库磁盘…（{}）", file.filename)
            } else {
                format!("Waking the library disk… ({})", file.filename)
            });
        }
        // The server route with its fallbacks: 415 on `codec=pcm` → `codec=flac`; FLAC
        // refused (415, or 503 TRANSCODER_UNAVAILABLE) → the original `/stream`; a preview
        // endpoint the server lacks (404/400 from an older server) → `/stream`. A 503 BUSY
        // is retried by the stream itself (Retry-After), like `/stream`.
        let open_primary = |route: &mut AudioRoute| loop {
            let source = client.audio_source(file, route);
            let key = memory_key(route, "server");
            let mut result = self.open_stream(&key, file, source.clone(), cancel, generation);
            if matches!(result, Err(RemoteError::Changed(_))) {
                // The file was replaced between requests: restart once from scratch.
                result = self.open_stream(&key, file, source, cancel, generation);
            }
            let refused = match &result {
                Err(RemoteError::PreviewUnavailable { code, .. }) => Some(code.clone()),
                Err(RemoteError::NotFound(_) | RemoteError::Validation(_))
                    if route.is_preview() =>
                {
                    Some(String::new())
                }
                _ => None,
            };
            let Some(code) = refused else {
                return result;
            };
            let next = {
                let mut remote = self.remote.lock().unwrap();
                if remote.preview_refused.len() > 4096 {
                    remote.preview_refused.clear();
                }
                let entry = remote.preview_refused.entry(file.id).or_default();
                match route {
                    AudioRoute::Pcm(_) if code == "PREVIEW_UNSUPPORTED_FORMAT" => entry.0 = true,
                    AudioRoute::Flac(_) if code == "TRANSCODER_UNAVAILABLE" => {
                        remote.flac_unavailable = true;
                    }
                    AudioRoute::Flac(_) => entry.1 = true,
                    // Anything else unexpected from /preview: the original for this record.
                    _ => {
                        entry.0 = true;
                        entry.1 = true;
                    }
                }
                let refused = remote.preview_refused[&file.id];
                choose_route(
                    file,
                    bandwidth().estimate_bps(),
                    RouteOptions {
                        pcm_unsupported: refused.0,
                        flac_unavailable: refused.1 || remote.flac_unavailable,
                        ..options
                    },
                )
            };
            tracing::info!(id = file.id, from = %route.tag(), to = %next.tag(), %code, "audition preview refused; falling back");
            // Never loop: each refusal narrows the choice, and /stream is the end.
            *route = if next == *route || !route.is_preview() {
                AudioRoute::Stream
            } else {
                next
            };
        };
        let both_failed = |error: &RemoteError, fallback: &RemoteError| {
            if chinese {
                format!(
                    "{}；WebDAV 备用地址也失败：{}",
                    stream_error_text(error, root, true),
                    stream_error_text(fallback, root, true)
                )
            } else {
                format!(
                    "{} WebDAV fallback also failed: {}",
                    stream_error_text(error, root, false),
                    stream_error_text(fallback, root, false)
                )
            }
        };
        let cancelled = || RemoteError::Cancelled.localized(chinese);
        let open_webdav = |source: StreamSource| {
            let key = memory_key(&AudioRoute::Stream, "webdav");
            self.open_stream(&key, file, source, cancel, generation)
        };
        let stream = match webdav {
            // The server reports the root offline and a WebDAV URL is configured: the NAS
            // may still serve it directly, so try WebDAV first; the server is asked only if
            // that fails (its root status may be stale).
            Some(webdav) if webdav_first(root, true) => {
                tracing::info!(
                    "root offline on the server; streaming from the configured WebDAV URL first"
                );
                match open_webdav(webdav) {
                    Ok(stream) => {
                        route = AudioRoute::Stream;
                        stream
                    }
                    Err(RemoteError::Cancelled) => return Err(cancelled()),
                    Err(fallback) => match open_primary(&mut route) {
                        Ok(stream) => stream,
                        Err(RemoteError::Cancelled) => return Err(cancelled()),
                        Err(error) => {
                            if matches!(error, RemoteError::RootOffline { .. }) {
                                self.wake_revision_poller();
                            }
                            return Err(both_failed(&error, &fallback));
                        }
                    },
                }
            }
            webdav => match (open_primary(&mut route), webdav) {
                (Ok(stream), _) => stream,
                (Err(RemoteError::Cancelled), _) => return Err(cancelled()),
                // The whole root is offline and there is no WebDAV URL: the server's
                // message at once (no retry loop); refresh the root status shown in the
                // header and notice.
                (Err(error @ RemoteError::RootOffline { .. }), None) => {
                    self.wake_revision_poller();
                    return Err(stream_error_text(&error, root, chinese));
                }
                (Err(error), Some(webdav)) => {
                    if matches!(error, RemoteError::RootOffline { .. }) {
                        self.wake_revision_poller();
                    }
                    tracing::info!(%error, "server audio stream failed; trying the configured WebDAV URL");
                    match open_webdav(webdav) {
                        Ok(stream) => {
                            route = AudioRoute::Stream;
                            stream
                        }
                        Err(RemoteError::Cancelled) => return Err(cancelled()),
                        Err(fallback) => return Err(both_failed(&error, &fallback)),
                    }
                }
                (Err(error), None) => return Err(stream_error_text(&error, root, chinese)),
            },
        };
        let target = playback_path(&route);
        {
            let mut remote = self.remote.lock().unwrap();
            if remote.audio_ids.len() > 4096 {
                remote.audio_ids.clear();
                remote.audio_names.clear();
                remote.audio_files.clear();
            }
            remote.audio_ids.insert(target.clone(), file.id);
            remote.audio_files.insert(target.clone(), file.clone());
            remote
                .audio_names
                .insert(target.clone(), file.filename.clone());
        }
        let reader_source = stream.clone();
        audiolib_audio::register_buffered_stream_source(
            target.clone(),
            Arc::new(move || {
                let reader = reader_source.reader()?;
                let len = reader.len();
                let source: Box<dyn audiolib_audio::ByteSource> = Box::new(reader);
                Ok((source, len))
            }),
            Arc::new(StreamBuffer(stream.clone())),
        );
        tracing::debug!(id = file.id, route = %route.tag(), bandwidth = ?bandwidth().estimate_bps(), "audition stream ready");
        *self.remote_stream.lock().unwrap() = Some(RemotePlayback {
            path: target.clone(),
            file_id: file.id,
            byte_rate: route.output_bytes_per_second(file),
            stream,
        });
        Ok(target)
    }

    /// Start (or reuse) an in-memory stream and wait for its first response.
    fn open_stream(
        self: &Arc<Self>,
        key: &str,
        file: &AudioFile,
        source: StreamSource,
        cancel: &AtomicBool,
        generation: u64,
    ) -> Result<MemoryStream, RemoteError> {
        let stream = MemoryStream::start(MemoryCache::global(), key, source)?;
        // A NAS disk may need 20-30 s to spin up before the first byte: wait up to the
        // source's first-byte timeout, show a "waking the library disk…" status after
        // 1.5 s, and give up at once when a newer selection or Stop sets `cancel`.
        let chinese = self.chinese.load(Ordering::Acquire);
        let mut shown: Option<u64> = None;
        let waited = stream.wait_ready_with(
            cancel,
            stream.first_byte_timeout() + STREAM_READY_MARGIN,
            &mut |waited| {
                if self.selection_generation.load(Ordering::Acquire) != generation
                    || shown == Some(waited.as_secs())
                {
                    return;
                }
                if let Some(text) = waking_hint(&file.filename, waited, chinese) {
                    shown = Some(waited.as_secs());
                    self.status(text);
                }
            },
        );
        match waited {
            Ok(_) => {
                if shown.is_some() {
                    self.status(if chinese {
                        format!("音效库磁盘已唤醒：{}", file.filename)
                    } else {
                        format!("Library disk awake: {}", file.filename)
                    });
                }
                Ok(stream)
            }
            Err(error) => {
                stream.cancel();
                if matches!(error, RemoteError::Cancelled) && shown.is_some() {
                    self.status(if chinese {
                        format!("已取消读取：{}", file.filename)
                    } else {
                        format!("Cancelled: {}", file.filename)
                    });
                }
                Err(error)
            }
        }
    }

    /// Stop in server mode: give up a request still waiting for its first byte and fail
    /// decoder reads blocked on missing network data (the stream itself continues), so
    /// the audio engine handles Stop at once instead of after the read stall timeout.
    pub(super) fn interrupt_remote_audio(&self) {
        if !self.remote_read_only() {
            return;
        }
        self.remote_audio_cancel
            .lock()
            .unwrap()
            .store(true, Ordering::Release);
        if let Some(playback) = self.remote_stream.lock().unwrap().as_ref()
            && !playback.stream.is_complete()
        {
            playback.stream.interrupt_reads();
        }
        if let Some(playback) = self.remote_retained_stream.lock().unwrap().as_ref()
            && !playback.stream.is_complete()
        {
            playback.stream.interrupt_reads();
        }
    }

    /// Server-mode drag-out: make each record a complete local file in the download cache
    /// (`<data dir>/cache/<library>/<id>-<etag token>/<original name>`), the original
    /// full-quality file, written straight from a complete in-memory copy of the original
    /// with the same ETag when audition has one. Cancellable; progress per item.
    pub(super) fn remote_drag_source(&self) -> Result<RemoteDragSource, String> {
        let chinese = self.chinese.load(Ordering::Acquire);
        let state = self.remote.lock().unwrap().state.clone().ok_or_else(|| {
            cn_or(
                chinese,
                "服务器素材库尚未同步",
                "The server library has not been synced",
            )
        })?;
        let settings = self.config.lock().unwrap().data_source.clone();
        let secrets = self.secrets();
        let client = RemoteClient::new(&settings.server_url, &secrets.token)
            .map_err(|error| error.localized(chinese))?;
        Ok(RemoteDragSource {
            state,
            client,
            cache_directory: self.config.lock().unwrap().paths.cache.clone(),
        })
    }

    /// Whether a drag can still publish paths or progress for its original source.
    pub(super) fn drag_request_current(
        &self,
        source_generation: u64,
        library_generation: u64,
        cancel: &AtomicBool,
    ) -> bool {
        !cancel.load(Ordering::Acquire)
            && !self.shutdown.load(Ordering::Acquire)
            && self.source_generation.load(Ordering::Acquire) == source_generation
            && self.library_generation.load(Ordering::Acquire) == library_generation
    }

    /// Download original files using the destination captured at drag dispatch.
    pub(super) fn prepare_remote_drag(
        &self,
        source: &RemoteDragSource,
        files: &[AudioFile],
        progress: &mut dyn FnMut(DownloadProgress),
        cancel: &AtomicBool,
    ) -> Result<Vec<PathBuf>, String> {
        let chinese = self.chinese.load(Ordering::Acquire);
        let state = &source.state;
        let items: Vec<DownloadItem> = files
            .iter()
            .map(|file| DownloadItem {
                file: file.clone(),
                source: source.client.download_source(file.id),
            })
            .collect();
        let downloads =
            DownloadCache::new(source.cache_directory.clone(), DEFAULT_DOWNLOAD_CACHE_BYTES);
        // Name the root in errors when the whole selection lives in one root.
        let mut roots = files
            .iter()
            .map(|file| root_for_path(&file.file_path, &state.roots));
        let first = roots.next().flatten();
        let root = first.filter(|first| roots.all(|root| root == Some(*first)));
        downloads
            .ensure_all(
                &state.library_id,
                &items,
                Some(MemoryCache::global()),
                progress,
                cancel,
            )
            .map_err(|error| stream_error_text(&error, root, chinese))
    }

    /// Cancel and unregister the server-mode playback stream (mode switch, shutdown).
    pub(super) fn stop_remote_stream(&self) {
        if let Some(playback) = self.remote_retained_stream.lock().unwrap().take() {
            playback.stream.cancel();
            audiolib_audio::unregister_stream_source(&playback.path);
        }
        if let Some(playback) = self.remote_stream.lock().unwrap().take() {
            playback.stream.cancel();
            audiolib_audio::unregister_stream_source(&playback.path);
        }
    }

    /// Waveform of a server record for the waveform worker: `GET /audio/{id}/waveform`
    /// (computed by the server from the original, independent of audition), kept in memory
    /// and on disk with its ETag and revalidated with `If-None-Match` (`304`). Only when
    /// the server cannot answer is it computed from the audition stream.
    pub(super) fn remote_waveform(
        &self,
        file_id: i64,
        sample_rate: u32,
        request: WaveformRequest,
        cancel: &AtomicBool,
        publish: impl FnMut(&WaveformData, f64),
    ) -> Result<WaveformData, String> {
        let mut deferred_audio = false;
        crate::progressive_waveform::load(
            request,
            sample_rate,
            cancel,
            |part, buckets| {
                self.remote_waveform_segment(
                    file_id,
                    sample_rate,
                    part,
                    buckets,
                    cancel,
                    &mut deferred_audio,
                )
            },
            publish,
        )
    }

    fn remote_waveform_segment(
        &self,
        file_id: i64,
        sample_rate: u32,
        request: WaveformRequest,
        buckets: u32,
        cancel: &AtomicBool,
        deferred_audio: &mut bool,
    ) -> Result<WaveformData, String> {
        let chinese = self.chinese.load(Ordering::Acquire);
        let state = self.remote.lock().unwrap().state.clone().ok_or_else(|| {
            cn_or(
                chinese,
                "服务器素材库尚未同步",
                "The server library has not been synced",
            )
        })?;
        let query = WaveformQuery {
            buckets: buckets.clamp(64, 8192),
            colors: request.include_colors,
            start_frame: Some(request.start_frame),
            end_frame: request.end_frame,
        };
        let key = format!(
            "{}-{}-{}-{}-{}",
            file_id,
            request.start_frame,
            request.end_frame.unwrap_or(u64::MAX),
            query.buckets,
            u8::from(query.colors)
        );
        let memory_key = format!("{}/{}/{key}", state.library_id, state.revision);
        let frames = request
            .end_frame
            .unwrap_or(0)
            .saturating_sub(request.start_frame);
        {
            let mut remote = self.remote.lock().unwrap();
            if let Some(data) = remote.waveforms.get(&memory_key) {
                if crate::progressive_waveform::validate_part(data, frames, sample_rate).is_ok() {
                    return Ok(data.clone());
                }
                remote.waveforms.remove(&memory_key);
            }
        }
        let library = RemoteCache::new(&self.data_root()).library_dir(&state.library_id);
        let directory = self
            .config
            .lock()
            .unwrap()
            .paths
            .cache
            .join("waveforms")
            .join("server")
            .join(library.file_name().unwrap_or_default());
        let json_path = directory.join(format!("{key}.json"));
        let etag_path = directory.join(format!("{key}.etag"));
        let stored = std::fs::read(&json_path)
            .ok()
            .and_then(|json| decode_server_waveform(&json, &request, sample_rate).ok());
        let known_etag = std::fs::read_to_string(&etag_path)
            .ok()
            // A 304 is useful only with a validated payload; refetch damaged caches.
            .filter(|_| stored.is_some());
        if known_etag.is_none() && !*deferred_audio {
            // Preserve instant cache hits and defer only the first uncached segment.
            self.wait_for_audio_start(file_id, cancel);
            *deferred_audio = true;
        }
        let settings = self.config.lock().unwrap().data_source.clone();
        let secrets = self.secrets();
        let fetched = RemoteClient::new(&settings.server_url, &secrets.token)
            .and_then(|client| client.waveform(file_id, &query, known_etag.as_deref(), cancel));
        let data = match fetched {
            Ok(WaveformReply::NotModified) => stored,
            Ok(WaveformReply::Data { json, etag }) => {
                match decode_server_waveform(&json, &request, sample_rate) {
                    Ok(data) => {
                        // Best effort: a failed write only costs a refetch.
                        if std::fs::create_dir_all(&directory).is_ok()
                            && std::fs::write(&json_path, &json).is_ok()
                        {
                            match etag {
                                Some(etag) => {
                                    let _ = std::fs::write(&etag_path, etag);
                                }
                                None => {
                                    let _ = std::fs::remove_file(&etag_path);
                                }
                            }
                        }
                        Some(data)
                    }
                    Err(error) => {
                        tracing::warn!(%error, file_id, "server waveform unreadable");
                        None
                    }
                }
            }
            Err(RemoteError::Cancelled) => return Err(RemoteError::Cancelled.localized(chinese)),
            Err(error) => {
                tracing::info!(%error, file_id, "server waveform unavailable");
                // Offline: the copy on disk is still right for this file version.
                stored
            }
        };
        let data = match data {
            Some(data) => data,
            None => self.stream_waveform(file_id, sample_rate, request, cancel)?,
        };
        crate::progressive_waveform::validate_part(&data, frames, sample_rate)?;
        let mut remote = self.remote.lock().unwrap();
        if remote.waveforms.len() >= 64 {
            remote.waveforms.clear();
        }
        remote.waveforms.insert(memory_key, data.clone());
        Ok(data)
    }

    /// Wait (at most [`WAVEFORM_AFTER_AUDIO`]) until the selected record's audition stream
    /// holds its prebuffer, has failed, or the selection moved on.
    fn wait_for_audio_start(&self, file_id: i64, cancel: &AtomicBool) {
        let deadline = Instant::now() + WAVEFORM_AFTER_AUDIO;
        while !cancel.load(Ordering::Acquire) && Instant::now() < deadline {
            let started = match self.remote_stream.lock().unwrap().as_ref() {
                Some(playback) if playback.file_id == file_id => {
                    let stream = &playback.stream;
                    let need = 64 * 1024 + playback.byte_rate * 6 / 10;
                    stream.is_complete()
                        || stream.is_failed()
                        || stream
                            .len()
                            .is_some_and(|total| stream.contiguous_from(0) >= need.min(total))
                }
                // Another record's stream: this selection's has not started yet.
                _ => false,
            };
            if started {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Fallback waveform when the server has none: computed from the selected record's
    /// audition stream, only once it is complete in memory (never by downloading it), with
    /// frames mapped to the preview's sample rate.
    fn stream_waveform(
        &self,
        file_id: i64,
        sample_rate: u32,
        request: WaveformRequest,
        cancel: &AtomicBool,
    ) -> Result<WaveformData, String> {
        let chinese = self.chinese.load(Ordering::Acquire);
        let held = self
            .remote_stream
            .lock()
            .unwrap()
            .as_ref()
            .filter(|playback| playback.file_id == file_id && playback.stream.is_complete())
            .map(|playback| (playback.path.clone(), playback.stream.preview_format()));
        let Some((path, preview)) = held else {
            return Err(cn_or(
                chinese,
                "服务器波形不可用",
                "The server waveform is unavailable",
            ));
        };
        let request = match preview {
            Some(format) if sample_rate > 0 && format.sample_rate != sample_rate => {
                let scale = |frame: u64| {
                    (frame as f64 * f64::from(format.sample_rate) / f64::from(sample_rate)).round()
                        as u64
                };
                let start_frame = scale(request.start_frame);
                let end_frame = request.end_frame.map(scale);
                let frames = end_frame.unwrap_or(start_frame).saturating_sub(start_frame);
                WaveformRequest {
                    start_frame,
                    end_frame,
                    samples_per_pixel: (request.samples_per_pixel as f64
                        * f64::from(format.sample_rate)
                        / f64::from(sample_rate))
                    .round()
                    .max(1.0)
                    .min(frames.max(1) as f64) as u32,
                    include_colors: request.include_colors,
                }
            }
            _ => request,
        };
        audiolib_audio::waveform_with_cancel(&path, request, cancel)
            .map_err(|error| error.to_string())
    }

    /// Show (or clear) the "buffering" hint while network audition waits for data.
    pub(super) fn buffering_hint(&self, on: bool) {
        let chinese = self.chinese.load(Ordering::Acquire);
        let text = cn_or(
            chinese,
            "正在缓冲音频…（网络较慢，数据到达后自动继续）",
            "Buffering audio… (slow network; playback resumes by itself)",
        );
        post(self.ui.clone(), move |ui| {
            if on {
                ui.set_status(text.into());
            } else if ui.get_status().as_str() == text {
                ui.set_status(slint::SharedString::new());
            }
        });
    }
}

/// Validate the server's source range before storing or reusing its peaks.
fn decode_server_waveform(
    json: &[u8],
    request: &WaveformRequest,
    sample_rate: u32,
) -> Result<WaveformData, String> {
    let value: serde_json::Value =
        serde_json::from_slice(json).map_err(|error| error.to_string())?;
    let start = value["start_frame"].as_u64();
    let end = value["end_frame"].as_u64();
    let total = value["total_frames"].as_u64();
    let (Some(start), Some(end), Some(total), Some(expected_end)) =
        (start, end, total, request.end_frame)
    else {
        return Err("Missing remote Waveform source range".into());
    };
    if start != request.start_frame || end != expected_end.min(total) || start >= end {
        return Err("Remote Waveform source range does not match the request".into());
    }
    let data: WaveformData = serde_json::from_value(value).map_err(|error| error.to_string())?;
    if data.sample_rate != sample_rate || data.frames != end - start {
        return Err("Remote Waveform source format does not match the request".into());
    }
    crate::progressive_waveform::validate_part(&data, expected_end - start, sample_rate)?;
    Ok(data)
}

/// The server-mode audition stream the engine plays.
#[derive(Clone)]
pub(super) struct RemotePlayback {
    /// Virtual path the stream is registered under (never created on disk).
    pub(super) path: PathBuf,
    pub(super) file_id: i64,
    /// Audio bytes per second the player reads from it.
    pub(super) byte_rate: u64,
    pub(super) stream: MemoryStream,
}

/// The engine's view of what a [`MemoryStream`] holds (never blocks).
struct StreamBuffer(MemoryStream);

impl audiolib_audio::SourceBuffer for StreamBuffer {
    fn is_buffered(&self, offset: u64, len: u64) -> bool {
        self.0.is_buffered(offset, len)
    }
}

/// A selection waits for the stream's own first-byte timeout plus this margin, so the
/// stream thread's classified timeout arrives first.
const STREAM_READY_MARGIN: Duration = Duration::from_secs(5);

/// Status while a server request waits for its first byte (a NAS disk spinning up), or
/// `None` before [`WAKING_HINT_AFTER`].
pub(super) fn waking_hint(name: &str, waited: Duration, chinese: bool) -> Option<String> {
    (waited >= WAKING_HINT_AFTER).then(|| {
        let seconds = waited.as_secs();
        if chinese {
            format!("正在唤醒音效库磁盘… {seconds} 秒（{name}）")
        } else {
            format!("Waking the library disk… {seconds} s ({name})")
        }
    })
}

/// Header suffix for roots that are not plainly online (§6.4), e.g. ` · 根目录离线：SFX`.
pub(super) fn root_summary(roots: &[RemoteRoot], chinese: bool) -> String {
    let mut summary = String::new();
    for (state, label_cn, label_en) in [
        (RootState::Offline, "根目录离线", "root offline"),
        (RootState::Waking, "磁盘唤醒中", "disk waking"),
    ] {
        let names: Vec<_> = roots
            .iter()
            .filter(|root| root.state() == state)
            .map(|root| root.name.as_str())
            .collect();
        if names.is_empty() {
            continue;
        }
        if chinese {
            summary.push_str(&format!(" · {label_cn}：{}", names.join("、")));
        } else {
            summary.push_str(&format!(" · {label_en}: {}", names.join(", ")));
        }
    }
    summary
}

/// Notice for offline roots (shown when no revision notice is pending): their records stay
/// listed as usual, not as missing; cached copies still play.
pub(super) fn root_notice(roots: &[RemoteRoot], chinese: bool) -> Option<String> {
    let list = roots
        .iter()
        .filter(|root| root.state() == RootState::Offline)
        .map(|root| {
            let reason = root
                .status_message
                .as_deref()
                .map(str::trim)
                .filter(|reason| !reason.is_empty());
            match (reason, chinese) {
                (Some(reason), true) => format!("「{}」（{reason}）", root.name),
                (Some(reason), false) => format!("\"{}\" ({reason})", root.name),
                (None, true) => format!("「{}」", root.name),
                (None, false) => format!("\"{}\"", root.name),
            }
        })
        .collect::<Vec<_>>();
    if list.is_empty() {
        return None;
    }
    Some(if chinese {
        format!(
            "音效库根目录离线：{}。其中的音效照常列出（不标为缺失），已缓存的仍可试听，恢复后自动可用",
            list.join("、")
        )
    } else {
        format!(
            "Library root offline: {}. Its sounds stay listed (not missing); cached ones still play, the rest once it is back",
            list.join(", ")
        )
    })
}

/// User text for a failed stream or download; a first-byte timeout explains the disk, an
/// offline root (`503 ROOT_OFFLINE`) shows the server's reason, and a `404` (the file is
/// missing on a reachable root) names the root it was expected in.
fn stream_error_text(error: &RemoteError, root: Option<&RemoteRoot>, chinese: bool) -> String {
    match error {
        RemoteError::RootOffline { message, .. } => {
            if chinese {
                format!(
                    "音效库根目录离线，暂时无法试听：{message}（已缓存的音效仍可播放，根目录恢复后即可试听）"
                )
            } else {
                format!(
                    "The library root is offline, so this sound cannot play now: {message} (cached sounds still play; it plays once the root is back)"
                )
            }
        }
        RemoteError::NotFound(_) if let Some(root) = root => {
            let name = &root.name;
            if chinese {
                format!(
                    "服务器在根目录「{name}」中找不到该文件：它可能已被移动、改名或删除（服务器重新扫描后记录会更新）。"
                )
            } else {
                format!(
                    "The server cannot find this file in its root \"{name}\": it was moved, renamed or deleted (a rescan on the server updates the record)."
                )
            }
        }
        RemoteError::Timeout(detail) if detail.contains("no response within") => {
            let seconds = audiolib_remote::FIRST_BYTE_TIMEOUT.as_secs();
            if chinese {
                format!(
                    "服务器 {seconds} 秒内没有返回音频数据：音效库磁盘可能无法唤醒或已离线，请稍后重试。"
                )
            } else {
                format!(
                    "The server sent no audio within {seconds} s: the library disk may not wake up or is offline. Try again later."
                )
            }
        }
        error => error.localized(chinese),
    }
}

/// The WebDAV stream source for a record, only when the user configured a WebDAV address
/// for its root (per-root URL or global base); server-suggested URLs are not used.
/// Names of the roots `/roots` reports online (§6.4): list Waveforms that were
/// `root_offline` there are asked for again.
fn online_root_names(roots: &[RemoteRoot]) -> Vec<String> {
    roots
        .iter()
        .filter(|root| root.state() == RootState::Online)
        .map(|root| root.name.clone())
        .collect()
}

/// Replace the entries of `live` that a root check returned (matched by name).
fn merge_roots(live: &mut [RemoteRoot], updated: &[RemoteRoot]) {
    for root in updated {
        if let Some(entry) = live.iter_mut().find(|entry| entry.name == root.name) {
            *entry = root.clone();
        }
    }
}

/// Status line after Retry on the offline-root notice.
fn root_retry_message(still_offline: &[String], chinese: bool) -> String {
    match (still_offline.is_empty(), chinese) {
        (true, true) => "音效库根目录已恢复在线".into(),
        (true, false) => "Library roots are online again".into(),
        (false, true) => format!("音效库根目录仍然离线：{}", still_offline.join("、")),
        (false, false) => format!("Library roots still offline: {}", still_offline.join(", ")),
    }
}

/// Roots saved with a snapshot: names, paths and WebDAV URLs stay valid, but their status
/// is from the sync and must not decide anything now (no status = online, ask the server).
fn without_status(roots: &[RemoteRoot]) -> Vec<RemoteRoot> {
    roots
        .iter()
        .cloned()
        .map(|root| RemoteRoot {
            online: None,
            status: None,
            status_message: None,
            checked_at: None,
            ..root
        })
        .collect()
}

/// Whether to stream from the configured WebDAV URL before asking the server: only when
/// the server reports the file's root offline (otherwise the server stream is preferred).
fn webdav_first(root: Option<&RemoteRoot>, webdav_configured: bool) -> bool {
    webdav_configured && root.is_some_and(|root| root.state() == RootState::Offline)
}

fn configured_webdav_source(
    file: &AudioFile,
    roots: &[RemoteRoot],
    settings: &DataSourceSettings,
    secrets: &RemoteSecrets,
) -> Option<StreamSource> {
    let configured: Vec<_> = effective_roots(roots, &settings.webdav)
        .into_iter()
        .filter(|root| matches!(root.source, UrlSource::User | UrlSource::GlobalBase))
        .collect();
    let url = map_server_path(&file.file_path, &configured).ok()?;
    Some(StreamSource::new(
        url,
        SourceAuth::Basic(WebDavAuth {
            username: settings.webdav.username.clone(),
            password: secrets.webdav_password.clone(),
        }),
    ))
}

/// The record's name when known (a server-mode cache copy), else the path's file name.
pub(super) fn playback_name(path: &Path, record_name: Option<&str>) -> String {
    match record_name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => name.to_owned(),
        None => path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
    }
}

/// True when `path` is inside the server-mode remote cache (used to keep the status
/// worker's waveform detection aligned with cached playback copies).
pub(super) fn is_remote_cache_path(data_root: &Path, path: &Path) -> bool {
    path.starts_with(data_root.join("remote"))
}

/// Fill an empty `waveforms.db` from the server's bundle, or revalidate a bundle-filled one
/// (doc/api §9.4.2), on a background thread. `503 BUSY` is retried with `bundle_backoff`
/// (5 s doubling, at most 5 minutes) while page lookups continue; the thread ends when the
/// provider is replaced (its last strong reference is dropped) or on any other outcome.
fn spawn_waveform_bundle_import(service: &Arc<RemoteListWaveforms>) {
    // An empty store downloads the bundle; a store filled by an earlier bundle revalidates
    // it with `If-None-Match`. A `304`, or a `200` with the same ETag from a server that
    // ignores the condition, reuses the cache without reading a body. A store filled only by
    // page lookups (no recorded bundle ETag) keeps using page lookups.
    let wanted = service.offers_bundle()
        && (service
            .i8_store()
            .is_some_and(|store| store.is_empty().unwrap_or(false))
            || service.installed_bundle_etag().is_some());
    if !wanted {
        return;
    }
    let weak = Arc::downgrade(service);
    let spawned = std::thread::Builder::new()
        .name("waveform-bundle".into())
        .spawn(move || {
            let mut attempt = 0u32;
            loop {
                let Some(service) = weak.upgrade() else { return };
                // A library switch drops every other reference: cancel the download.
                let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
                cancel_when_released(weak.clone(), cancel.clone(), done.clone());
                let outcome = service.import_bundle(&cancel);
                done.store(true, std::sync::atomic::Ordering::Release);
                let wait = match outcome {
                    Ok(audiolib_remote::BundleImport::Installed { rows }) => {
                        tracing::info!(rows, "list Waveform bundle imported");
                        return;
                    }
                    Ok(audiolib_remote::BundleImport::NotModified) => {
                        tracing::info!("list Waveform bundle not modified; cached store kept");
                        return;
                    }
                    Err(audiolib_remote::RemoteError::Busy { retry_after, .. }) => {
                        audiolib_remote::bundle_backoff(attempt, retry_after)
                    }
                    Err(error) => {
                        tracing::warn!(%error, "list Waveform bundle import failed; using page lookups");
                        return;
                    }
                };
                drop(service);
                attempt = attempt.saturating_add(1);
                let until = std::time::Instant::now() + wait;
                while std::time::Instant::now() < until {
                    if weak.strong_count() == 0 {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "list Waveform bundle thread not started");
    }
}

/// Set `cancel` once only the importing thread still holds `weak`'s value (the provider
/// was replaced by a library switch). The watcher ends when `done` is set.
fn cancel_when_released<T: Send + Sync + 'static>(
    weak: std::sync::Weak<T>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    done: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    let spawned = std::thread::Builder::new()
        .name("waveform-bundle-cancel".into())
        .spawn(move || {
            while !done.load(Ordering::Acquire) {
                if weak.strong_count() <= 1 {
                    cancel.store(true, Ordering::Release);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "bundle cancel watcher not started");
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::cancel_when_released;
    use super::{
        AddressForm, AutoSync, DataSourceSwitch, NOTICE_RETRY, NOTICE_SETTINGS, NOTICE_UPDATE,
        RemoteStartup, address_hint, configured_webdav_source, connection_verdict,
        is_remote_cache_path, merge_roots, playback_name, revision_notice, root_retry_message,
        tokenless_note, webdav_first, without_status,
    };
    use audiolib_core::config::{DataSourceMode, DataSourceSettings};
    use audiolib_remote::RemoteError;

    #[test]
    fn startup_opens_the_manager_only_without_a_cached_snapshot_of_the_server() {
        use super::startup_needs_manager;
        let url = "https://sounds.example.test";
        let cached = |revision| audiolib_remote::CacheState {
            library_id: "library".into(),
            server_url: url.into(),
            revision,
            ..Default::default()
        };
        // A newer server revision (r793 cached, r794 live) is synced in the background.
        assert!(!startup_needs_manager(Some(&cached(793)), url));
        assert!(!startup_needs_manager(
            Some(&cached(794)),
            "https://sounds.example.test/"
        ));
        // First run, no configured address, or a cache of another server.
        assert!(startup_needs_manager(None, url));
        assert!(startup_needs_manager(Some(&cached(794)), ""));
        assert!(startup_needs_manager(
            Some(&cached(794)),
            "https://other.example.test"
        ));
    }

    #[test]
    fn startup_sync_opens_the_manager_only_on_failure_or_a_new_library() {
        use super::startup_sync_opens_manager;
        let state = |library_id: &str, revision| audiolib_remote::CacheState {
            library_id: library_id.into(),
            revision,
            ..Default::default()
        };
        let outcome = |previous: Option<audiolib_remote::CacheState>, changed| {
            Ok(audiolib_remote::SyncOutcome {
                state: state("library", 794),
                downloaded: true,
                library_changed: changed,
                previous,
                elapsed_ms: 0,
            })
        };
        // r793 -> r794 of the same library: stay on the main window.
        assert!(!startup_sync_opens_manager(&outcome(
            Some(state("library", 793)),
            false
        )));
        assert!(startup_sync_opens_manager(&outcome(
            Some(state("other", 793)),
            false
        )));
        assert!(startup_sync_opens_manager(&outcome(None, true)));
        assert!(startup_sync_opens_manager(&Err(RemoteError::Unreachable(
            "x".into()
        ))));
        assert!(startup_sync_opens_manager(&Err(RemoteError::Snapshot(
            "bad".into()
        ))));
        assert!(!startup_sync_opens_manager(&Err(RemoteError::Cancelled)));
    }
    use audiolib_remote::{
        CacheState, CachedIdentity, RemoteCache, RemoteRoot, RemoteSecrets, RevisionCheck,
        RootState, SourceAuth,
    };

    #[test]
    fn bundle_import_is_cancelled_when_the_library_switches() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let provider = Arc::new(());
        let importing = provider.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        cancel_when_released(Arc::downgrade(&provider), cancel.clone(), done.clone());
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert!(
            !cancel.load(Ordering::Acquire),
            "still owned by the provider"
        );
        drop(provider);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !cancel.load(Ordering::Acquire) && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(cancel.load(Ordering::Acquire));
        drop(importing);
        // A finished import stops the watcher without cancelling.
        let owner = Arc::new(());
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(true));
        cancel_when_released(Arc::downgrade(&owner), cancel.clone(), done);
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert!(!cancel.load(Ordering::Acquire));
    }

    #[test]
    fn damaged_waveform_caches_are_refetched_and_bad_replies_do_not_poison_retries() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::remote_mode::tests::child_waveform_cache_validation",
                "--ignored",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "helper invoked by the waveform cache regression in an isolated process"]
    fn child_waveform_cache_validation() {
        use audiolib_audio::{WaveformData, WaveformPeak, WaveformRequest};
        use std::io::{Read, Write};

        i_slint_backend_testing::init_no_event_loop();
        let root = std::env::temp_dir().join(format!(
            "audiolib-waveform-cache-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
        let engine = audiolib_audio::AudioEngine::new(PlaybackConfig::default()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(
            &ui,
            provider,
            engine.handle(),
            AppConfig::load_in(root.join("preferences")).unwrap(),
            PlaybackConfig::default(),
        );
        controller.remote.lock().unwrap().state = Some(CacheState {
            library_id: "lib-1".into(),
            revision: 1,
            ..Default::default()
        });
        let request = WaveformRequest {
            start_frame: 100,
            end_frame: Some(164),
            samples_per_pixel: 1,
            include_colors: true,
        };
        let valid = WaveformData {
            sample_rate: 48000,
            channels: 1,
            frames: 64,
            samples_per_pixel: 1,
            peaks: vec![vec![
                WaveformPeak {
                    min: -0.5,
                    max: 0.5,
                    rms: 0.25,
                    ..Default::default()
                };
                64
            ]],
        };
        let mut value = serde_json::to_value(&valid).unwrap();
        value["start_frame"] = 100.into();
        value["end_frame"] = 164.into();
        value["total_frames"] = 1000.into();
        let valid_json = serde_json::to_vec(&value).unwrap();
        value["start_frame"] = 200.into();
        value["end_frame"] = 264.into();
        let wrong_range_json = serde_json::to_vec(&value).unwrap();
        let mut damaged = valid.clone();
        damaged.peaks[0].pop();
        let key = "7-100-164-64-1";
        controller
            .remote
            .lock()
            .unwrap()
            .waveforms
            .insert(format!("lib-1/1/{key}"), damaged);
        let library = RemoteCache::new(&controller.data_root()).library_dir("lib-1");
        let directory = controller
            .config
            .lock()
            .unwrap()
            .paths
            .cache
            .join("waveforms/server")
            .join(library.file_name().unwrap());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(format!("{key}.json")), &wrong_range_json).unwrap();
        std::fs::write(directory.join(format!("{key}.etag")), "\"damaged\"").unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        controller.config.lock().unwrap().data_source.server_url =
            format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            for json in [&valid_json, &wrong_range_json, &valid_json] {
                let deadline = Instant::now() + Duration::from_secs(5);
                let (mut socket, _) = loop {
                    match listener.accept() {
                        Ok((socket, address)) => {
                            // Windows inherits the listener's nonblocking mode.
                            socket.set_nonblocking(false).unwrap();
                            break (socket, address);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "Waveform request did not arrive");
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("Accept Waveform request: {error}"),
                    }
                };
                socket
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut bytes = Vec::new();
                let mut chunk = [0; 2048];
                let head = loop {
                    assert!(Instant::now() < deadline, "Waveform headers did not arrive");
                    let count = match socket.read(&mut chunk) {
                        Ok(0) => panic!("Request ended before its headers"),
                        Ok(count) => count,
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::TimedOut
                                    | std::io::ErrorKind::Interrupted
                            ) =>
                        {
                            continue;
                        }
                        Err(error) => panic!("Read Waveform headers: {error}"),
                    };
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        break String::from_utf8(bytes[..end + 2].to_vec()).unwrap();
                    }
                };
                assert!(
                    !head.to_ascii_lowercase().contains("if-none-match:"),
                    "{head}"
                );
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: \"valid\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", json.len()).unwrap();
                socket.write_all(json).unwrap();
            }
        });
        let cancel = AtomicBool::new(false);
        let mut deferred = true;
        let data = controller
            .remote_waveform_segment(7, 48000, request.clone(), 64, &cancel, &mut deferred)
            .unwrap();
        assert_eq!(data.peaks[0].len(), 64);
        // A new ID has no cache: reject the wrong source interval without storing it.
        assert!(
            controller
                .remote_waveform_segment(8, 48000, request.clone(), 64, &cancel, &mut deferred)
                .is_err()
        );
        assert!(!directory.join("8-100-164-64-1.json").exists());
        assert!(
            !controller
                .remote
                .lock()
                .unwrap()
                .waveforms
                .contains_key("lib-1/1/8-100-164-64-1")
        );
        let data = controller
            .remote_waveform_segment(8, 48000, request, 64, &cancel, &mut deferred)
            .unwrap();
        assert_eq!(data.peaks[0].len(), 64);
        server.join().unwrap();
        controller.shutdown();
        drop(controller);
        drop(ui);
        drop(engine);
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn player_label_uses_the_record_name_for_cached_server_copies() {
        let cached = Path::new("C:/data/remote/lib/audio-cache/3785-18445718-083eb760f0a530e4.wav");
        assert_eq!(
            playback_name(
                cached,
                Some("1966 Chevelle SS Big Block APPROACH FAST, AWAY MEDIUM ORTF A.wav")
            ),
            "1966 Chevelle SS Big Block APPROACH FAST, AWAY MEDIUM ORTF A.wav"
        );
        // Local mode (no record name) keeps the file's own name.
        assert_eq!(
            playback_name(Path::new("Z:/ExampleLibrary/雷声 近.wav"), None),
            "雷声 近.wav"
        );
        assert_eq!(
            playback_name(cached, Some("  ")),
            "3785-18445718-083eb760f0a530e4.wav"
        );
    }

    /// Loopback stand-in for the metadata write endpoints: records `head + body` of each
    /// request and answers every one with `response` (JSON, 200).
    fn serve_write(response: String, requests: Arc<Mutex<Vec<String>>>) -> String {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                requests
                    .lock()
                    .unwrap()
                    .push(format!("{head}\r\n{}", String::from_utf8_lossy(&body)));
                let response = if head.starts_with("GET /api/v1/library ") {
                    // Writer preflight binds mutations to the displayed snapshot.
                    r#"{"library_id":"lib-1","identity_bound_writes":true,"revision":1,"schema_version":5}"#.to_owned()
                } else if head.starts_with("GET /api/v1/health ") {
                    serde_json::json!({"status":"ok","version":audiolib_core::compatibility::compatibility_manifest().application_version}).to_string()
                } else if head.starts_with("GET /api/v1/capabilities ") {
                    serde_json::json!({"compatibility":audiolib_core::compatibility::compatibility_manifest()}).to_string()
                } else {
                    response.clone()
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        url
    }

    /// Loopback stand-in for `GET /api/v1/audio/{id}/stream` (single `bytes=N-` ranges).
    fn serve_stream(body: Vec<u8>, requests: Arc<Mutex<Vec<String>>>) -> String {
        serve_stream_after(body, requests, Duration::ZERO)
    }

    /// [`serve_stream`] whose first connection waits `wake` before answering (a NAS disk
    /// spinning up); later connections answer at once.
    fn serve_stream_after(
        body: Vec<u8>,
        requests: Arc<Mutex<Vec<String>>>,
        wake: Duration,
    ) -> String {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut wake = Some(wake).filter(|wake| !wake.is_zero());
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                if let Some(delay) = wake.take() {
                    std::thread::sleep(delay);
                }
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut head = String::new();
                let mut start = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        start = value.trim().trim_end_matches('-').parse::<usize>().ok();
                    }
                    head.push_str(&line);
                }
                requests.lock().unwrap().push(head.clone());
                let len = body.len();
                let from = start.unwrap_or(0).min(len);
                let (status, range) = match start {
                    Some(_) => (
                        "206 Partial Content",
                        format!("Content-Range: bytes {from}-{}/{len}\r\n", len - 1),
                    ),
                    None => ("200 OK", String::new()),
                };
                let part = &body[from..];
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nETag: \"e1\"\r\n{range}Connection: close\r\n\r\n",
                    part.len()
                );
                let _ = stream.write_all(response.as_bytes());
                if !head.starts_with("HEAD") {
                    let _ = stream.write_all(part);
                }
            }
        });
        url
    }

    #[test]
    fn waking_hint_appears_after_the_threshold_with_seconds() {
        assert_eq!(
            super::waking_hint("雷声.wav", Duration::from_millis(1000), true),
            None
        );
        assert_eq!(
            super::waking_hint("雷声.wav", Duration::from_millis(12_400), true).as_deref(),
            Some("正在唤醒音效库磁盘… 12 秒（雷声.wav）")
        );
        assert_eq!(
            super::waking_hint("a.wav", Duration::from_secs(2), false).as_deref(),
            Some("Waking the library disk… 2 s (a.wav)")
        );
        let timeout = RemoteError::Timeout("http://x/s: no response within 100 s".into());
        assert!(super::stream_error_text(&timeout, None, true).contains("音效库磁盘可能无法唤醒"));
        assert!(
            !super::stream_error_text(&RemoteError::Timeout("x".into()), None, false)
                .contains("library disk")
        );
        // 404: the file is missing on a reachable root (an offline root is a 503).
        let root = RemoteRoot {
            name: "SFX".into(),
            server_path: "/volume1/ExampleLibrary/SFX".into(),
            ..Default::default()
        };
        let missing = RemoteError::NotFound("audio 7".into());
        let text = super::stream_error_text(&missing, Some(&root), true);
        assert!(
            text.contains("「SFX」") && text.contains("改名或删除") && !text.contains("离线"),
            "{text}"
        );
        let text = super::stream_error_text(&missing, Some(&root), false);
        assert!(
            text.contains("root \"SFX\"") && !text.contains("offline"),
            "{text}"
        );
        assert!(!super::stream_error_text(&missing, None, true).contains("离线"));
        // §6.4 root status: a stream's 503 UNAVAILABLE shows the server's message.
        let offline = RemoteError::RootOffline {
            message:
                "Library root \"SFX\" is offline: Root directory not found (volume not mounted?)"
                    .into(),
            retry_after: Some(5),
        };
        let text = super::stream_error_text(&offline, Some(&root), true);
        assert!(
            text.contains("根目录离线") && text.contains("volume not mounted"),
            "{text}"
        );
        let roots = vec![
            RemoteRoot {
                name: "SFX".into(),
                online: Some(false),
                status: Some("offline".into()),
                status_message: Some("Root directory not found (volume not mounted?)".into()),
                ..Default::default()
            },
            RemoteRoot {
                name: "Foley".into(),
                online: Some(true),
                status: Some("waking".into()),
                ..Default::default()
            },
            RemoteRoot {
                name: "Ambience".into(),
                ..Default::default()
            },
        ];
        assert_eq!(
            super::root_summary(&roots, true),
            " · 根目录离线：SFX · 磁盘唤醒中：Foley"
        );
        assert_eq!(super::root_summary(&roots[2..], false), "");
        let notice = super::root_notice(&roots, true).unwrap();
        assert!(
            notice.contains("「SFX」（Root directory not found") && notice.contains("不标为缺失"),
            "{notice}"
        );
        assert!(!notice.contains("Foley"));
        assert!(super::root_notice(&roots[1..], false).is_none());
    }

    #[test]
    fn address_forms_normalise_to_the_saved_base_url() {
        let url = |text: &str| AddressForm {
            url: text.into(),
            ..Default::default()
        };
        // Legacy value: saved verbatim, same API base as before.
        let legacy = url(" http://server.example.lan:8790 ").address().unwrap();
        assert_eq!(legacy.base_url(), "http://server.example.lan:8790");
        assert_eq!(legacy.api_url(), "http://server.example.lan:8790/api/v1");
        // No scheme means https; a pasted /api/v1 is not doubled.
        let bare = url("audiolib.example.com:8443/audiolib/api/v1/")
            .address()
            .unwrap();
        assert_eq!(
            bare.base_url(),
            "https://audiolib.example.com:8443/audiolib"
        );
        let parts = AddressForm {
            parts: true,
            scheme: 0,
            host: " audiolib.example.com ".into(),
            port: "8443".into(),
            prefix: "audiolib/".into(),
            ..Default::default()
        };
        assert_eq!(
            parts.address().unwrap().api_url(),
            "https://audiolib.example.com:8443/audiolib/api/v1"
        );
        let http = AddressForm {
            scheme: 1,
            host: "server.example.lan".into(),
            port: "8790".into(),
            prefix: String::new(),
            ..parts.clone()
        };
        assert_eq!(
            http.address().unwrap().base_url(),
            "http://server.example.lan:8790"
        );
        // Invalid input explains itself.
        let bad = AddressForm {
            port: "99999".into(),
            ..parts
        };
        let error = bad.address().unwrap_err();
        let (hint, is_error) = address_hint(&Err(error), true);
        assert!(is_error && hint.contains("端口"), "{hint}");
        let (hint, is_error) = address_hint(&Ok(bare), false);
        assert!(!is_error && hint.contains("https://audiolib.example.com:8443/audiolib/api/v1"));
        let (hint, _) = address_hint(&Ok(legacy), true);
        assert!(hint.contains("不加密"), "{hint}");
        let (hint, is_error) = address_hint(&url("  ").address(), true);
        assert!(!is_error && hint.contains("https://"), "{hint}");
    }

    #[test]
    fn connection_verdict_mentions_optional_warnings_without_failing() {
        assert_eq!(connection_verdict(0, true), "\n全部检查通过。");
        assert_eq!(connection_verdict(0, false), "\nAll checks passed.");
        assert!(connection_verdict(1, true).contains("连接正常；1 项可选检查未通过"));
        assert!(connection_verdict(2, false).contains("2 optional check(s) failed"));
    }

    #[test]
    fn a_root_check_result_replaces_the_live_entry() {
        let root = |name: &str, status: &str| RemoteRoot {
            name: name.into(),
            server_path: format!("/srv/{name}"),
            status: Some(status.into()),
            ..Default::default()
        };
        let mut live = vec![root("a", "offline"), root("b", "offline")];
        merge_roots(&mut live, &[root("b", "online"), root("unknown", "online")]);
        assert_eq!(live.len(), 2);
        assert_eq!(live[0].state(), RootState::Offline);
        assert_eq!(live[1].state(), RootState::Online);
        assert!(super::root_notice(&live, false).unwrap().contains("\"a\""));
        assert_eq!(
            root_retry_message(&[], false),
            "Library roots are online again"
        );
        assert!(root_retry_message(&["a".into()], false).contains("still offline: a"));
    }

    #[test]
    fn persisted_roots_lose_their_stale_status() {
        let roots = vec![RemoteRoot {
            name: "sfx".into(),
            server_path: "/srv/sfx".into(),
            webdav_url: Some("http://nas/sfx".into()),
            online: Some(false),
            status: Some("offline".into()),
            status_message: Some("Root directory not found".into()),
            checked_at: Some(1),
            ..Default::default()
        }];
        let fresh = without_status(&roots);
        assert_eq!(fresh[0].state(), RootState::Online);
        assert_eq!(fresh[0].status_message, None);
        assert_eq!(fresh[0].server_path, "/srv/sfx");
        assert_eq!(fresh[0].webdav_url.as_deref(), Some("http://nas/sfx"));
        assert!(!webdav_first(Some(&fresh[0]), true));
    }

    #[test]
    fn an_offline_root_streams_from_a_configured_webdav_url_first() {
        let root = |status: &str| RemoteRoot {
            name: "sfx".into(),
            server_path: "/srv/sfx".into(),
            status: Some(status.into()),
            ..Default::default()
        };
        assert!(webdav_first(Some(&root("offline")), true));
        assert!(!webdav_first(Some(&root("offline")), false));
        for status in ["online", "waking", "checking"] {
            assert!(!webdav_first(Some(&root(status)), true), "{status}");
        }
        assert!(!webdav_first(None, true));
    }

    #[test]
    fn webdav_fallback_needs_a_user_configured_address() {
        let file = AudioFile {
            file_path: "/srv/sfx/a b.wav".into(),
            ..Default::default()
        };
        let roots = vec![RemoteRoot {
            name: "sfx".into(),
            server_path: "/srv/sfx".into(),
            webdav_url: Some("http://server/dav/sfx".into()),
            ..Default::default()
        }];
        let mut settings = DataSourceSettings::default();
        let secrets = RemoteSecrets::default();
        assert!(
            configured_webdav_source(&file, &roots, &settings, &secrets).is_none(),
            "a server-suggested WebDAV URL is not a fallback"
        );
        settings
            .webdav
            .root_urls
            .insert("sfx".into(), "http://nas/sfx".into());
        settings.webdav.username = "dav".into();
        let source = configured_webdav_source(&file, &roots, &settings, &secrets).unwrap();
        assert!(source.url.starts_with("http://nas/sfx/a"), "{}", source.url);
        assert!(matches!(source.auth, SourceAuth::Basic(_)));
    }
    #[test]
    fn revision_notice_decides_what_the_user_sees() {
        assert_eq!(
            revision_notice(
                &RevisionCheck::UpToDate { revision: 3 },
                true,
                AutoSync::Off,
                None
            ),
            None
        );
        let (text, kind) = revision_notice(
            &RevisionCheck::NewRevision {
                cached: 3,
                server: 5,
            },
            true,
            AutoSync::Off,
            None,
        )
        .unwrap();
        assert_eq!(kind, NOTICE_UPDATE);
        assert!(text.contains("r5") && text.contains("r3"), "{text}");
        let (text, kind) = revision_notice(
            &RevisionCheck::NewRevision {
                cached: 3,
                server: 9,
            },
            true,
            AutoSync::Deferred,
            None,
        )
        .unwrap();
        assert_eq!(kind, NOTICE_UPDATE);
        assert!(
            text.contains("试听结束后自动同步") && text.contains("r9"),
            "{text}"
        );
        let (text, _) = revision_notice(
            &RevisionCheck::NewRevision {
                cached: 3,
                server: 5,
            },
            false,
            AutoSync::Now,
            None,
        )
        .unwrap();
        assert!(text.contains("automatically"), "{text}");
        let (text, kind) = revision_notice(
            &RevisionCheck::Offline("x".into()),
            true,
            AutoSync::Off,
            Some("12:00:00"),
        )
        .unwrap();
        assert_eq!(kind, NOTICE_RETRY);
        assert!(text.contains("离线") && text.contains("12:00:00"), "{text}");
        // A slow answer is not offline: its own wording, still a retry notice.
        let (text, kind) = revision_notice(
            &RevisionCheck::Slow("x".into()),
            true,
            AutoSync::Off,
            Some("12:00:00"),
        )
        .unwrap();
        assert_eq!(kind, NOTICE_RETRY);
        assert!(
            text.contains("响应慢") && text.contains("12:00:00") && !text.contains("离线"),
            "{text}"
        );
        let (text, _) =
            revision_notice(&RevisionCheck::Slow("x".into()), false, AutoSync::Off, None).unwrap();
        assert!(
            text.contains("slowly") && !text.contains("offline"),
            "{text}"
        );
        let (text, kind) = revision_notice(
            &RevisionCheck::Starting {
                message: "503".into(),
                retry_after: Some(5),
            },
            false,
            AutoSync::Off,
            None,
        )
        .unwrap();
        assert_eq!(kind, NOTICE_RETRY);
        assert!(text.contains("starting") && !text.contains("401"), "{text}");
        let (_, kind) = revision_notice(
            &RevisionCheck::Unauthorized("401".into()),
            true,
            AutoSync::Off,
            None,
        )
        .unwrap();
        assert_eq!(kind, NOTICE_SETTINGS);
        // No token saved and the server wants one: the notice says so.
        let required = RemoteError::TokenRequired("GET /library".into()).localized(true);
        let (text, kind) = revision_notice(
            &RevisionCheck::Unauthorized(required),
            true,
            AutoSync::Off,
            None,
        )
        .unwrap();
        assert_eq!(kind, NOTICE_SETTINGS);
        assert!(text.contains("服务器要求访问令牌"), "{text}");
        assert!(tokenless_note(true).contains("未填写访问令牌"));
        let (text, kind) = revision_notice(
            &RevisionCheck::LibraryChanged {
                cached_id: "a".into(),
                server_id: "b".into(),
                server_revision: 1,
            },
            false,
            AutoSync::Off,
            None,
        )
        .unwrap();
        assert_eq!(kind, NOTICE_UPDATE);
        assert!(text.contains("a → b"), "{text}");
    }

    #[test]
    fn server_mode_writes_annotations_to_the_server_and_switches_back_to_local() {
        // Slint test backends are process-global; run the flow in an isolated child.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::remote_mode::tests::child_server_mode_routes_annotation_writes",
                "--ignored",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "Isolated server-mode regression failed: {}\n{stdout}\n{stderr}",
            output.status
        );
    }

    #[test]
    #[ignore = "helper invoked by the server-mode regression in an isolated process"]
    fn child_server_mode_routes_annotation_writes() {
        i_slint_backend_testing::init_no_event_loop();
        let root = std::env::temp_dir().join(format!(
            "audiolib-desktop-remote-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("sounds")).unwrap();
        let mut wav = Vec::new();
        wav.extend(b"RIFF");
        wav.extend(38u32.to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16u32.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(1u16.to_le_bytes());
        wav.extend(48000u32.to_le_bytes());
        wav.extend(96000u32.to_le_bytes());
        wav.extend(2u16.to_le_bytes());
        wav.extend(16u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend(2u32.to_le_bytes());
        wav.extend(0i16.to_le_bytes());
        std::fs::write(root.join("sounds/remote.wav"), &wav).unwrap();
        let preferences = root.join("preferences");
        let snapshot = preferences.join("remote/lib-1/library-r1-1.db");
        std::fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
        {
            // The "server" library; its backup is the snapshot the client opens.
            let server = Arc::new(SqliteProvider::open(&root.join("server.db")).unwrap());
            ScanService::new(server.clone())
                .scan(
                    &root.join("sounds"),
                    &ScanOptions::default(),
                    &ScanCancellation::default(),
                    |_| {},
                )
                .unwrap();
            server.backup_library(&snapshot).unwrap();
        }
        let provider = Arc::new(SqliteProvider::open_read_only(&snapshot).unwrap());
        let file = provider.search(&SearchRequest::default()).unwrap().results[0].clone();
        let mut config = AppConfig::load_in(preferences.clone()).unwrap();
        config.data_source.mode = DataSourceMode::Server;
        config.data_source.server_url = "http://127.0.0.1:9".into();
        let local_database = config.paths.database.clone();
        let engine = audiolib_audio::AudioEngine::new(PlaybackConfig::default()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new_with_source(
            &ui,
            provider.clone(),
            engine.handle(),
            config,
            PlaybackConfig::default(),
            Some(RemoteStartup {
                provider: provider.clone(),
                state: None,
            }),
        );
        assert!(controller.remote_read_only());
        assert!(ui.get_remote_mode());
        assert!(ui.get_data_source_label().contains("127.0.0.1:9"));
        assert!(
            !local_database.exists(),
            "server mode must not create the local library"
        );

        // Annotation edits go to the server; before the first sync they fail cleanly and
        // never reach the snapshot.
        *controller.selected.lock().unwrap() = Some(file.clone());
        present_file(&ui, &file);
        ui.set_edit_notes("must not be saved".into());
        ui.set_metadata_dirty(true);
        controller.save_metadata(&ui);
        let sequence = controller.metadata.lock().unwrap().sequence;
        let error = controller.wait_metadata(sequence, true).unwrap_err();
        assert!(
            error.contains("尚未同步") || error.contains("not been synced"),
            "{error}"
        );
        assert_eq!(provider.get_audio(file.id).unwrap().notes, file.notes);
        ui.invoke_prepare_scan(0, "".into());
        assert!(!ui.get_import_visible(), "scans are refused in server mode");

        // No structural write reaches the snapshot: repair, removal, maintenance, restore,
        // new/open library and import are refused; quick edits and an empty batch fail
        // or stop before writing (not synced yet).
        let snapshot_bytes = std::fs::read(&snapshot).unwrap();
        let record = format!("{:?}", provider.get_audio(file.id).unwrap());
        for action in [5, 10, 11, 12, 13, 14, 15, 20, 21] {
            controller.file_action(file.id, action);
        }
        for action in [2, 4, 5, 6, 7, 10, 11] {
            controller.library_action(action);
        }
        ui.invoke_folder_action_requested(root.join("sounds").display().to_string().into(), 1);
        controller.apply_batch();
        assert!(
            !ui.get_confirmation_visible(),
            "no destructive confirmation opens"
        );
        assert!(!ui.get_library_busy());
        assert_eq!(
            format!("{:?}", provider.get_audio(file.id).unwrap()),
            record
        );
        assert_eq!(std::fs::read(&snapshot).unwrap(), snapshot_bytes);
        // Background revision checks surface a non-blocking notice and the offline state.
        controller.apply_revision_check(
            &ui,
            None,
            RevisionCheck::Offline("down".into()),
            Some(super::unix_now() + 60),
            true,
        );
        assert_eq!(ui.get_remote_notice_kind(), super::NOTICE_RETRY);
        assert!(ui.get_remote_notice().contains("offline"));
        assert!(ui.get_metadata_writes_blocked());
        assert!(controller.require_metadata_writable().is_err());
        let offline_sequence = controller.metadata.lock().unwrap().sequence;
        controller.file_action(file.id, 5);
        assert_eq!(
            controller.metadata.lock().unwrap().sequence,
            offline_sequence
        );
        ui.invoke_remote_notice_dismissed();
        assert!(
            ui.get_remote_notice().is_empty(),
            "dismissed until it changes"
        );
        controller.apply_revision_check(
            &ui,
            None,
            RevisionCheck::Offline("still down".into()),
            Some(super::unix_now() + 120),
            true,
        );
        assert!(
            ui.get_remote_notice().is_empty(),
            "a later retry stays dismissed"
        );
        controller.apply_revision_check(
            &ui,
            None,
            RevisionCheck::NotSynced {
                server_id: "lib-9".into(),
                server_revision: 4,
            },
            Some(super::unix_now() + 60),
            true,
        );
        assert_eq!(ui.get_remote_notice_kind(), super::NOTICE_UPDATE);
        // auto_sync with a revision that has not settled yet (AutoSyncGate not due): no
        // sync starts, and the notice says it waits for the server to stop changing.
        controller.config.lock().unwrap().data_source.auto_sync = true;
        // Wait for the real startup worker; busy must clear even though this test
        // never runs the event loop that would consume its completion publication.
        let deadline = Instant::now() + Duration::from_secs(5);
        while controller.remote_busy.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!controller.remote_busy.load(Ordering::Acquire));
        let generation = controller.remote_generation.load(Ordering::Acquire);
        controller.apply_revision_check(
            &ui,
            None,
            RevisionCheck::NewRevision {
                cached: 3,
                server: 9,
            },
            Some(super::unix_now() + 60),
            false,
        );
        assert!(controller.remote.lock().unwrap().auto_deferred);
        assert_eq!(
            controller.remote_generation.load(Ordering::Acquire),
            generation,
            "no automatic sync while the revision is still changing"
        );
        assert_eq!(ui.get_remote_notice_kind(), super::NOTICE_UPDATE);
        assert!(
            ui.get_remote_notice().contains("once the server settles"),
            "{}",
            ui.get_remote_notice()
        );
        controller.config.lock().unwrap().data_source.auto_sync = false;
        // A result checked against another snapshot than the open one is stale.
        controller.apply_revision_check(
            &ui,
            Some(CachedIdentity {
                library_id: "other".into(),
                revision: 1,
            }),
            RevisionCheck::Offline("stale".into()),
            None,
            true,
        );
        assert_eq!(ui.get_remote_notice_kind(), super::NOTICE_UPDATE);
        // Library up to date but a root offline (§6.4, from the polled /roots): the notice
        // names it with Retry; nothing about it marks records missing.
        controller.remote.lock().unwrap().live_roots = Some(vec![RemoteRoot {
            name: "SFX".into(),
            online: Some(false),
            status: Some("offline".into()),
            status_message: Some("Root directory not found".into()),
            ..Default::default()
        }]);
        controller.apply_revision_check(
            &ui,
            None,
            RevisionCheck::UpToDate { revision: 4 },
            None,
            true,
        );
        assert_eq!(ui.get_remote_notice_kind(), super::NOTICE_RETRY);
        assert!(
            ui.get_remote_notice().contains("SFX"),
            "{}",
            ui.get_remote_notice()
        );
        controller.remote.lock().unwrap().live_roots = None;

        // Without a synced snapshot the selection reports why it cannot play.
        let cancel = AtomicBool::new(false);
        let error = controller.playable_path(&file, &cancel, 0).unwrap_err();
        assert!(!error.is_empty());
        assert!(is_remote_cache_path(&preferences, &snapshot));

        // With a snapshot the record streams from /audio/{id}/stream into the audio cache
        // and decoders read it through the registered byte source meanwhile.
        let served = std::fs::read(root.join("sounds/remote.wav")).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        controller.config.lock().unwrap().data_source.server_url =
            serve_stream(served.clone(), requests.clone());
        controller.remote.lock().unwrap().state = Some(CacheState {
            library_id: "lib-1".into(),
            revision: 1,
            ..Default::default()
        });
        let path = controller.playable_path(&file, &cancel, 0).unwrap();
        assert!(is_remote_cache_path(&preferences, &path));
        // The player shows the record's name, not its virtual registration name.
        assert_ne!(path.file_name().unwrap().to_string_lossy(), file.filename);
        assert_eq!(controller.playback_display_name(&path), file.filename);
        assert!(audiolib_audio::is_stream_source(&path));
        assert_eq!(audiolib_audio::probe(&path).unwrap().sample_rate, 48000);
        let original_stream = controller
            .remote_stream
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .stream
            .clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        let stream = loop {
            // Re-selecting the record reuses its in-memory stream.
            let again = controller.playable_path(&file, &cancel, 0).unwrap();
            assert_eq!(again, path);
            let stream = controller
                .remote_stream
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .stream
                .clone();
            assert!(stream.shares_buffer_with(&original_stream));
            if stream.is_complete() || Instant::now() > deadline {
                break stream;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // Held in memory only: nothing is written to disk for an audition.
        assert!(!path.exists());
        let mut held = Vec::new();
        std::io::Read::read_to_end(&mut stream.reader().unwrap(), &mut held).unwrap();
        assert_eq!(held, served);
        drop(stream);
        drop(original_stream);
        assert_eq!(
            requests.lock().unwrap().len(),
            1,
            "reuse must not refetch audio"
        );
        let stream_path = format!("/api/v1/audio/{}/stream", file.id);
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .any(|head: &String| head.contains(&stream_path))
        );
        // A synced snapshot: saves PATCH the server and, on a matching revision, the
        // returned record is applied to the open snapshot without a full sync.
        let cache = RemoteCache::new(&controller.data_root());
        let synced = CacheState {
            library_id: "lib-1".into(),
            revision: 1,
            database_file: "library-r1-1.db".into(),
            ..Default::default()
        };
        assert_eq!(cache.database_path(&synced), snapshot);
        cache.save_state(&synced).unwrap();
        let mut saved = provider.get_audio(file.id).unwrap();
        saved.notes = "saved on the server".into();
        let writes = Arc::new(Mutex::new(Vec::new()));
        controller.config.lock().unwrap().data_source.server_url = serve_write(
            serde_json::json!({"file": saved, "revision_before": 1, "revision": 2}).to_string(),
            writes.clone(),
        );
        controller.remote.lock().unwrap().state = Some(synced);
        ui.set_edit_notes("saved on the server".into());
        ui.set_metadata_dirty(true);
        controller.save_metadata(&ui);
        let sequence = controller.metadata.lock().unwrap().sequence;
        controller.wait_metadata(sequence, false).unwrap();
        let request = writes.lock().unwrap().join("\n---\n");
        assert!(
            request.contains(&format!("PATCH /api/v1/audio/{}/metadata", file.id)),
            "{request}"
        );
        assert!(
            request.contains(r#""notes":"saved on the server""#),
            "{request}"
        );
        assert!(controller.metadata.lock().unwrap().failures.is_empty());
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-audiolib-library-id: lib-1")
        );
        assert_eq!(
            provider.get_audio(file.id).unwrap().notes,
            "saved on the server"
        );
        assert_eq!(
            controller
                .remote
                .lock()
                .unwrap()
                .state
                .as_ref()
                .unwrap()
                .revision,
            2
        );
        assert_eq!(cache.state("lib-1").unwrap().revision, 2);

        // Once a replacement is known, later offline results cannot make cached
        // IDs writable. A provider created before the check is server-bound too.
        controller.apply_revision_check(
            &ui,
            Some(CachedIdentity {
                library_id: "lib-1".into(),
                revision: 2,
            }),
            RevisionCheck::LibraryChanged {
                cached_id: "lib-1".into(),
                server_id: "lib-2".into(),
                server_revision: 1,
            },
            None,
            false,
        );
        assert!(ui.get_remote_writes_blocked());
        assert!(controller.remote_provider().is_err());
        controller.apply_revision_check(
            &ui,
            Some(CachedIdentity {
                library_id: "lib-1".into(),
                revision: 2,
            }),
            RevisionCheck::Offline("down".into()),
            None,
            false,
        );
        assert!(controller.remote_provider().is_err());
        // Opening the validated snapshot clears the gate; a late writer from
        // a different database filename cannot block this current provider.
        controller
            .switch_data_source(&DataSourceSwitch::Server {
                database: snapshot.clone(),
                state: cache.state("lib-1").map(Box::new),
            })
            .unwrap();
        controller.remote_write_event(
            "lib-1",
            "stale-snapshot.db",
            audiolib_remote::WriteEvent::LibraryChanged {
                reason: "old provider conflict".into(),
            },
        );
        assert!(controller.remote_provider().is_ok());
        controller.remote_write_event(
            "another-library",
            "library-r1-1.db",
            audiolib_remote::WriteEvent::LibraryChanged {
                reason: "same filename in an old library".into(),
            },
        );
        assert!(controller.remote_provider().is_ok());
        controller.publish_data_source(&ui);
        assert!(!ui.get_remote_writes_blocked());
        let stale = CacheState {
            library_id: "lib-1".into(),
            database_file: "stale-snapshot.db".into(),
            ..Default::default()
        };
        controller.note_remote_identity_error(
            &ui,
            &stale,
            &RemoteError::LibraryChanged("late scan result".into()),
        );
        assert!(!ui.get_remote_writes_blocked());
        assert!(controller.remote_provider().is_ok());

        // A sleeping NAS disk: the first byte takes 3 s (20-30 s in reality). Stop gives up
        // the pending request at once; otherwise the selection simply waits for it, and
        // the slow stream never marks the server offline.
        controller.stop_remote_stream();
        // Forget the complete copy so the selection has to ask the server again.
        audiolib_remote::MemoryCache::global().clear();
        let check_before = format!("{:?}", controller.remote.lock().unwrap().check);
        let roots_before = format!("{:?}", controller.remote.lock().unwrap().live_roots);
        let polls_before = *controller.poll_signal.lock().unwrap();
        let wake = Duration::from_secs(3);
        controller.config.lock().unwrap().data_source.server_url =
            serve_stream_after(served.clone(), Arc::new(Mutex::new(Vec::new())), wake);
        let pending = Arc::new(AtomicBool::new(false));
        *controller.remote_audio_cancel.lock().unwrap() = pending.clone();
        let started = Instant::now();
        let waiting = {
            let (c, f, pending) = (controller.clone(), file.clone(), pending.clone());
            let generation = controller.selection_generation.load(Ordering::Acquire);
            std::thread::spawn(move || c.playable_path(&f, &pending, generation))
        };
        std::thread::sleep(Duration::from_millis(500));
        controller.interrupt_remote_audio();
        assert!(
            waiting.join().unwrap().is_err(),
            "Stop cancels the pending request"
        );
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "cancelled after {:?}",
            started.elapsed()
        );
        let woken_requests = Arc::new(Mutex::new(Vec::new()));
        controller.config.lock().unwrap().data_source.server_url =
            serve_stream_after(served.clone(), woken_requests.clone(), wake);
        let started = Instant::now();
        let generation = controller.selection_generation.load(Ordering::Acquire);
        let woken = controller
            .playable_path(&file, &AtomicBool::new(false), generation)
            .unwrap();
        assert!(started.elapsed() >= wake - Duration::from_millis(200));
        // Stop removed the previous registration and the owned cache was cleared.
        // This fresh response owns a new path; selecting it again must still reuse
        // the same bytes and registration without another network request.
        assert_ne!(woken, path);
        assert!(!audiolib_audio::is_stream_source(&path));
        assert!(audiolib_audio::is_stream_source(&woken));
        assert_eq!(controller.remote_audio_id(&woken), Some(file.id));
        assert_eq!(controller.playback_display_name(&woken), file.filename);
        let woken_stream = controller.remote_stream.lock().unwrap().clone().unwrap();
        let reused = controller
            .playable_path(&file, &AtomicBool::new(false), generation)
            .unwrap();
        assert_eq!(reused, woken);
        assert!(
            controller
                .remote_stream
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .stream
                .shares_buffer_with(&woken_stream.stream)
        );
        assert_eq!(
            woken_requests.lock().unwrap().len(),
            1,
            "reuse must not refetch audio"
        );
        drop(woken_stream);
        // A slow first byte is waited for: it neither records a failed check nor asks the
        // poller to re-check the server or its roots (only ROOT_OFFLINE does that).
        assert_eq!(
            format!("{:?}", controller.remote.lock().unwrap().check),
            check_before
        );
        assert_eq!(
            format!("{:?}", controller.remote.lock().unwrap().live_roots),
            roots_before
        );
        assert_eq!(
            *controller.poll_signal.lock().unwrap(),
            polls_before,
            "a slow stream does not wake the revision poller"
        );

        // Menu removals: refused while the server has not let this PC manage (above);
        // once it has, the record removal asks with the 服务器音效库 page's wording.
        let server_url = controller
            .config
            .lock()
            .unwrap()
            .data_source
            .server_url
            .clone();
        // (The snapshot already holds the PATCH applied above.)
        let snapshot_bytes = std::fs::read(&snapshot).unwrap();
        controller.remote.lock().unwrap().manage =
            super::server_removal::ServerManage::allowed_for_test(&server_url);
        controller.publish_server_manage(&ui);
        assert!(ui.get_server_can_manage());
        controller.file_action(file.id, 21);
        assert!(ui.get_confirmation_visible());
        let confirmation = ui.get_confirmation_text().to_string();
        assert!(
            confirmation.contains("服务器上的 WAV 文件不会改动")
                || confirmation.contains("WAV files on the server are untouched"),
            "{confirmation}"
        );
        ui.invoke_confirmation_requested(false);
        assert!(controller.pending_operation.lock().unwrap().is_none());
        assert_eq!(std::fs::read(&snapshot).unwrap(), snapshot_bytes);
        // After the server's 200 the removed records vanish from the view at once (an
        // in-memory overlay; the snapshot file is untouched) until a later snapshot opens.
        let folder = provider.folders(None).unwrap()[0].clone();
        controller.hide_removed(&[file.id], &controller.removed_folder_subtree(folder.id), 1);
        let mut page = provider.search(&SearchRequest::default()).unwrap();
        controller.hide_removed_in_page(&mut page);
        assert!(page.results.is_empty() && page.total_count == 0, "{page:?}");
        let mut folders = provider.search_folders(&SearchRequest::default()).unwrap();
        assert!(!folders.is_empty());
        controller.hide_removed_folders(&mut folders);
        assert!(folders.is_empty(), "{folders:?}");
        assert_eq!(controller.hidden_record_count(), 1);
        assert_eq!(std::fs::read(&snapshot).unwrap(), snapshot_bytes);
        // The same revision keeps hiding; the resync's later snapshot wins.
        assert!(!controller.settle_hidden());
        controller
            .remote
            .lock()
            .unwrap()
            .state
            .as_mut()
            .unwrap()
            .revision = 3;
        assert!(controller.settle_hidden());
        let mut page = provider.search(&SearchRequest::default()).unwrap();
        controller.hide_removed_in_page(&mut page);
        assert_eq!(page.results.len(), 1);
        assert_eq!(controller.hidden_record_count(), 0);

        // Switching back reopens (here: creates the default) local library.
        let message = controller
            .switch_data_source(&DataSourceSwitch::Local)
            .unwrap();
        assert!(message.contains("local mode"), "{message}");
        assert!(!controller.remote_read_only());
        assert!(local_database.exists());
        assert!(
            !audiolib_audio::is_stream_source(&path),
            "leaving server mode stops the stream"
        );

        controller.shutdown();
        drop(controller);
        drop(ui);
        drop(engine);
        drop(provider);
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        std::thread::sleep(Duration::from_millis(100));
        std::fs::remove_dir_all(root).ok();
    }
}
