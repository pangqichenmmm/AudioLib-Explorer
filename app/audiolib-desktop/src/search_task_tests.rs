// app/audiolib-desktop/src/search_task_tests.rs
//! Isolated controller regressions for task identity, saved definitions and persistence.
use super::super::*;

#[path = "search_task_favorite_review_tests.rs"]
mod favorite_review;
#[path = "search_task_lifecycle_review_tests.rs"]
mod lifecycle_review;
#[path = "search_task_ui_review_tests.rs"]
mod ui_review;
#[path = "search_task_waveform_review_tests.rs"]
mod waveform_review;

const ROOT_ENV: &str = "AUDIOLIB_SEARCH_TASK_TEST_ROOT";

fn isolated(child: &str) {
    let root = std::env::temp_dir().join(format!(
        "audiolib-search-tasks-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("controller::search_tasks::tests::{child}"),
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(ROOT_ENV, &root)
        .env("AUDIOLIB_DATA_DIR", root.join("preferences"))
        .output()
        .unwrap();
    // This subprocess owns this exact generated subtree; no working library is used.
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    if root.exists() {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match std::fs::remove_dir_all(&root) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) if Instant::now() >= deadline => {
                    eprintln!("warning: could not remove {}: {error}", root.display());
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{child}: {}\n{stdout}\n{stderr}",
        output.status
    );
}

struct Fixture {
    root: PathBuf,
    provider: Arc<SqliteProvider>,
    controller: Arc<Controller>,
    ui: AppWindow,
    engine: audiolib_audio::AudioEngine,
}

fn fixture() -> Fixture {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root =
        PathBuf::from(std::env::var_os(ROOT_ENV).expect("isolated parent supplies the root"));
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    let sounds = root.join("sounds");
    std::fs::create_dir_all(sounds.join("gore/deep")).unwrap();
    std::fs::create_dir_all(sounds.join("wind")).unwrap();
    let mut wav = Vec::new();
    wav.extend(b"RIFF");
    wav.extend(38u32.to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(48_000u32.to_le_bytes());
    wav.extend(96_000u32.to_le_bytes());
    wav.extend(2u16.to_le_bytes());
    wav.extend(16u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(2u32.to_le_bytes());
    wav.extend(0i16.to_le_bytes());
    for path in [
        "gore/gore one.wav",
        "gore/gore two.wav",
        "gore/deep/gore three.wav",
        "wind/wind.wav",
    ] {
        std::fs::write(sounds.join(path), &wav).unwrap();
    }
    let preferences = root.join("preferences");
    std::fs::create_dir_all(&preferences).unwrap();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    // Existing explicit preferences disable first-run legacy-library discovery.
    std::fs::write(
        preferences.join("settings.json"),
        serde_json::to_vec(&serde_json::json!({
            "active_database": root.join("library.db"),
            "language": "en",
            "audition_play_on_single_click": false,
            "max_search_tasks": 6,
            "playback": playback,
        }))
        .unwrap(),
    )
    .unwrap();
    let config = AppConfig::load_in(preferences).unwrap();
    let provider = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
    ScanService::new(provider.clone())
        .scan(
            &sounds,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    for file in provider.search(&SearchRequest::default()).unwrap().results {
        provider
            .update_metadata(
                file.id,
                &MetadataPatch {
                    tags: Some(if file.filename.starts_with("gore") {
                        "gore".into()
                    } else {
                        "wind".into()
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        // Do not let background thumbnail derivation race revision-cache assertions.
        provider
            .compute_list_waveform(file.id, Arc::new(AtomicBool::new(false)))
            .unwrap();
    }
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1600.0, 900.0));
    let controller = Controller::new(&ui, provider.clone(), engine.handle(), config, playback);
    let ready = controller.clone();
    wait_for(&ui, move |ui| {
        !ui.get_search_workspace_loading()
            && ui.get_search_tasks().row_count() == 1
            && !ui.get_searching()
            && fully_published(&ready)
    });
    Fixture {
        root,
        provider,
        controller,
        ui,
        engine,
    }
}

fn wait_for(ui: &AppWindow, ready: impl Fn(&AppWindow) -> bool + 'static) {
    let started = Instant::now();
    let weak = ui.as_weak();
    let completed = std::rc::Rc::new(std::cell::Cell::new(false));
    let observed = completed.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(10),
        move || {
            // The test backend can fire timers again before consuming a queued Quit.
            if observed.get() {
                return;
            }
            let Some(ui) = weak.upgrade() else { return };
            assert!(
                started.elapsed() < Duration::from_secs(12),
                "Task transition timed out: {}",
                ui.get_status()
            );
            if ready(&ui) {
                observed.set(true);
                slint::quit_event_loop().unwrap();
            }
        },
    );
    // Re-showing a ComponentHandle reattaches its component and resets pointer
    // tracking. Keep the same visible window across consecutive interactions.
    if !ui.window().is_visible() {
        ui.show().unwrap();
    }
    slint::run_event_loop().unwrap();
    timer.stop();
    assert!(
        completed.get(),
        "Event loop exited before the task transition completed"
    );
}

fn active_id(ui: &AppWindow) -> String {
    ui.get_search_tasks()
        .iter()
        .find(|task| task.active)
        .expect("one task is active")
        .id
        .to_string()
}

fn search(fixture: &Fixture, query: &str, count: usize) {
    fixture.ui.set_query(query.into());
    fixture
        .controller
        .request_navigation_search(&fixture.ui, true, true);
    let controller = fixture.controller.clone();
    wait_for(&fixture.ui, move |ui| {
        !ui.get_searching() && ui.get_rows().row_count() == count && fully_published(&controller)
    });
}

fn fully_published(controller: &Controller) -> bool {
    controller
        .search_publication
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|publication| {
            publication.page == SearchPagePhase::Published && publication.model_published
        })
}

fn choose_gore(fixture: &Fixture) {
    let ui = &fixture.ui;
    if !ui.get_folders().iter().any(|folder| folder.name == "gore") {
        let root = ui
            .get_folders()
            .iter()
            .position(|folder| folder.name == "sounds")
            .expect("fixture scan root is visible");
        ui.invoke_folder_toggle_requested(root as i32);
        wait_for(ui, |ui| {
            ui.get_folders().iter().any(|folder| folder.name == "gore")
        });
    }
    let index = ui
        .get_folders()
        .iter()
        .position(|folder| folder.name == "gore")
        .unwrap();
    fixture.controller.choose_folder(index as i32);
    let controller = fixture.controller.clone();
    wait_for(ui, move |ui| {
        !ui.get_searching() && fully_published(&controller)
    });
}

fn finish(fixture: &Fixture) {
    fixture.controller.capture_search_task(&fixture.ui);
    fixture.controller.shutdown();
    fixture.ui.hide().unwrap();
}

fn shared_metadata_draft(fixture: &Fixture, first: &str, second: &str, file: &AudioFile) {
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    ui.set_edit_notes("notes from task A".into());
    ui.set_metadata_dirty(true);
    controller.capture_metadata(ui);
    let first_revision = controller.metadata.lock().unwrap().drafts[&file.id].revision;
    // Represent another task which previously selected the same indexed sound.
    // Restore through the real task path instead of selecting audio (which loads it).
    controller.capture_search_task(ui);
    controller
        .task_workspace
        .lock()
        .unwrap()
        .workspace
        .session
        .tasks
        .iter_mut()
        .find(|task| task.id == second)
        .unwrap()
        .view
        .selected_audio = Some(audiolib_core::search_workspace::SearchAudioRef {
        id: file.id,
        file_path: file.file_path.clone(),
    });
    ui.invoke_search_task_selected(second.into());
    let wanted = second.to_owned();
    let audio_id = file.id.to_string();
    wait_for(ui, move |ui| {
        active_id(ui) == wanted
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && ui.get_selected_id().as_str() == audio_id
    });
    assert_eq!(ui.get_edit_notes(), "notes from task A");
    ui.set_edit_notes("notes from task B".into());
    ui.set_metadata_dirty(true);
    controller.capture_metadata(ui);
    let second_revision = controller.metadata.lock().unwrap().drafts[&file.id].revision;
    assert!(second_revision > first_revision);
    ui.invoke_search_task_selected(first.into());
    let wanted = first.to_owned();
    let audio_id = file.id.to_string();
    wait_for(ui, move |ui| {
        active_id(ui) == wanted
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && ui.get_selected_id().as_str() == audio_id
    });
    assert_eq!(ui.get_edit_notes(), "notes from task B");
    controller.save_metadata(ui);
    let provider = fixture.provider.clone();
    let pending = controller.clone();
    let id = file.id;
    wait_for(ui, move |_| {
        provider.get_audio(id).unwrap().notes == "notes from task B"
            && !pending.metadata.lock().unwrap().drafts.contains_key(&id)
    });
    assert_eq!(
        controller.selected.lock().unwrap().as_ref().unwrap().id,
        file.id
    );
    assert_eq!(fixture.engine.handle().status().generation, 0);
}

fn workspace_failures_block_library_switch(fixture: &Fixture) {
    let controller = &fixture.controller;
    let ui = &fixture.ui;
    controller.persist_search_task(ui);
    controller.wait_workspace_writes().unwrap();
    let target = controller
        .task_workspace
        .lock()
        .unwrap()
        .target
        .clone()
        .unwrap();
    let path = target
        .data_root
        .join("search-workspaces")
        .join(audiolib_db::local_play_history::safe_library_id(&target.library_scope).unwrap())
        .join("workspace.json");
    assert!(path.starts_with(&fixture.root));
    let original = std::fs::read(&path).unwrap();
    let mut unknown: serde_json::Value = serde_json::from_slice(&original).unwrap();
    unknown["format_version"] = serde_json::json!(999);
    let protected = serde_json::to_vec(&unknown).unwrap();
    std::fs::write(&path, &protected).unwrap();
    controller.persist_search_task(ui);
    assert!(controller.wait_workspace_writes().is_err());

    let other = audiolib_core::search_workspace::SearchWorkspaceTarget::new(
        target.data_root.clone(),
        "server-independent-fixture",
    );
    {
        let mut writes = controller.workspace_writes.lock().unwrap();
        writes.jobs.push_back(super::SaveJob {
            provider: fixture.provider.clone(),
            target: other.clone(),
            workspace: audiolib_core::search_workspace::SearchWorkspace::for_scope(
                other.library_scope.clone(),
            ),
        });
        controller.workspace_wake.notify_one();
    }
    assert!(controller.wait_workspace_writes().is_err());
    assert!(
        controller
            .workspace_writes
            .lock()
            .unwrap()
            .errors
            .iter()
            .any(|(failed, _)| failed == &target)
    );
    let before = controller
        .task_workspace
        .lock()
        .unwrap()
        .workspace
        .session
        .clone();
    let replacement = fixture.root.join("must-not-create.db");
    controller.run_library_operation(LibraryOperation::Switch {
        path: replacement.clone(),
        create: true,
    });
    let pending = controller.clone();
    wait_for(ui, move |ui| {
        !pending.library_busy.load(Ordering::Acquire)
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
    });
    assert!(Arc::ptr_eq(&controller.provider(), &fixture.provider));
    assert!(!replacement.exists());
    let after = controller
        .task_workspace
        .lock()
        .unwrap()
        .workspace
        .session
        .clone();
    assert_eq!(after.active_task_id, before.active_task_id);
    assert_eq!(
        after
            .tasks
            .iter()
            .map(|task| (&task.id, &task.definition))
            .collect::<Vec<_>>(),
        before
            .tasks
            .iter()
            .map(|task| (&task.id, &task.definition))
            .collect::<Vec<_>>()
    );
    assert_eq!(std::fs::read(&path).unwrap(), protected);
    controller.wait_workspace_writes().unwrap_err();
    std::fs::write(&path, original).unwrap();
    controller.persist_search_task(ui);
    controller.wait_workspace_writes().unwrap();
}

#[test]
fn task_switch_preserves_search_drafts_navigation_and_selection_without_loading_audio() {
    isolated("child_task_state_isolation");
}

#[test]
#[ignore = "Slint controller task state requires an isolated child process"]
fn child_task_state_isolation() {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    let first = active_id(ui);
    let offscreen = controller
        .records
        .lock()
        .unwrap()
        .iter()
        .find(|file| file.filename == "wind.wav")
        .unwrap()
        .clone();
    controller
        .multi_selection
        .lock()
        .unwrap()
        .insert(offscreen.id);
    *controller.selection_anchor.lock().unwrap() = Some(offscreen.id);
    controller.capture_search_task(ui);
    ui.set_tag_filter("gore -wind".into());
    ui.set_sample_filter(2);
    ui.set_sort_index(2);
    ui.set_sort_descending(true);
    search(&fixture, "gore", 3);
    choose_gore(&fixture);
    let folder_id = controller.request.lock().unwrap().folder_id;
    assert!(folder_id.is_some());
    let selected = controller.records.lock().unwrap()[0].clone();
    *controller.selected.lock().unwrap() = Some(selected.clone());
    present_file(ui, &selected);
    controller
        .multi_selection
        .lock()
        .unwrap()
        .insert(selected.id);
    *controller.selection_anchor.lock().unwrap() = Some(offscreen.id);
    ui.set_selected_row(0);
    ui.set_tag_draft("unsubmitted -".into());
    ui.set_duration_min("unfinished duration".into());
    ui.set_color_filter("unfinished color".into());
    ui.set_library_sidebar_page(1);
    ui.set_favorites_category(1);
    ui.set_filters_visible(true);
    controller.request_navigation_search(ui, true, false);
    let old_generation = controller.search_generation.load(Ordering::Acquire);
    let audio_before = fixture.engine.handle().status();

    ui.invoke_search_task_created();
    wait_for(ui, |ui| {
        ui.get_search_tasks().row_count() == 2
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
    });
    let second = active_id(ui);
    assert_ne!(first, second);
    assert_eq!(ui.get_query(), "");
    assert_eq!(ui.get_tag_draft(), "");
    assert_eq!(ui.get_duration_min(), "");
    assert_eq!(controller.request.lock().unwrap().folder_id, None);
    assert!(controller.multi_selection.lock().unwrap().is_empty());
    assert!(controller.search_generation.load(Ordering::Acquire) > old_generation);
    search(&fixture, "wind", 1);

    ui.invoke_search_task_selected(first.clone().into());
    let selected_id = selected.id.to_string();
    let wanted_first = first.clone();
    wait_for(ui, move |ui| {
        active_id(ui) == wanted_first
            && !ui.get_task_restoring()
            && ui.get_selected_id() == selected_id
    });
    assert_eq!(ui.get_query(), "gore");
    assert_eq!(ui.get_tag_filter(), "gore -wind");
    assert_eq!(ui.get_tag_draft(), "unsubmitted -");
    assert_eq!(ui.get_duration_min(), "unfinished duration");
    assert_eq!(ui.get_color_filter(), "unfinished color");
    assert_eq!(ui.get_sample_filter(), 2);
    assert_eq!((ui.get_sort_index(), ui.get_sort_descending()), (2, true));
    assert_eq!(
        (ui.get_library_sidebar_page(), ui.get_favorites_category()),
        (1, 1)
    );
    assert!(ui.get_filters_visible());
    assert_eq!(controller.request.lock().unwrap().folder_id, folder_id);
    assert!(
        controller
            .multi_selection
            .lock()
            .unwrap()
            .contains(&selected.id)
    );
    assert!(
        controller
            .multi_selection
            .lock()
            .unwrap()
            .contains(&offscreen.id)
    );
    assert_eq!(
        *controller.selection_anchor.lock().unwrap(),
        Some(offscreen.id)
    );
    assert!(controller.selection_audio_job.lock().unwrap().is_none());
    let audio_after = fixture.engine.handle().status();
    assert_eq!(audio_after.generation, audio_before.generation);
    assert_eq!(
        audio_after.transport_revision,
        audio_before.transport_revision
    );
    assert_eq!(audio_after.play_session_id, audio_before.play_session_id);
    assert_eq!(audio_after.file_path, audio_before.file_path);
    shared_metadata_draft(&fixture, &first, &second, &selected);
    finish(&fixture);
}

#[test]
fn task_capacity_and_lower_limit_never_discard_an_open_search() {
    isolated("child_task_capacity");
}

#[test]
#[ignore = "Slint controller capacity requires an isolated child process"]
fn child_task_capacity() {
    let fixture = fixture();
    let ui = &fixture.ui;
    for count in 2..=6 {
        ui.invoke_search_task_created();
        wait_for(ui, move |ui| {
            ui.get_search_tasks().row_count() == count
                && !ui.get_search_workspace_loading()
                && !ui.get_task_restoring()
        });
    }
    let active = active_id(ui);
    let ids: Vec<_> = ui.get_search_tasks().iter().map(|task| task.id).collect();
    ui.invoke_search_task_created();
    assert_eq!(ui.get_search_tasks().row_count(), 6);
    assert_eq!(active_id(ui), active);
    ui.invoke_max_search_tasks_requested(2);
    assert_eq!(ui.get_max_search_tasks(), 6);
    assert_eq!(ui.get_search_tasks().row_count(), 6);
    for id in ids {
        ui.invoke_search_task_closed(id);
    }
    wait_for(ui, |ui| {
        ui.get_search_tasks().row_count() == 1
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
    });
    assert_eq!(ui.get_query(), "");
    ui.invoke_max_search_tasks_requested(1);
    assert_eq!(ui.get_max_search_tasks(), 1);
    ui.invoke_search_task_created();
    assert_eq!(ui.get_search_tasks().row_count(), 1);
    ui.invoke_max_search_tasks_requested(6);
    ui.invoke_search_task_created();
    assert_eq!(ui.get_search_tasks().row_count(), 2);
    wait_for(ui, |ui| {
        !ui.get_search_workspace_loading() && !ui.get_task_restoring() && !ui.get_searching()
    });
    search(&fixture, "wind", 1);
    workspace_failures_block_library_switch(&fixture);
    finish(&fixture);
}

#[test]
fn saved_searches_update_explicitly_and_unfavorited_tasks_restore_after_restart() {
    isolated("child_favorites_and_restart");
}

#[test]
#[ignore = "Slint controller workspace restart requires an isolated child process"]
fn child_favorites_and_restart() {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    search(&fixture, "gore", 3);
    let first = active_id(ui);
    ui.set_duration_min("10".into());
    ui.set_duration_max("1".into());
    controller.request_navigation_search(ui, true, false);
    ui.invoke_search_task_favorite_toggled(first.clone().into());
    assert_eq!(ui.get_saved_search_tasks().row_count(), 0);
    ui.set_duration_min("".into());
    ui.set_duration_max("".into());
    search(&fixture, "gore", 3);
    ui.invoke_search_task_favorite_toggled(first.clone().into());
    assert_eq!(ui.get_saved_search_tasks().row_count(), 1);
    let saved_id = ui.get_saved_search_tasks().row_data(0).unwrap().id;
    search(&fixture, "wind", 1);
    controller.capture_search_task(ui);
    assert_eq!(
        controller
            .task_workspace
            .lock()
            .unwrap()
            .workspace
            .favorites[0]
            .definition
            .query,
        "gore"
    );
    ui.invoke_saved_search_opened(saved_id.clone());
    wait_for(ui, |ui| {
        ui.get_search_tasks().row_count() == 2
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
    });
    assert_ne!(active_id(ui), first);
    assert_eq!(ui.get_query(), "gore");
    assert_eq!(
        controller
            .task_workspace
            .lock()
            .unwrap()
            .workspace
            .session
            .tasks[0]
            .definition
            .query,
        "wind"
    );
    ui.invoke_search_task_selected(first.clone().into());
    let wanted_first = first.clone();
    wait_for(ui, move |ui| {
        active_id(ui) == wanted_first
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
    });
    ui.invoke_saved_search_updated(saved_id.clone());
    assert_eq!(
        controller
            .task_workspace
            .lock()
            .unwrap()
            .workspace
            .favorites[0]
            .definition
            .query,
        "wind"
    );
    ui.invoke_saved_search_opened(saved_id);
    assert_eq!(ui.get_search_tasks().row_count(), 2);
    assert_eq!(active_id(ui), first);
    ui.invoke_search_task_created();
    wait_for(ui, |ui| {
        ui.get_search_tasks().row_count() == 3
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
    });
    search(&fixture, "gore", 3);
    ui.set_tag_draft("still typing".into());
    ui.set_duration_min("unfinished".into());
    let second = active_id(ui);
    controller.capture_search_task(ui);
    controller.shutdown();

    let config = AppConfig::load_in(fixture.root.join("preferences")).unwrap();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let restored_engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let restored_ui = AppWindow::new().unwrap();
    let restored = Controller::new(
        &restored_ui,
        fixture.provider.clone(),
        restored_engine.handle(),
        config,
        playback,
    );
    // Keep the previous window visible until its replacement is shown: hiding
    // the last window queues an unrelated quit event in the testing backend.
    restored_ui.show().unwrap();
    ui.hide().unwrap();
    let wanted = second.clone();
    wait_for(&restored_ui, move |ui| {
        !ui.get_search_workspace_loading()
            && ui.get_search_tasks().row_count() == 3
            && active_id(ui) == wanted
            && !ui.get_task_restoring()
    });
    assert_eq!(restored_ui.get_query(), "gore");
    assert_eq!(restored_ui.get_tag_draft(), "still typing");
    assert_eq!(restored_ui.get_duration_min(), "unfinished");
    let state = restored.task_workspace.lock().unwrap();
    assert_eq!(state.workspace.session.tasks[0].id, first);
    assert_eq!(state.workspace.session.tasks[2].id, second);
    assert!(state.workspace.session.tasks[2].saved_search_id.is_none());
    assert_eq!(state.workspace.favorites[0].definition.query, "wind");
    drop(state);
    assert_eq!(restored_engine.handle().status().generation, 0);
    restored.capture_search_task(&restored_ui);
    restored.shutdown();
    restored_ui.hide().unwrap();
}

#[test]
fn inactive_tasks_reject_late_facets_and_refresh_after_external_metadata_writes() {
    isolated("child_late_facets_and_external_revision");
}

#[test]
#[ignore = "Slint controller revision refresh requires an isolated child process"]
fn child_late_facets_and_external_revision() {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    let gore = fixture
        .provider
        .search(&SearchRequest {
            query: "gore".into(),
            order_by: SortOrder::Filename,
            ..Default::default()
        })
        .unwrap()
        .results;
    fixture
        .provider
        .update_metadata(
            gore[0].id,
            &MetadataPatch {
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    let first = active_id(ui);
    ui.set_favorites_only(true);
    search(&fixture, "gore", 1);
    let obsolete = controller.search_generation.load(Ordering::Acquire);
    ui.invoke_search_task_created();
    wait_for(ui, |ui| {
        ui.get_search_tasks().row_count() == 2
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
    });
    search(&fixture, "wind", 1);
    let old_folders = ui.get_folders();
    let old_status = ui.get_status();
    controller.search_folders_computed(
        ui,
        obsolete,
        Ok(Arc::new(vec![Folder {
            id: 999,
            parent_id: None,
            name: "must not publish".into(),
            path: "unrelated-folder".into(),
            file_count: 999,
        }])),
    );
    controller.search_folders_computed(ui, obsolete, Err("obsolete failure".into()));
    assert_eq!(ui.get_folders(), old_folders);
    assert_eq!(ui.get_status(), old_status);

    let external = SqliteProvider::open(&fixture.root.join("library.db")).unwrap();
    external
        .update_metadata(
            gore[1].id,
            &MetadataPatch {
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    ui.invoke_search_task_selected(first.clone().into());
    wait_for(ui, move |ui| {
        active_id(ui) == first && !ui.get_searching() && ui.get_rows().row_count() == 2
    });
    assert!(ui.get_rows().iter().all(|row| row.favorite));
    assert!(ui.get_favorites_only());
    finish(&fixture);
}

#[test]
fn unavailable_directory_scope_survives_repeated_task_switches() {
    isolated("child_unavailable_directory_scope");
}

#[test]
#[ignore = "Slint controller directory recovery requires an isolated child process"]
fn child_unavailable_directory_scope() {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    let first = active_id(ui);
    search(&fixture, "gore", 3);
    choose_gore(&fixture);
    let folder_id = controller.request.lock().unwrap().folder_id.unwrap();
    let folder_path = ui.get_selected_folder_path();
    ui.invoke_search_task_created();
    wait_for(ui, |ui| {
        ui.get_search_tasks().row_count() == 2
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
    });
    search(&fixture, "wind", 1);
    let second = active_id(ui);
    assert_eq!(fixture.provider.remove_folder(folder_id).unwrap(), 3);
    for _ in 0..2 {
        ui.invoke_search_task_selected(first.clone().into());
        let wanted = first.clone();
        wait_for(ui, move |ui| {
            active_id(ui) == wanted
                && !ui.get_search_workspace_loading()
                && !ui.get_task_restoring()
        });
        assert_eq!(
            controller.request.lock().unwrap().folder_id,
            Some(folder_id)
        );
        assert_eq!(ui.get_selected_folder_path(), "");
        assert_eq!(ui.get_rows().row_count(), 0);
        ui.set_query("wind".into());
        controller.request_navigation_search(ui, true, true);
        assert_eq!(ui.get_rows().row_count(), 0);
        assert_eq!(
            controller.request.lock().unwrap().folder_id,
            Some(folder_id)
        );
        assert!(controller.search_job.lock().unwrap().is_none());
        controller.capture_search_task(ui);
        let state = controller.task_workspace.lock().unwrap();
        let task = state
            .workspace
            .session
            .tasks
            .iter()
            .find(|task| task.id == first)
            .unwrap();
        assert_eq!(task.definition.folder.as_ref().unwrap().id, folder_id);
        assert_eq!(
            task.definition.folder.as_ref().unwrap().path,
            folder_path.as_str()
        );
        drop(state);
        ui.invoke_search_task_selected(second.clone().into());
        let wanted = second.clone();
        wait_for(ui, move |ui| {
            active_id(ui) == wanted
                && !ui.get_search_workspace_loading()
                && !ui.get_task_restoring()
        });
    }
    finish(&fixture);
}
