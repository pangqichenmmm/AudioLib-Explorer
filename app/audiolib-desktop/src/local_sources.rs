// app/audiolib-desktop/src/local_sources.rs
//! Local source registration and validated provider swaps. Removing a source only
//! removes its saved registration; database files and source WAVs remain untouched.
use super::{Controller, LibraryOperation, Selection, cn_or, local_selection, post};
use crate::AppWindow;
use crate::controller::library_progress::LibraryProgress;
use audiolib_core::config::{ActiveSource, DEFAULT_LOCAL_LIBRARY_ID, LocalLibrary};
use slint::ComponentHandle;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

static NEXT_LOCAL_DATABASE: AtomicU64 = AtomicU64::new(1);

/// A local source swap executed behind the ordinary library operation write gate.
#[derive(Clone, Debug)]
pub(in crate::controller) enum LocalSourceRequest {
    /// Register a verified database, or create an owned database for a sound folder.
    Register {
        name: String,
        description: String,
        path: PathBuf,
        existing: bool,
        pregenerate: bool,
    },
    /// Open a registered local library after pending writes and scans drain.
    Switch { id: String },
}

impl Controller {
    pub(super) fn bind_local_sources(self: &Arc<Self>, ui: &AppWindow) {
        let c = self.clone();
        ui.on_sm_local_add_requested(move || c.open_local_form(false));
        let c = self.clone();
        ui.on_sm_local_edit_requested(move || c.open_local_form(true));
        let c = self.clone();
        ui.on_sm_local_browse_requested(move || c.browse_local_source());
        let c = self.clone();
        ui.on_sm_local_save_requested(move || c.save_local_form());
    }

    pub(super) fn selected_local_id(&self) -> Option<String> {
        match self.selection() {
            Selection::Local => Some(DEFAULT_LOCAL_LIBRARY_ID.into()),
            Selection::LocalLibrary(id) => Some(id),
            _ => None,
        }
    }

    /// Fill saved summaries without replacing an in-progress editor draft.
    pub(super) fn publish_local_selection(&self, ui: &AppWindow) {
        let selected = self.selected_local_id();
        let config = self.config.lock().unwrap();
        let library = selected.as_deref().and_then(|id| config.local_library(id));
        ui.set_sm_local_default(selected.as_deref() == Some(DEFAULT_LOCAL_LIBRARY_ID));
        ui.set_sm_local_description(
            library
                .map(|library| library.description.clone())
                .unwrap_or_default()
                .into(),
        );
        ui.set_sm_local_path(
            library
                .map(|library| library.database.display().to_string())
                .unwrap_or_else(|| config.paths.database.display().to_string())
                .into(),
        );
    }

    fn open_local_form(self: &Arc<Self>, editing: bool) {
        let pending_sync_form = !editing
            && self.source_changing.load(Ordering::Acquire)
            && !self.library_busy.load(Ordering::Acquire)
            && !self.closing.load(Ordering::Acquire)
            && !self.shutdown.load(Ordering::Acquire);
        if !self.available() && !pending_sync_form {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        let config = self.config.lock().unwrap();
        let library = editing
            .then(|| self.selected_local_id())
            .flatten()
            .and_then(|id| config.local_library(&id).cloned());
        if editing && library.is_none() {
            return;
        }
        ui.set_sm_local_name(
            library
                .as_ref()
                .map(|library| library.name.clone())
                .unwrap_or_default()
                .into(),
        );
        ui.set_sm_local_description_draft(
            library
                .as_ref()
                .map(|library| library.description.clone())
                .unwrap_or_default()
                .into(),
        );
        ui.set_sm_local_path_draft(
            library
                .as_ref()
                .map(|library| library.database.display().to_string())
                .unwrap_or_default()
                .into(),
        );
        ui.set_sm_local_pregenerate(library.as_ref().is_none_or(|library| {
            if config.data_source.active_local_library.as_deref() == Some(library.id.as_str()) {
                self.pregenerate_list_waveforms.load(Ordering::Acquire)
            } else {
                library.pregenerate_list_waveforms
            }
        }));
        ui.set_sm_local_existing(false);
        ui.set_sm_local_editing(editing);
        ui.set_sm_local_message("".into());
        ui.set_sm_local_form(true);
    }

    fn browse_local_source(self: &Arc<Self>) {
        if !self.available() {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        if !ui.get_sm_local_form() || ui.get_sm_local_editing() || ui.get_ds_applying() {
            return;
        }
        let existing = ui.get_sm_local_existing();
        let owner = crate::dialogs::owner_handle(ui.window());
        let chinese = self.chinese.load(Ordering::Acquire);
        let generation = self.remote.lock().unwrap().manager.form_generation;
        ui.set_ds_applying(true);
        let c = self.clone();
        std::thread::spawn(move || {
            let result = if existing {
                crate::dialogs::choose(false, crate::dialogs::FileKind::Database, chinese, owner)
            } else {
                crate::dialogs::choose_folder(chinese, owner)
            };
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                ui.set_ds_applying(false);
                if !c2.form_current(generation) || !ui.get_sm_local_form() {
                    return;
                }
                match result {
                    Ok(Some(path)) => ui.set_sm_local_path_draft(path.display().to_string().into()),
                    Ok(None) => {}
                    Err(error) => ui.set_sm_local_message(error.into()),
                }
                if c2.close_after_operation.swap(false, Ordering::AcqRel) {
                    c2.request_close(&ui);
                }
            });
        });
    }

    fn save_local_form(self: &Arc<Self>) {
        if !self.available() {
            return;
        }
        let Some(ui) = self.ui.upgrade() else { return };
        if !ui.get_sm_local_form() || ui.get_ds_applying() {
            return;
        }
        let name = ui.get_sm_local_name().trim().to_owned();
        let description = ui.get_sm_local_description_draft().trim().to_owned();
        let pregenerate = ui.get_sm_local_pregenerate();
        if ui.get_sm_local_editing() {
            let Some(id) = self.selected_local_id() else {
                return;
            };
            self.update_local_preferences(
                &ui,
                move |config| config.update_local_library(&id, &name, &description, pregenerate),
                move |controller, ui| {
                    controller.publish_local_selection(ui);
                    ui.set_sm_local_form(false);
                },
            );
        } else {
            let path = PathBuf::from(ui.get_sm_local_path_draft().trim());
            self.run_library_operation(LibraryOperation::LocalSource(Box::new(
                LocalSourceRequest::Register {
                    name,
                    description,
                    path,
                    existing: ui.get_sm_local_existing(),
                    pregenerate,
                },
            )));
        }
    }

    /// Preference-only edits serialize with other settings saves and never touch a library.
    fn update_local_preferences(
        self: &Arc<Self>,
        ui: &AppWindow,
        update: impl FnOnce(&mut audiolib_core::config::AppConfig) -> audiolib_core::Result<()>
        + Send
        + 'static,
        complete: impl FnOnce(&Arc<Self>, &AppWindow) + Send + 'static,
    ) {
        if !self.available()
            || ui.get_ds_applying()
            || self.library_busy.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let generation = self.remote.lock().unwrap().manager.form_generation;
        ui.set_library_busy(true);
        ui.set_ds_applying(true);
        ui.set_sm_local_message("".into());
        let c = self.clone();
        std::thread::spawn(move || {
            let result = (|| {
                let mut config = c.config.lock().unwrap();
                if c.shutdown.load(Ordering::Acquire) {
                    return Err(audiolib_core::Error::Cancelled);
                }
                let mut next = config.clone();
                c.mirror_local_waveform_preference(&mut next);
                next.reconcile_local_libraries()?;
                update(&mut next)?;
                next.save()?;
                *config = next;
                let enabled = config.pregenerate_list_waveforms;
                c.pregenerate_list_waveforms
                    .store(enabled, Ordering::Release);
                c.list_waveforms.set_pregenerate(enabled);
                Ok::<_, audiolib_core::Error>(())
            })();
            let c2 = c.clone();
            post(c.ui.clone(), move |ui| {
                c2.library_busy.store(false, Ordering::Release);
                ui.set_library_busy(false);
                ui.set_ds_applying(false);
                let succeeded = result.is_ok();
                match result {
                    Ok(()) => {
                        c2.publish_sources(&ui);
                        ui.set_pregenerate_list_waveforms(
                            c2.pregenerate_list_waveforms.load(Ordering::Acquire),
                        );
                        if c2.form_current(generation) {
                            complete(&c2, &ui);
                        }
                    }
                    Err(error) => {
                        ui.set_sm_local_message(error.to_string().into());
                        ui.set_ds_message(error.to_string().into());
                    }
                }
                if c2.close_after_operation.swap(false, Ordering::AcqRel) && succeeded {
                    c2.request_close(&ui);
                }
            });
        });
    }

    pub(super) fn delete_local_selected(self: &Arc<Self>, ui: &AppWindow) {
        if !self.available() || ui.get_ds_applying() {
            return;
        }
        let Some(id) = self.selected_local_id() else {
            return;
        };
        let chinese = self.chinese.load(Ordering::Acquire);
        self.update_local_preferences(ui, move |config| config.remove_local_library(&id), move |controller, ui| {
            controller.set_selection(Selection::Local);
            controller.reload_selection();
            ui.set_sm_confirm_delete(false);
            ui.set_ds_message(cn_or(chinese,
                "已移除本地音效库配置；数据库和源 WAV 文件保留。",
                "Local library removed from sources; its database and WAV files are retained.").into());
        });
    }

    #[cfg(test)]
    fn execute_local_source(&self, request: &LocalSourceRequest) -> Result<String, String> {
        self.execute_local_source_with_progress(request, &mut LibraryProgress::new(self))
    }

    /// Worker-side open after pending writes/scans drain. Preferences are committed
    /// before the provider; failures retain the current source.
    pub(in crate::controller) fn execute_local_source_with_progress(
        &self,
        request: &LocalSourceRequest,
        progress: &mut LibraryProgress,
    ) -> Result<String, String> {
        let chinese = self.chinese.load(Ordering::Acquire);
        let configuration = {
            let mut config = self.config.lock().unwrap();
            self.mirror_local_waveform_preference(&mut config);
            let mut snapshot = config.clone();
            snapshot
                .reconcile_local_libraries()
                .map_err(|error| error.to_string())?;
            snapshot
        };
        let (path, create, registration) = match request {
            LocalSourceRequest::Register {
                name,
                description,
                path,
                existing,
                pregenerate,
            } => {
                let source = if *existing {
                    path.canonicalize().map_err(|error| error.to_string())?
                } else {
                    audiolib_core::validate_scan_root(path, &configuration.allowed_scan_roots)
                        .map_err(|error| error.to_string())?
                };
                let database = if *existing {
                    source
                } else {
                    new_database_path(&configuration.data_root())?
                };
                LocalLibrary {
                    id: "pending".into(),
                    name: name.clone(),
                    description: description.clone(),
                    database: database.clone(),
                    pregenerate_list_waveforms: *pregenerate,
                }
                .validate()
                .map_err(|error| error.to_string())?;
                (
                    database,
                    !*existing,
                    Some((name.as_str(), description.as_str(), *pregenerate)),
                )
            }
            LocalSourceRequest::Switch { id } => {
                let library = configuration.local_library(id).ok_or_else(|| {
                    cn_or(
                        chinese,
                        "本地音效库不存在。",
                        "Local library is not registered.",
                    )
                })?;
                (
                    library.database.clone(),
                    id == DEFAULT_LOCAL_LIBRARY_ID
                        && !library.database.exists()
                        && crate::controller::same_path(
                            &library.database,
                            &configuration.data_root().join("library.db"),
                        ),
                    None,
                )
            }
        };
        if create && path.exists() {
            return Err(cn_or(
                chinese,
                "新建音效库的文件已存在。",
                "New library destination already exists.",
            ));
        }
        let provider =
            self.open_local_for_switch(&path, create, registration.is_some(), progress)?;
        progress.stage(
            "applying_local",
            "正在应用音效库并保存设置…",
            "Applying the library and saving settings…",
        );
        // Open can be slow. Apply only this source operation to the latest settings,
        // including any preference or server profile updates saved in the meantime.
        {
            let mut config = self.config.lock().unwrap();
            if self.shutdown.load(Ordering::Acquire) {
                return Err("Desktop is shutting down".into());
            }
            let mut committed = config.clone();
            self.mirror_local_waveform_preference(&mut committed);
            committed
                .reconcile_local_libraries()
                .map_err(|error| error.to_string())?;
            let id = match registration {
                Some((name, description, pregenerate)) => committed
                    .register_local_library(name, description, &path, pregenerate)
                    .map_err(|error| error.to_string())?,
                None => match request {
                    LocalSourceRequest::Switch { id } => id.clone(),
                    _ => unreachable!(),
                },
            };
            committed
                .select_local_library(&id)
                .map_err(|error| error.to_string())?;
            committed
                .data_source
                .set_active(&ActiveSource::Local)
                .map_err(|error| error.to_string())?;
            committed.data_source_fallback = None;
            committed.save().map_err(|error| error.to_string())?;
            self.pregenerate_list_waveforms
                .store(committed.pregenerate_list_waveforms, Ordering::Release);
            self.list_waveforms
                .set_pregenerate(committed.pregenerate_list_waveforms);
            *config = committed;
        }
        self.cancel_remote();
        self.source_generation.fetch_add(1, Ordering::AcqRel);
        self.stop_remote_stream();
        *self.sqlite.write().unwrap() = Arc::new(provider);
        self.remote_mode.store(false, Ordering::Release);
        {
            let mut remote = self.remote.lock().unwrap();
            remote.state = None;
            remote.library_changed = false;
            remote.live_roots = None;
            remote.check = None;
            remote.pending_sync = None;
            remote.last_error = None;
            remote.manager.invalidate_library();
        }
        Ok(if chinese {
            format!("已切换到本地音效库：{}", path.display())
        } else {
            format!("Opened local library: {}", path.display())
        })
    }

    /// UI completion after the parent resets the library view and releases its busy gate.
    pub(in crate::controller) fn finish_local_source(
        self: &Arc<Self>,
        request: &LocalSourceRequest,
        ui: &AppWindow,
    ) {
        let selected = self
            .config
            .lock()
            .unwrap()
            .selected_local_library()
            .map(|library| library.id.clone());
        self.set_selection(local_selection(
            selected.as_deref().unwrap_or(DEFAULT_LOCAL_LIBRARY_ID),
        ));
        ui.set_sm_local_form(false);
        ui.set_sm_local_message("".into());
        ui.set_pregenerate_list_waveforms(self.pregenerate_list_waveforms.load(Ordering::Acquire));
        self.publish_data_source(ui);
        self.reload_selection();
        if let LocalSourceRequest::Register {
            path,
            existing: false,
            ..
        } = request
        {
            ui.set_scan_path(path.display().to_string().into());
            ui.set_scan_mode(0);
            self.scan(path.clone());
        }
    }
}

fn new_database_path(data_root: &Path) -> Result<PathBuf, String> {
    let directory = data_root.join("local-libraries");
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let directory = directory
        .canonicalize()
        .map_err(|error| error.to_string())?;
    loop {
        let serial = NEXT_LOCAL_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "library-{}-{}-{serial}.db",
            std::process::id(),
            super::unix_now()
        ));
        if !path.exists() {
            return Ok(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::source_transition_tests;
    use super::*;
    use audiolib_audio::{AudioEngine, PlaybackConfig};
    use audiolib_core::DataProvider;
    use audiolib_core::{
        MetadataPatch,
        config::{AppConfig, ServerProfile},
    };
    use audiolib_db::SqliteProvider;
    use slint::{ComponentHandle, Model};
    use std::sync::atomic::AtomicBool;

    fn local_controller(root: &Path) -> (AppWindow, Arc<Controller>, AudioEngine) {
        let mut config = source_transition_tests::configuration(&root.join("preferences"));
        config.reconcile_local_libraries().unwrap();
        let provider = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
        let playback = PlaybackConfig {
            volume: 0.0,
            ..Default::default()
        };
        let engine = AudioEngine::new(playback.clone()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
        ui.set_library_visible(true);
        (ui, controller, engine)
    }

    #[test]
    fn ordinary_switch_keeps_integrity_audits_explicit_and_rejects_invalid_identity() {
        source_transition_tests::isolated_module(
            "source_manager::local_sources::tests",
            "child_switch_without_integrity_audit",
        );
    }

    #[test]
    #[ignore = "isolated ordinary switch versus full admission validation"]
    fn child_switch_without_integrity_audit() {
        i_slint_backend_testing::init_integration_test_with_system_time();
        let root = source_transition_tests::root();
        let (ui, controller, _engine) = local_controller(&root);
        // Worker progress reaches the busy overlay through the event loop. A late
        // post after completion must not leave stale progress for the next picker.
        ui.set_library_busy(true);
        let mut progress = LibraryProgress::new(&controller);
        progress.stage(
            "draining_scans",
            "正在等待扫描安全结束…",
            "Waiting for scans to finish safely…",
        );
        let weak = ui.as_weak();
        source_transition_tests::run_until(&ui, move || {
            !weak
                .upgrade()
                .unwrap()
                .get_library_operation_stage()
                .is_empty()
        });
        ui.set_library_busy(false);
        ui.set_library_operation_stage("".into());
        progress.stage("late_progress", "已完成", "Completed");
        drop(progress);
        let delivered = Arc::new(AtomicBool::new(false));
        let completed = delivered.clone();
        post(ui.as_weak(), move |ui| {
            assert!(ui.get_library_operation_stage().is_empty());
            completed.store(true, Ordering::Release);
        });
        source_transition_tests::run_until(&ui, move || delivered.load(Ordering::Acquire));
        let (other, audio) = source_transition_tests::indexed_sounds(&root.join("other"));
        let path = other.database_path().to_owned();
        // An extension's orphan is detectable only by an integrity audit. Ordinary
        // activation must not copy/audit every data page, or alter extension data.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF;
             CREATE TABLE extension_links(audio_id INTEGER REFERENCES audio_files(id));
             INSERT INTO extension_links VALUES(-1);",
        )
        .unwrap();
        drop(conn);
        assert!(!other.check_library().unwrap().is_ok());
        let id = controller
            .config
            .lock()
            .unwrap()
            .register_local_library("Other", "", &path, false)
            .unwrap();
        controller.remote_mode.store(true, Ordering::Release);
        controller.set_selection(Selection::LocalLibrary(id));
        controller.switch_to_selected();
        let switched = controller.clone();
        let expected = path.clone();
        source_transition_tests::run_until(&ui, move || {
            !switched.library_busy.load(Ordering::Acquire)
                && !switched.source_changing.load(Ordering::Acquire)
                && switched.provider().database_path() == expected
        });
        assert!(!controller.remote_read_only());
        assert_eq!(
            controller.provider().get_audio(audio.id).unwrap().file_path,
            audio.file_path
        );
        assert_eq!(ui.get_library_operation_stage(), "");
        assert!(!controller.provider().check_library().unwrap().is_ok());
        // Adding this same file as a new source still performs the full audit.
        assert!(
            controller
                .execute_local_source(&LocalSourceRequest::Register {
                    name: "Audit required".into(),
                    description: String::new(),
                    path: path.clone(),
                    existing: true,
                    pregenerate: false,
                })
                .is_err()
        );
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            conn.query_row("SELECT audio_id FROM extension_links", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            -1
        );
        drop(conn);
        // Even a previously registered path cannot substitute an unrelated SQLite
        // file or a library from a future version. Failure preserves the provider.
        for future in [false, true] {
            let invalid = root.join(if future { "future.db" } else { "unrelated.db" });
            if future {
                drop(SqliteProvider::open(&invalid).unwrap());
            }
            let conn = rusqlite::Connection::open(&invalid).unwrap();
            conn.execute_batch(if future {
                "PRAGMA user_version=9999;"
            } else {
                "CREATE TABLE unrelated(value TEXT);"
            })
            .unwrap();
            drop(conn);
            let id = controller
                .config
                .lock()
                .unwrap()
                .register_local_library("Invalid", "", &invalid, false)
                .unwrap();
            assert!(
                controller
                    .execute_local_source(&LocalSourceRequest::Switch { id })
                    .is_err()
            );
            assert_eq!(controller.provider().database_path(), path);
        }
        controller.shutdown();
        ui.hide().unwrap();
    }

    #[test]
    fn pending_checkbox_changes_survive_local_switch_and_inactive_library_edit() {
        source_transition_tests::isolated_module(
            "source_manager::local_sources::tests",
            "child_pending_checkbox_changes",
        );
    }

    #[test]
    #[ignore = "isolated pending preferences, source selection and local editor regression"]
    fn child_pending_checkbox_changes() {
        i_slint_backend_testing::init_integration_test_with_system_time();
        let root = source_transition_tests::root();
        let (ui, controller, _engine) = local_controller(&root);
        let other = SqliteProvider::open(&root.join("other.db")).unwrap();
        let id = {
            let mut config = controller.config.lock().unwrap();
            config
                .update_local_library(DEFAULT_LOCAL_LIBRARY_ID, "default", "", true)
                .unwrap();
            config
                .register_local_library("Other", "", other.database_path(), true)
                .unwrap()
        };
        // Model a clicked checkbox before its asynchronous preference worker runs:
        // the atomic value is current, while both saved fields still say true.
        controller
            .pregenerate_list_waveforms
            .store(false, Ordering::Release);
        controller
            .execute_local_source(&LocalSourceRequest::Switch { id: id.clone() })
            .unwrap();
        {
            let config = controller.config.lock().unwrap();
            assert!(
                !config
                    .local_library(DEFAULT_LOCAL_LIBRARY_ID)
                    .unwrap()
                    .pregenerate_list_waveforms
            );
            assert!(config.pregenerate_list_waveforms);
        }
        let loaded = AppConfig::load_in(controller.data_root()).unwrap();
        assert!(
            !loaded
                .local_library(DEFAULT_LOCAL_LIBRARY_ID)
                .unwrap()
                .pregenerate_list_waveforms
        );
        controller
            .execute_local_source(&LocalSourceRequest::Switch {
                id: DEFAULT_LOCAL_LIBRARY_ID.into(),
            })
            .unwrap();
        // Editing inactive Other must not overwrite the current default library's
        // newer checkbox value or its running background service.
        controller
            .pregenerate_list_waveforms
            .store(true, Ordering::Release);
        controller.set_selection(Selection::LocalLibrary(id.clone()));
        ui.invoke_sm_local_edit_requested();
        ui.set_sm_local_name("Other edited".into());
        ui.set_sm_local_pregenerate(false);
        ui.invoke_sm_local_save_requested();
        let saved = controller.clone();
        source_transition_tests::run_until(&ui, move || {
            !saved.library_busy.load(Ordering::Acquire)
        });
        let config = controller.config.lock().unwrap();
        assert!(config.pregenerate_list_waveforms);
        assert!(
            config
                .local_library(DEFAULT_LOCAL_LIBRARY_ID)
                .unwrap()
                .pregenerate_list_waveforms
        );
        assert!(
            !config
                .local_library(&id)
                .unwrap()
                .pregenerate_list_waveforms
        );
        assert_eq!(config.local_library(&id).unwrap().name, "Other edited");
        drop(config);
        assert!(
            controller
                .pregenerate_list_waveforms
                .load(Ordering::Acquire)
        );
        assert!(controller.list_waveforms.progress().enabled);
        controller.shutdown();
        ui.hide().unwrap();
    }

    #[test]
    fn closing_waits_for_requested_local_preference_save() {
        source_transition_tests::isolated_module(
            "source_manager::local_sources::tests",
            "child_close_during_local_save",
        );
    }

    #[test]
    #[ignore = "isolated native-close admission while local settings save is pending"]
    fn child_close_during_local_save() {
        i_slint_backend_testing::init_integration_test_with_system_time();
        let root = source_transition_tests::root();
        let (ui, controller, _engine) = local_controller(&root);
        let (entered, writing) = std::sync::mpsc::sync_channel(1);
        let (release, released) = std::sync::mpsc::sync_channel(1);
        controller.update_local_preferences(
            &ui,
            move |config| {
                entered.send(()).unwrap();
                released
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                config.update_local_library(
                    DEFAULT_LOCAL_LIBRARY_ID,
                    "Saved before close",
                    "Kept",
                    true,
                )
            },
            |_, _| {},
        );
        writing
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        controller.request_close(&ui);
        assert!(controller.close_after_operation.load(Ordering::Acquire));
        assert!(!controller.closing.load(Ordering::Acquire));
        release.send(()).unwrap();
        // The save completion, rather than this predicate, must initiate the
        // normal close drain and end this single event-loop invocation.
        source_transition_tests::run_until(&ui, || false);
        assert!(!ui.window().is_visible());
        assert!(controller.closing.load(Ordering::Acquire));
        let loaded = AppConfig::load_in(controller.data_root()).unwrap();
        assert_eq!(
            loaded.local_library(DEFAULT_LOCAL_LIBRARY_ID).unwrap().name,
            "Saved before close"
        );
        assert_eq!(
            loaded
                .local_library(DEFAULT_LOCAL_LIBRARY_ID)
                .unwrap()
                .description,
            "Kept"
        );
        controller.shutdown();
    }

    #[test]
    fn failed_local_preference_save_keeps_requested_close_open() {
        source_transition_tests::isolated_module(
            "source_manager::local_sources::tests",
            "child_failed_local_save_keeps_window",
        );
    }

    #[test]
    #[ignore = "isolated local settings failure with a queued native close"]
    fn child_failed_local_save_keeps_window() {
        i_slint_backend_testing::init_integration_test_with_system_time();
        let root = source_transition_tests::root();
        let (ui, controller, _engine) = local_controller(&root);
        let original_settings = controller.config.lock().unwrap().paths.settings.clone();
        let blocked = root.join("settings-is-directory");
        std::fs::create_dir(&blocked).unwrap();
        controller.config.lock().unwrap().paths.settings = blocked;
        let (entered, writing) = std::sync::mpsc::sync_channel(1);
        let (release, released) = std::sync::mpsc::sync_channel(1);
        controller.update_local_preferences(
            &ui,
            move |config| {
                entered.send(()).unwrap();
                released
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                config.update_local_library(DEFAULT_LOCAL_LIBRARY_ID, "Must not commit", "", true)
            },
            |_, _| panic!("A failed save must not complete the editor"),
        );
        writing
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        controller.request_close(&ui);
        assert!(controller.close_after_operation.load(Ordering::Acquire));
        release.send(()).unwrap();
        let completed = controller.clone();
        let form = ui.as_weak();
        source_transition_tests::run_until(&ui, move || {
            !completed.library_busy.load(Ordering::Acquire)
                && !form.upgrade().unwrap().get_ds_applying()
        });
        assert!(ui.window().is_visible());
        assert!(!controller.closing.load(Ordering::Acquire));
        assert!(!controller.close_after_operation.load(Ordering::Acquire));
        assert!(!ui.get_sm_local_message().is_empty());
        assert!(!ui.get_ds_message().is_empty());
        let mut config = controller.config.lock().unwrap();
        assert_eq!(
            config.local_library(DEFAULT_LOCAL_LIBRARY_ID).unwrap().name,
            "default"
        );
        config.paths.settings = original_settings;
        drop(config);
        controller.shutdown();
        ui.hide().unwrap();
    }

    #[test]
    fn local_registration_switch_and_rejection_preserve_library_data() {
        source_transition_tests::isolated_module(
            "source_manager::local_sources::tests",
            "child_local_source_lifecycle",
        );
    }

    #[test]
    #[ignore = "isolated local source worker, settings and model regression"]
    fn child_local_source_lifecycle() {
        i_slint_backend_testing::init_integration_test_with_system_time();
        let root = source_transition_tests::root();
        let mut config = source_transition_tests::configuration(&root.join("preferences"));
        config.reconcile_local_libraries().unwrap();
        std::fs::create_dir_all(config.data_root()).unwrap();
        let default = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
        let (other, audio) = source_transition_tests::indexed_sounds(&root.join("other"));
        other
            .update_metadata(
                audio.id,
                &MetadataPatch {
                    notes: Some("retain this annotation".into()),
                    favorite: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        let wav = PathBuf::from(&audio.file_path);
        let wav_before = std::fs::read(&wav).unwrap();
        let playback = PlaybackConfig {
            volume: 0.0,
            ..Default::default()
        };
        let engine = AudioEngine::new(playback.clone()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(&ui, default.clone(), engine.handle(), config, playback);
        controller.remote_mode.store(true, Ordering::Release);
        controller
            .execute_local_source(&LocalSourceRequest::Register {
                name: "Foley".into(),
                description: "Local effects".into(),
                path: other.database_path().to_owned(),
                existing: true,
                pregenerate: false,
            })
            .unwrap();
        assert!(!controller.remote_read_only());
        assert_eq!(controller.provider().database_path(), other.database_path());
        assert!(
            !controller
                .pregenerate_list_waveforms
                .load(Ordering::Acquire)
        );
        assert_eq!(
            controller.provider().get_audio(audio.id).unwrap().notes,
            "retain this annotation"
        );
        let id = controller
            .config
            .lock()
            .unwrap()
            .selected_local_library()
            .unwrap()
            .id
            .clone();
        controller.set_selection(Selection::LocalLibrary(id.clone()));
        controller
            .config
            .lock()
            .unwrap()
            .data_source
            .profiles
            .push(ServerProfile {
                id: "server-profile".into(),
                name: "Server".into(),
                server_url: "http://127.0.0.1:18086".into(),
                ..Default::default()
            });
        controller.publish_sources(&ui);
        assert_eq!(ui.get_sm_local_count(), 2);
        assert_eq!(ui.get_sm_selected(), 1);
        assert_eq!(ui.get_sm_sources().row_data(1).unwrap().kind, 0);
        controller.select_source(2);
        assert_eq!(
            controller.selection(),
            Selection::Profile("server-profile".into())
        );
        controller.select_source(1);
        assert_eq!(controller.selection(), Selection::LocalLibrary(id.clone()));
        let loaded = AppConfig::load_in(controller.data_root()).unwrap();
        assert_eq!(loaded.selected_local_library().unwrap().id, id);
        // A queued Add local action may open while saving a server connection
        // starts a sync. Keep a real source selection behind the new form.
        controller.source_changing.store(true, Ordering::Release);
        ui.set_ds_source_syncing(true);
        ui.set_ds_applying(true);
        ui.invoke_sm_local_add_requested();
        assert!(ui.get_sm_local_form());
        assert_eq!(ui.get_sm_selected(), 1);
        // Recovery must open the selected default, not the remembered Foley DB.
        controller.set_selection(Selection::Local);
        controller.switch_to_selected();
        let recovered = controller.clone();
        let expected = default.database_path().to_owned();
        source_transition_tests::run_until(&ui, move || {
            !recovered.library_busy.load(Ordering::Acquire)
                && !recovered.source_changing.load(Ordering::Acquire)
                && recovered.provider().database_path() == expected
        });
        assert_eq!(
            controller.provider().database_path(),
            default.database_path()
        );
        let invalid = root.join("unrelated.db");
        std::fs::write(&invalid, b"not an AudioLib database").unwrap();
        assert!(
            controller
                .execute_local_source(&LocalSourceRequest::Register {
                    name: "Invalid".into(),
                    description: String::new(),
                    path: invalid.clone(),
                    existing: true,
                    pregenerate: true,
                })
                .is_err()
        );
        assert_eq!(
            std::fs::read(&invalid).unwrap(),
            b"not an AudioLib database"
        );
        assert_eq!(
            controller.provider().database_path(),
            default.database_path()
        );
        {
            let mut config = controller.config.lock().unwrap();
            assert!(
                config
                    .remove_local_library(DEFAULT_LOCAL_LIBRARY_ID)
                    .is_err()
            );
            config.remove_local_library(&id).unwrap();
            config.save().unwrap();
        }
        assert!(other.database_path().is_file());
        assert_eq!(
            other.get_audio(audio.id).unwrap().notes,
            "retain this annotation"
        );
        assert_eq!(std::fs::read(wav).unwrap(), wav_before);
        // Opening Edit immediately after a checkbox change must not wait for the
        // preference worker to mirror its value into the saved registry.
        controller
            .pregenerate_list_waveforms
            .store(false, Ordering::Release);
        ui.invoke_sm_local_edit_requested();
        assert!(!ui.get_sm_local_pregenerate());
        ui.set_sm_local_form(false);
        // A migrated default may refer to a custom working database. If that
        // file goes missing, opening it must not silently create an empty one.
        let saved_configuration = controller.config.lock().unwrap().clone();
        let missing = root.join("missing-custom-default.db");
        {
            let mut config = controller.config.lock().unwrap();
            config.paths.database = missing.clone();
            config
                .data_source
                .local_libraries
                .iter_mut()
                .find(|library| library.id == DEFAULT_LOCAL_LIBRARY_ID)
                .unwrap()
                .database = missing.clone();
        }
        assert!(
            controller
                .execute_local_source(&LocalSourceRequest::Switch {
                    id: DEFAULT_LOCAL_LIBRARY_ID.into(),
                })
                .is_err()
        );
        assert!(!missing.exists());
        assert_eq!(
            controller.provider().database_path(),
            default.database_path()
        );
        *controller.config.lock().unwrap() = saved_configuration;
        controller.shutdown();
        ui.hide().unwrap();
    }

    #[test]
    fn new_local_folder_form_scans_and_pregenerates_stored_waveforms() {
        source_transition_tests::isolated_module(
            "source_manager::local_sources::tests",
            "child_new_local_folder",
        );
    }

    #[test]
    #[ignore = "isolated local-folder form, automatic scan and stored Waveform regression"]
    fn child_new_local_folder() {
        i_slint_backend_testing::init_integration_test_with_system_time();
        let root = source_transition_tests::root();
        let mut config = source_transition_tests::configuration(&root.join("preferences"));
        config.reconcile_local_libraries().unwrap();
        let default = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
        let (source, audio) = source_transition_tests::indexed_sounds(&root.join("other"));
        let wav_before = std::fs::read(&audio.file_path).unwrap();
        let playback = PlaybackConfig {
            volume: 0.0,
            ..Default::default()
        };
        let engine = AudioEngine::new(playback.clone()).unwrap();
        let ui = AppWindow::new().unwrap();
        let controller = Controller::new(&ui, default, engine.handle(), config, playback);
        // Exercise the actual folder form: create a managed database, switch to
        // it, scan in the background, then fill persistent list Waveforms.
        ui.invoke_sm_local_add_requested();
        assert!(ui.get_sm_local_form());
        assert!(
            ui.get_sm_local_pregenerate(),
            "new libraries default to pre-generation"
        );
        ui.set_sm_local_name("Folder library".into());
        ui.set_sm_local_description_draft("Scanned from a local sound folder".into());
        ui.set_sm_local_path_draft(root.join("other/sounds").display().to_string().into());
        ui.invoke_sm_local_save_requested();
        let scanned = controller.clone();
        let form = ui.as_weak();
        source_transition_tests::run_until(&ui, move || {
            let message = form.upgrade().unwrap().get_sm_local_message();
            assert!(
                message.is_empty(),
                "Local folder creation failed: {message}"
            );
            if scanned.library_busy.load(Ordering::Acquire)
                || scanned.scan_cancel.lock().unwrap().is_some()
                || !scanned
                    .config
                    .lock()
                    .unwrap()
                    .selected_local_library()
                    .is_some_and(|library| library.name == "Folder library")
            {
                return false;
            }
            let provider = scanned.provider();
            let Ok(page) = provider.search(&audiolib_core::SearchRequest::default()) else {
                return false;
            };
            let ids = page
                .results
                .into_iter()
                .map(|audio| audio.id)
                .collect::<Vec<_>>();
            ids.len() == 2
                && provider
                    .list_waveforms(&ids, Arc::new(AtomicBool::new(false)))
                    .is_ok_and(|batch| {
                        batch.items.len() == 2
                            && batch.items.iter().all(|item| {
                                matches!(item.state, audiolib_core::ListWaveformState::Ready { .. })
                            })
                    })
        });
        assert!(!ui.get_sm_local_form(), "{}", ui.get_sm_local_message());
        let managed = controller.provider();
        assert!(
            managed.database_path().starts_with(
                controller
                    .data_root()
                    .join("local-libraries")
                    .canonicalize()
                    .unwrap()
            )
        );
        let current = controller
            .config
            .lock()
            .unwrap()
            .selected_local_library()
            .unwrap()
            .clone();
        assert_eq!(current.name, "Folder library");
        assert_eq!(current.description, "Scanned from a local sound folder");
        assert!(current.pregenerate_list_waveforms);
        assert_eq!(managed.stats().unwrap().audio_files, 2);
        assert!(controller.list_waveforms.progress().enabled);
        assert_eq!(
            std::fs::read(root.join("other/sounds/metal.wav")).unwrap(),
            wav_before
        );
        controller.shutdown();
        ui.hide().unwrap();
        drop(source);
    }

    #[test]
    fn owned_database_paths_do_not_reuse_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let first = new_database_path(directory.path()).unwrap();
        std::fs::write(&first, b"owned library").unwrap();
        let second = new_database_path(directory.path()).unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(first).unwrap(), b"owned library");
        assert!(!second.exists());
        assert_eq!(
            second.parent(),
            Some(
                directory
                    .path()
                    .join("local-libraries")
                    .canonicalize()
                    .unwrap()
                    .as_path()
            )
        );
    }
}
