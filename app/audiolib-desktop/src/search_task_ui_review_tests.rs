// app/audiolib-desktop/src/search_task_ui_review_tests.rs
//! Regression cases found while reviewing cold task restoration and filter drafts.
use super::*;
use crate::{SavedSearchTaskRow, SearchTaskRow};
use i_slint_backend_testing::ElementHandle;
use slint::platform::{PointerEventButton, WindowEvent};
use std::{cell::Cell, rc::Rc};

#[test]
fn directory_double_click_survives_real_worker_publication_without_all_sounds_flicker() {
    isolated("ui_review::child_directory_double_click");
}

#[test]
#[ignore = "isolated real-controller directory pointer regression"]
fn child_directory_double_click() {
    let fixture = fixture();
    let ui = &fixture.ui;
    for scale in [1.0, 2.0] {
        ui.global::<crate::Theme>().set_ui_scale(scale);
        ui.window()
            .set_size(slint::LogicalSize::new(1600.0 * scale, 900.0 * scale));
        ui.invoke_all_library_requested();
        let ready = fixture.controller.clone();
        wait_for(ui, move |ui| !ui.get_searching() && fully_published(&ready));
        let before_pair = Instant::now();
        wait_for(ui, move |_| {
            before_pair.elapsed() >= Duration::from_millis(600)
        });
        let id = fixture.controller.tree.lock().unwrap().visible[0].0.id;
        let folder = ElementHandle::find_by_accessible_label(ui, "Folder: sounds")
            .next()
            .unwrap();
        let position = slint::LogicalPosition::new(
            folder.absolute_position().x + folder.size().width / 2.0,
            folder.absolute_position().y + folder.size().height / 2.0,
        );
        // Hold the physical pointer position across publication. A global name
        // lookup could instead match the newly published breadcrumb.
        let click_folder = || {
            ui.window()
                .dispatch_event(WindowEvent::PointerMoved { position });
            ui.window().dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
            ui.window().dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
        };
        let before_model = ui.get_folders();
        let first_click_started = Instant::now();
        click_folder();
        let scope_was_blank = ui.get_selected_folder_path().is_empty();
        let ready = fixture.controller.clone();
        wait_for(ui, move |ui| {
            ui.get_selected_folder_path().ends_with("sounds")
                && !ui.get_searching()
                && fully_published(&ready)
        });
        // Unlike the previous in-place model mock, let the real selection
        // workers publish between the two physical clicks on the same row.
        click_folder();
        assert!(
            fixture
                .controller
                .tree
                .lock()
                .unwrap()
                .expanded
                .contains(&id),
            "Directory double-click was lost after worker publication: scale={scale}, elapsed={:?}, model_retained={}",
            first_click_started.elapsed(),
            ui.get_folders() == before_model
        );
        assert!(
            !scope_was_blank && !ui.get_selected_folder_path().is_empty(),
            "Selecting a directory must not temporarily activate All sounds"
        );
        wait_for(ui, |ui| ui.get_folders().iter().any(|f| f.name == "gore"));

        let before_pair = Instant::now();
        wait_for(ui, move |_| {
            before_pair.elapsed() >= Duration::from_millis(600)
        });
        click_folder();
        let ready = fixture.controller.clone();
        wait_for(ui, move |ui| !ui.get_searching() && fully_published(&ready));
        click_folder();
        assert!(
            !fixture
                .controller
                .tree
                .lock()
                .unwrap()
                .expanded
                .contains(&id),
            "Double-click must also collapse an already selected directory"
        );
        wait_for(ui, |ui| ui.get_folders().row_count() == 1);
        assert!(ui.get_selected_folder_path().ends_with("sounds"));
    }
    finish(&fixture);
}

#[test]
fn spectral_noise_and_variance_autosave_and_restore_in_a_fresh_controller() {
    isolated("ui_review::child_spectral_preferences");
}

#[test]
#[ignore = "isolated spectral display preferences and restart regression"]
fn child_spectral_preferences() {
    let fixture = fixture();
    let ui = &fixture.ui;
    ui.set_peak_noise(0.1);
    ui.set_peak_variance(0.05);
    ui.invoke_peak_style_requested();
    let settings_path = fixture.root.join("preferences/settings.json");
    let saved_path = settings_path.clone();
    wait_for(ui, move |_| {
        std::fs::read(&saved_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|saved| {
                (saved["spectral_peaks"]["noise"].as_f64().unwrap_or(-1.0) - 0.1).abs() < 1e-6
                    && (saved["spectral_peaks"]["variance"].as_f64().unwrap_or(-1.0) - 0.05).abs()
                        < 1e-6
            })
    });
    finish(&fixture);

    let reopened = AppWindow::new().unwrap();
    let config = AppConfig::load_in(fixture.root.join("preferences")).unwrap();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let controller = Controller::new(
        &reopened,
        fixture.provider.clone(),
        engine.handle(),
        config,
        playback,
    );
    assert!((reopened.get_peak_noise() - 0.1).abs() < 1e-6);
    assert!((reopened.get_peak_variance() - 0.05).abs() < 1e-6);
    {
        let view = controller.wave_view.lock().unwrap().1;
        assert!((view.noise - 0.1).abs() < 1e-6);
        assert!((view.variance - 0.05).abs() < 1e-6);
    }
    // Closing immediately must retain the latest edit even with a pending save.
    for (noise, variance) in [(2.0, 1.0), (0.1, 0.05)] {
        reopened.set_peak_noise(noise);
        reopened.set_peak_variance(variance);
        reopened.invoke_peak_style_requested();
    }
    controller.shutdown();
    reopened.hide().unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(settings_path).unwrap()).unwrap();
    assert!((saved["spectral_peaks"]["noise"].as_f64().unwrap() - 0.1).abs() < 1e-6);
    assert!((saved["spectral_peaks"]["variance"].as_f64().unwrap() - 0.05).abs() < 1e-6);
}

#[test]
fn cold_directory_hydration_keeps_expansion_on_a_quick_switch() {
    isolated("ui_review::child_cold_directory_hydration");
}

#[test]
#[ignore = "isolated cold directory-hydration task regression"]
fn child_cold_directory_hydration() {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    search(&fixture, "gore", 3);
    choose_gore(&fixture);
    let gore_index = ui
        .get_folders()
        .iter()
        .position(|folder| folder.name == "gore")
        .unwrap();
    ui.invoke_folder_toggle_requested(gore_index as i32);
    wait_for(ui, |ui| {
        ui.get_folders().iter().any(|folder| folder.name == "deep")
    });
    controller.capture_search_task(ui);
    let first = active_id(ui);
    let expanded = controller
        .task_workspace
        .lock()
        .unwrap()
        .workspace
        .session
        .tasks[0]
        .view
        .expanded_folders
        .clone();
    assert!(expanded.iter().any(|folder| folder.path.ends_with("/gore")));

    // Replay the interval after validating a cold task's expansion references,
    // before the asynchronous directory facets hydrate its known-folder model.
    {
        let mut state = controller.task_workspace.lock().unwrap();
        state.caches.remove(&first);
        state.page_complete = false;
    }
    {
        let mut tree = controller.tree.lock().unwrap();
        tree.known = Arc::default();
        tree.matches = Arc::default();
        tree.visible = Arc::default();
    }
    ui.invoke_search_task_created();
    let retained = controller
        .task_workspace
        .lock()
        .unwrap()
        .workspace
        .session
        .tasks
        .iter()
        .find(|task| task.id == first)
        .unwrap()
        .view
        .expanded_folders
        .clone();
    assert_eq!(retained, expanded);
    wait_for(ui, |ui| {
        !ui.get_search_workspace_loading() && !ui.get_task_restoring() && !ui.get_searching()
    });

    ui.invoke_search_task_selected(first.clone().into());
    let ready = controller.clone();
    wait_for(ui, move |ui| {
        !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
            && fully_published(&ready)
            && ui.get_folders().iter().any(|folder| folder.name == "deep")
    });
    let gore_index = ui
        .get_folders()
        .iter()
        .position(|folder| folder.name == "gore")
        .unwrap();
    ui.invoke_folder_toggle_requested(gore_index as i32);
    wait_for(ui, |ui| {
        !ui.get_folders().iter().any(|folder| folder.name == "deep")
    });
    controller.capture_search_task(ui);
    let state = controller.task_workspace.lock().unwrap();
    let task = state
        .workspace
        .session
        .tasks
        .iter()
        .find(|task| task.id == first)
        .unwrap();
    assert!(
        !task
            .view
            .expanded_folders
            .iter()
            .any(|folder| folder.path.ends_with("/gore"))
    );
    drop(state);
    finish(&fixture);
}

#[test]
fn repaired_unsubmitted_drafts_do_not_save_stale_combo_or_boolean_filters() {
    isolated("ui_review::child_unsubmitted_filter_drafts");
}

#[test]
#[ignore = "isolated favorite filter-draft regression"]
fn child_unsubmitted_filter_drafts() {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    type FilterChange = (&'static str, fn(&AppWindow));
    let changes: &[FilterChange] = &[
        ("sample rate", |ui| ui.set_sample_filter(4)),
        ("bit depth", |ui| ui.set_bit_filter(2)),
        ("channels", |ui| ui.set_channel_filter(2)),
        ("file visibility", |ui| ui.set_visibility_index(1)),
        ("favorite audio", |ui| ui.set_favorites_only(true)),
        ("tagged audio", |ui| ui.set_tagged_only(true)),
    ];
    for (name, change) in changes {
        ui.invoke_reset_filters();
        search(&fixture, "gore", 3);
        let task_id = active_id(ui);
        ui.invoke_search_task_favorite_toggled(task_id.clone().into());
        let saved = ui.get_saved_search_tasks().row_data(0).unwrap().id;
        search(&fixture, "wind", 1);

        ui.set_duration_min("unfinished".into());
        change(ui);
        ui.invoke_filter_requested();
        // Clearing the text is a valid draft edit, but only Enter/Apply commits it.
        ui.set_duration_min("".into());
        ui.invoke_saved_search_updated(saved.clone());
        assert_eq!(
            controller
                .task_workspace
                .lock()
                .unwrap()
                .workspace
                .favorites[0]
                .definition
                .query,
            "gore",
            "{name}: a failed filter request must not update a saved search from stale conditions"
        );
        ui.invoke_saved_search_deleted(saved);
        ui.invoke_search_task_favorite_toggled(task_id.clone().into());
        assert_eq!(
            ui.get_saved_search_tasks().row_count(),
            0,
            "{name}: do not save an unapplied visible filter as the prior executable definition"
        );

        ui.invoke_filter_requested();
        let ready = controller.clone();
        wait_for(ui, move |ui| !ui.get_searching() && fully_published(&ready));
        ui.invoke_search_task_favorite_toggled(task_id.into());
        assert_eq!(ui.get_saved_search_tasks().row_count(), 1);
        let state = controller.task_workspace.lock().unwrap();
        let request = controller.request.lock().unwrap();
        assert_eq!(state.workspace.favorites[0].definition.query, "wind");
        assert_eq!(
            state.workspace.favorites[0].definition.filters,
            request.filters
        );
        drop(request);
        drop(state);
        ui.invoke_saved_search_deleted(ui.get_saved_search_tasks().row_data(0).unwrap().id);
    }
    finish(&fixture);
}

fn settle_read_only_ui() {
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(10));
}

#[test]
fn read_only_workspace_disables_task_mutations_without_blocking_search() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_search_tasks(ModelRc::new(VecModel::from(vec![SearchTaskRow {
        id: "task-1".into(),
        name: "Task 1".into(),
        active: true,
        ..Default::default()
    }])));
    ui.set_saved_search_tasks(ModelRc::new(VecModel::from(vec![SavedSearchTaskRow {
        id: "saved-1".into(),
        name: "Forest".into(),
        summary: "wind".into(),
    }])));
    ui.set_library_sidebar_page(1);
    ui.set_favorites_category(1);
    ui.on_search_task_created(|| panic!("read-only workspace cannot create tasks"));
    ui.on_search_task_closed(|_| panic!("read-only workspace cannot close tasks"));
    ui.on_search_task_favorite_toggled(|_| panic!("read-only workspace cannot save tasks"));
    ui.on_saved_search_opened(|_| panic!("read-only workspace cannot open a persistent task"));
    ui.on_saved_search_updated(|_| panic!("read-only workspace cannot update favorites"));
    ui.on_saved_search_deleted(|_| panic!("read-only workspace cannot delete favorites"));
    let searches = Rc::new(Cell::new(0));
    let observed = searches.clone();
    ui.on_search_requested(move |_| observed.set(observed.get() + 1));
    ui.set_search_workspace_read_only(true);
    ui.show().unwrap();
    settle_read_only_ui();
    for label in [
        "New search task",
        "Favorite search task: Task 1",
        "Close search task: Task 1",
        "Open saved search: Forest",
        "Manage saved search: Forest",
    ] {
        let action = ElementHandle::find_by_accessible_label(&ui, label)
            .next()
            .unwrap_or_else(|| panic!("Missing read-only task action: {label}"));
        assert_eq!(action.accessible_enabled(), Some(false), "{label}");
        action.mock_single_click(PointerEventButton::Left);
        settle_read_only_ui();
    }
    ui.invoke_begin_search_task_rename("task-1".into(), "Task 1".into(), false);
    assert!(!ui.get_search_task_rename_visible());
    ui.set_query("temporary query".into());
    ElementHandle::find_by_accessible_label(&ui, "Search")
        .next()
        .unwrap()
        .mock_single_click(PointerEventButton::Left);
    settle_read_only_ui();
    assert_eq!(searches.get(), 1);
    ui.hide().unwrap();
}
