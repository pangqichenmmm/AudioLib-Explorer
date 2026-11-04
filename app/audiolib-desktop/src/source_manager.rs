// app/audiolib-desktop/src/source_manager.rs
//! The unified 「音效库管理」 window (design `server-mgmt-gui.md` A1–B2): a
//! source list (the local library plus every saved server profile) and, per server,
//! the pages 连接 (connection form, token, transcode, test), 服务器音效库
//! (roots, capabilities, scan start/pause/resume/cancel and record removal when
//! `can_manage`, otherwise read-only) and 缓存与导出 (the cached snapshot; for the
//! active server the library management content working on that snapshot).
//!
//! Editing a profile never switches the data source: only 「切换到此数据源」 does, through
//! [`Controller::activate_data_source`]. Saving the active profile's connection resets the
//! per-server runtime state like a switch. Tokens are never loaded into the window (the
//! field stays empty; a placeholder says whether one is saved). Network and disk work
//! runs on worker threads; results are discarded when the selection, page or window
//! changed in the meantime.
use super::{
    ADDRESS_FORM_URL, Controller, LibraryOperation, ServerAddress, cached_source_matches, cn_or,
    format_clock, format_time, host_of, post, set_address_parts, short_id, tokenless_note,
    unix_now, without_status,
};
use crate::{AppWindow, ServerRootRow, SourceRow};
use audiolib_core::config::DEFAULT_LOCAL_LIBRARY_ID;
use audiolib_core::config::{ActiveSource, DataSourceMode, DataSourceSettings, TestSummary};
use audiolib_core::profiles::{MAX_PROFILE_NAME_CHARS, ServerProfile, same_server_url};
use audiolib_core::{ScanSession, ScanState};
use audiolib_remote::{
    CacheState, Capabilities, ConnectionReport, ListWaveformStatus, ManageMode, ProfileEdit,
    RemoteCache, RemoteClient, RemoteError, RemoteRoot, RemoteSecrets, RootScan, RootState,
    ScanControl, ScanMode, ScanRequest, ServerScan, ServerSettings, edit_transcode,
    manage_error_text, remove_server_profile, root_status_text, save_profile_edit, save_secrets,
    test_connection, use_external_address,
};
use slint::{Model, ModelRc, SharedString, VecModel};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// How often the 服务器音效库 page polls an active server scan.
const SCAN_POLL: Duration = Duration::from_millis(500);
/// How often the 服务器音效库 page polls `GET /waveforms/status` while it stays open.
const WAVE_STATUS_POLL: Duration = Duration::from_secs(5);
/// Scan sessions shown on the 服务器音效库 page.
const HISTORY_LIMIT: usize = 20;
/// Server pages.
const PAGE_CONNECTION: i32 = 0;
const PAGE_LIBRARY: i32 = 1;
const PAGE_CACHE: i32 = 2;
/// `sm-open-requested` focus values.
const FOCUS_LIBRARY: i32 = 1;
const FOCUS_HISTORY: i32 = 2;

#[path = "source_roots.rs"]
mod source_roots;
pub(in crate::controller) use source_roots::RootOperation;

#[path = "local_sources.rs"]
mod local_sources;
pub(in crate::controller) use local_sources::LocalSourceRequest;

#[path = "detail_waveform_cache.rs"]
mod detail_waveform_cache;

#[path = "waveform_cache_page.rs"]
mod waveform_cache_page;
use waveform_cache_page::{
    WaveformCacheAction, WaveformCacheFetch, WaveformCacheState, parse_rate,
};

/// Row selected in the source list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum Selection {
    #[default]
    Local,
    LocalLibrary(String),
    Profile(String),
    /// A new server that is not saved yet.
    New,
}

fn local_selection(id: &str) -> Selection {
    if id == DEFAULT_LOCAL_LIBRARY_ID || id == "local" {
        Selection::Local
    } else {
        Selection::LocalLibrary(id.into())
    }
}

/// What to do after a successful save from the connection page.
#[derive(Clone, Copy, Debug)]
enum AfterSave {
    Nothing,
    Switch,
    /// Close the manager only after the requested settings have reached disk.
    Close,
    /// Open this source-list index (or -1 for a new server).
    Select(i32),
}

/// Window state kept with the remote runtime.
#[derive(Debug, Default)]
pub(super) struct SourceManager {
    selected: Selection,
    /// Bumped when the selection changes or the window closes: pending tests and saves
    /// no longer refill the form.
    form_generation: u64,
    /// Bumped on every page, selection or visibility change: server library loads and
    /// scan polling stop when theirs is no longer current.
    library_generation: u64,
    /// Bumped by each list Waveform status poll start; older polls stop.
    wave_poll: u64,
    /// The 「波形缓存」 section's capability, settings and 403 state (active server).
    wave_cache: WaveformCacheState,
    /// A busy indicator belongs to one request; invalidation releases it and a
    /// delayed completion cannot release a newer request's indicator.
    manage_sequence: u64,
    manage_pending: Option<u64>,
    /// Verified root list and edit identity belong to the displayed server page.
    roots: Vec<RemoteRoot>,
    root_edit_target: Option<String>,
    /// Each navigation or close supersedes pending directory listings.
    browser_sequence: u64,
    /// A structural roots refresh waits for the current management completion before
    /// replacing its page generation and request ownership.
    root_refresh_pending: bool,
    /// Close requested by Save-on-Close, once its active-source sync commits.
    close_after_sync: bool,
    /// Structured connection verdict retained for language changes; no credential fields.
    last_report: Option<(ConnectionReport, bool)>,
}

impl SourceManager {
    pub(super) fn invalidate_library(&mut self) {
        self.library_generation += 1;
        self.manage_pending = None;
        self.browser_sequence += 1;
        self.roots.clear();
        self.root_edit_target = None;
        self.root_refresh_pending = false;
    }
}

/// Root editor identity follows names and exact server paths. Health/count updates,
/// WebDAV suggestions and response ordering do not supersede a user's path draft.
pub(super) fn root_configuration_changed(before: &[RemoteRoot], after: &[RemoteRoot]) -> bool {
    fn identities(roots: &[RemoteRoot]) -> Vec<(&str, &str)> {
        let mut identities: Vec<_> = roots
            .iter()
            .map(|root| (root.name.as_str(), root.server_path.as_str()))
            .collect();
        identities.sort_unstable();
        identities
    }
    identities(before) != identities(after)
}

/// Everything the 服务器音效库 page shows, fetched together off the UI thread.
struct LibraryPage {
    host: String,
    roots: Vec<RemoteRoot>,
    capabilities: Result<Capabilities, RemoteError>,
    scan: Result<Option<ServerScan>, RemoteError>,
    history: Result<Vec<ScanSession>, RemoteError>,
    /// `GET /settings` and `GET /waveforms/status`, only when the server offers list
    /// Waveforms.
    waves: Option<(
        Result<ServerSettings, RemoteError>,
        Result<ListWaveformStatus, RemoteError>,
    )>,
    /// The 「波形缓存」 section through the active server's `DataProvider`; `None` hides
    /// it (another profile, local mode or no list Waveforms).
    waveform_cache: Option<WaveformCacheFetch>,
}

impl LibraryPage {
    fn fetch(
        client: &RemoteClient,
        host: String,
        cache: Option<&dyn audiolib_core::DataProvider>,
    ) -> Result<Self, RemoteError> {
        let roots = client.roots()?;
        let capabilities = client.capabilities();
        let waves = capabilities
            .as_ref()
            .is_ok_and(Capabilities::list_waveforms_usable)
            .then(|| (client.server_settings(), client.list_waveform_status()));
        Ok(Self {
            host,
            roots,
            capabilities,
            scan: client.current_scan(),
            history: client.scan_history(HISTORY_LIMIT, 0),
            waveform_cache: waves
                .is_some()
                .then(|| cache.map(WaveformCacheFetch::fetch))
                .flatten(),
            waves,
        })
    }

    fn waves_supported(&self) -> bool {
        self.waves.is_some()
    }

    fn scan_active(&self) -> bool {
        matches!(&self.scan, Ok(Some(scan)) if scan.active)
    }
}

/// The connection form as entered (read on the UI thread, applied in a worker).
struct FormInput {
    name: String,
    url: String,
    /// `None` keeps the saved token; `Some("")` clears it.
    token: Option<String>,
    transcode: bool,
}

impl Controller {
    pub(super) fn bind_source_manager(self: &Arc<Self>, ui: &AppWindow) {
        self.bind_source_roots(ui);
        self.bind_detail_waveform_cache(ui);
        self.bind_local_sources(ui);
        let c = self.clone();
        ui.on_sm_open_requested(move |focus| c.open_source_manager(focus));
        let c = self.clone();
        ui.on_sm_source_selected(move |index| c.select_source(index));
        let c = self.clone();
        ui.on_sm_add_requested(move || {
            c.set_selection(Selection::New);
            c.reload_selection();
        });
        let c = self.clone();
        ui.on_sm_revert_requested(move || c.reload_selection());
        let c = self.clone();
        ui.on_sm_page_selected(move |page| c.show_page(page));
        let c = self.clone();
        ui.on_sm_open_cache_requested(move || c.open_selected_cache());
        let c = self.clone();
        ui.on_sm_library_refresh_requested(move || c.show_page(PAGE_LIBRARY));
        let c = self.clone();
        ui.on_sm_save_requested(move || c.save_selected(AfterSave::Nothing));
        let c = self.clone();
        ui.on_shared_sync_save_requested(move || c.save_shared_sync_settings());
        let c = self.clone();
        ui.on_sm_pending_save_requested(move |index| {
            c.save_selected(if index == -3 {
                AfterSave::Close
            } else {
                AfterSave::Select(index)
            });
        });
        let c = self.clone();
        ui.on_sm_switch_requested(move || c.switch_to_selected());
        let c = self.clone();
        ui.on_sm_delete_requested(move || c.delete_selected());
        let c = self.clone();
        ui.on_sm_scan_start_requested(move || c.start_server_scan());
        let c = self.clone();
        ui.on_sm_scan_control_requested(move |action| c.control_server_scan(action));
        let c = self.clone();
        ui.on_sm_wave_pregenerate_requested(move |enabled| c.set_server_pregenerate(enabled));
        let c = self.clone();
        ui.on_sm_wc_job_requested(move |mode| {
            let mode = match mode {
                1 => audiolib_core::waveform_cache::WaveformJobMode::Failed,
                2 => audiolib_core::waveform_cache::WaveformJobMode::Rebuild,
                _ => audiolib_core::waveform_cache::WaveformJobMode::Missing,
            };
            // Never confirmed here: a rebuild only starts from the second confirmation.
            c.run_waveform_cache_action(WaveformCacheAction::Start(mode, false));
        });
        let c = self.clone();
        ui.on_sm_wc_rebuild_confirmed(move || {
            c.run_waveform_cache_action(WaveformCacheAction::Start(
                audiolib_core::waveform_cache::WaveformJobMode::Rebuild,
                true,
            ));
        });
        let c = self.clone();
        ui.on_sm_wc_control_requested(move |control| {
            let control = match control {
                1 => audiolib_core::waveform_cache::WaveformJobControl::Resume,
                2 => audiolib_core::waveform_cache::WaveformJobControl::Stop,
                _ => audiolib_core::waveform_cache::WaveformJobControl::Pause,
            };
            c.run_waveform_cache_action(WaveformCacheAction::Control(control));
        });
        let c = self.clone();
        ui.on_sm_wc_rate_requested(move |text| {
            let chinese = c.chinese.load(Ordering::Acquire);
            match parse_rate(&text, chinese) {
                Ok(rate) => c.run_waveform_cache_action(WaveformCacheAction::Rate(rate)),
                Err(message) => post(c.ui.clone(), move |ui| {
                    ui.set_sm_wave_message(message.into());
                }),
            }
        });
        let c = self.clone();
        ui.on_sm_remove_records_requested(move || c.remove_server_records());
        let c = self.clone();
        ui.on_sm_remove_folder_requested(move || c.remove_server_folder());
        let c = self.clone();
        ui.on_sm_closed(move || {
            let mut remote = c.remote.lock().unwrap();
            remote.manager.close_after_sync = false;
            remote.manager.form_generation += 1;
            remote.manager.invalidate_library();
            drop(remote);
            // A typed but unsaved token does not outlive the window.
            if let Some(ui) = c.ui.upgrade() {
                ui.set_sm_fs_visible(false);
                ui.set_sm_fs_busy(false);
                ui.set_sm_root_form(false);
                ui.set_sm_local_form(false);
                ui.set_ds_token("".into());
                c.publish_manage_busy(&ui);
            }
        });
        let c = self.clone();
        ui.on_data_source_test_requested(move || c.test_selected());
        let c = self.clone();
        ui.on_data_source_use_public_url(move || c.use_public_url());
    }

    /// Open on the active source: its connection page (数据源设置), its library page
    /// (音效库管理) or its history (local content, or the snapshot's on 缓存与导出).
    fn open_source_manager(self: &Arc<Self>, focus: i32) {
        let Some(ui) = self.ui.upgrade() else { return };
        let config = self.config.lock().unwrap();
        let active = config.data_source.active_source();
        let local = config
            .selected_local_library()
            .map(|library| library.id.clone());
        drop(config);
        let page = match (&active, focus) {
            (ActiveSource::Profile(_), FOCUS_LIBRARY) => PAGE_LIBRARY,
            (ActiveSource::Profile(_), FOCUS_HISTORY) => PAGE_CACHE,
            _ => PAGE_CONNECTION,
        };
        self.set_selection(match active {
            ActiveSource::Local => {
                local_selection(local.as_deref().unwrap_or(DEFAULT_LOCAL_LIBRARY_ID))
            }
            ActiveSource::Profile(id) => Selection::Profile(id),
        });
        ui.set_sm_page(page);
        ui.set_ds_message("".into());
        ui.set_sm_pending_select(-2);
        ui.set_sm_confirm(0);
        ui.set_sm_scan_form(false);
        self.reload_selection();
    }

    fn select_source(self: &Arc<Self>, index: i32) {
        let selection = self
            .ui
            .upgrade()
            .and_then(|ui| {
                usize::try_from(index)
                    .ok()
                    .and_then(|index| ui.get_sm_sources().row_data(index))
            })
            .map(|row| {
                if row.kind == 0 {
                    local_selection(row.id.as_str())
                } else {
                    Selection::Profile(row.id.to_string())
                }
            })
            .unwrap_or(Selection::Local);
        self.set_selection(selection);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_sm_page(PAGE_CONNECTION);
            ui.set_ds_message("".into());
        }
        self.reload_selection();
    }

    fn set_selection(&self, selection: Selection) {
        let mut remote = self.remote.lock().unwrap();
        remote.manager.close_after_sync = false;
        remote.manager.selected = selection;
        remote.manager.form_generation += 1;
        remote.manager.invalidate_library();
        drop(remote);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_sm_local_form(false);
            self.publish_manage_busy(&ui);
        }
    }

    fn selection(&self) -> Selection {
        self.remote.lock().unwrap().manager.selected.clone()
    }

    /// The saved profile behind the selection.
    fn selected_profile(&self) -> Option<ServerProfile> {
        match self.selection() {
            Selection::Profile(id) => self
                .config
                .lock()
                .unwrap()
                .data_source
                .profile(&id)
                .cloned(),
            _ => None,
        }
    }

    fn form_current(&self, generation: u64) -> bool {
        self.remote.lock().unwrap().manager.form_generation == generation
    }

    fn library_current(&self, generation: u64) -> bool {
        self.remote.lock().unwrap().manager.library_generation == generation
    }

    fn begin_manage_request(&self, ui: &AppWindow) -> (u64, u64) {
        let mut remote = self.remote.lock().unwrap();
        let manager = &mut remote.manager;
        manager.manage_sequence += 1;
        let request = manager.manage_sequence;
        manager.manage_pending = Some(request);
        let generation = manager.library_generation;
        drop(remote);
        ui.set_sm_manage_busy(true);
        (generation, request)
    }

    fn finish_manage_request(self: &Arc<Self>, ui: &AppWindow, request: u64) -> bool {
        let mut remote = self.remote.lock().unwrap();
        if remote.manager.manage_pending != Some(request) {
            return false;
        }
        remote.manager.manage_pending = None;
        let refresh_roots = std::mem::take(&mut remote.manager.root_refresh_pending);
        let generation = remote.manager.library_generation;
        drop(remote);
        ui.set_sm_manage_busy(false);
        if refresh_roots {
            // The caller still owns its completion callback and captured generation.
            // Refresh on the next UI turn so its success/error is handled first.
            let c = self.clone();
            post(self.ui.clone(), move |ui| {
                if c.library_current(generation) {
                    c.refresh_source_manager_roots(&ui);
                }
            });
        }
        true
    }

    /// Publish on the UI thread after page/selection/visibility or provider invalidation.
    pub(super) fn publish_manage_busy(&self, ui: &AppWindow) {
        ui.set_sm_manage_busy(self.remote.lock().unwrap().manager.manage_pending.is_some());
    }

    /// Reconcile the visible manager after the worker invalidated its snapshot requests.
    /// Keep the selected profile, page, form and any newer management request intact.
    pub(in crate::controller) fn reconcile_source_manager_after_swap(
        self: &Arc<Self>,
        ui: &AppWindow,
    ) {
        self.publish_manage_busy(ui);
        if !ui.get_library_visible() || self.selected_profile().is_none() {
            return;
        }
        match ui.get_sm_page() {
            // Reattach to the selected server's current scan and refresh roots/history
            // using the generation established by the successful provider swap.
            PAGE_LIBRARY => self.load_server_library(ui),
            // 缓存与导出 also hosts the 「波形缓存」 block, read with the library page.
            PAGE_CACHE => {
                self.publish_cache_info(ui);
                self.load_server_library(ui);
            }
            _ => {}
        }
    }

    /// Refresh the list and refill the form and current page from the saved settings.
    fn reload_selection(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        self.publish_sources(&ui);
        self.publish_local_selection(&ui);
        self.load_form(&ui);
        let page = ui.get_sm_page();
        self.show_page(page);
    }

    /// The source list (row 0 = local) with the active marker and latest test verdicts.
    pub(super) fn publish_sources(&self, ui: &AppWindow) {
        let chinese = self.chinese.load(Ordering::Acquire);
        let mut config = self.config.lock().unwrap().clone();
        if config.data_source.local_libraries.is_empty() {
            let _ = config.reconcile_local_libraries();
        }
        let settings = &config.data_source;
        let active = settings.active_source();
        let selected_local = config.selected_local_library();
        let mut rows = settings
            .local_libraries
            .iter()
            .map(|library| SourceRow {
                id: library.id.clone().into(),
                name: library.name.clone().into(),
                subtitle: library.database.display().to_string().into(),
                kind: 0,
                active: active == ActiveSource::Local
                    && selected_local.is_some_and(|selected| selected.id == library.id),
                status: SharedString::new(),
                status_kind: 0,
            })
            .collect::<Vec<_>>();
        for profile in &settings.profiles {
            let (status, status_kind) = test_status(profile.last_test.as_ref(), chinese);
            rows.push(SourceRow {
                id: profile.id.clone().into(),
                name: profile.name.clone().into(),
                subtitle: profile.server_url.clone().into(),
                kind: 1,
                active: active == ActiveSource::Profile(profile.id.clone()),
                status: status.into(),
                status_kind,
            });
        }
        let selected = match &self.remote.lock().unwrap().manager.selected {
            Selection::Local => rows
                .iter()
                .position(|row| row.kind == 0 && row.id == DEFAULT_LOCAL_LIBRARY_ID)
                .map_or(0, |index| index as i32),
            Selection::LocalLibrary(id) => rows
                .iter()
                .position(|row| row.kind == 0 && row.id.as_str() == id)
                .map_or(0, |index| index as i32),
            Selection::New => -1,
            Selection::Profile(id) => rows
                .iter()
                .position(|row| row.kind == 1 && row.id.as_str() == id)
                .map_or(0, |index| index as i32),
        };
        let local_path = config
            .local_library(match &self.remote.lock().unwrap().manager.selected {
                Selection::LocalLibrary(id) => id.as_str(),
                _ => DEFAULT_LOCAL_LIBRARY_ID,
            })
            .map(|library| library.database.display().to_string())
            .unwrap_or_default();
        let local_count = settings.local_libraries.len();
        drop(config);
        ui.set_sm_local_count(local_count as i32);
        ui.set_sm_sources(ModelRc::new(VecModel::from(rows)));
        ui.set_sm_selected(selected);
        ui.set_sm_local_path(
            if chinese {
                format!("本地素材库：{local_path}")
            } else {
                format!("Local library: {local_path}")
            }
            .into(),
        );
    }

    /// Fill the connection page from the selected profile (empty for a new server).
    fn load_form(&self, ui: &AppWindow) {
        let chinese = self.chinese.load(Ordering::Acquire);
        let settings = self.config.lock().unwrap().data_source.clone();
        let secrets = self.secrets();
        let selection = self.selection();
        let profile = match &selection {
            Selection::Profile(id) => settings.profile(id).cloned(),
            _ => None,
        };
        let url = profile
            .as_ref()
            .map(|profile| profile.server_url.clone())
            .unwrap_or_default();
        ui.set_ds_name(
            profile
                .as_ref()
                .map(|profile| profile.name.clone())
                .unwrap_or_default()
                .into(),
        );
        ui.set_ds_server_url(url.clone().into());
        ui.set_ds_address_form(ADDRESS_FORM_URL);
        set_address_parts(ui, ServerAddress::parse(&url).ok().as_ref());
        // The token is never shown: the field starts empty and keeps the saved one.
        ui.set_ds_token("".into());
        ui.set_ds_token_clear(false);
        ui.set_ds_token_saved(
            profile
                .as_ref()
                .is_some_and(|profile| !secrets.token_for(&profile.id).is_empty()),
        );
        ui.set_ds_transcode_touched(false);
        ui.set_ds_transcode(edit_transcode(profile.as_ref(), &url));
        if !ui.get_shared_sync_dirty() {
            ui.set_ds_poll_interval(settings.poll_interval_secs.to_string().into());
            ui.set_ds_auto_sync(settings.auto_sync);
        }
        self.remote.lock().unwrap().manager.last_report = None;
        ui.set_ds_report("".into());
        ui.set_ds_public_url("".into());
        ui.set_ds_last_test(last_test_line(profile.as_ref(), chinese).into());
        let active = profile.as_ref().is_some_and(|profile| {
            settings.active_source() == ActiveSource::Profile(profile.id.clone())
        });
        if active {
            let error = self.remote.lock().unwrap().last_error.clone();
            ui.set_ds_message(error.unwrap_or_default().into());
        }
        self.refresh_address_hint(ui);
        ui.set_ds_dirty(false);
    }

    /// Roots for the WebDAV table: live for the active server, else from its cached
    /// snapshot (none before the first sync).
    fn profile_roots(&self, profile: Option<&ServerProfile>, active: bool) -> Vec<RemoteRoot> {
        if active {
            return self.known_roots();
        }
        profile
            .and_then(|profile| profile.library_id.as_deref())
            .and_then(|id| RemoteCache::new(&self.data_root()).state(id))
            .map(|state| without_status(&state.roots))
            .unwrap_or_default()
    }

    fn show_page(self: &Arc<Self>, page: i32) {
        let Some(ui) = self.ui.upgrade() else { return };
        {
            let mut remote = self.remote.lock().unwrap();
            remote.manager.close_after_sync = false;
            remote.manager.invalidate_library();
        }
        self.publish_manage_busy(&ui);
        ui.set_sm_root_controls(false);
        ui.set_sm_root_selected(-1);
        ui.set_sm_root_form(false);
        ui.set_sm_fs_visible(false);
        ui.set_sm_fs_busy(false);
        self.refresh_detail_cache(false);
        match page {
            PAGE_LIBRARY => self.load_server_library(&ui),
            PAGE_CACHE => {
                self.publish_cache_info(&ui);
                self.load_server_library(&ui);
            }
            _ => {}
        }
    }

    /// Clear root dialogs invalidated by the worker's provider swap. The manager
    /// reconciliation refills the page without invalidating a newer request.
    pub(in crate::controller) fn refresh_source_manager_after_swap(
        self: &Arc<Self>,
        ui: &AppWindow,
    ) {
        ui.set_sm_root_form(false);
        ui.set_sm_fs_visible(false);
        ui.set_sm_fs_busy(false);
        ui.set_sm_root_controls(false);
        ui.set_sm_root_selected(-1);
    }

    /// Root configuration can change without a new snapshot file or audio revision.
    /// Immediately discard the old editor/browser identity, then refill the page. An
    /// in-flight management request keeps its captured generation until completion.
    pub(super) fn refresh_source_manager_roots(self: &Arc<Self>, ui: &AppWindow) {
        let deferred = {
            let mut remote = self.remote.lock().unwrap();
            let manager = &mut remote.manager;
            if manager.manage_pending.is_some() {
                manager.browser_sequence += 1;
                manager.root_edit_target = None;
                manager.root_refresh_pending = true;
                true
            } else {
                manager.invalidate_library();
                false
            }
        };
        ui.set_sm_root_controls(false);
        ui.set_sm_root_selected(-1);
        ui.set_sm_root_form(false);
        ui.set_sm_root_editing(false);
        ui.set_sm_root_name("".into());
        ui.set_sm_root_path("".into());
        ui.set_sm_root_message("".into());
        ui.set_sm_fs_visible(false);
        ui.set_sm_fs_busy(false);
        ui.set_sm_fs_can_select(false);
        ui.set_sm_fs_path("".into());
        ui.set_sm_fs_parent("".into());
        ui.set_sm_fs_error("".into());
        ui.set_sm_fs_rows(ModelRc::default());
        ui.set_sm_fs_breadcrumbs(ModelRc::default());
        if ui.get_sm_confirm() == 5 {
            ui.set_sm_confirm(0);
        }
        if !deferred {
            self.publish_manage_busy(ui);
            if ui.get_library_visible() && matches!(ui.get_sm_page(), PAGE_LIBRARY | PAGE_CACHE) {
                self.load_server_library(ui);
            }
        }
    }

    /// The transcode box follows the address until the user changes it: the saved
    /// profile's choice, or the default of the address class (LAN or internet).
    pub(super) fn form_transcode(&self, url: &str) -> bool {
        edit_transcode(self.selected_profile().as_ref(), url)
    }

    /// Read shared synchronization fields while preserving settings-file WebDAV configuration.
    fn shared_form(&self, ui: &AppWindow) -> DataSourceSettings {
        let mut settings = self.config.lock().unwrap().data_source.clone();
        let interval = ui.get_ds_poll_interval();
        let interval = interval.trim();
        // Empty means the default; anything unparsable fails validation with a message.
        settings.poll_interval_secs = if interval.is_empty() {
            audiolib_core::config::DEFAULT_POLL_INTERVAL_SECS
        } else {
            interval.parse().unwrap_or(u32::MAX)
        };
        settings.auto_sync = ui.get_ds_auto_sync();
        settings
    }

    /// Persist global polling preferences without replacing the active provider or drafts.
    fn save_shared_sync_settings(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        if !self.available() || ui.get_ds_applying() {
            return;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        let settings = self.shared_form(&ui);
        let mut checked = settings.clone();
        checked.mode = DataSourceMode::Local;
        if let Err(error) = checked.validate() {
            ui.set_ds_message(error.to_string().into());
            return;
        }
        ui.set_ds_applying(true);
        let c = self.clone();
        std::thread::spawn(move || {
            let result = c.update_sources(chinese, false, |stored, _| {
                stored.poll_interval_secs = settings.poll_interval_secs;
                stored.auto_sync = settings.auto_sync;
                Ok(())
            });
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                ui.set_ds_applying(false);
                match result {
                    Ok(()) => {
                        ui.set_shared_sync_dirty(false);
                        ui.set_ds_message(
                            cn_or(
                                c2.chinese.load(Ordering::Acquire),
                                "同步设置已保存。",
                                "Synchronization settings saved.",
                            )
                            .into(),
                        );
                        c2.wake_revision_poller();
                        if ui.get_shared_sync_close_pending() {
                            ui.set_shared_sync_close_pending(false);
                            ui.invoke_close_library_manager();
                        }
                    }
                    Err(error) => ui.set_ds_message(error.into()),
                }
            });
        });
    }

    fn publish_connection_report(&self, ui: &AppWindow) {
        let chinese = self.chinese.load(Ordering::Acquire);
        let remote = self.remote.lock().unwrap();
        let Some((report, tokenless)) = &remote.manager.last_report else {
            return;
        };
        let mut summary = report.summary(chinese);
        if *tokenless {
            summary.insert_str(0, &tokenless_note(chinese));
        }
        if report.ok() {
            summary.push_str(&super::connection_verdict(
                report.warnings(chinese).len(),
                chinese,
            ));
        }
        ui.set_ds_report(summary.into());
    }

    /// Regenerate UI-owned text without discarding connection drafts or starting network I/O.
    pub(in crate::controller) fn refresh_generated_source_text(&self, ui: &AppWindow) {
        self.publish_connection_report(ui);
        let chinese = self.chinese.load(Ordering::Acquire);
        ui.set_ds_last_test(last_test_line(self.selected_profile().as_ref(), chinese).into());
        if ui.get_library_visible() {
            self.publish_cache_info(ui);
        }
    }

    /// Read and check the connection page (messages in the desktop language).
    fn read_form(&self, ui: &AppWindow, id: Option<&str>) -> Result<FormInput, String> {
        let chinese = self.chinese.load(Ordering::Acquire);
        let address = super::AddressForm::read(ui)
            .address()
            .map_err(|error| error.message(chinese))?;
        let url = address.base_url();
        let name = ui.get_ds_name().trim().to_owned();
        if name.chars().count() > MAX_PROFILE_NAME_CHARS {
            return Err(if chinese {
                format!("名称最多 {MAX_PROFILE_NAME_CHARS} 个字符。")
            } else {
                format!("The name can have at most {MAX_PROFILE_NAME_CHARS} characters.")
            });
        }
        let settings = self.config.lock().unwrap().data_source.clone();
        if let Some(other) = settings.profiles.iter().find(|profile| {
            Some(profile.id.as_str()) != id && same_server_url(&profile.server_url, &url)
        }) {
            return Err(if chinese {
                format!(
                    "这个地址已保存在服务器配置「{}」中，请直接选择那个配置。",
                    other.name
                )
            } else {
                format!(
                    "This address is already saved in the server profile \"{}\"; select that profile instead.",
                    other.name
                )
            });
        }
        let mut check = settings.clone();
        check.mode = DataSourceMode::Local;
        check.validate().map_err(|error| error.to_string())?;
        let typed = ui.get_ds_token().trim().to_owned();
        let token = if !typed.is_empty() {
            Some(typed)
        } else if ui.get_ds_token_clear() {
            Some(String::new())
        } else {
            None
        };
        Ok(FormInput {
            name,
            url,
            token,
            transcode: ui.get_ds_transcode(),
        })
    }

    /// Store settings and secrets without the runtime reset of a source switch (other
    /// profiles, names, test verdicts). `change` sees the latest saved values. Runs off
    /// the UI thread (DPAPI, disk).
    fn update_sources<T>(
        &self,
        chinese: bool,
        store_secrets: bool,
        change: impl FnOnce(&mut DataSourceSettings, &mut RemoteSecrets) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut secrets = self.secrets();
        let mut config = self.config.lock().unwrap();
        let mut preferences = config.clone();
        let value = change(&mut preferences.data_source, &mut secrets)?;
        if store_secrets {
            save_secrets(&preferences.data_root(), &secrets)
                .map_err(|error| error.localized(chinese))?;
        }
        preferences.save().map_err(|error| error.to_string())?;
        *config = preferences;
        drop(config);
        let mut remote = self.remote.lock().unwrap();
        remote.secrets = secrets;
        remote.secrets_loaded = true;
        Ok(value)
    }

    /// Save the connection page into the selected (or a new) profile. The active
    /// profile's connection change resets the server runtime and re-syncs, like a switch;
    /// any other profile is only stored. [`AfterSave`] chooses what follows.
    fn save_selected(self: &Arc<Self>, after: AfterSave) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_ds_applying() || !self.available() {
            return;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        let id = match self.selection() {
            Selection::Local | Selection::LocalLibrary(_) => return,
            Selection::New => None,
            Selection::Profile(id) => Some(id),
        };
        let input = match self.read_form(&ui, id.as_deref()) {
            Ok(input) => input,
            Err(error) => {
                ui.set_ds_message(error.into());
                return;
            }
        };
        let generation = self.remote.lock().unwrap().manager.form_generation;
        let settings = self.config.lock().unwrap().data_source.clone();
        let active = settings.active_source();
        let changes_source = profile_save_changes_source(
            &active,
            id.as_deref(),
            id.as_deref()
                .and_then(|id| settings.profile(id))
                .map(|profile| profile.server_url.as_str()),
            &input.url,
            after,
        );
        let barrier = match if changes_source {
            self.begin_source_change(&ui)
        } else {
            self.begin_source_change_for_settings(&ui)
        } {
            Ok(barrier) => barrier,
            Err(error) => {
                ui.set_ds_message(error.into());
                return;
            }
        };
        ui.set_ds_applying(true);
        ui.set_ds_message(cn_or(chinese, "正在保存…", "Saving…").into());
        let c = self.clone();
        std::thread::spawn(move || {
            let drained = if changes_source {
                c.wait_transition_metadata(barrier)
            } else {
                c.wait_metadata(barrier, true).map(|_| ())
            };
            let saved = drained
                .and_then(|_| c.wait_remote_plays())
                .and_then(|_| c.store_profile(id.as_deref(), &input, chinese));
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                let (id, resync) = match saved {
                    Ok(saved) => saved,
                    Err(error) => {
                        ui.set_ds_message(error.into());
                        c2.finish_source_change(&ui, false);
                        c2.publish_metadata_recovery_notice(&ui);
                        return;
                    }
                };
                if c2.form_current(generation) {
                    c2.set_selection(Selection::Profile(id.clone()));
                    c2.reload_selection();
                    ui.set_ds_message(
                        if resync {
                            cn_or(chinese, "已保存，正在重新同步…", "Saved; syncing again…")
                        } else {
                            cn_or(chinese, "已保存。", "Saved.")
                        }
                        .into(),
                    );
                } else {
                    c2.publish_sources(&ui);
                }
                if resync {
                    // Editing the active connection already changed its endpoint.
                    // Keep the original rollback until this sync opens its provider.
                    c2.start_sync_requested(false, true, false);
                } else {
                    c2.finish_source_change(&ui, true);
                    if !matches!(after, AfterSave::Switch) {
                        c2.resume_source_reads(&ui, true);
                    }
                }
                match after {
                    AfterSave::Nothing => {}
                    AfterSave::Close => {
                        ui.set_sm_pending_select(-2);
                        // Keep local recovery reachable while a changed active
                        // connection is syncing under the source-change gate.
                        if !resync {
                            ui.set_library_visible(false);
                        } else {
                            c2.remote.lock().unwrap().manager.close_after_sync = true;
                        }
                    }
                    AfterSave::Switch => {
                        if !resync {
                            c2.activate_data_source(ActiveSource::Profile(id));
                        }
                    }
                    AfterSave::Select(index) => {
                        ui.set_sm_pending_select(-2);
                        ui.set_sm_confirm_delete(false);
                        ui.set_sm_confirm(0);
                        ui.set_sm_scan_form(false);
                        ui.set_sm_page(PAGE_CONNECTION);
                        if index == -4 {
                            ui.invoke_sm_local_add_requested();
                        } else if index < 0 {
                            ui.set_sm_selected(index);
                            c2.set_selection(Selection::New);
                            c2.reload_selection();
                        } else {
                            c2.select_source(index);
                        }
                    }
                }
                c2.publish_data_source(&ui);
                c2.publish_metadata_recovery_notice(&ui);
                if !resync
                    && !matches!(after, AfterSave::Switch)
                    && c2.close_after_operation.swap(false, Ordering::AcqRel)
                {
                    c2.request_close(&ui);
                }
            });
        });
    }

    /// Complete Save-on-Close only after its provider is committed. Navigation or
    /// failure cancels that request so local recovery remains visible.
    pub(super) fn finish_source_manager_sync(&self, ui: &AppWindow, committed: bool) {
        let close = std::mem::take(&mut self.remote.lock().unwrap().manager.close_after_sync);
        if close && committed {
            ui.set_library_visible(false);
        }
    }

    /// Worker half of [`Self::save_selected`]: the saved id and whether the active
    /// server must sync again.
    fn store_profile(
        &self,
        id: Option<&str>,
        input: &FormInput,
        chinese: bool,
    ) -> Result<(String, bool), String> {
        let mut secrets = self.secrets();
        let previous = self.config.lock().unwrap().data_source.clone();
        let mut settings = previous.clone();
        let edit = ProfileEdit {
            name: &input.name,
            server_url: &input.url,
            token: input.token.as_deref(),
            transcode: input.transcode,
        };
        let saved_id = save_profile_edit(&mut settings, &mut secrets, id, &edit)
            .map_err(|error| error.to_string())?;
        let active = previous.active_source() == ActiveSource::Profile(saved_id.clone());
        let connection_changed = {
            let before = previous.profile(&saved_id);
            let after = settings.profile(&saved_id);
            input.token.is_some()
                || before.map(|profile| (&profile.server_url, profile.transcode_enabled()))
                    != after.map(|profile| (&profile.server_url, profile.transcode_enabled()))
        };
        if active && connection_changed {
            // Same path as a switch: the old server's runtime state goes.
            self.save_data_source(settings, secrets, chinese, false)?;
        } else {
            self.update_sources(chinese, true, move |stored, stored_secrets| {
                *stored = settings;
                *stored_secrets = secrets;
                Ok(())
            })?;
            self.wake_revision_poller();
        }
        Ok((saved_id, active && connection_changed))
    }

    /// 「切换到此数据源」 (A2): unsaved connection edits are saved first.
    fn switch_to_selected(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        let chinese = self.chinese.load(Ordering::Acquire);
        match self.selection() {
            Selection::New => {}
            Selection::Local | Selection::LocalLibrary(_) => {
                let (path, default_database) = {
                    let config = self.config.lock().unwrap();
                    (
                        config
                            .local_library(
                                self.selected_local_id()
                                    .as_deref()
                                    .unwrap_or(DEFAULT_LOCAL_LIBRARY_ID),
                            )
                            .map(|library| library.database.clone())
                            .unwrap_or_else(|| config.paths.database.clone()),
                        config.data_root().join("library.db"),
                    )
                };
                if let Some(problem) = missing_local_library(&path, &default_database, chinese) {
                    // Checked before settings are saved: a missing library keeps the
                    // current source and its persisted selection unchanged.
                    ui.set_ds_message(problem.into());
                    return;
                }
                ui.set_ds_message(
                    cn_or(
                        chinese,
                        "正在切换到本地素材库…",
                        "Switching to the local library…",
                    )
                    .into(),
                );
                // An explicit server activation can still be using the original
                // local provider while its sync waits. Cancelling that choice must
                // retain the provider and search context, rather than reopen it.
                let current = self.provider();
                #[cfg(windows)]
                let current_path = crate::controller::same_path(&path, current.database_path());
                #[cfg(not(windows))]
                let current_path = path == current.database_path();
                if !self.remote_read_only() && current_path {
                    self.activate_data_source(ActiveSource::Local);
                    return;
                }
                // Startup and ordinary server syncs need immediate cancellation
                // too, even when they did not establish a source-changing gate.
                if let Err(error) = self.begin_local_library_change(&ui) {
                    ui.set_ds_message(error.into());
                    return;
                }
                self.run_library_operation(LibraryOperation::LocalSource(Box::new(
                    LocalSourceRequest::Switch {
                        id: self
                            .selected_local_id()
                            .unwrap_or_else(|| DEFAULT_LOCAL_LIBRARY_ID.into()),
                    },
                )));
            }
            Selection::Profile(id) => {
                if ui.get_ds_dirty() {
                    self.save_selected(AfterSave::Switch);
                } else {
                    ui.set_ds_message(
                        cn_or(chinese, "正在切换并同步…", "Switching and syncing…").into(),
                    );
                    self.activate_data_source(ActiveSource::Profile(id));
                }
            }
        }
    }

    /// Delete the selected (inactive) profile and its token. Caches, snapshots and data
    /// directories are never deleted with it.
    fn delete_selected(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        if self.selected_local_id().is_some() {
            self.delete_local_selected(&ui);
            return;
        }
        let Selection::Profile(id) = self.selection() else {
            return;
        };
        let chinese = self.chinese.load(Ordering::Acquire);
        let active = self.config.lock().unwrap().data_source.active_source();
        if ui.get_ds_applying() || active == ActiveSource::Profile(id.clone()) {
            ui.set_ds_message(
                cn_or(
                    chinese,
                    "当前数据源不能删除，请先切换到其他数据源。",
                    "The active source cannot be deleted; switch to another source first.",
                )
                .into(),
            );
            return;
        }
        ui.set_ds_applying(true);
        let c = self.clone();
        std::thread::spawn(move || {
            let removed = c.update_sources(chinese, true, |settings, secrets| {
                remove_server_profile(settings, secrets, &id)
                    .map(|profile| profile.name)
                    .map_err(|error| error.to_string())
            });
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                ui.set_ds_applying(false);
                match removed {
                    Ok(name) => {
                        c2.set_selection(Selection::Local);
                        ui.set_sm_page(PAGE_CONNECTION);
                        c2.reload_selection();
                        ui.set_ds_message(
                            if chinese {
                                format!("已删除服务器配置「{name}」；缓存和数据目录保留。")
                            } else {
                                format!(
                                    "Deleted the server profile \"{name}\"; caches and data directories stay."
                                )
                            }
                            .into(),
                        );
                    }
                    Err(error) => ui.set_ds_message(error.into()),
                }
            });
        });
    }

    /// 「使用服务器外网地址」: a saved profile gets (or reuses) a sibling external profile
    /// (token copied, not switched); a new server just takes the address.
    fn use_public_url(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        let url = ui.get_ds_public_url().to_string();
        let chinese = self.chinese.load(Ordering::Acquire);
        let Some(profile) = self.selected_profile() else {
            ui.set_ds_address_form(ADDRESS_FORM_URL);
            ui.set_ds_server_url(url.clone().into());
            set_address_parts(&ui, ServerAddress::parse(&url).ok().as_ref());
            ui.set_ds_public_url("".into());
            ui.set_ds_dirty(true);
            self.refresh_address_hint(&ui);
            return;
        };
        if ui.get_ds_applying() {
            return;
        }
        ui.set_ds_applying(true);
        let name = format!(
            "{}{}",
            profile.name,
            cn_or(chinese, "（外网）", " (external)")
        );
        let c = self.clone();
        std::thread::spawn(move || {
            let added = c.update_sources(chinese, true, |settings, secrets| {
                use_external_address(settings, secrets, &profile.id, &url, Some(&name))
                    .map_err(|error| error.to_string())
            });
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                ui.set_ds_applying(false);
                match added {
                    Ok(id) => {
                        c2.set_selection(Selection::Profile(id));
                        ui.set_sm_page(PAGE_CONNECTION);
                        c2.reload_selection();
                        ui.set_ds_message(
                            cn_or(
                                chinese,
                                "已保存外网地址配置（令牌已复制）。点「切换到此数据源」后生效。",
                                "Saved the external address profile (token copied). Switch to it to use it.",
                            )
                            .into(),
                        );
                    }
                    Err(error) => ui.set_ds_message(error.into()),
                }
            });
        });
    }

    /// Test the connection page as entered: the typed token, else the saved one (unless
    /// cleared). The verdict is recorded on the profile when the address is its saved one.
    fn test_selected(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_ds_testing() {
            return;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        let url = match super::AddressForm::read(&ui).address() {
            Ok(address) => address.base_url(),
            Err(error) => {
                ui.set_ds_message(error.message(chinese).into());
                return;
            }
        };
        let mut settings = self.shared_form(&ui);
        settings.server_url = url.clone();
        let profile = self.selected_profile();
        let active = profile.as_ref().is_some_and(|profile| {
            settings.active_source() == ActiveSource::Profile(profile.id.clone())
        });
        let mut secrets = self.secrets();
        let typed = ui.get_ds_token().trim().to_owned();
        secrets.token = if !typed.is_empty() {
            typed
        } else if ui.get_ds_token_clear() {
            String::new()
        } else {
            profile
                .as_ref()
                .map(|profile| secrets.token_for(&profile.id).to_owned())
                .unwrap_or_default()
        };
        let record = profile
            .as_ref()
            .filter(|profile| same_server_url(&profile.server_url, &url))
            .map(|profile| profile.id.clone());
        let generation = self.remote.lock().unwrap().manager.form_generation;
        ui.set_ds_testing(true);
        ui.set_ds_message("".into());
        ui.set_ds_report(cn_or(chinese, "正在测试连接…", "Testing connection…").into());
        let cached = self.profile_roots(profile.as_ref(), active);
        let cache = RemoteCache::new(&self.data_root());
        let tokenless = secrets.token.trim().is_empty();
        let c = self.clone();
        std::thread::spawn(move || {
            let report = test_connection(&settings, &secrets, &cached, Some(&cache), chinese);
            if let Some(id) = &record {
                let verdict = report.verdict(chinese);
                let library = report
                    .library
                    .as_ref()
                    .map(|library| library.library_id.clone());
                let recorded = c.update_sources(chinese, false, |stored, _| {
                    stored
                        .record_test(id, report.ok(), &verdict, unix_now(), library.as_deref())
                        .map_err(|error| error.to_string())
                });
                if let Err(error) = recorded {
                    tracing::warn!(%error, "could not record the connection test");
                }
            }
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                ui.set_ds_testing(false);
                if ui.get_library_visible() {
                    c2.publish_sources(&ui);
                }
                if !c2.form_current(generation) {
                    return;
                }
                c2.remote.lock().unwrap().manager.last_report = Some((report.clone(), tokenless));
                c2.publish_connection_report(&ui);
                let tested = TestSummary {
                    at: unix_now(),
                    ok: report.ok(),
                    summary: report.verdict(chinese),
                };
                ui.set_ds_last_test(tested_line(&tested, record.is_some(), chinese).into());
                // §6.2: offer the server's external address after e.g. a LAN test.
                ui.set_ds_public_url(report.other_public_url().unwrap_or_default().into());
                if report.unauthorized {
                    ui.set_ds_token_focus_serial(ui.get_ds_token_focus_serial().wrapping_add(1));
                    ui.set_ds_message(
                        cn_or(
                            chinese,
                            "访问令牌被拒绝（401）：请检查或填写令牌后重试。",
                            "Access token rejected (401): check or enter the token and try again.",
                        )
                        .into(),
                    );
                }
            });
        });
    }

    /// Resolve the displayed endpoint or a saved sibling's known shared library.
    /// A newly opened library can precede its directly matching profile identity.
    fn selected_cache_state(
        &self,
        profile: &ServerProfile,
    ) -> (Option<String>, Option<CacheState>) {
        let settings = self.config.lock().unwrap().data_source.clone();
        let active = settings.active_source() == ActiveSource::Profile(profile.id.clone())
            && cached_source_matches(&settings.server_url, &profile.server_url);
        let matches = |state: &CacheState| {
            if cached_source_matches(&state.server_url, &profile.server_url) {
                return true;
            }
            let known_library = Some(state.library_id.as_str());
            profile.library_id.as_deref() == known_library
                && settings.profile(&profile.id).is_some_and(|saved| {
                    saved.library_id.as_deref() == known_library
                        && saved.group_key() == profile.group_key()
                        && cached_source_matches(&saved.server_url, &profile.server_url)
                })
                && settings.profiles.iter().any(|sibling| {
                    sibling.id != profile.id
                        && sibling.group_key() == profile.group_key()
                        && sibling.library_id.as_deref() == known_library
                        && cached_source_matches(&state.server_url, &sibling.server_url)
                })
        };
        let cache = RemoteCache::new(&self.data_root());
        let open = (active && self.remote_read_only())
            .then(|| self.remote.lock().unwrap().state.clone())
            .flatten()
            .filter(|state| {
                matches(state)
                    && !state.database_file.contains(['/', '\\'])
                    && cache.database_path(state).is_file()
            });
        let library_id = open
            .as_ref()
            .map(|state| state.library_id.clone())
            .or_else(|| profile.library_id.clone());
        let state = open.or_else(|| {
            library_id
                .as_deref()
                .and_then(|id| cache.state(id))
                .filter(matches)
        });
        (library_id, state)
    }

    /// Open an existing, endpoint-bound mirror directory without creating a cache.
    fn open_selected_cache(self: &Arc<Self>) {
        let Some(profile) = self.selected_profile() else {
            return;
        };
        let (_, state) = self.selected_cache_state(&profile);
        let cache = RemoteCache::new(&self.data_root());
        let directory = state.map(|state| cache.library_dir(&state.library_id));
        if let Some(directory) = directory.filter(|path| path.is_dir()) {
            self.open_explorer(directory.display().to_string(), false);
        } else if let Some(ui) = self.ui.upgrade() {
            ui.set_sm_cache_available(false);
            ui.set_ds_message(
                cn_or(
                    self.chinese.load(Ordering::Acquire),
                    "镜像目录尚不存在，请先同步快照。",
                    "The mirror directory does not exist yet. Sync a snapshot first.",
                )
                .into(),
            );
        }
    }

    /// 缓存与导出: what is cached locally for the selected server's library.
    fn publish_cache_info(&self, ui: &AppWindow) {
        let chinese = self.chinese.load(Ordering::Acquire);
        ui.set_sm_cache_available(false);
        let Some(profile) = self.selected_profile() else {
            ui.set_sm_cache_info("".into());
            ui.set_sm_cache_summary("".into());
            ui.set_sm_cache_dir("".into());
            return;
        };
        let cache = RemoteCache::new(&self.data_root());
        let (library_id, state) = self.selected_cache_state(&profile);
        ui.set_sm_cache_available(
            state
                .as_ref()
                .is_some_and(|state| cache.library_dir(&state.library_id).is_dir()),
        );
        let directory = library_id
            .as_deref()
            .map(|id| cache.library_dir(id).display().to_string());
        ui.set_sm_cache_summary(
            cache_summary(library_id.as_deref(), state.as_ref(), chinese).into(),
        );
        ui.set_sm_cache_dir(
            directory
                .as_deref()
                .map(|directory| {
                    if chinese {
                        format!("缓存目录：{directory}")
                    } else {
                        format!("Cache directory: {directory}")
                    }
                })
                .unwrap_or_default()
                .into(),
        );
        ui.set_sm_cache_info(
            cache_info(
                library_id.as_deref(),
                directory.as_deref(),
                state.as_ref(),
                chinese,
            )
            .into(),
        );
    }

    /// 服务器音效库: roots, capabilities, the active scan and history through the
    /// management client; an active scan is polled every 500 ms while this page stays
    /// open on this server, and its end reloads the page (and syncs the active server's
    /// snapshot when the revision moved).
    fn load_server_library(self: &Arc<Self>, ui: &AppWindow) {
        let chinese = self.chinese.load(Ordering::Acquire);
        let Some(profile) = self.selected_profile() else {
            return;
        };
        let token = self.secrets().token_for(&profile.id).to_owned();
        let generation = self.remote.lock().unwrap().manager.library_generation;
        let host = host_of(&profile.server_url);
        ui.set_sm_library_loading(true);
        ui.set_sm_library_error(false);
        ui.set_sm_library_status(
            if chinese {
                format!("正在读取 {host} 的音效库…")
            } else {
                format!("Loading the library on {host}…")
            }
            .into(),
        );
        let c = self.clone();
        std::thread::spawn(move || {
            let client = match RemoteClient::new(&profile.server_url, &token) {
                Ok(client) => client,
                Err(error) => {
                    c.post_library_error(generation, &error, chinese);
                    return;
                }
            };
            let cache = c.waveform_cache_provider(&profile.id);
            let page = match LibraryPage::fetch(&client, host.clone(), cache.as_deref()) {
                Ok(page) => page,
                Err(error) => {
                    c.post_library_error(generation, &error, chinese);
                    return;
                }
            };
            let mut polling = page.scan_active();
            let mut last_session = page
                .scan
                .as_ref()
                .ok()
                .and_then(|scan| scan.as_ref())
                .map(|scan| scan.progress.session_id)
                .filter(|id| *id > 0);
            match (&page.waveform_cache, &cache) {
                (Some(_), Some(provider)) => {
                    c.poll_waveform_cache(generation, provider.clone(), chinese);
                }
                _ if page.waves_supported() => {
                    c.poll_wave_status(generation, client.clone(), chinese);
                }
                _ => {}
            }
            c.post_library_page(generation, page, chinese);
            while polling {
                std::thread::sleep(SCAN_POLL);
                if !c.library_current(generation) || c.shutdown.load(Ordering::Acquire) {
                    return;
                }
                match client.current_scan() {
                    Ok(Some(scan)) if scan.active => {
                        if scan.progress.session_id > 0 {
                            last_session = Some(scan.progress.session_id);
                        }
                        let c2 = c.clone();
                        post(c.ui.clone(), move |ui| {
                            if c2.library_current(generation) {
                                ui.set_sm_scan_active(true);
                                ui.set_sm_scan_paused(scan.progress.paused || scan.pause_requested);
                                ui.set_sm_scan_text(scan_text(Some(&scan), chinese).into());
                            }
                        });
                    }
                    Ok(finished) => {
                        // Finished: roots and history changed once more; sync when the
                        // revision moved past the open snapshot.
                        polling = false;
                        // `/scans/current` is empty once idle: ask the last session.
                        let finished = match finished {
                            Some(scan) => Some(scan),
                            None => last_session.and_then(|id| client.scan_status(id).ok()),
                        };
                        let sync = finished
                            .as_ref()
                            .is_some_and(|scan| c.scan_moved_revision(scan));
                        match LibraryPage::fetch(&client, host.clone(), cache.as_deref()) {
                            Ok(page) => {
                                c.post_library_page(generation, page, chinese);
                                if sync {
                                    let c2 = c.clone();
                                    post(c.ui.clone(), move |_| {
                                        c2.start_sync(false, false);
                                    });
                                }
                            }
                            Err(error) => c.post_library_error(generation, &error, chinese),
                        }
                    }
                    Err(error) => {
                        polling = false;
                        c.post_library_error(generation, &error, chinese);
                    }
                }
            }
        });
    }

    /// `GET /waveforms/status` every 5 s while this page stays open on this server
    /// (the first reading came with the page).
    fn poll_wave_status(self: &Arc<Self>, generation: u64, client: RemoteClient, chinese: bool) {
        let poll = {
            let mut remote = self.remote.lock().unwrap();
            remote.manager.wave_poll += 1;
            remote.manager.wave_poll
        };
        let c = self.clone();
        let current = move |c: &Self| {
            c.library_current(generation)
                && c.remote.lock().unwrap().manager.wave_poll == poll
                && !c.shutdown.load(Ordering::Acquire)
        };
        let spawned = std::thread::Builder::new()
            .name("audiolib-wave-status".into())
            .spawn(move || {
                loop {
                    let deadline = std::time::Instant::now() + WAVE_STATUS_POLL;
                    while std::time::Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(250));
                        if !current(&c) {
                            return;
                        }
                    }
                    let status = client.list_waveform_status();
                    let c2 = c.clone();
                    post(c.ui.clone(), move |ui| {
                        if c2.library_current(generation) {
                            publish_wave_status(&ui, &status, chinese);
                        }
                    });
                }
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "could not start the list Waveform status poll");
        }
    }

    /// 服务器音效库 「服务器预生成列表波形」: `PATCH /settings` (gated by `manage_mode`).
    fn set_server_pregenerate(self: &Arc<Self>, enabled: bool) {
        // The active server's 「波形缓存」 section goes through its DataProvider.
        if self
            .remote
            .lock()
            .unwrap()
            .manager
            .wave_cache
            .capabilities
            .is_some()
        {
            self.run_waveform_cache_action(WaveformCacheAction::Pregenerate(enabled));
            return;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        let generation = self.remote.lock().unwrap().manager.library_generation;
        let client = match self.active_manage_client() {
            Ok((_, client, _)) => client,
            Err(message) => {
                post(self.ui.clone(), move |ui| {
                    ui.set_sm_wave_pregenerate(!enabled);
                    ui.set_sm_wave_message(message.into());
                });
                return;
            }
        };
        post(self.ui.clone(), |ui| {
            ui.set_sm_wave_busy(true);
            ui.set_sm_wave_message("".into());
        });
        let c = self.clone();
        std::thread::spawn(move || {
            let result = client.set_pregenerate_list_waveforms(enabled);
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                if !c2.library_current(generation) {
                    return;
                }
                ui.set_sm_wave_busy(false);
                match result {
                    Ok(settings) => {
                        ui.set_sm_wave_pregenerate(settings.pregenerate_list_waveforms);
                        ui.set_sm_wave_can_change(settings.can_change);
                    }
                    Err(error) => {
                        ui.set_sm_wave_pregenerate(!enabled);
                        if matches!(error, RemoteError::Forbidden(_)) {
                            ui.set_sm_wave_can_change(false);
                        }
                        ui.set_sm_wave_message(
                            if chinese {
                                format!(
                                    "修改服务器设置失败：{}",
                                    manage_error_text(&error, chinese)
                                )
                            } else {
                                format!(
                                    "Could not change the server setting: {}",
                                    manage_error_text(&error, chinese)
                                )
                            }
                            .into(),
                        );
                    }
                }
            });
        });
    }

    /// Active profile whose connection page is selected, with a usable manage client.
    fn active_manage_client(&self) -> Result<(ServerProfile, RemoteClient, CacheState), String> {
        self.require_remote_write_identity()?;
        let chinese = self.chinese.load(Ordering::Acquire);
        let Some(profile) = self.selected_profile() else {
            return Err(cn_or(
                chinese,
                "请先选择一个已保存的服务器配置。",
                "Select a saved server profile first.",
            ));
        };
        let active = self.config.lock().unwrap().data_source.active_source()
            == ActiveSource::Profile(profile.id.clone());
        if !active || !self.remote_read_only() {
            return Err(cn_or(
                chinese,
                "只有当前数据源可以管理；请先切换到此数据源。",
                "Only the active source can be managed; switch to this source first.",
            ));
        }
        let token = self.secrets().token_for(&profile.id).to_owned();
        let snapshot = self
            .remote
            .lock()
            .unwrap()
            .state
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                cn_or(
                    chinese,
                    "请先同步服务器素材库",
                    "Sync the server library first",
                )
            })?;
        RemoteClient::new(&profile.server_url, &token)
            .and_then(|client| client.with_library_id(&snapshot.library_id))
            .map(|client| (profile, client, snapshot))
            .map_err(|error| manage_error_text(&error, chinese))
    }

    fn manage_text(error: &RemoteError, chinese: bool) -> String {
        let mut text = manage_error_text(error, chinese);
        if let Some(seconds) = error.retry_after() {
            text.push(' ');
            text.push_str(&if chinese {
                format!("请约 {seconds} 秒后再试。")
            } else {
                format!("Retry in about {seconds} s.")
            });
        }
        text
    }

    /// 「开始扫描」: root / optional subpath / 增量|完整; workers = server default.
    fn start_server_scan(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_sm_manage_busy() || ui.get_sm_scan_active() || !self.available() {
            return;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        let names = ui.get_sm_root_names();
        let index = ui.get_sm_scan_root();
        let root = names
            .row_data(index.max(0) as usize)
            .map(|name| name.to_string())
            .unwrap_or_default();
        if root.is_empty() {
            ui.set_sm_manage_error(true);
            ui.set_sm_manage_message(cn_or(chinese, "请选择一个根目录。", "Choose a root.").into());
            return;
        }
        let subpath = ui.get_sm_scan_subpath().trim().to_owned();
        let mode = if ui.get_sm_scan_mode() == 1 {
            ScanMode::Full
        } else {
            ScanMode::Incremental
        };
        let request = ScanRequest {
            root,
            subpath: (!subpath.is_empty()).then_some(subpath),
            mode,
            workers: None,
        };
        let request = match request.normalized() {
            Ok(request) => request,
            Err(error) => {
                ui.set_sm_manage_error(true);
                ui.set_sm_manage_message(Self::manage_text(&error, chinese).into());
                return;
            }
        };
        let (client, snapshot) = match self.active_manage_client() {
            Ok((_, client, snapshot)) => (client, snapshot),
            Err(error) => {
                ui.set_sm_manage_error(true);
                ui.set_sm_manage_message(error.into());
                return;
            }
        };
        let (generation, request_token) = self.begin_manage_request(&ui);
        ui.set_sm_manage_error(false);
        ui.set_sm_manage_message(cn_or(chinese, "正在开始扫描…", "Starting the scan…").into());
        let c = self.clone();
        std::thread::spawn(move || {
            let started = client.start_scan(&request);
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                if !c2.finish_manage_request(&ui, request_token)
                    || !c2.library_current(generation)
                    || !c2.remote_snapshot_current(&snapshot)
                {
                    return;
                }
                match started {
                    Ok(scan) => {
                        ui.set_sm_scan_form(false);
                        ui.set_sm_manage_error(false);
                        ui.set_sm_manage_message("".into());
                        ui.set_sm_scan_active(scan.active);
                        ui.set_sm_scan_paused(scan.progress.paused || scan.pause_requested);
                        ui.set_sm_scan_text(scan_text(Some(&scan), chinese).into());
                        if scan.active {
                            c2.poll_server_scan(
                                client,
                                scan.progress.session_id,
                                generation,
                                chinese,
                            );
                        } else {
                            // Finished at once (e.g. an empty subfolder).
                            if c2.scan_moved_revision(&scan) {
                                c2.start_sync(false, false);
                            }
                            c2.show_page(PAGE_LIBRARY);
                        }
                    }
                    Err(error) => {
                        c2.note_remote_identity_error(&ui, &snapshot, &error);
                        ui.set_sm_manage_error(true);
                        ui.set_sm_manage_message(Self::manage_text(&error, chinese).into());
                        if matches!(error, RemoteError::Busy { .. }) {
                            // Show the scan that is already running.
                            c2.show_page(PAGE_LIBRARY);
                        }
                    }
                }
            });
        });
    }

    /// 「暂停」 / 「继续」 / 「取消扫描」 (0 / 1 / 2).
    fn control_server_scan(self: &Arc<Self>, action: i32) {
        let Some(ui) = self.ui.upgrade() else { return };
        if ui.get_sm_manage_busy() || !ui.get_sm_scan_active() {
            return;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        let control = match action {
            0 => ScanControl::Pause,
            1 => ScanControl::Resume,
            2 => ScanControl::Cancel,
            _ => return,
        };
        let (_, client, snapshot) = match self.active_manage_client() {
            Ok(pair) => pair,
            Err(error) => {
                ui.set_sm_manage_error(true);
                ui.set_sm_manage_message(error.into());
                return;
            }
        };
        let (generation, request_token) = self.begin_manage_request(&ui);
        ui.set_sm_manage_error(false);
        ui.set_sm_manage_message(
            match control {
                ScanControl::Pause => cn_or(chinese, "正在请求暂停…", "Requesting pause…"),
                ScanControl::Resume => cn_or(chinese, "正在继续…", "Resuming…"),
                ScanControl::Cancel => cn_or(chinese, "正在取消…", "Cancelling…"),
            }
            .into(),
        );
        let c = self.clone();
        std::thread::spawn(move || {
            let current = client.current_scan();
            let outcome = match current {
                Ok(Some(scan)) if scan.active && scan.progress.session_id > 0 => {
                    client.control_scan(scan.progress.session_id, control)
                }
                Ok(Some(_)) | Ok(None) => Err(RemoteError::ScanState("no active scan".into())),
                Err(error) => Err(error),
            };
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                if !c2.finish_manage_request(&ui, request_token)
                    || !c2.library_current(generation)
                    || !c2.remote_snapshot_current(&snapshot)
                {
                    return;
                }
                match outcome {
                    // The page's poller sees the end of the scan, reloads and syncs.
                    Ok(scan) => {
                        ui.set_sm_manage_error(false);
                        ui.set_sm_manage_message("".into());
                        if scan.active {
                            ui.set_sm_scan_paused(scan.progress.paused || scan.pause_requested);
                            ui.set_sm_scan_text(scan_text(Some(&scan), chinese).into());
                        }
                    }
                    Err(error) => {
                        c2.note_remote_identity_error(&ui, &snapshot, &error);
                        ui.set_sm_manage_error(true);
                        ui.set_sm_manage_message(Self::manage_text(&error, chinese).into());
                        // SCAN_STATE etc.: show the server's current state.
                        c2.show_page(PAGE_LIBRARY);
                    }
                }
            });
        });
    }

    /// Poll `GET /scans/{id}` every 500 ms until the scan is inactive; then reload the
    /// page and sync when the revision moved.
    fn poll_server_scan(
        self: &Arc<Self>,
        client: RemoteClient,
        session_id: i64,
        generation: u64,
        chinese: bool,
    ) {
        let c = self.clone();
        std::thread::spawn(move || {
            let mut id = session_id;
            loop {
                std::thread::sleep(SCAN_POLL);
                if !c.library_current(generation) || c.shutdown.load(Ordering::Acquire) {
                    return;
                }
                if id == 0 {
                    match client.current_scan() {
                        Ok(Some(scan)) if scan.active => {
                            id = scan.progress.session_id;
                            let c2 = c.clone();
                            post(c.ui.clone(), move |ui| {
                                if c2.library_current(generation) {
                                    ui.set_sm_scan_active(true);
                                    ui.set_sm_scan_paused(
                                        scan.progress.paused || scan.pause_requested,
                                    );
                                    ui.set_sm_scan_text(scan_text(Some(&scan), chinese).into());
                                }
                            });
                            continue;
                        }
                        Ok(_) => break,
                        Err(error) => {
                            c.post_library_error(generation, &error, chinese);
                            return;
                        }
                    }
                }
                match client.scan_status(id) {
                    Ok(scan) if scan.active => {
                        let c2 = c.clone();
                        post(c.ui.clone(), move |ui| {
                            if c2.library_current(generation) {
                                ui.set_sm_scan_active(true);
                                ui.set_sm_scan_paused(scan.progress.paused || scan.pause_requested);
                                ui.set_sm_scan_text(scan_text(Some(&scan), chinese).into());
                            }
                        });
                    }
                    Ok(scan) => {
                        let sync = c.scan_moved_revision(&scan);
                        let c2 = c.clone();
                        post(c.ui.clone(), move |ui| {
                            if !c2.library_current(generation) {
                                return;
                            }
                            ui.set_sm_scan_active(false);
                            ui.set_sm_scan_paused(false);
                            c2.load_server_library(&ui);
                            if sync {
                                c2.start_sync(false, false);
                            }
                        });
                        return;
                    }
                    Err(error) => {
                        c.post_library_error(generation, &error, chinese);
                        return;
                    }
                }
            }
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                if c2.library_current(generation) {
                    c2.load_server_library(&ui);
                }
            });
        });
    }

    /// The finished scan changed the library past the open snapshot (active server only).
    fn scan_moved_revision(&self, scan: &ServerScan) -> bool {
        let active = self.selected_profile().is_some_and(|profile| {
            self.config.lock().unwrap().data_source.active_source()
                == ActiveSource::Profile(profile.id)
        });
        if !active || !self.remote_read_only() {
            return false;
        }
        let cached = self
            .remote
            .lock()
            .unwrap()
            .state
            .as_ref()
            .map(|state| state.revision);
        cached.is_some_and(|cached| scan.needs_sync(cached))
    }

    /// After a server-mode removal (called by the library operation on the UI thread):
    /// cached ids are dropped at once (the removed rows are already hidden, see
    /// `server_removal`); a revision bump re-syncs through the provider's write listener.
    /// The 服务器音效库 page shows the outcome.
    pub(crate) fn server_removal_finished(
        &self,
        ui: &AppWindow,
        result: &Result<String, String>,
        removed_ids: Option<&[i64]>,
    ) {
        match result {
            Ok(report) => {
                let ids = removed_ids.map(|ids| ids.iter().copied().collect());
                self.invalidate_server_ids(ids.as_ref());
                ui.set_sm_manage_error(false);
                ui.set_sm_manage_message(report.clone().into());
            }
            Err(error) => {
                ui.set_sm_manage_error(true);
                ui.set_sm_manage_message(error.clone().into());
            }
        }
    }

    /// 「移除所选记录…」: ids from the main list's multi-selection (server mode).
    fn remove_server_records(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        if !self.remote_read_only() || ui.get_sm_scan_active() || !self.available() {
            return;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        if let Err(error) = self.active_manage_client() {
            ui.set_sm_manage_error(true);
            ui.set_sm_manage_message(error.into());
            return;
        }
        let mut ids = self
            .multi_selection
            .lock()
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>();
        ids.sort_unstable();
        if ids.is_empty() {
            ui.set_sm_manage_error(true);
            ui.set_sm_manage_message(
                cn_or(
                    chinese,
                    "请先在主列表勾选要移除的记录。",
                    "Check the records to remove in the main list first.",
                )
                .into(),
            );
            return;
        }
        ui.set_sm_confirm(0);
        self.run_library_operation(LibraryOperation::RemoveAudio(ids));
    }

    /// 「删除目录记录…」: the folder selected in the main tree (server mode).
    fn remove_server_folder(self: &Arc<Self>) {
        let Some(ui) = self.ui.upgrade() else { return };
        if !self.remote_read_only() || ui.get_sm_scan_active() || !self.available() {
            return;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        if let Err(error) = self.active_manage_client() {
            ui.set_sm_manage_error(true);
            ui.set_sm_manage_message(error.into());
            return;
        }
        let path = ui.get_selected_folder_path().to_string();
        if path.is_empty() {
            ui.set_sm_manage_error(true);
            ui.set_sm_manage_message(
                cn_or(
                    chinese,
                    "请先在主窗口的目录树中选择一个目录。",
                    "Select a folder in the main window's folder tree first.",
                )
                .into(),
            );
            return;
        }
        let id = self
            .tree
            .lock()
            .unwrap()
            .known
            .values()
            .find(|folder| folder.path == path)
            .map(|folder| folder.id);
        let Some(id) = id else {
            ui.set_sm_manage_error(true);
            ui.set_sm_manage_message(
                cn_or(
                    chinese,
                    "找不到所选目录的记录，请刷新后再试。",
                    "The selected folder is unknown; reload the tree and try again.",
                )
                .into(),
            );
            return;
        };
        ui.set_sm_confirm(0);
        self.run_library_operation(LibraryOperation::RemoveFolder(id));
    }

    fn post_library_error(self: &Arc<Self>, generation: u64, error: &RemoteError, chinese: bool) {
        let text = manage_error_text(error, chinese);
        let c = self.clone();
        post(self.ui.clone(), move |ui| {
            if !c.library_current(generation) {
                return;
            }
            ui.set_sm_library_loading(false);
            ui.set_sm_library_error(true);
            ui.set_sm_wave_supported(false);
            ui.set_sm_wc_visible(false);
            ui.set_sm_library_status(
                if chinese {
                    format!("读取服务器音效库失败：{text}")
                } else {
                    format!("Could not load the server library: {text}")
                }
                .into(),
            );
        });
    }

    fn post_library_page(self: &Arc<Self>, generation: u64, page: LibraryPage, chinese: bool) {
        let c = self.clone();
        post(self.ui.clone(), move |ui| {
            if !c.library_current(generation) {
                return;
            }
            let mut page = page;
            let waveform_cache = page.waveform_cache.take();
            ui.set_sm_library_loading(false);
            let mut problems = Vec::new();
            let mut note = |what: &str, error: &RemoteError| {
                problems.push(format!("{what}：{}", manage_error_text(error, chinese)));
            };
            match &page.capabilities {
                Ok(capabilities) => {
                    ui.set_sm_capabilities(capabilities_text(capabilities, chinese).into());
                    ui.set_sm_can_manage(
                        capabilities.may_manage() && c.require_remote_write_identity().is_ok(),
                    );
                    ui.set_sm_root_controls(
                        capabilities.may_manage_roots() && c.active_manage_client().is_ok(),
                    );
                }
                Err(error) => {
                    note(&cn_or(chinese, "权限信息", "capabilities"), error);
                    ui.set_sm_capabilities("".into());
                    ui.set_sm_can_manage(false);
                    ui.set_sm_root_controls(false);
                }
            }
            match &page.scan {
                Ok(scan) => {
                    ui.set_sm_scan_active(scan.as_ref().is_some_and(|scan| scan.active));
                    ui.set_sm_scan_paused(
                        scan.as_ref()
                            .is_some_and(|scan| scan.progress.paused || scan.pause_requested),
                    );
                    ui.set_sm_scan_text(scan_text(scan.as_ref(), chinese).into());
                }
                Err(error) => {
                    note(&cn_or(chinese, "当前扫描", "current scan"), error);
                    ui.set_sm_scan_active(false);
                    ui.set_sm_scan_paused(false);
                    ui.set_sm_scan_text("".into());
                }
            }
            let history = match &page.history {
                Ok(history) => history
                    .iter()
                    .map(|scan| SharedString::from(history_line(scan, chinese)))
                    .collect(),
                Err(error) => {
                    note(&cn_or(chinese, "扫描历史", "scan history"), error);
                    Vec::new()
                }
            };
            ui.set_sm_history(ModelRc::new(VecModel::from(history)));
            ui.set_sm_wave_supported(page.waves.is_some());
            ui.set_sm_wave_busy(false);
            ui.set_sm_wave_message("".into());
            match &page.waves {
                Some((settings, status)) => {
                    match settings {
                        Ok(settings) => {
                            ui.set_sm_wave_pregenerate(settings.pregenerate_list_waveforms);
                            ui.set_sm_wave_can_change(settings.can_change);
                        }
                        Err(error) => {
                            note(&cn_or(chinese, "服务器设置", "server settings"), error);
                            ui.set_sm_wave_can_change(false);
                        }
                    }
                    publish_wave_status(&ui, status, chinese);
                }
                None => {
                    ui.set_sm_wave_can_change(false);
                    ui.set_sm_wave_status("".into());
                }
            }
            match waveform_cache {
                Some(fetch) => c.publish_waveform_cache_fetch(&ui, fetch, generation, chinese),
                None => {
                    c.remote.lock().unwrap().manager.wave_cache = WaveformCacheState::default();
                    ui.set_sm_wc_visible(false);
                    ui.set_sm_wc_confirm(0);
                }
            }
            let rows: Vec<ServerRootRow> = page
                .roots
                .iter()
                .map(|root| root_row(root, chinese))
                .collect();
            let names: Vec<SharedString> = page
                .roots
                .iter()
                .map(|root| SharedString::from(root.name.clone()))
                .collect();
            ui.set_sm_roots(ModelRc::new(VecModel::from(rows)));
            c.remote.lock().unwrap().manager.roots = page.roots.clone();
            if ui.get_sm_root_selected() < 0
                || ui.get_sm_root_selected() as usize >= page.roots.len()
            {
                ui.set_sm_root_selected(if page.roots.is_empty() { -1 } else { 0 });
            }
            let previous = ui.get_sm_scan_root();
            ui.set_sm_root_names(ModelRc::new(VecModel::from(names)));
            if previous >= 0 && (previous as usize) < page.roots.len() {
                ui.set_sm_scan_root(previous);
            } else {
                ui.set_sm_scan_root(0);
            }
            let mut status = if chinese {
                format!(
                    "{} · {} 个根目录 · 读取于 {}",
                    page.host,
                    page.roots.len(),
                    format_clock(unix_now())
                )
            } else {
                format!(
                    "{} · {} roots · loaded {}",
                    page.host,
                    page.roots.len(),
                    format_clock(unix_now())
                )
            };
            if !problems.is_empty() {
                status.push_str(" · ");
                status.push_str(&problems.join(" · "));
            }
            ui.set_sm_library_error(!problems.is_empty());
            ui.set_sm_library_status(status.into());
        });
    }
}

/// Settings-only edits still require ordinary Metadata persistence. Recovery is for
/// switching profiles or changing the active profile's endpoint, which replaces its provider.
fn profile_save_changes_source(
    active: &ActiveSource,
    id: Option<&str>,
    old_url: Option<&str>,
    new_url: &str,
    after: AfterSave,
) -> bool {
    let selected_is_active = id.is_some_and(|id| active == &ActiveSource::Profile(id.to_owned()));
    (matches!(after, AfterSave::Switch) && !selected_is_active)
        || (selected_is_active && old_url.is_some_and(|url| !same_source_endpoint(url, new_url)))
}

/// The address parser removes the API suffix but preserves an explicit default port.
/// Source identity follows the effective connection endpoint, with case-sensitive paths.
fn same_source_endpoint(old_url: &str, new_url: &str) -> bool {
    match (ServerAddress::parse(old_url), ServerAddress::parse(new_url)) {
        (Ok(old), Ok(new)) => {
            old.scheme == new.scheme
                && old.host.eq_ignore_ascii_case(&new.host)
                && old.effective_port() == new.effective_port()
                && old.prefix == new.prefix
        }
        _ => old_url.trim() == new_url.trim(),
    }
}

/// List status of a profile from its latest test: text and kind (0 none, 1 ok, 3 failed).
fn test_status(test: Option<&TestSummary>, chinese: bool) -> (String, i32) {
    match test {
        None => (cn_or(chinese, "未测试", "not tested"), 0),
        Some(test) => (
            format!("{} · {}", format_time(test.at), test.summary),
            if test.ok { 1 } else { 3 },
        ),
    }
}

/// 「上次测试」 line of the connection page.
fn last_test_line(profile: Option<&ServerProfile>, chinese: bool) -> String {
    match profile.and_then(|profile| profile.last_test.as_ref()) {
        Some(test) => tested_line(test, true, chinese),
        None if profile.is_some() => cn_or(chinese, "上次测试：尚未测试", "Last test: never"),
        None => String::new(),
    }
}

fn tested_line(test: &TestSummary, recorded: bool, chinese: bool) -> String {
    let mark = if test.ok { "√" } else { "×" };
    let line = if chinese {
        format!("上次测试：{} {mark} {}", format_time(test.at), test.summary)
    } else {
        format!(
            "Last test: {} {mark} {}",
            format_time(test.at),
            test.summary
        )
    };
    if recorded {
        line
    } else {
        line + &cn_or(
            chinese,
            "（地址未保存，不记录）",
            " (unsaved address, not recorded)",
        )
    }
}

fn cache_info(
    library_id: Option<&str>,
    directory: Option<&str>,
    state: Option<&CacheState>,
    chinese: bool,
) -> String {
    let Some(library_id) = library_id else {
        return cn_or(
            chinese,
            "还不知道这个服务器的音效库（library id）：测试连接或同步后显示。",
            "The server library (library id) is not known yet: test or sync first.",
        );
    };
    let directory = directory.unwrap_or_default();
    match (state, chinese) {
        (Some(state), true) => format!(
            "音效库 {}（{}）\n缓存目录：{directory}\n快照：r{} · {} 个音效 · {:.1} MiB · 下载于 {} · 最近检查 {} · 服务器 {}",
            short_id(library_id),
            library_id,
            state.revision,
            state.audio_files,
            state.snapshot_bytes as f64 / 1_048_576.0,
            format_time(state.downloaded_at),
            format_time(state.checked_at),
            host_of(&state.server_url),
        ),
        (Some(state), false) => format!(
            "Library {} ({})\nCache directory: {directory}\nSnapshot: r{} · {} sounds · {:.1} MiB · downloaded {} · checked {} · server {}",
            short_id(library_id),
            library_id,
            state.revision,
            state.audio_files,
            state.snapshot_bytes as f64 / 1_048_576.0,
            format_time(state.downloaded_at),
            format_time(state.checked_at),
            host_of(&state.server_url),
        ),
        (None, true) => format!(
            "音效库 {}（{library_id}）\n缓存目录：{directory}\n本机还没有这个音效库的快照。",
            short_id(library_id)
        ),
        (None, false) => format!(
            "Library {} ({library_id})\nCache directory: {directory}\nNo snapshot of this library on this PC yet.",
            short_id(library_id)
        ),
    }
}

/// One line for the compact 缓存与导出 header; [`cache_info`] keeps the full details.
fn cache_summary(library_id: Option<&str>, state: Option<&CacheState>, chinese: bool) -> String {
    let Some(library_id) = library_id else {
        return cn_or(
            chinese,
            "还不知道这个服务器的音效库：测试连接或同步后显示。",
            "Server library not known yet: test or sync first.",
        );
    };
    let id = short_id(library_id);
    match (state, chinese) {
        (Some(state), true) => format!(
            "音效库 {id} · 快照 r{} · {} 个音效 · {:.1} MiB · 最近检查 {}",
            state.revision,
            state.audio_files,
            state.snapshot_bytes as f64 / 1_048_576.0,
            format_time(state.checked_at),
        ),
        (Some(state), false) => format!(
            "Library {id} · snapshot r{} · {} sounds · {:.1} MiB · checked {}",
            state.revision,
            state.audio_files,
            state.snapshot_bytes as f64 / 1_048_576.0,
            format_time(state.checked_at),
        ),
        (None, true) => format!("音效库 {id} · 本机还没有快照"),
        (None, false) => format!("Library {id} · no snapshot on this PC yet"),
    }
}

fn publish_wave_status(
    ui: &AppWindow,
    status: &Result<ListWaveformStatus, RemoteError>,
    chinese: bool,
) {
    match status {
        Ok(status) => {
            ui.set_sm_wave_error(false);
            ui.set_sm_wave_status(wave_status_text(status, chinese).into());
        }
        Err(error) => {
            ui.set_sm_wave_error(true);
            ui.set_sm_wave_status(
                if chinese {
                    format!("读取预生成进度失败：{}", manage_error_text(error, chinese))
                } else {
                    format!(
                        "Could not read the pre-generation progress: {}",
                        manage_error_text(error, chinese)
                    )
                }
                .into(),
            );
        }
    }
}

/// One line for `GET /waveforms/status` (§9.2).
fn wave_status_text(status: &ListWaveformStatus, chinese: bool) -> String {
    let state = match (
        status.state.as_str(),
        status.yield_reason.as_deref(),
        chinese,
    ) {
        ("off", _, true) => "未开启".to_owned(),
        ("off", _, false) => "off".to_owned(),
        ("idle", _, true) => "空闲（已补齐）".to_owned(),
        ("idle", _, false) => "idle (complete)".to_owned(),
        ("running", _, true) => "进行中".to_owned(),
        ("running", _, false) => "running".to_owned(),
        ("yielding", Some("audio"), true) => "让路中（试听）".to_owned(),
        ("yielding", Some("scan"), true) => "让路中（扫描）".to_owned(),
        ("yielding", _, true) => "让路中".to_owned(),
        ("yielding", Some(reason), false) => format!("yielding ({reason})"),
        ("yielding", None, false) => "yielding".to_owned(),
        ("paused", _, true) => "已暂停".to_owned(),
        ("paused", _, false) => "paused".to_owned(),
        (other, _, _) => other.to_owned(),
    };
    let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    let mut parts = vec![if chinese {
        format!("预生成：{state}")
    } else {
        format!("Pre-generation: {state}")
    }];
    parts.push(if chinese {
        format!("已缓存 {} / {}", status.cached, status.total)
    } else {
        format!("cached {} / {}", status.cached, status.total)
    });
    if let Some(remaining) = status.remaining {
        parts.push(if chinese {
            format!("本轮剩余 {remaining}")
        } else {
            format!("{remaining} left in this pass")
        });
    }
    if status.queued_on_demand > 0 {
        parts.push(if chinese {
            format!("按需队列 {}", status.queued_on_demand)
        } else {
            format!("{} queued on demand", status.queued_on_demand)
        });
    }
    if status.failed > 0 {
        parts.push(if chinese {
            format!("失败 {}", status.failed)
        } else {
            format!("{} failed", status.failed)
        });
    }
    if status.skipped_offline > 0 {
        parts.push(if chinese {
            format!("根离线跳过 {}", status.skipped_offline)
        } else {
            format!("{} skipped (root offline)", status.skipped_offline)
        });
    }
    parts.push(format!(
        "{} {:.0} / {:.0} MiB{}",
        if chinese { "缓存" } else { "cache" },
        mib(status.cache_bytes),
        mib(status.cache_limit_bytes),
        match (status.over_budget, chinese) {
            (true, true) => "（超出容量，已淘汰旧分片）",
            (true, false) => " (over budget, old shards evicted)",
            _ => "",
        }
    ));
    parts.join(" · ")
}

fn capabilities_text(capabilities: &Capabilities, chinese: bool) -> String {
    if !capabilities.supported {
        return cn_or(
            chinese,
            "服务器版本较旧，没有管理接口（/capabilities）：只能查看。",
            "Older server without management endpoints (/capabilities): view only.",
        );
    }
    let (mode, raw) = match (capabilities.manage_mode, chinese) {
        (ManageMode::All, true) => ("所有已认证的请求", "all"),
        (ManageMode::All, false) => ("any authenticated request", "all"),
        (ManageMode::Lan, true) => ("仅局域网直连", "lan"),
        (ManageMode::Lan, false) => ("direct LAN requests only", "lan"),
        (ManageMode::Off, true) => ("已关闭（只能在服务器上管理）", "off"),
        (ManageMode::Off, false) => ("off (manage on the server only)", "off"),
        (ManageMode::Unknown, true) => ("未知", "unknown"),
        (ManageMode::Unknown, false) => ("unknown", "unknown"),
    };
    let network = match (capabilities.external, chinese) {
        (Some(true), true) => " · 服务器将本机视为外网请求",
        (Some(true), false) => " · the server sees this PC as external",
        (Some(false), true) => " · 局域网直连",
        (Some(false), false) => " · direct LAN request",
        (None, _) => "",
    };
    let allowed = match (capabilities.may_manage(), chinese) {
        (true, true) => "本机可以管理（扫描、移除记录）",
        (true, false) => "this PC may manage (scans, record removal)",
        (false, true) => "本机不能管理，只能查看",
        (false, false) => "this PC may not manage: view only",
    };
    if chinese {
        format!("管理权限：{mode}（manage_mode = {raw}）· {allowed}{network}")
    } else {
        format!("Management: {mode} (manage_mode = {raw}) · {allowed}{network}")
    }
}

/// Short reason shown on greyed menu removals (empty when this PC may manage).
pub(super) fn capabilities_note(capabilities: &Capabilities, chinese: bool) -> String {
    if capabilities.may_manage() {
        return String::new();
    }
    if !capabilities.supported {
        return cn_or(
            chinese,
            "服务器版本较旧，没有管理接口",
            "older server without management endpoints",
        );
    }
    match (capabilities.manage_mode, chinese) {
        (ManageMode::Lan, true) => "服务器只允许局域网直连管理：manage_mode = lan".into(),
        (ManageMode::Lan, false) => {
            "the server allows management over direct LAN requests only: manage_mode = lan".into()
        }
        (ManageMode::Off, true) => "服务器已关闭远程管理：manage_mode = off".into(),
        (ManageMode::Off, false) => "management is off on the server: manage_mode = off".into(),
        (ManageMode::All, true) => "服务器不允许本机管理：manage_mode = all".into(),
        (ManageMode::All, false) => {
            "the server does not let this PC manage: manage_mode = all".into()
        }
        (ManageMode::Unknown, true) => "服务器没有说明管理权限".into(),
        (ManageMode::Unknown, false) => "the server did not state its management setting".into(),
    }
}

fn scan_mode_label(mode: &str, chinese: bool) -> String {
    match (mode, chinese) {
        ("full", true) => "全量".into(),
        ("incremental", true) => "增量".into(),
        (other, _) => other.to_owned(),
    }
}

fn root_scan_state(state: &str, chinese: bool) -> String {
    if !chinese {
        return state.to_owned();
    }
    match state {
        "running" => "扫描中".into(),
        "paused" => "已暂停".into(),
        "completed" => "已完成".into(),
        "cancelled" => "已取消".into(),
        "failed" => "失败".into(),
        other => other.to_owned(),
    }
}

/// 「上次扫描」 cell: time, mode, state and changes.
fn root_scan_text(scan: &RootScan, chinese: bool) -> String {
    let mut parts = Vec::new();
    if let Some(started) = scan.started_at {
        parts.push(format_time(started));
    }
    if !scan.mode.is_empty() {
        parts.push(scan_mode_label(&scan.mode, chinese));
    }
    if !scan.subpath.is_empty() {
        parts.push(scan.subpath.clone());
    }
    parts.push(root_scan_state(&scan.state, chinese));
    parts.push(format!(
        "+{} ~{} −{}",
        scan.added, scan.updated, scan.removed
    ));
    if scan.errors > 0 {
        parts.push(if chinese {
            format!("问题 {}", scan.errors)
        } else {
            format!("{} issues", scan.errors)
        });
    }
    parts.join(" · ")
}

fn root_row(root: &RemoteRoot, chinese: bool) -> ServerRootRow {
    let scanning = root.scanning.unwrap_or(false);
    let offline = root.state() == RootState::Offline;
    let count = |value: Option<u64>| value.map(|value| value.to_string()).unwrap_or("–".into());
    ServerRootRow {
        name: root.name.clone().into(),
        path: root.server_path.clone().into(),
        status: if scanning {
            cn_or(chinese, "扫描中", "scanning")
        } else if offline {
            cn_or(chinese, "离线", "offline")
        } else {
            root_status_text(root, chinese)
        }
        .into(),
        offline,
        scanning,
        file_count: count(root.file_count).into(),
        missing: count(root.missing_count).into(),
        last_scan: root
            .last_scan
            .as_ref()
            .map(|scan| root_scan_text(scan, chinese))
            .unwrap_or_else(|| cn_or(chinese, "从未扫描", "never"))
            .into(),
        message: if offline {
            root_status_text(root, chinese)
        } else {
            String::new()
        }
        .into(),
    }
}

/// 「服务器扫描」 text: the active scan's target, state and progress.
fn scan_text(scan: Option<&ServerScan>, chinese: bool) -> String {
    let Some(scan) = scan.filter(|scan| scan.active) else {
        return cn_or(chinese, "当前没有进行中的扫描。", "No scan is running.");
    };
    let progress = &scan.progress;
    let state = if progress.finalizing {
        cn_or(chinese, "最终对账中", "finalizing")
    } else if progress.paused {
        cn_or(chinese, "已暂停", "paused")
    } else if scan.cancel_requested {
        cn_or(chinese, "正在取消", "cancelling")
    } else if scan.pause_requested {
        cn_or(chinese, "正在暂停", "pausing")
    } else {
        super::super::scan_state_label(progress.state, chinese).to_owned()
    };
    let mut target = scan.root.clone().unwrap_or_default();
    if let Some(subpath) = scan
        .subpath
        .as_deref()
        .filter(|subpath| !subpath.is_empty())
    {
        target = format!("{target}/{subpath}");
    }
    let mode = scan
        .mode
        .as_deref()
        .map(|mode| scan_mode_label(mode, chinese))
        .unwrap_or_default();
    let mut text = if chinese {
        format!(
            "{state} · {mode} {target}\n已处理 {}/{} · 新增 {} · 刷新 {} · 未变化 {} · 缺失 {} · 问题 {}",
            progress.processed,
            progress.discovered,
            progress.added,
            progress.updated,
            progress.unchanged,
            progress.removed,
            progress.errors
        )
    } else {
        format!(
            "{state} · {mode} {target}\nprocessed {}/{} · added {} · refreshed {} · unchanged {} · missing {} · issues {}",
            progress.processed,
            progress.discovered,
            progress.added,
            progress.updated,
            progress.unchanged,
            progress.removed,
            progress.errors
        )
    };
    if !progress.current_path.is_empty() {
        text.push('\n');
        text.push_str(&progress.current_path);
    }
    if !progress.message.is_empty() {
        text.push('\n');
        text.push_str(&progress.message);
    }
    text
}

/// Why switching to the configured local library cannot start, or `None` when it can.
/// The default library may be absent (it is created empty, as on a first start); any
/// other selected library must still exist.
fn missing_local_library(
    path: &std::path::Path,
    default_database: &std::path::Path,
    chinese: bool,
) -> Option<String> {
    if path.is_file() || path == default_database {
        return None;
    }
    Some(if chinese {
        format!(
            "本地素材库不存在：{}。请先在「音效库管理」中重新选择本地素材库。",
            path.display()
        )
    } else {
        format!(
            "The local library does not exist: {}. Select a local library in library management first.",
            path.display()
        )
    })
}

/// One scan history row, in the local history's format.
fn history_line(scan: &ScanSession, chinese: bool) -> String {
    let mode = if chinese {
        if scan.scan_type == "full" {
            "完整"
        } else {
            "增量"
        }
    } else {
        scan.scan_type.as_str()
    };
    let state = if matches!(scan.state, ScanState::Running) && scan.end_time.is_some() {
        cn_or(chinese, "已结束", "ended")
    } else {
        super::super::scan_state_label(scan.state, chinese).to_owned()
    };
    // The server reports milliseconds ("2026-10-08 18:37:06.089"); seconds are enough.
    let started = scan.start_time.get(..19).unwrap_or(&scan.start_time);
    format!(
        "{started} · {mode} · {state} · +{} / ~{} / −{} · {} {}\n{}",
        scan.added,
        scan.updated,
        scan.removed,
        scan.errors,
        if chinese { "个错误" } else { "errors" },
        scan.scan_path
    )
}

#[cfg(test)]
mod tests {
    use super::super::RemoteRuntime;
    use super::*;
    use audiolib_core::{DataProvider, ScanProgress};

    #[test]
    fn mirror_folder_resolution_rejects_other_endpoints_and_missing_snapshots() {
        super::super::source_transition_tests::isolated_module(
            "source_manager::tests",
            "child_mirror_folder_identity",
        );
    }

    #[test]
    #[ignore = "isolated mirror directory identity and existence regression"]
    fn child_mirror_folder_identity() {
        use super::super::source_transition_tests::{configuration, root as test_root};
        use audiolib_audio::{AudioEngine, PlaybackConfig};
        use audiolib_db::SqliteProvider;
        use slint::ComponentHandle;

        i_slint_backend_testing::init_integration_test_with_system_time();
        let config = configuration(&test_root().join("preferences"));
        let provider = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
        let playback = PlaybackConfig {
            volume: 0.0,
            ..Default::default()
        };
        let engine = AudioEngine::new(playback.clone()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(&ui, provider.clone(), engine.handle(), config, playback);
        let cache = RemoteCache::new(&controller.data_root());
        let mut profile = ServerProfile {
            id: "mirror".into(),
            server_url: "https://SERVER:443/prefix/api/v1/".into(),
            library_id: Some("mirror-library".into()),
            ..Default::default()
        };
        let mut state = CacheState {
            library_id: "mirror-library".into(),
            database_file: "library-r1.db".into(),
            server_url: "https://server/prefix".into(),
            ..Default::default()
        };
        let database = cache.database_path(&state);
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        provider.backup_library(&database).unwrap();
        cache.save_state(&state).unwrap();
        assert_eq!(
            controller.selected_cache_state(&profile).1,
            Some(state.clone())
        );

        profile.server_url = "https://other-server/prefix".into();
        assert!(controller.selected_cache_state(&profile).1.is_none());
        profile.server_url = "https://server/prefix".into();
        profile.library_id = Some("other-library".into());
        assert!(controller.selected_cache_state(&profile).1.is_none());

        // The supported external-profile workflow copies the group and known ID.
        profile.name = "Mirror".into();
        profile.library_id = Some(state.library_id.clone());
        let mut siblings = controller.config.lock().unwrap().data_source.clone();
        siblings.profiles = vec![profile.clone()];
        let alias_id = siblings
            .upsert_external_profile(&profile.id, "https://mirror.example/prefix", None)
            .unwrap();
        let alias = siblings.profile(&alias_id).unwrap().clone();
        controller.config.lock().unwrap().data_source = siblings.clone();
        assert_eq!(
            controller.selected_cache_state(&alias).1,
            Some(state.clone())
        );

        let mut unknown_alias = alias.clone();
        unknown_alias.library_id = None;
        assert!(controller.selected_cache_state(&unknown_alias).1.is_none());
        let mut moved_alias = alias.clone();
        moved_alias.server_url = "https://new.example/prefix".into();
        assert!(controller.selected_cache_state(&moved_alias).1.is_none());

        let mut unrelated = siblings.clone();
        unrelated.profile_mut(&alias_id).unwrap().group = "unrelated-group".into();
        let unrelated_alias = unrelated.profile(&alias_id).unwrap().clone();
        controller.config.lock().unwrap().data_source = unrelated;
        assert!(
            controller
                .selected_cache_state(&unrelated_alias)
                .1
                .is_none()
        );

        let mut unknown_sibling = siblings.clone();
        unknown_sibling.profile_mut(&profile.id).unwrap().library_id = None;
        controller.config.lock().unwrap().data_source = unknown_sibling;
        assert!(controller.selected_cache_state(&alias).1.is_none());

        let mut moved_sibling = siblings.clone();
        moved_sibling.profile_mut(&profile.id).unwrap().server_url =
            "https://unrecognised.example/prefix".into();
        controller.config.lock().unwrap().data_source = moved_sibling;
        assert!(controller.selected_cache_state(&alias).1.is_none());
        controller.config.lock().unwrap().data_source = siblings;

        // The opened identity can arrive before this endpoint's profile is updated.
        profile.library_id = None;
        {
            let mut config = controller.config.lock().unwrap();
            config.data_source.mode = DataSourceMode::Server;
            config.data_source.active = Some(profile.id.clone());
            config.data_source.server_url = profile.server_url.clone();
            config.data_source.profiles = vec![profile.clone()];
        }
        controller.remote_mode.store(true, Ordering::Release);
        controller.remote.lock().unwrap().state = Some(state.clone());
        controller.set_selection(Selection::Profile(profile.id.clone()));
        controller.publish_cache_info(&ui);
        assert!(ui.get_sm_cache_available());
        assert_eq!(
            controller.selected_cache_state(&profile).0,
            Some(state.library_id.clone())
        );
        state.server_url = "https://other-server/prefix".into();
        controller.remote.lock().unwrap().state = Some(state);
        controller.publish_cache_info(&ui);
        assert!(!ui.get_sm_cache_available());

        profile.library_id = Some("never-synced".into());
        controller.config.lock().unwrap().data_source.profiles = vec![profile];
        let missing = cache.library_dir("never-synced");
        controller.open_selected_cache();
        assert!(
            !missing.exists(),
            "opening a mirror must not create its directory"
        );
        assert!(!ui.get_sm_cache_available());
        assert!(ui.get_ds_message().contains("does not exist"));

        std::fs::remove_file(&database).unwrap();
        let mut profile = controller.selected_profile().unwrap();
        profile.library_id = Some("mirror-library".into());
        controller
            .remote
            .lock()
            .unwrap()
            .state
            .as_mut()
            .unwrap()
            .server_url = profile.server_url.clone();
        assert!(controller.selected_cache_state(&profile).1.is_none());
        controller.shutdown();
        ui.hide().unwrap();
    }

    #[test]
    fn snapshot_swaps_refresh_the_visible_server_manager_and_resume_scan_polling() {
        let root = std::env::temp_dir().join(format!(
            "audiolib-manager-swap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::remote_mode::source_manager::tests::child_snapshot_swap_refresh",
                "--ignored",
                "--test-threads=1",
            ])
            .env("AUDIOLIB_MANAGER_SWAP_TEST_ROOT", &root)
            .output()
            .unwrap();
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "Manager snapshot-swap regression failed: {stdout}\n{stderr}"
        );
    }

    #[test]
    #[ignore = "isolated Slint event loop invoked by the manager snapshot-swap regression"]
    fn child_snapshot_swap_refresh() {
        use audiolib_audio::{AudioEngine, PlaybackConfig};
        use audiolib_core::config::AppConfig;
        use audiolib_db::SqliteProvider;
        use slint::ComponentHandle;
        use std::io::{BufRead, BufReader, Write};
        use std::sync::atomic::{AtomicBool, AtomicUsize};

        i_slint_backend_testing::init_integration_test_with_system_time();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let roots = Arc::new(AtomicUsize::new(0));
        let polls = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let (initial_page, initial_loaded) = std::sync::mpsc::sync_channel(1);
        let (server_roots, server_polls, server_finished) =
            (roots.clone(), polls.clone(), finished.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                assert!(first.starts_with("GET "), "unexpected mutation: {first}");
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line.trim().is_empty() {
                        break;
                    }
                }
                let path = first.split_whitespace().nth(1).unwrap();
                let response = match path {
                    "/api/v1/roots" => {
                        let request = server_roots.fetch_add(1, Ordering::AcqRel);
                        let name = if request == 0 {
                            "Stale"
                        } else if server_finished.load(Ordering::Acquire) {
                            "Finished"
                        } else {
                            "Fresh"
                        };
                        serde_json::to_string(&vec![root(name)]).unwrap()
                    }
                    "/api/v1/capabilities" => {
                        r#"{"manage_mode":"all","can_manage":true}"#.to_owned()
                    }
                    "/api/v1/scans/current" => {
                        server_polls.fetch_add(1, Ordering::AcqRel);
                        let scan =
                            (!server_finished.load(Ordering::Acquire)).then_some(ServerScan {
                                progress: ScanProgress {
                                    session_id: 7,
                                    ..Default::default()
                                },
                                active: true,
                                ..Default::default()
                            });
                        serde_json::json!({ "scan": scan }).to_string()
                    }
                    "/api/v1/scans/7" => serde_json::to_string(&ServerScan::default()).unwrap(),
                    "/api/v1/history/scans?limit=20&offset=0" => "[]".to_owned(),
                    _ => panic!("unexpected request: {first}"),
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
                stream.write_all(reply.as_bytes()).unwrap();
                if path.starts_with("/api/v1/history/scans") {
                    let _ = initial_page.try_send(());
                }
            }
        });

        let root =
            std::path::PathBuf::from(std::env::var_os("AUDIOLIB_MANAGER_SWAP_TEST_ROOT").unwrap());
        let mut config = AppConfig::load_in(root.clone()).unwrap();
        config.data_source.profiles = vec![
            ServerProfile {
                id: "active".into(),
                name: "Active server".into(),
                server_url: "http://127.0.0.1:9".into(),
                ..Default::default()
            },
            ServerProfile {
                id: "viewed".into(),
                name: "Viewed server".into(),
                server_url: url,
                ..Default::default()
            },
        ];
        config.data_source.active = Some("active".into());
        config.data_source.mirror_active();
        config.data_source.poll_interval_secs = 0;
        let provider = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
        let snapshot = root.join("snapshot.db");
        provider.backup_library(&snapshot).unwrap();
        let playback = PlaybackConfig::default();
        let engine = AudioEngine::new(playback.clone()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
        controller.set_selection(Selection::Profile("viewed".into()));
        controller.publish_sources(&ui);
        ui.set_library_visible(true);
        ui.set_sm_page(PAGE_LIBRARY);
        ui.set_ds_name("Unsaved name".into());
        ui.set_ds_token("Unsaved token".into());
        ui.set_ds_dirty(true);
        controller.show_page(PAGE_LIBRARY);
        initial_loaded.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(ui.get_sm_library_loading());
        let previous_generation = controller.remote.lock().unwrap().manager.library_generation;
        controller
            .switch_data_source(&super::super::DataSourceSwitch::Server {
                database: snapshot,
                state: None,
            })
            .unwrap();
        assert!(!controller.library_current(previous_generation));
        controller.publish_data_source(&ui);
        let (generation, request) = controller.begin_manage_request(&ui);
        ui.set_sm_root_form(true);
        ui.set_sm_fs_visible(true);
        ui.set_sm_fs_busy(true);
        controller.refresh_source_manager_after_swap(&ui);
        controller.reconcile_source_manager_after_swap(&ui);
        assert!(controller.library_current(generation));
        assert!(ui.get_sm_manage_busy());
        assert!(!ui.get_sm_root_form());
        assert!(!ui.get_sm_fs_visible());
        assert!(!ui.get_sm_fs_busy());

        let refreshed_and_polled = std::rc::Rc::new(std::cell::Cell::new(false));
        let observed = refreshed_and_polled.clone();
        let weak = ui.as_weak();
        let started = std::time::Instant::now();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(10),
            move || {
                let ui = weak.upgrade().unwrap();
                let name = ui.get_sm_root_names().row_data(0).unwrap_or_default();
                if !ui.get_sm_library_loading()
                    && ui.get_sm_scan_active()
                    && name == "Fresh"
                    && polls.load(Ordering::Acquire) >= 3
                {
                    observed.set(true);
                    finished.store(true, Ordering::Release);
                }
                if (observed.get() && !ui.get_sm_scan_active() && name == "Finished")
                    || started.elapsed() > Duration::from_secs(8)
                {
                    slint::quit_event_loop().unwrap();
                }
            },
        );
        slint::run_event_loop().unwrap();
        timer.stop();
        assert!(
            refreshed_and_polled.get(),
            "the scan poller did not restart"
        );
        assert!(!ui.get_sm_library_loading());
        assert!(!ui.get_sm_library_error(), "{}", ui.get_sm_library_status());
        assert!(
            !ui.get_sm_scan_active(),
            "scan completion was not published"
        );
        assert_eq!(ui.get_sm_root_names().row_data(0).unwrap(), "Finished");
        assert_eq!(controller.selection(), Selection::Profile("viewed".into()));
        assert_eq!(ui.get_sm_selected(), 2);
        assert_eq!(ui.get_sm_page(), PAGE_LIBRARY);
        assert_eq!(ui.get_ds_name(), "Unsaved name");
        assert_eq!(ui.get_ds_token(), "Unsaved token");
        assert!(ui.get_ds_dirty());
        assert!(ui.get_sm_manage_busy(), "refresh released a newer request");
        assert!(controller.finish_manage_request(&ui, request));

        let requests = roots.load(Ordering::Acquire);
        ui.set_library_visible(false);
        controller.reconcile_source_manager_after_swap(&ui);
        assert!(!ui.get_sm_library_loading());
        assert_eq!(roots.load(Ordering::Acquire), requests);
        ui.set_library_visible(true);
        ui.set_sm_page(PAGE_CONNECTION);
        controller.reconcile_source_manager_after_swap(&ui);
        assert_eq!(ui.get_sm_page(), PAGE_CONNECTION);
        assert!(!ui.get_sm_library_loading());
        ui.set_sm_page(PAGE_CACHE);
        ui.set_sm_cache_info("Stale cache info".into());
        controller.reconcile_source_manager_after_swap(&ui);
        assert_ne!(ui.get_sm_cache_info(), "Stale cache info");
        assert_eq!(controller.selection(), Selection::Profile("viewed".into()));
    }

    #[test]
    fn profile_settings_recover_metadata_only_when_the_source_changes() {
        let active = ActiveSource::Profile("active".into());
        for (old_url, new_url) in [
            ("http://server", "http://server"),
            ("http://server:80", "http://server/api/v1"),
            (
                "https://SERVER:443/prefix/",
                "https://server/prefix/api/v1/",
            ),
        ] {
            for after in [AfterSave::Nothing, AfterSave::Close, AfterSave::Switch] {
                assert!(!profile_save_changes_source(
                    &active,
                    Some("active"),
                    Some(old_url),
                    new_url,
                    after,
                ));
            }
        }
        for (old_url, new_url) in [
            ("http://server", "https://server"),
            ("http://server", "http://server:81"),
            ("http://server/Library", "http://server/library"),
        ] {
            assert!(profile_save_changes_source(
                &active,
                Some("active"),
                Some(old_url),
                new_url,
                AfterSave::Nothing,
            ));
        }
        assert!(profile_save_changes_source(
            &active,
            Some("active"),
            Some("http://old-server"),
            "http://new-server",
            AfterSave::Nothing,
        ));
        assert!(!profile_save_changes_source(
            &active,
            Some("other"),
            Some("http://old-server"),
            "http://new-server",
            AfterSave::Nothing,
        ));
        assert!(profile_save_changes_source(
            &active,
            Some("other"),
            Some("http://server"),
            "http://server",
            AfterSave::Switch,
        ));
        assert!(profile_save_changes_source(
            &ActiveSource::Local,
            None,
            None,
            "http://server",
            AfterSave::Switch,
        ));
    }

    #[test]
    fn close_after_save_preserves_webdav_and_waits_for_persistence() {
        let root = std::env::temp_dir().join(format!(
            "audiolib-manager-close-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::remote_mode::source_manager::tests::child_close_after_save",
                "--ignored",
                "--test-threads=1",
            ])
            .env("AUDIOLIB_MANAGER_CLOSE_TEST_ROOT", &root)
            .output()
            .unwrap();
        // The isolated child has exited; only this test's owned directory is removed.
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "Close/save regression failed: {stdout}\n{stderr}"
        );
    }

    #[test]
    #[ignore = "isolated Slint event loop invoked by the close/save regression"]
    fn child_close_after_save() {
        use audiolib_audio::{AudioEngine, PlaybackConfig};
        use audiolib_core::config::AppConfig;
        use audiolib_db::SqliteProvider;
        use slint::ComponentHandle;

        i_slint_backend_testing::init_integration_test_with_system_time();
        let root =
            std::path::PathBuf::from(std::env::var_os("AUDIOLIB_MANAGER_CLOSE_TEST_ROOT").unwrap());
        let mut config = AppConfig::load_in(root.clone()).unwrap();
        config.data_source.webdav.username = "existing-user".into();
        config.data_source.webdav.base_url = Some("https://example.com/dav".into());
        config
            .data_source
            .webdav
            .root_urls
            .insert("SFX".into(), "https://example.com/dav/custom-root".into());
        let webdav = config.data_source.webdav.clone();
        config.save().unwrap();
        let provider = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
        let playback = PlaybackConfig::default();
        let engine = AudioEngine::new(playback.clone()).unwrap();
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
        let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
        // Every invalidation releases its own busy indicator. A late completion
        // must preserve a subsequent request's indicator, even after a real
        // provider swap (which occurs off the UI thread in normal operation).
        for action in 0..4 {
            let (_, old) = controller.begin_manage_request(&ui);
            assert!(ui.get_sm_manage_busy());
            match action {
                0 => controller.show_page(PAGE_CONNECTION),
                1 => ui.invoke_sm_closed(),
                2 => controller.set_selection(Selection::Local),
                _ => {
                    controller
                        .switch_data_source(&super::super::DataSourceSwitch::Local)
                        .unwrap();
                    controller.publish_data_source(&ui);
                }
            }
            assert!(
                !ui.get_sm_manage_busy(),
                "invalidation {action} retained busy state"
            );
            let (_, current) = controller.begin_manage_request(&ui);
            assert!(!controller.finish_manage_request(&ui, old));
            assert!(
                ui.get_sm_manage_busy(),
                "old completion cleared current request"
            );
            assert!(controller.finish_manage_request(&ui, current));
            assert!(!ui.get_sm_manage_busy());
        }
        {
            let mut remote = controller.remote.lock().unwrap();
            remote.secrets_loaded = true;
            remote.secrets.webdav_password = "existing-password".into();
        }
        ui.invoke_open_data_source();
        ui.invoke_sm_select_row(-1);
        ui.set_ds_dirty(true);
        ui.invoke_close_library_manager();
        assert_eq!(ui.get_sm_pending_select(), -3);
        // Validation failure must leave the new profile and its draft open.
        ui.invoke_sm_pending_save_requested(-3);
        assert!(ui.get_library_visible() && ui.get_ds_dirty());
        assert!(!ui.get_ds_applying());
        assert!(!ui.get_ds_message().is_empty());
        ui.show().unwrap();
        crate::ui_tests::click_connection_field(&ui, "Server profile name");
        crate::ui_tests::send_keys(&ui, &["Saved before closing".into()]);
        crate::ui_tests::click_connection_field(&ui, "Server URL");
        crate::ui_tests::send_keys(&ui, &["http://127.0.0.1:18790".into()]);
        ui.invoke_sm_pending_save_requested(-3);
        assert!(ui.get_ds_applying() && ui.get_library_visible());
        let weak = ui.as_weak();
        let started = std::time::Instant::now();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(10),
            move || {
                let ui = weak.upgrade().unwrap();
                if !ui.get_ds_applying() || started.elapsed() > Duration::from_secs(8) {
                    slint::quit_event_loop().unwrap();
                }
            },
        );
        slint::run_event_loop().unwrap();
        timer.stop();
        assert!(!ui.get_ds_applying(), "save timed out");
        assert!(!ui.get_library_visible(), "{}", ui.get_ds_message());
        ui.hide().unwrap();
        assert_eq!(ui.get_sm_pending_select(), -2);
        let saved = AppConfig::load_in(root.clone()).unwrap();
        assert_eq!(saved.data_source.profiles.len(), 1);
        assert_eq!(saved.data_source.profiles[0].name, "Saved before closing");
        assert_eq!(saved.data_source.webdav, webdav);
        assert_eq!(saved.data_source.active_source(), ActiveSource::Local);
        let secrets = audiolib_remote::load_secrets(&root).unwrap();
        assert_eq!(secrets.webdav_password, "existing-password");
    }

    #[test]
    fn close_after_active_connection_save_keeps_local_recovery_available() {
        super::super::source_transition_tests::isolated_module(
            "source_manager::tests",
            "child_close_during_active_connection_sync",
        );
    }

    #[test]
    #[ignore = "isolated Slint local recovery after saving an active connection on close"]
    fn child_close_during_active_connection_sync() {
        use super::super::source_transition_tests::{
            accept, configuration, request, root as test_root, run_until,
        };
        use audiolib_audio::{AudioEngine, PlaybackConfig};
        use audiolib_core::config::AppConfig;
        use audiolib_db::SqliteProvider;
        use slint::ComponentHandle;

        i_slint_backend_testing::init_integration_test_with_system_time();
        let root = test_root();
        let mut config = configuration(&root.join("preferences"));
        config.data_source.mode = DataSourceMode::Server;
        config.data_source.server_url = "http://127.0.0.1:9".into();
        config.data_source.poll_interval_secs = 0;
        config.data_source.migrate_profiles();
        config.save().unwrap();
        let local = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
        let placeholder = super::super::open_remote_placeholder(&config).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let new_url = format!("http://{}", listener.local_addr().unwrap());
        let (entered, request_entered) = std::sync::mpsc::sync_channel(1);
        let (release, wait_release) = std::sync::mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            assert!(request(&mut socket).starts_with("GET /api/v1/health"));
            entered.send(()).unwrap();
            wait_release.recv_timeout(Duration::from_secs(15)).unwrap();
            drop(socket);
        });
        let playback = PlaybackConfig::default();
        let engine = AudioEngine::new(playback.clone()).unwrap();
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
        let controller =
            Controller::new(&ui, placeholder.provider, engine.handle(), config, playback);
        controller.remote_mode.store(true, Ordering::Release);
        controller.publish_data_source(&ui);
        ui.show().unwrap();
        ui.invoke_open_data_source();
        ui.set_ds_server_url(new_url.clone().into());
        ui.set_ds_dirty(true);
        ui.invoke_close_library_manager();
        assert_eq!(ui.get_sm_pending_select(), -3);
        ui.invoke_sm_pending_save_requested(-3);
        assert!(ui.get_library_visible() && ui.get_ds_applying());
        let requested = std::rc::Rc::new(std::cell::Cell::new(false));
        let released = std::rc::Rc::new(std::cell::Cell::new(false));
        let expected_path = local.database_path().to_path_buf();
        let weak = ui.as_weak();
        let c = controller.clone();
        run_until(&ui, move || {
            let ui = weak.upgrade().unwrap();
            if !requested.get() {
                match request_entered.try_recv() {
                    Ok(()) => {}
                    Err(std::sync::mpsc::TryRecvError::Empty) => return false,
                    Err(error) => panic!("active connection resync request: {error}"),
                }
                assert!(ui.get_library_visible(), "resync hid the recovery controls");
                assert!(ui.get_ds_syncing() && ui.get_ds_source_syncing());
                assert!(c.remote.lock().unwrap().manager.close_after_sync);
                assert_eq!(ui.get_sm_pending_select(), -2);
                assert_eq!(c.config.lock().unwrap().data_source.server_url, new_url);
                crate::ui_tests::click_label(&ui, "Local settings");
                assert_eq!(ui.get_sm_selected(), 0);
                assert!(!c.remote.lock().unwrap().manager.close_after_sync);
                crate::ui_tests::click_label(&ui, "Switch to this source");
                assert!(!ui.get_ds_syncing());
                requested.set(true);
                return false;
            }
            if !c.available()
                || c.remote_read_only()
                || c.provider().database_path() != expected_path
            {
                return false;
            }
            if !released.get() {
                release.send(()).unwrap();
                released.set(true);
            }
            !c.remote_busy.load(Ordering::Acquire)
                && c.remote_plays.lock().unwrap().publication_gates == 0
        });
        server.join().unwrap();
        assert!(ui.get_library_visible());
        assert!(!controller.remote_read_only());
        assert_eq!(
            AppConfig::load_in(root.join("preferences"))
                .unwrap()
                .data_source
                .active_source(),
            ActiveSource::Local
        );
        controller.shutdown();
        ui.hide().unwrap();
    }

    #[test]
    fn save_on_close_follows_source_handoff_success_and_cancellation() {
        super::super::source_transition_tests::isolated_module(
            "source_manager::tests",
            "child_save_on_close_handoff_completion",
        );
    }

    #[test]
    #[ignore = "isolated Slint Save-on-Close handoff completion regression"]
    fn child_save_on_close_handoff_completion() {
        use super::super::source_transition_tests::{configuration, root as test_root, run_until};
        use audiolib_audio::{AudioEngine, PlaybackConfig};
        use audiolib_db::SqliteProvider;
        use slint::ComponentHandle;

        i_slint_backend_testing::init_integration_test_with_system_time();
        let config = configuration(&test_root().join("preferences"));
        let provider = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
        let playback = PlaybackConfig::default();
        let engine = AudioEngine::new(playback.clone()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
        ui.set_library_visible(true);
        let barrier = controller.begin_source_change(&ui).unwrap();
        controller.wait_metadata(barrier, true).unwrap();
        controller.remote.lock().unwrap().manager.close_after_sync = true;
        // Cache publication invalidates library requests before provider commit;
        // that invalidation must retain the original Save-on-Close request.
        controller
            .remote
            .lock()
            .unwrap()
            .manager
            .invalidate_library();
        controller.finish_source_change(&ui, true);
        assert!(
            !ui.get_library_visible(),
            "successful handoff must finish Close"
        );
        assert!(!controller.remote.lock().unwrap().manager.close_after_sync);

        ui.set_library_visible(true);
        let barrier = controller.begin_source_change(&ui).unwrap();
        controller.wait_metadata(barrier, true).unwrap();
        controller.remote.lock().unwrap().manager.close_after_sync = true;
        controller.select_source(0);
        assert!(!controller.remote.lock().unwrap().manager.close_after_sync);
        controller.finish_source_change(&ui, true);
        assert!(
            ui.get_library_visible(),
            "navigation supersedes the old Close"
        );

        let barrier = controller.begin_source_change(&ui).unwrap();
        controller.wait_metadata(barrier, true).unwrap();
        controller.remote.lock().unwrap().manager.close_after_sync = true;
        controller.finish_source_change(&ui, false);
        assert!(ui.get_library_visible(), "failure must keep recovery open");
        assert!(!controller.remote.lock().unwrap().manager.close_after_sync);
        let c = controller.clone();
        run_until(&ui, move || c.available());
        assert!(ui.get_library_visible());
        // A later successful transition must not inherit a failed request's Close.
        let barrier = controller.begin_source_change(&ui).unwrap();
        controller.wait_metadata(barrier, true).unwrap();
        controller.finish_source_change(&ui, true);
        assert!(ui.get_library_visible());
        controller.shutdown();
        ui.hide().unwrap();
    }

    fn root(name: &str) -> RemoteRoot {
        RemoteRoot {
            name: name.into(),
            server_path: format!("/Volumes/{name}"),
            ..RemoteRoot::default()
        }
    }

    #[test]
    fn root_configuration_identity_ignores_status_and_order_but_keeps_exact_paths() {
        let initial = vec![root("SFX"), root("Music")];
        let mut refreshed = vec![initial[1].clone(), initial[0].clone()];
        refreshed[1].online = Some(false);
        refreshed[1].status = Some("offline".into());
        refreshed[1].checked_at = Some(123);
        refreshed[1].file_count = Some(17);
        refreshed[1].webdav_url = Some("https://example.com/new-suggestion".into());
        assert!(!root_configuration_changed(&initial, &refreshed));
        refreshed[1].server_path = "/Volumes/sfx".into();
        assert!(root_configuration_changed(&initial, &refreshed));
        refreshed[1].server_path = "/Volumes/SFX ".into();
        assert!(root_configuration_changed(&initial, &refreshed));
        assert!(root_configuration_changed(&initial, &initial[..1]));
        refreshed = initial.clone();
        refreshed[1].name = "Replacement".into();
        assert!(root_configuration_changed(&initial, &refreshed));
    }

    #[test]
    fn unchanged_snapshot_root_updates_preserve_status_drafts_and_drain_management() {
        super::super::source_transition_tests::isolated_module(
            "source_manager::tests",
            "child_root_config_refresh",
        );
    }

    #[test]
    #[ignore = "isolated Slint root configuration refresh regression"]
    fn child_root_config_refresh() {
        use super::super::source_transition_tests::{configuration, root as test_root, run_until};
        use super::super::{RevisionCheck, RevisionPoll};
        use audiolib_audio::{AudioEngine, PlaybackConfig};
        use audiolib_db::SqliteProvider;
        use audiolib_remote::{CachedIdentity, SyncOutcome};

        i_slint_backend_testing::init_integration_test_with_system_time();
        let mut config = configuration(&test_root().join("preferences"));
        config.data_source.mode = DataSourceMode::Server;
        config.data_source.server_url = "http://127.0.0.1:9".into();
        config.data_source.poll_interval_secs = 0;
        let cache = RemoteCache::new(&config.data_root());
        let provider = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
        let playback = PlaybackConfig::default();
        let engine = AudioEngine::new(playback.clone()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
        controller.remote_mode.store(true, Ordering::Release);
        let state = CacheState {
            library_id: "roots-refresh-library".into(),
            server_url: "http://127.0.0.1:9".into(),
            revision: 7,
            database_file: "unchanged-snapshot.db".into(),
            roots: vec![root("SFX")],
            ..Default::default()
        };
        {
            let mut remote = controller.remote.lock().unwrap();
            remote.state = Some(state.clone());
            remote.live_roots = Some(state.roots.clone());
        }
        ui.set_library_visible(true);
        ui.set_sm_page(PAGE_LIBRARY);
        let arm_draft = |roots: &[RemoteRoot]| {
            let mut remote = controller.remote.lock().unwrap();
            remote.manager.roots = roots.to_vec();
            remote.manager.root_edit_target = Some("SFX".into());
            let identity = (
                remote.manager.library_generation,
                remote.manager.browser_sequence,
            );
            drop(remote);
            ui.set_sm_root_controls(true);
            ui.set_sm_root_selected(0);
            ui.set_sm_root_form(true);
            ui.set_sm_root_editing(true);
            ui.set_sm_root_name("SFX".into());
            ui.set_sm_root_path("/user/path draft ".into());
            ui.set_sm_fs_visible(true);
            ui.set_sm_fs_busy(true);
            ui.set_sm_fs_path("/old/browser".into());
            ui.set_sm_confirm(5);
            identity
        };
        let checked = Some(CachedIdentity {
            library_id: state.library_id.clone(),
            revision: state.revision,
        });
        let poll = |roots| RevisionPoll {
            source_generation: controller.source_generation.load(Ordering::Acquire),
            checked: checked.clone(),
            check: RevisionCheck::UpToDate { revision: 7 },
            roots: Some(roots),
            next_check: None,
            auto_due: false,
        };
        let generation = arm_draft(&state.roots);
        let mut health_update = state.roots.clone();
        health_update[0].online = Some(false);
        health_update[0].status = Some("waking".into());
        health_update[0].file_count = Some(42);
        controller.apply_revision_poll(&ui, poll(health_update.clone()));
        assert!(ui.get_sm_root_form() && ui.get_sm_fs_visible());
        assert_eq!(ui.get_sm_root_path(), "/user/path draft ");
        {
            let remote = controller.remote.lock().unwrap();
            assert_eq!(remote.manager.library_generation, generation.0);
            assert_eq!(remote.manager.browser_sequence, generation.1);
            assert_eq!(remote.manager.root_edit_target.as_deref(), Some("SFX"));
        }
        let mut next = state.clone();
        next.roots = health_update;
        let outcome = |state| {
            Ok(SyncOutcome {
                state,
                downloaded: false,
                library_changed: false,
                previous: None,
                elapsed_ms: 0,
            })
        };
        controller.complete_sync(&ui, outcome(next.clone()), None, &cache);
        assert!(ui.get_sm_root_form() && ui.get_sm_fs_visible());
        assert_eq!(ui.get_sm_root_path(), "/user/path draft ");

        next.roots[0].server_path = "/Volumes/Replacement".into();
        controller.apply_revision_poll(&ui, poll(next.roots.clone()));
        assert!(!ui.get_sm_root_form() && !ui.get_sm_fs_visible());
        assert!(!ui.get_sm_fs_busy() && !ui.get_sm_root_controls());
        assert!(ui.get_sm_root_name().is_empty() && ui.get_sm_root_path().is_empty());
        assert!(ui.get_sm_fs_path().is_empty());
        assert_eq!(ui.get_sm_confirm(), 0);
        assert!(!controller.library_current(generation.0));
        assert!(
            controller
                .remote
                .lock()
                .unwrap()
                .manager
                .root_edit_target
                .is_none()
        );

        // The poll already invalidated this exact configuration. Publishing its
        // unchanged snapshot must retain a newly entered, current draft.
        let generation = arm_draft(&next.roots);
        controller.complete_sync(&ui, outcome(next.clone()), None, &cache);
        assert!(ui.get_sm_root_form() && ui.get_sm_fs_visible());
        assert!(controller.library_current(generation.0));

        // A structural same-snapshot sync clears stale editing immediately, but
        // preserves ownership until the current management callback drains.
        let (request_generation, request) = controller.begin_manage_request(&ui);
        next.roots.push(root("New"));
        controller.complete_sync(&ui, outcome(next), None, &cache);
        assert!(!ui.get_sm_root_form() && !ui.get_sm_fs_visible());
        assert!(ui.get_sm_manage_busy());
        assert!(controller.library_current(request_generation));
        {
            let remote = controller.remote.lock().unwrap();
            assert_eq!(remote.manager.manage_pending, Some(request));
            assert!(remote.manager.root_refresh_pending);
            assert_ne!(remote.manager.browser_sequence, generation.1);
        }
        assert!(controller.finish_manage_request(&ui, request));
        assert!(controller.library_current(request_generation));
        let c = controller.clone();
        run_until(&ui, move || !c.library_current(request_generation));
        assert!(!ui.get_sm_manage_busy());
        assert!(controller.remote.lock().unwrap().manager.roots.is_empty());
        assert!(
            !controller
                .remote
                .lock()
                .unwrap()
                .manager
                .root_refresh_pending
        );
    }

    #[test]
    fn root_rows_show_counts_last_scan_and_offline_reason() {
        let mut online = root("SFX");
        online.online = Some(true);
        online.status = Some("online".into());
        online.file_count = Some(3009);
        online.missing_count = Some(2);
        online.last_scan = Some(RootScan {
            id: 7,
            mode: "incremental".into(),
            state: "completed".into(),
            started_at: Some(1_700_000_000),
            added: 3,
            updated: 1,
            ..RootScan::default()
        });
        let row = root_row(&online, true);
        assert_eq!(row.file_count, "3009");
        assert_eq!(row.missing, "2");
        assert!(!row.offline && !row.scanning);
        assert!(
            row.last_scan.contains("增量 · 已完成 · +3 ~1 −0"),
            "{}",
            row.last_scan
        );
        assert!(row.message.is_empty());

        let mut offline = root("NAS");
        offline.online = Some(false);
        offline.status = Some("offline".into());
        offline.status_message = Some("volume not mounted".into());
        let row = root_row(&offline, true);
        assert!(row.offline);
        assert_eq!(row.status, "离线");
        assert_eq!(row.file_count, "–");
        assert_eq!(row.last_scan, "从未扫描");
        assert!(row.message.contains("volume not mounted"));

        let mut scanning = root("SFX");
        scanning.scanning = Some(true);
        assert_eq!(root_row(&scanning, false).status, "scanning");
    }

    #[test]
    fn scan_text_shows_the_active_scan_only() {
        assert_eq!(scan_text(None, true), "当前没有进行中的扫描。");
        let mut scan = ServerScan {
            progress: ScanProgress {
                session_id: 9,
                processed: 40,
                discovered: 100,
                added: 2,
                current_path: "/Volumes/SFX/a.wav".into(),
                ..ScanProgress::default()
            },
            root: Some("SFX".into()),
            subpath: Some("Doors".into()),
            mode: Some("full".into()),
            active: true,
            ..ServerScan::default()
        };
        let text = scan_text(Some(&scan), true);
        assert!(text.starts_with("扫描中 · 全量 SFX/Doors"), "{text}");
        assert!(text.contains("已处理 40/100 · 新增 2"), "{text}");
        assert!(text.ends_with("/Volumes/SFX/a.wav"), "{text}");
        scan.progress.paused = true;
        assert!(scan_text(Some(&scan), false).starts_with("paused"));
        scan.active = false;
        assert_eq!(scan_text(Some(&scan), false), "No scan is running.");
    }

    #[test]
    fn wave_status_text_summarises_pre_generation_progress() {
        let status = ListWaveformStatus {
            enabled: true,
            state: "yielding".into(),
            yield_reason: Some("audio".into()),
            total: 909_342,
            cached: 412_000,
            remaining: Some(497_342),
            queued_on_demand: 3,
            failed: 12,
            cache_bytes: 221 * 1024 * 1024,
            cache_limit_bytes: 640 * 1024 * 1024,
            ..ListWaveformStatus::default()
        };
        assert_eq!(
            wave_status_text(&status, true),
            "预生成：让路中（试听） · 已缓存 412000 / 909342 · 本轮剩余 497342 · 按需队列 3 · 失败 12 · 缓存 221 / 640 MiB"
        );
        let off = ListWaveformStatus {
            state: "off".into(),
            over_budget: true,
            ..ListWaveformStatus::default()
        };
        assert_eq!(
            wave_status_text(&off, false),
            "Pre-generation: off · cached 0 / 0 · cache 0 / 0 MiB (over budget, old shards evicted)"
        );
    }

    #[test]
    fn capabilities_text_names_the_mode_and_whether_this_pc_may_manage() {
        let unknown = capabilities_text(&Capabilities::unknown(), true);
        assert!(unknown.contains("只能查看"), "{unknown}");
        let lan = Capabilities {
            supported: true,
            manage_mode: ManageMode::Lan,
            external: Some(true),
            can_manage: Some(false),
            ..Capabilities::default()
        };
        let text = capabilities_text(&lan, true);
        assert!(text.contains("仅局域网直连（manage_mode = lan）"), "{text}");
        assert!(text.contains("本机不能管理"), "{text}");
        assert!(text.contains("外网请求"), "{text}");
        let all = Capabilities {
            supported: true,
            manage_mode: ManageMode::All,
            ..Capabilities::default()
        };
        assert!(capabilities_text(&all, false).contains("this PC may manage"));
    }

    #[test]
    fn history_lines_follow_the_local_history_format() {
        let scan = ScanSession {
            id: 1,
            start_time: "2026-10-08 12:00:00.089".into(),
            end_time: Some("2026-10-08 12:01:00".into()),
            scan_path: "/Volumes/SFX".into(),
            scan_type: "full".into(),
            state: ScanState::Completed,
            added: 3,
            updated: 1,
            errors: 0,
            ..ScanSession::default()
        };
        assert_eq!(
            history_line(&scan, true),
            "2026-10-08 12:00:00 · 完整 · 已完成 · +3 / ~1 / −0 · 0 个错误\n/Volumes/SFX"
        );
    }

    #[test]
    fn list_status_and_last_test_line_come_from_the_recorded_verdict() {
        assert_eq!(test_status(None, true), ("未测试".to_string(), 0));
        let failed = TestSummary {
            at: 1_700_000_000,
            ok: false,
            summary: "健康检查 · 无法连接".into(),
        };
        let (text, kind) = test_status(Some(&failed), true);
        assert_eq!(kind, 3);
        assert!(text.ends_with("健康检查 · 无法连接"));
        assert!(tested_line(&failed, true, true).contains("× 健康检查"));
        assert!(tested_line(&failed, false, true).ends_with("（地址未保存，不记录）"));
        assert_eq!(last_test_line(None, true), "");
    }

    #[test]
    fn cache_summary_is_one_line() {
        assert!(cache_summary(None, None, true).contains("测试连接或同步后显示"));
        let missing = cache_summary(Some("11111111-5e97"), None, true);
        assert!(
            missing.contains("本机还没有快照") && !missing.contains('\n'),
            "{missing}"
        );
    }

    #[test]
    fn cache_info_explains_unknown_and_missing_snapshots() {
        assert!(cache_info(None, None, None, true).contains("测试连接或同步后显示"));
        let missing = cache_info(
            Some("11111111-5e97"),
            Some("D:/data/remote/11111111-5e97"),
            None,
            true,
        );
        assert!(missing.contains("本机还没有这个音效库的快照"), "{missing}");
        let state = CacheState {
            library_id: "11111111-5e97".into(),
            revision: 6,
            audio_files: 3009,
            snapshot_bytes: 2 * 1_048_576,
            server_url: "http://server.example.lan:8790".into(),
            ..CacheState::default()
        };
        let text = cache_info(Some("11111111-5e97"), Some("D:/x"), Some(&state), true);
        assert!(text.contains("快照：r6 · 3009 个音效 · 2.0 MiB"), "{text}");
    }

    #[test]
    fn switching_to_a_missing_local_library_is_refused_with_a_message() {
        let directory = std::env::temp_dir().join(format!(
            "audiolib-missing-local-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let default_database = directory.join("library.db");
        assert_eq!(
            missing_local_library(&default_database, &default_database, true),
            None
        );
        let other = directory.join("other.db");
        let message = missing_local_library(&other, &default_database, true).unwrap();
        assert!(message.contains("本地素材库不存在"), "{message}");
        let message = missing_local_library(&other, &default_database, false).unwrap();
        assert!(message.contains("does not exist"), "{message}");
        std::fs::write(&other, b"").unwrap();
        assert_eq!(
            missing_local_library(&other, &default_database, false),
            None
        );
    }

    #[test]
    fn the_manager_starts_on_the_local_library() {
        let runtime = RemoteRuntime::default();
        assert_eq!(runtime.manager.selected, Selection::Local);
        assert_eq!(runtime.manager.library_generation, 0);
    }
}
