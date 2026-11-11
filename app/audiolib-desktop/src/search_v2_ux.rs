// app/audiolib-desktop/src/search_v2_ux.rs
//! #209 client UX: the §7.1 local path-index preparation dialog and the §4
//! vendor table import dialog (local or server with manage rights). All library work goes
//! through `DataProvider`; this module only drives the UI and worker threads.
use super::{Controller, post};
use crate::AppWindow;
use audiolib_core::DataProvider;
use audiolib_core::local_upgrade::{self as lu, LocalUpgradeOutcome, LocalUpgradePlan};
use audiolib_core::vendor_import::VendorLimits;
#[path = "vendor_import_ui.rs"]
mod vendor_ui;
use slint::ComponentHandle;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Dialog states shared with `app-window.slint` (`lu-state`).
pub(super) const LU_HIDDEN: i32 = 0;
pub(super) const LU_CONFIRM: i32 = 1;
pub(super) const LU_PROGRESS: i32 = 2;
pub(super) const LU_CANCEL_CONFIRM: i32 = 3;
pub(super) const LU_FAILED: i32 = 4;
pub(super) const LU_NO_SPACE: i32 = 5;

/// `vi-state`: 0 hidden, 1 choose a file, 2 working, 3 preview ready, 4 committed.
const VI_HIDDEN: i32 = 0;
const VI_CHOOSE: i32 = 1;
const VI_BUSY: i32 = 2;
const VI_PREVIEW: i32 = 3;
const VI_COMMITTED: i32 = 4;

#[derive(Default)]
pub(super) struct UxState {
    /// Library already checked this session, so a declined or cancelled upgrade is not
    /// asked again until the next open.
    offered: Option<u64>,
    pub(super) declined: bool,
    cancel: Option<Arc<AtomicBool>>,
    vendor: vendor_ui::VendorUiState,
    migration_request: u64,
}

impl UxState {
    pub(super) fn reset(&mut self) {
        *self = Self {
            migration_request: self.migration_request.wrapping_add(1),
            vendor: self.vendor.next_session(),
            ..Self::default()
        };
    }
}

fn idempotency_key(import_id: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("audiolib-{import_id}-{nanos:x}")
}

pub(super) fn limits_text(limits: VendorLimits, chinese: bool) -> String {
    let size = lu::size_text(limits.max_file_bytes);
    let line = lu::size_text(limits.max_row_text_bytes);
    if chinese {
        format!(
            "支持 CSV / XLS / XLSX（按文件头识别）。上限：单文件 {size}、{} 行、每行 {line}；超限整份拒绝。",
            limits.max_rows
        )
    } else {
        format!(
            "CSV / XLS / XLSX (detected from the file header). Limits: {size} per file, {} rows, {line} per row; an oversized file is rejected whole.",
            limits.max_rows
        )
    }
}

impl Controller {
    pub(super) fn path_probe_provider(
        self: &Arc<Self>,
        request: &audiolib_core::SearchRequest,
    ) -> Option<Arc<dyn DataProvider>> {
        if request.scope != audiolib_core::SearchScope::Path
            || !self.remote_read_only()
            || self.provider().path_search_ready().unwrap_or(false)
        {
            return None;
        }
        self.remote_provider()
            .ok()
            .map(|provider| provider as Arc<dyn DataProvider>)
    }

    pub(super) fn probe_migrating_path(
        self: &Arc<Self>,
        job: &super::SearchJob,
    ) -> audiolib_core::Result<()> {
        let Some(provider) = &job.path_probe else {
            return Ok(());
        };
        if provider.probe_path_search(job.cancel.clone())? {
            return Ok(());
        }
        // The server now accepts path searches. Keep result IDs and Metadata pinned
        // to the current snapshot until the user refreshes it through the normal flow.
        let generation = job.generation;
        let c = self.clone();
        post(self.ui.clone(), move |ui| {
            if c.search_generation.load(Ordering::Acquire) == generation {
                ui.set_remote_notice(
                    super::remote_mode::refresh_notice_text(c.chinese.load(Ordering::Acquire))
                        .into(),
                );
                ui.set_remote_notice_kind(super::remote_mode::NOTICE_UPDATE);
                ui.set_remote_notice_refresh(true);
                c.wake_revision_poller();
            }
        });
        Err(audiolib_core::Error::Busy(
            if self.chinese.load(Ordering::Acquire) {
                "路径索引已发布，点击刷新以更新本地快照。".into()
            } else {
                "The path index is published. Click Refresh to update the local snapshot.".into()
            },
        ))
    }

    pub(super) fn bind_search_v2_ux(self: &Arc<Self>, ui: &AppWindow) {
        let c = self.clone();
        ui.on_migration_requested(move || {
            let Some(ui) = c.ui.upgrade() else { return };
            if !c.remote_read_only() || !c.available() {
                return;
            }
            ui.set_migration_visible(true);
            ui.set_migration_can_manage(false);
            ui.set_migration_state("".into());
            ui.set_migration_text("".into());
            c.server_migration(String::new());
        });
        let c = self.clone();
        ui.on_migration_action(move |action| c.server_migration(action.to_string()));
        let c = self.clone();
        ui.on_lu_accept(move || c.start_local_upgrade(false));
        let c = self.clone();
        ui.on_lu_decline(move || {
            if let Some(ui) = c.ui.upgrade() {
                c.invalidate_library_reads();
                // Keep drafts bound to this provider until the normal source-switch
                // barrier drains them. Hiding a declined library must not discard writes.
                c.records.lock().unwrap().clear();
                ui.set_rows(slint::ModelRc::new(
                    slint::VecModel::<crate::AudioRow>::default(),
                ));
                ui.set_folders(slint::ModelRc::new(
                    slint::VecModel::<crate::FolderRow>::default(),
                ));
                ui.invoke_stop_requested();
                c.search_v2_ux.lock().unwrap().declined = true;
                ui.set_lu_state(6);
                ui.invoke_open_library(0);
            }
        });
        let c = self.clone();
        ui.on_lu_cancel_requested(move || {
            if let Some(ui) = c.ui.upgrade()
                && ui.get_lu_state() == LU_PROGRESS
            {
                let chinese = c.chinese.load(Ordering::Acquire);
                ui.set_lu_question(lu::cancel_confirm_text(chinese).into());
                ui.set_lu_state(LU_CANCEL_CONFIRM);
            }
        });
        let c = self.clone();
        ui.on_lu_cancel_answer(move |cancel| {
            let Some(ui) = c.ui.upgrade() else { return };
            if cancel && let Some(flag) = c.search_v2_ux.lock().unwrap().cancel.as_ref() {
                flag.store(true, Ordering::Release);
            }
            if ui.get_lu_state() == LU_CANCEL_CONFIRM {
                ui.set_lu_state(LU_PROGRESS);
            }
        });
        let c = self.clone();
        ui.on_lu_close(move || {
            if let Some(ui) = c.ui.upgrade() {
                ui.set_lu_state(LU_HIDDEN);
            }
        });

        self.bind_vendor_import(ui);
    }

    /// Poll the shared server report and dispatch only explicit administrator actions.
    /// Responses and delayed retries belong to the captured library/source generation.
    fn server_migration(self: &Arc<Self>, action: String) {
        let Some(ui) = self.ui.upgrade() else { return };
        if !ui.get_migration_visible()
            || ui.get_migration_busy()
            || !self.available()
            || !self.remote_read_only()
        {
            return;
        }
        let provider = match self.remote_provider() {
            Ok(provider) => provider,
            Err(error) => {
                ui.set_migration_text(error.into());
                return;
            }
        };
        let generation = self.library_generation.load(Ordering::Acquire);
        let source = self.source_generation.load(Ordering::Acquire);
        let chinese = self.chinese.load(Ordering::Acquire);
        let request_id = {
            let mut state = self.search_v2_ux.lock().unwrap();
            state.migration_request = state.migration_request.wrapping_add(1);
            state.migration_request
        };
        ui.set_migration_busy(true);
        if !action.is_empty() {
            self.search_v2_busy.store(true, Ordering::Release);
        }
        let c = self.clone();
        std::thread::spawn(move || {
            let client = provider.writer().client();
            let result = client.capabilities().and_then(|capabilities| {
                let manage = capabilities.may_manage();
                let report = if action.is_empty() {
                    client.migration_status()
                } else {
                    client.migration_action(&action, manage)
                }?;
                Ok((manage, report))
            });
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                if !action.is_empty() {
                    c2.search_v2_busy.store(false, Ordering::Release);
                }
                if c2.library_generation.load(Ordering::Acquire) != generation
                    || c2.source_generation.load(Ordering::Acquire) != source
                    || c2.search_v2_ux.lock().unwrap().migration_request != request_id
                {
                    return;
                }
                ui.set_migration_busy(false);
                if c2.close_after_operation.swap(false, Ordering::AcqRel) {
                    c2.request_close(&ui);
                    return;
                }
                let next = match result {
                    Ok((manage, report)) => {
                        ui.set_migration_can_manage(manage);
                        ui.set_migration_state(report.state.clone().into());
                        ui.set_migration_text(migration_text(&report, chinese).into());
                        let fraction = report.progress.as_ref().map_or(0.0, |p| {
                            if p.total == 0 {
                                0.0
                            } else {
                                p.done as f32 / p.total as f32
                            }
                        });
                        ui.set_migration_progress(fraction);
                        if report.state == "done" {
                            c2.wake_revision_poller();
                        }
                        Some((Duration::from_secs(3), String::new()))
                    }
                    Err(error) => {
                        ui.set_migration_text(error.localized(chinese).into());
                        error.migration_refusal().map(|(refusal, delay)| {
                            (Duration::from_secs(refusal.retry_after(delay)), action)
                        })
                    }
                };
                if let Some((delay, action)) = next {
                    let c = Arc::downgrade(&c2);
                    slint::Timer::single_shot(delay, move || {
                        if let Some(c) = c.upgrade()
                            && c.library_generation.load(Ordering::Acquire) == generation
                            && c.source_generation.load(Ordering::Acquire) == source
                            && c.search_v2_ux.lock().unwrap().migration_request == request_id
                        {
                            c.server_migration(action);
                        }
                    });
                }
            });
        });
    }

    pub(super) fn retry_migrating_search(
        self: &Arc<Self>,
        ui: &AppWindow,
        error: &audiolib_core::Error,
        generation: u64,
    ) -> bool {
        let audiolib_core::Error::Migration {
            refusal,
            retry_after,
        } = error
        else {
            return false;
        };
        ui.set_searching(false);
        ui.set_status(refusal.text(self.chinese.load(Ordering::Acquire)).into());
        self.search_page_published(ui, generation, false);
        let library = self.library_generation.load(Ordering::Acquire);
        let c = Arc::downgrade(self);
        slint::Timer::single_shot(Duration::from_secs(*retry_after), move || {
            if let Some(c) = c.upgrade()
                && c.library_generation.load(Ordering::Acquire) == library
                && c.search_generation.load(Ordering::Acquire) == generation
                && let Some(ui) = c.ui.upgrade()
            {
                c.request_search(&ui, true, false);
            }
        });
        true
    }

    /// Ask once per opened local library whether its derived path index is missing.
    /// Preparing this local index does not publish the server's schema-9 format.
    /// The plan is read on a worker; nothing runs while another operation holds the
    /// library.
    pub(super) fn maybe_offer_local_upgrade(self: &Arc<Self>, ui: &AppWindow) {
        if self.remote_read_only()
            || !self.available()
            || ui.get_lu_state() != LU_HIDDEN
            || self.scan_cancel.lock().unwrap().is_some()
        {
            return;
        }
        let provider = self.provider();
        let generation = self.library_generation.load(Ordering::Acquire);
        {
            let mut state = self.search_v2_ux.lock().unwrap();
            if state.declined || state.offered == Some(generation) {
                return;
            }
            state.offered = Some(generation);
        }
        let c = self.clone();
        std::thread::spawn(move || {
            let plan = (provider.as_ref() as &dyn DataProvider).local_upgrade_plan();
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                if c2.library_generation.load(Ordering::Acquire) != generation || !c2.available() {
                    return;
                }
                match plan {
                    Ok(Some(plan)) => c2.show_local_upgrade(&ui, plan),
                    Ok(None) => c2.refresh_path_search_availability(&ui),
                    Err(error) => tracing::warn!(%error, "Read local upgrade plan"),
                }
            });
        });
    }

    fn show_local_upgrade(self: &Arc<Self>, ui: &AppWindow, plan: LocalUpgradePlan) {
        if self.remote_read_only() {
            return;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        ui.set_lu_title(lu::confirm_title(chinese).into());
        let (accept, decline) = lu::confirm_buttons(chinese);
        ui.set_lu_accept_label(accept.into());
        ui.set_lu_decline_label(decline.into());
        ui.set_lu_copy_label(lu::copy_diagnostics_label(chinese).into());
        if plan.insufficient_space() {
            ui.set_lu_body(lu::insufficient_space_text(&plan, chinese).into());
            ui.set_lu_state(LU_NO_SPACE);
        } else if plan.resuming {
            // §6: a resume neither asks again nor backs up again.
            ui.set_lu_body(lu::resume_text(chinese).into());
            self.start_local_upgrade(true);
        } else {
            ui.set_lu_body(lu::confirm_body(&plan, chinese).into());
            ui.set_lu_state(LU_CONFIRM);
        }
    }

    fn start_local_upgrade(self: &Arc<Self>, resuming: bool) {
        let Some(ui) = self.ui.upgrade() else { return };
        if self.remote_read_only()
            || !self.available()
            || self.search_v2_busy.swap(true, Ordering::AcqRel)
        {
            return;
        }
        self.capture_metadata(&ui);
        let barrier = match self.metadata.lock().unwrap().enqueue_all() {
            Ok(barrier) => barrier,
            Err(error) => {
                self.search_v2_busy.store(false, Ordering::Release);
                ui.set_lu_body(error.into());
                return;
            }
        };
        self.metadata_wake.notify_all();
        let chinese = self.chinese.load(Ordering::Acquire);
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut state = self.search_v2_ux.lock().unwrap();
            state.cancel = Some(cancel.clone());
            state.declined = false;
        }
        ui.set_lu_progress(0.0);
        ui.set_lu_progress_text(if resuming {
            lu::resume_text(chinese).into()
        } else {
            "".into()
        });
        ui.set_lu_state(LU_PROGRESS);
        let provider = self.provider();
        let c = self.clone();
        std::thread::spawn(move || {
            let weak = c.ui.clone();
            let mut last: Option<Instant> = None;
            let mut report = |progress: lu::LocalUpgradeProgress| {
                let finished = progress.completed >= progress.total;
                if !finished && last.is_some_and(|at| at.elapsed() < Duration::from_millis(100)) {
                    return;
                }
                last = Some(Instant::now());
                let text = lu::progress_text(&progress, chinese);
                let fraction = f32::from(progress.percent()) / 100.0;
                post(weak.clone(), move |ui| {
                    ui.set_lu_progress_text(text.into());
                    ui.set_lu_progress(fraction);
                });
            };
            let result = c
                .wait_metadata(barrier, true)
                .map_err(audiolib_core::Error::Database)
                .and_then(|_| {
                    (provider.as_ref() as &dyn DataProvider).run_local_upgrade(&cancel, &mut report)
                });
            {
                let mut state = c.search_v2_ux.lock().unwrap();
                state.cancel = None;
            }
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                c2.search_v2_busy.store(false, Ordering::Release);
                c2.finish_local_upgrade(&ui, result);
                if c2.close_after_operation.swap(false, Ordering::AcqRel) {
                    c2.request_close(&ui);
                }
            });
        });
    }

    fn finish_local_upgrade(
        self: &Arc<Self>,
        ui: &AppWindow,
        result: audiolib_core::Result<LocalUpgradeOutcome>,
    ) {
        let chinese = self.chinese.load(Ordering::Acquire);
        match result {
            Ok(LocalUpgradeOutcome::Done { .. }) => {
                ui.set_lu_state(LU_HIDDEN);
                self.refresh_path_search_availability(ui);
                self.resume_search(ui, Some(lu::done_text(chinese).into()));
            }
            Ok(LocalUpgradeOutcome::Cancelled) => {
                ui.set_lu_state(LU_HIDDEN);
                self.resume_search(ui, Some(lu::cancelled_text(chinese).into()));
            }
            Err(error) => {
                let backup = self.provider().database_path().to_path_buf();
                let backup = latest_backup(&backup);
                ui.set_lu_body(lu::failed_text(backup.as_deref(), chinese).into());
                ui.set_lu_diagnostics(
                    format!(
                        "AudioLib Explorer {}\nLocal path-index preparation failed\n{error}",
                        env!("CARGO_PKG_VERSION")
                    )
                    .into(),
                );
                ui.set_lu_state(LU_FAILED);
            }
        }
    }
}

fn migration_text(
    report: &audiolib_core::server_upgrade::MigrationStatus,
    chinese: bool,
) -> String {
    use audiolib_core::server_upgrade::{migration_error_text, migration_state_text};
    let mut text = migration_state_text(&report.state, chinese);
    if let Some(progress) = &report.progress {
        text.push_str(&format!(" · {} / {}", progress.done, progress.total));
    }
    if let Some(eta) = report.eta_s.filter(|eta| eta.is_finite() && *eta >= 0.0) {
        text.push_str(&format!(" · ETA {eta:.0}s"));
    }
    if let Some(code) = &report.error_code {
        text.push('\n');
        text.push_str(&migration_error_text(code, chinese));
    }
    text
}

/// Newest `*.pre-schema9-*.db` next to the library, for the failure text.
fn latest_backup(database: &std::path::Path) -> Option<PathBuf> {
    let prefix = format!("{}.pre-schema9-", database.file_stem()?.to_str()?);
    std::fs::read_dir(database.parent()?)
        .ok()?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".db"))
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_selection_preserves_directory_metadata_sort_and_cursor() {
        use audiolib_core::{SearchFilters, SearchRequest, SearchScope, SortOrder};
        i_slint_backend_testing::init_no_event_loop();
        let root = tempfile::tempdir().unwrap();
        let config =
            audiolib_core::config::AppConfig::load_in(root.path().join("preferences")).unwrap();
        let provider = Arc::new(audiolib_db::SqliteProvider::open(&config.paths.database).unwrap());
        provider.rebuild_directory_search().unwrap();
        let engine = audiolib_audio::AudioEngine::new(Default::default()).unwrap();
        let ui = AppWindow::new().unwrap();
        let c = Controller::new(&ui, provider, engine.handle(), config, Default::default());
        let requests = [
            SearchRequest {
                folder_id: Some(4),
                ..Default::default()
            },
            SearchRequest {
                order_by: SortOrder::Filename,
                ..Default::default()
            },
            SearchRequest {
                filters: SearchFilters {
                    album_id: Some(7),
                    favorites_only: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        ];
        for mut request in requests {
            request.query = "wind".into();
            for scope in [SearchScope::Path, SearchScope::All, SearchScope::Filename] {
                request.scope = scope;
                request.cursor = Some("current-page".into());
                let wanted = serde_json::to_value(&request).unwrap();
                ui.set_search_scope_index(super::super::search_scope_index(scope));
                c.apply_search_scope(&ui, &mut request);
                assert_eq!(serde_json::to_value(&request).unwrap(), wanted);
            }
        }
        c.shutdown();
    }

    #[test]
    fn controller_instances_keep_declines_and_cancel_flags_independent() {
        i_slint_backend_testing::init_no_event_loop();
        let root = tempfile::tempdir().unwrap();
        let engine = audiolib_audio::AudioEngine::new(Default::default()).unwrap();
        let make = |name: &str| {
            let config = audiolib_core::config::AppConfig::load_in(root.path().join(name)).unwrap();
            let provider =
                Arc::new(audiolib_db::SqliteProvider::open(&config.paths.database).unwrap());
            let ui = AppWindow::new().unwrap();
            let c = Controller::new(&ui, provider, engine.handle(), config, Default::default());
            (ui, c)
        };
        let (first, a) = make("first");
        let (_second, b) = make("second");
        first.invoke_lu_decline();
        assert!(a.search_v2_ux.lock().unwrap().declined);
        assert!(!b.search_v2_ux.lock().unwrap().declined);
        assert!(a.records.lock().unwrap().is_empty());
        let before = a.search_generation.load(Ordering::Acquire);
        a.request_search(&first, true, true);
        assert_eq!(a.search_generation.load(Ordering::Acquire), before);
        let flag = Arc::new(AtomicBool::new(false));
        a.search_v2_ux.lock().unwrap().cancel = Some(flag.clone());
        first.set_lu_state(LU_CANCEL_CONFIRM);
        first.invoke_lu_cancel_answer(false);
        assert!(!flag.load(Ordering::Acquire));
        first.set_lu_state(LU_CANCEL_CONFIRM);
        first.invoke_lu_cancel_answer(true);
        assert!(flag.load(Ordering::Acquire));
        assert!(b.search_v2_ux.lock().unwrap().cancel.is_none());
        a.reset_library_view(&first, true, false);
        assert!(!a.search_v2_ux.lock().unwrap().declined);
        assert_eq!(first.get_lu_state(), LU_HIDDEN);
        let before = a.search_generation.load(Ordering::Acquire);
        a.finish_local_upgrade(&first, Ok(LocalUpgradeOutcome::Done { backup: None }));
        assert!(a.search_generation.load(Ordering::Acquire) > before);
        a.search_v2_busy.store(true, Ordering::Release);
        a.request_close(&first);
        assert!(a.close_after_operation.load(Ordering::Acquire));
        assert!(!a.closing.load(Ordering::Acquire));
        a.search_v2_busy.store(false, Ordering::Release);
        a.shutdown();
        b.shutdown();
    }

    #[test]
    fn migration_search_retry_waits_and_discards_superseded_requests() {
        i_slint_backend_testing::init_no_event_loop();
        let root = tempfile::tempdir().unwrap();
        let config =
            audiolib_core::config::AppConfig::load_in(root.path().join("preferences")).unwrap();
        let provider = Arc::new(audiolib_db::SqliteProvider::open(&config.paths.database).unwrap());
        let engine = audiolib_audio::AudioEngine::new(Default::default()).unwrap();
        let ui = AppWindow::new().unwrap();
        let c = Controller::new(&ui, provider, engine.handle(), config, Default::default());
        let error = audiolib_core::Error::Migration {
            refusal: audiolib_core::server_upgrade::MigrationRefusal::PathSearchInProgress,
            retry_after: 30,
        };
        let generation = c.search_generation.load(Ordering::Acquire);
        assert!(c.retry_migrating_search(&ui, &error, generation));
        assert!(!ui.get_searching());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(29));
        assert_eq!(c.search_generation.load(Ordering::Acquire), generation);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(1));
        assert!(c.search_generation.load(Ordering::Acquire) > generation);
        let generation = c.search_generation.load(Ordering::Acquire);
        c.retry_migrating_search(&ui, &error, generation);
        c.search_generation.fetch_add(1, Ordering::AcqRel);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(30));
        assert_eq!(c.search_generation.load(Ordering::Acquire), generation + 1);
        c.shutdown();
    }

    #[test]
    fn unadvertised_vendor_import_cannot_open_a_modal_through_a_stale_callback() {
        i_slint_backend_testing::init_no_event_loop();
        let root = tempfile::tempdir().unwrap();
        let config =
            audiolib_core::config::AppConfig::load_in(root.path().join("preferences")).unwrap();
        let provider = Arc::new(audiolib_db::SqliteProvider::open(&config.paths.database).unwrap());
        let engine = audiolib_audio::AudioEngine::new(Default::default()).unwrap();
        let ui = AppWindow::new().unwrap();
        let c = Controller::new(&ui, provider, engine.handle(), config, Default::default());
        // A stale menu/property from the previous server cannot grant capability
        // after switching to an unsupported server library.
        c.remote_mode.store(true, Ordering::Release);
        ui.set_remote_mode(true);
        ui.set_vendor_import_available(true);
        ui.invoke_vendor_import_requested();
        assert_eq!(ui.get_vi_state(), VI_HIDDEN);
        assert!(!ui.get_vi_available());
        assert!(!c.search_v2_busy.load(Ordering::Acquire));
        assert!(c.search_v2_ux.lock().unwrap().vendor.pending.is_none());
        c.shutdown();
    }

    #[test]
    fn limits_text_names_all_three_limits() {
        let text = limits_text(VendorLimits::default(), true);
        assert!(text.contains("50.0 MB"), "{text}");
        assert!(text.contains("200000"), "{text}");
        assert!(text.contains("256.0 KB"), "{text}");
    }

    #[test]
    fn idempotency_key_is_stable_per_call_and_names_the_batch() {
        let key = idempotency_key("abc");
        assert!(key.starts_with("audiolib-abc-"));
    }

    #[test]
    fn latest_backup_picks_the_newest_pre_schema9_copy() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["lib.pre-schema9-100.db", "lib.pre-schema9-200.db", "lib.db"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        std::fs::write(dir.path().join("other.pre-schema9-999.db"), b"x").unwrap();
        let found = latest_backup(&dir.path().join("lib.db")).unwrap();
        assert!(found.ends_with("lib.pre-schema9-200.db"));
    }
}
