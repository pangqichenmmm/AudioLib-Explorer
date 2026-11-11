// app/audiolib-desktop/src/remote_sources.rs
//! Saving and switching the data source (design `server-mgmt-gui.md` A0/A2): the
//! unified window (`source_manager`) edits profiles and switches through
//! [`Controller::activate_data_source`]; both paths save settings and secrets together
//! and reset the per-server runtime state. Tokens never reach logs.
use super::{Controller, DataSourceSwitch, LibraryOperation, post};
use crate::AppWindow;
use audiolib_core::config::{ActiveSource, DataSourceMode, DataSourceSettings};
use audiolib_remote::{RemoteSecrets, activate_source, save_secrets};
use std::sync::Arc;
use std::sync::atomic::Ordering;

impl Controller {
    /// Cancel a pending server sync before opening the requested local library.
    /// The library operation then owns the existing Metadata drain and rollback.
    pub(in crate::controller) fn begin_local_library_change(
        &self,
        ui: &AppWindow,
    ) -> Result<(), String> {
        self.begin_source_change_guarded(ui, true, true).map(|_| ())
    }

    /// Freeze new GUI writes before draining the old source; its connection stays
    /// published until requested annotations persist or their replaced-library
    /// drafts are retained in a durable local recovery file.
    pub(super) fn begin_source_change(&self, ui: &AppWindow) -> Result<u64, String> {
        self.begin_source_change_guarded(ui, true, false)
    }

    /// Preference-only saves keep the ordinary successful-write barrier.
    pub(super) fn begin_source_change_for_settings(&self, ui: &AppWindow) -> Result<u64, String> {
        self.begin_source_change_guarded(ui, false, false)
    }

    /// A saved server selection can be abandoned while its sync is pending, before
    /// any provider-open operation starts. Settings writes and rollback stay gated.
    fn can_interrupt_source_sync(&self, ui: &AppWindow) -> bool {
        ui.get_ds_source_syncing()
            && self.source_changing.load(Ordering::Acquire)
            && !self.library_busy.load(Ordering::Acquire)
            && !self.closing.load(Ordering::Acquire)
            && !self.shutdown.load(Ordering::Acquire)
    }

    fn begin_source_change_guarded(
        &self,
        ui: &AppWindow,
        recover_replaced_library: bool,
        local: bool,
    ) -> Result<u64, String> {
        let interrupt_sync = local && self.can_interrupt_source_sync(ui);
        if !self.available() && !interrupt_sync {
            return Err("The library is busy".into());
        }
        let barrier = if interrupt_sync {
            // The preceding transition already drained these drafts using the old
            // source. Do not recapture them using the pending server's credentials.
            self.metadata.lock().unwrap().sequence
        } else {
            self.capture_metadata(ui);
            let barrier = if recover_replaced_library {
                self.enqueue_transition_metadata()?
            } else {
                self.metadata.lock().unwrap().enqueue_all()?
            };
            let old_settings = self.config.lock().unwrap().data_source.clone();
            let old_secrets = self.secrets();
            self.remote.lock().unwrap().source_rollback = Some((old_settings, old_secrets));
            barrier
        };
        {
            let _plays = self.remote_plays.lock().unwrap();
            self.source_changing.store(true, Ordering::Release);
            self.source_generation.fetch_add(1, Ordering::AcqRel);
        }
        self.drag_cancel
            .lock()
            .unwrap()
            .store(true, Ordering::Release);
        self.cancel_remote();
        // The cancelled worker's generation is stale, so it cannot clear its UI.
        // Its busy guard still owns cache work until that worker actually drains.
        ui.set_ds_source_syncing(false);
        ui.set_ds_syncing(false);
        ui.set_ds_sync_pausing(false);
        ui.set_ds_sync_can_pause(false);
        ui.set_ds_sync_paused(false);
        ui.set_ds_sync_pending(false);
        ui.set_ds_progress(0.0);
        ui.set_ds_decompression_info("".into());
        ui.set_ds_decompression_progress(-1.0);
        ui.set_library_busy(true);
        self.invalidate_library_reads();
        self.remote_audio_cancel
            .lock()
            .unwrap()
            .store(true, Ordering::Release);
        self.metadata_wake.notify_all();
        Ok(barrier)
    }

    /// Successful swaps release the gate. Failed connection changes restore the old
    /// settings off the event loop, keeping the old provider and drafts usable.
    pub(in crate::controller) fn finish_source_change(
        self: &Arc<Self>,
        ui: &AppWindow,
        committed: bool,
    ) {
        ui.set_ds_source_syncing(false);
        let rollback = self.remote.lock().unwrap().source_rollback.take();
        if !self.source_changing.load(Ordering::Acquire) {
            return;
        }
        self.finish_source_manager_sync(ui, committed);
        if !committed && let Some((settings, secrets)) = rollback {
            let c = self.clone();
            std::thread::spawn(move || {
                let result = c.wait_remote_plays().and_then(|_| {
                    // Restore credentials before draining cache work, so a close
                    // requested during this wait can safely use the old provider.
                    let settings_result = c.save_data_source(
                        settings.clone(),
                        secrets.clone(),
                        c.chinese.load(Ordering::Acquire),
                        false,
                    );
                    // A failed local escape can roll back while the cancelled sync
                    // is still publishing its cache. Restore the original state only
                    // after that worker's owned writes finish; successful escapes
                    // never wait for this cache drain.
                    while c.remote_busy.load(Ordering::Acquire) {
                        if c.shutdown.load(Ordering::Acquire) {
                            return Err("Desktop is shutting down".into());
                        }
                        if c.close_after_operation.load(Ordering::Acquire) {
                            return Err(super::cn_or(
                                c.chinese.load(Ordering::Acquire),
                                "已请求关闭；将按正常关闭流程等待已取消的服务器缓存任务。",
                                "Close requested; continuing shutdown while cancelled server cache work drains.",
                            ));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    // An old play may have advanced this state while the failed
                    // operation drained. Capture and restore it only afterwards.
                    let previous_state = c.remote.lock().unwrap().state.clone();
                    // Sync publishes cache state before opening the provider.
                    // Restore the old cache so restart cannot open B with A's URL.
                    let cache_result = previous_state.as_ref().map_or(Ok(()), |state| {
                        audiolib_remote::RemoteCache::new(&c.data_root())
                            .save_state(state)
                            .map_err(|error| error.localized(c.chinese.load(Ordering::Acquire)))
                    });
                    settings_result.and(cache_result)
                });
                if result.is_err() {
                    // Disk failure must not leave the old snapshot using new credentials.
                    c.config.lock().unwrap().data_source = settings;
                    let mut remote = c.remote.lock().unwrap();
                    remote.secrets = secrets;
                    remote.secrets_loaded = true;
                }
                let c2 = c.clone();
                post(c.ui.clone(), move |ui| {
                    c2.source_changing.store(false, Ordering::Release);
                    let close_requested = c2.close_after_operation.swap(false, Ordering::AcqRel);
                    ui.set_library_busy(false);
                    ui.set_ds_applying(false);
                    if let Err(error) = result {
                        ui.set_ds_message(error.into());
                    }
                    c2.publish_data_source(&ui);
                    c2.sync_editor(&ui);
                    c2.resume_source_reads(&ui, false);
                    if close_requested {
                        c2.request_close(&ui);
                    }
                });
            });
            return;
        }
        self.source_changing.store(false, Ordering::Release);
        ui.set_library_busy(false);
        ui.set_ds_applying(false);
    }

    /// The provider stayed open, but the gate cancelled its readers. Re-publish the
    /// current query and Waveform; successful saves may resume an unfinished selection.
    pub(super) fn resume_source_reads(self: &Arc<Self>, ui: &AppWindow, resume_selection: bool) {
        if !self.available() {
            return;
        }
        self.refresh_library_info();
        self.reload_tree();
        self.request_search(ui, true, false);
        self.resume_source_selection(resume_selection);
    }

    /// Save data source settings and secrets together and reset the server-mode state
    /// that belonged to the previous source. Runs off the UI thread (DPAPI, disk).
    pub(super) fn save_data_source(
        &self,
        settings: DataSourceSettings,
        secrets: RemoteSecrets,
        chinese: bool,
        explicit_choice: bool,
    ) -> Result<(), String> {
        let mut config = self.config.lock().unwrap();
        let mut preferences = config.clone();
        preferences.data_source = settings;
        if explicit_choice {
            // The user chose a source in this session: write it exactly as chosen, even
            // when it equals the selection a startup fallback replaced.
            preferences.data_source_fallback = None;
        }
        save_secrets(&preferences.data_root(), &secrets)
            .map_err(|error| error.localized(chinese))?;
        preferences.save().map_err(|error| error.to_string())?;
        super::apply_audition_memory(&preferences.data_source);
        *config = preferences;
        drop(config);
        let mut remote = self.remote.lock().unwrap();
        remote.secrets = secrets;
        remote.secrets_loaded = true;
        remote.last_error = None;
        remote.check = None;
        remote.dismissed_notice = None;
        remote.pending_sync = None;
        // Root status came from the old server or token; the poller refetches it.
        remote.live_roots = None;
        // Preview capabilities belong to the server.
        remote.preview_refused.clear();
        remote.flac_unavailable = false;
        drop(remote);
        // The throughput was measured against the previous server.
        audiolib_remote::bandwidth().reset();
        // New settings (interval, token, URL) take effect for the poller now.
        self.wake_revision_poller();
        Ok(())
    }

    /// Make `source` the active data source (「切换到此数据源」): save settings and secrets
    /// (old fields mirrored for older clients), then switch — local through
    /// `LibraryOperation::DataSource(Local)`, a server profile through a sync that swaps
    /// to its snapshot; a failure restores the previous connection and library.
    pub(super) fn activate_data_source(self: &Arc<Self>, source: ActiveSource) {
        let Some(ui) = self.ui.upgrade() else { return };
        let local = source == ActiveSource::Local;
        if !self.available() && !(local && self.can_interrupt_source_sync(&ui)) {
            return;
        }
        let chinese = self.chinese.load(Ordering::Acquire);
        let mut secrets = self.secrets();
        let mut settings = self.config.lock().unwrap().data_source.clone();
        if let Err(error) = activate_source(&mut settings, &mut secrets, &source) {
            if let Some(ui) = self.ui.upgrade() {
                ui.set_ds_message(error.to_string().into());
            }
            return;
        }
        let barrier = match self.begin_source_change_guarded(&ui, true, local) {
            Ok(barrier) => barrier,
            Err(error) => {
                ui.set_ds_message(error.into());
                return;
            }
        };
        if let Some(ui) = self.ui.upgrade() {
            ui.set_ds_applying(true);
        }
        // Like a dialog apply: supersede running tests and syncs.
        self.remote_generation.fetch_add(1, Ordering::AcqRel);
        self.remote_cancel.lock().unwrap().cancel();
        let mode = settings.mode;
        let c = self.clone();
        std::thread::spawn(move || {
            let saved = c
                .wait_transition_metadata(barrier)
                .and_then(|_| c.wait_remote_plays())
                .and_then(|_| c.save_data_source(settings, secrets, chinese, true));
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                if let Err(error) = saved {
                    ui.set_ds_message(error.clone().into());
                    c2.remote.lock().unwrap().last_error = Some(error);
                    c2.publish_data_source(&ui);
                    c2.finish_source_change(&ui, false);
                    c2.publish_metadata_recovery_notice(&ui);
                    return;
                }
                if ui.get_library_visible() {
                    ui.set_ds_message("".into());
                    c2.publish_sources(&ui);
                }
                c2.publish_metadata_recovery_notice(&ui);
                match mode {
                    DataSourceMode::Local => {
                        if c2.remote_read_only() {
                            c2.run_library_operation(LibraryOperation::DataSource(
                                DataSourceSwitch::Local,
                            ));
                        } else {
                            c2.finish_source_change(&ui, true);
                            c2.resume_source_reads(&ui, true);
                            if c2.close_after_operation.swap(false, Ordering::AcqRel) {
                                c2.request_close(&ui);
                            }
                        }
                        c2.publish_data_source(&ui);
                    }
                    DataSourceMode::Server => c2.start_sync_requested(false, true, false),
                }
            });
        });
    }
}
