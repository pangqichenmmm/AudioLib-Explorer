// app/audiolib-desktop/src/studio_switch_review_tests.rs
//! Theme restoration must yield to newer input and keep semantic viewport anchors.
use crate::{AppWindow, AudioRow, FolderRow, SearchTaskRow, theme};
use audiolib_core::config::{DEFAULT_THEME, STUDIO_THEME};
use i_slint_backend_testing::{ElementHandle, TestingBackend, TestingBackendOptions};
use slint::{
    ComponentHandle, ModelRc, VecModel,
    platform::{Key, WindowEvent},
};
use std::time::Duration;

fn init_backend() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .expect("platform already initialized");
}

fn window() -> AppWindow {
    let ui = AppWindow::new().unwrap();
    ui.window()
        .set_size(slint::LogicalSize::new(1600.0, 1000.0));
    ui.global::<crate::Theme>().set_motion(0.0);
    ui.set_rows(ModelRc::new(VecModel::from(
        (0..200)
            .map(|index| AudioRow {
                id: index.to_string().into(),
                name: format!("Owned sound {index:03}.wav").into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_folders(ModelRc::new(VecModel::from(
        (0..80)
            .map(|index| FolderRow {
                name: format!("Owned folder {index:02}").into(),
                path: format!("C:/OwnedFixture/{index}").into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_selected_id("42".into());
    ui.set_selected_row(42);
    ui.set_has_selection(true);
    ui.set_edit_tags("unsaved fixture annotation".into());
    ui.set_metadata_dirty(true);
    ui
}

fn paint(ui: &AppWindow) {
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    ui.window().take_snapshot().unwrap();
}

fn settle(ui: &AppWindow) {
    for _ in 0..16 {
        paint(ui);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
        paint(ui);
        if !ui.get_task_restoring() {
            return;
        }
    }
    panic!("The viewport restore did not finish");
}

fn restore_anchor(ui: &AppWindow, anchor: f32) {
    // Prime the native repeaters before establishing the fixture's baseline.
    // The scenarios below concern changes after a settled visible library.
    paint(ui);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1000));
    paint(ui);
    ui.invoke_restore_search_task_viewport(
        -anchor * ui.get_task_result_row_height(),
        0.0,
        -120.0,
        0.0,
        0.0,
    );
    settle(ui);
    assert!(
        (ui.get_task_result_scroll_y() / ui.get_task_result_row_height() + anchor).abs() < 0.01,
        "Fixture anchor: y={}, row-height={}, anchor={anchor}, Studio={}, restoring={}",
        ui.get_task_result_scroll_y(),
        ui.get_task_result_row_height(),
        ui.global::<crate::Theme>().get_modern_style(),
        ui.get_task_restoring()
    );
}

#[test]
fn pending_theme_restore_uses_the_current_font_and_scale_row_height() {
    init_backend();
    for (target, scale, font) in [(STUDIO_THEME, 2.0, 20), (DEFAULT_THEME, 1.5, 24)] {
        let ui = window();
        let initial = if target == STUDIO_THEME {
            DEFAULT_THEME
        } else {
            STUDIO_THEME
        };
        theme::apply_theme(&ui, initial);
        ui.show().unwrap();
        settle(&ui);
        restore_anchor(&ui, 31.5);
        theme::apply_theme(&ui, target);
        assert!(ui.get_task_restoring());
        theme::apply_font_sizes(&ui, 13, font);
        ui.global::<crate::Theme>().set_ui_scale(scale);
        settle(&ui);
        paint(&ui);
        assert!(
            (ui.get_task_result_scroll_y() / ui.get_task_result_row_height() + 31.5).abs() < 0.01,
            "{target}: y={}, height={}",
            ui.get_task_result_scroll_y(),
            ui.get_task_result_row_height()
        );
        assert_eq!(ui.get_edit_tags(), "unsaved fixture annotation");
        assert_eq!(ui.get_selected_id(), "42");
        ui.hide().unwrap();
    }
}

#[test]
fn source_reset_cancels_the_old_workbench_sidebar_restore_in_both_phases() {
    init_backend();
    for after_first_pass in [false, true] {
        let ui = window();
        ui.show().unwrap();
        settle(&ui);
        restore_anchor(&ui, 31.5);
        assert!((ui.get_task_folder_scroll_y() + 120.0).abs() < 1.0);
        theme::apply_theme(&ui, STUDIO_THEME);
        if after_first_pass {
            paint(&ui);
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
            paint(&ui);
        }
        assert!(ui.get_task_restoring());
        // A source reset must not let the old source's pending -120px folder
        // offset reach the incoming native view after the event loop resumes.
        ui.invoke_reset_result_list_scroll();
        ui.set_selected_id("".into());
        ui.set_selected_row(-1);
        ui.set_has_selection(false);
        // The real source reset clears the old folder model before publishing
        // its replacement. Test that its incoming view survives cancellation,
        // rather than asking cancellation to undo already applied offsets.
        ui.set_folders(ModelRc::default());
        paint(&ui);
        ui.set_folders(ModelRc::new(VecModel::from(
            (0..80)
                .map(|index| FolderRow {
                    name: format!("Incoming folder {index:02}").into(),
                    path: format!("C:/IncomingFixture/{index}").into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )));
        paint(&ui);
        assert!(ui.get_task_folder_scroll_y().abs() < 1.0);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
        paint(&ui);
        assert!(!ui.get_task_restoring());
        assert!(ui.get_task_result_scroll_y().abs() < 1.0);
        assert!(
            ui.get_task_folder_scroll_y().abs() < 1.0,
            "An old workbench timer restored a stale folder offset: {}",
            ui.get_task_folder_scroll_y()
        );
        ui.hide().unwrap();
    }
}

#[test]
fn explicit_end_key_selection_supersedes_a_pending_workbench_restore() {
    init_backend();
    for target in [STUDIO_THEME, DEFAULT_THEME] {
        let ui = window();
        let initial = if target == STUDIO_THEME {
            DEFAULT_THEME
        } else {
            STUDIO_THEME
        };
        theme::apply_theme(&ui, initial);
        ui.show().unwrap();
        settle(&ui);
        restore_anchor(&ui, 31.5);
        ui.on_row_clicked({
            let weak = ui.as_weak();
            move |index, _, _, _| {
                let ui = weak.upgrade().unwrap();
                // Match the real Controller's explicit-selection publication.
                ui.set_preserve_result_scroll(false);
                ui.set_selected_row(index);
            }
        });
        ui.set_settings_visible(true);
        paint(&ui);
        theme::apply_theme(&ui, target);
        ui.set_settings_visible(false);
        paint(&ui);
        assert!(ui.get_task_restoring());
        for event in [
            WindowEvent::KeyPressed {
                text: Key::End.into(),
            },
            WindowEvent::KeyReleased {
                text: Key::End.into(),
            },
        ] {
            ui.window().dispatch_event(event);
        }
        settle(&ui);
        assert_eq!(ui.get_selected_row(), 199);
        let bottom = 200.0 * ui.get_task_result_row_height() + ui.get_task_result_scroll_y();
        assert!(
            bottom <= ui.get_task_result_visible_height() + 1.0,
            "{target}: the explicitly selected last row stayed offscreen: y={}, row-height={}, bottom={bottom}, visible-height={}, restoring={}",
            ui.get_task_result_scroll_y(),
            ui.get_task_result_row_height(),
            ui.get_task_result_visible_height(),
            ui.get_task_restoring()
        );
        assert_eq!(ui.get_edit_tags(), "unsaved fixture annotation");
        ui.hide().unwrap();
    }
}

#[test]
fn switching_tab_widths_keeps_the_active_search_task_visible() {
    init_backend();
    let ui = window();
    ui.window().set_size(slint::LogicalSize::new(1320.0, 900.0));
    ui.set_search_tasks(ModelRc::new(VecModel::from(
        (0..6)
            .map(|index| SearchTaskRow {
                id: index.to_string().into(),
                name: format!("Task {index}").into(),
                active: index == 2,
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_active_search_task_index(2);
    ui.show().unwrap();
    settle(&ui);
    let strip = ElementHandle::find_by_element_id(&ui, "AppWindow::task-tabs-scroll")
        .next()
        .expect("Classic task strip");
    let task = ElementHandle::find_by_accessible_label(&ui, "Search task: Task 2")
        .next()
        .unwrap();
    let strip_origin = strip.absolute_position();
    let position = slint::LogicalPosition::new(
        strip_origin.x + strip.size().width / 2.0,
        strip_origin.y + strip.size().height / 2.0,
    );
    // Leave the active task near the left edge while inspecting later tabs.
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x: strip_origin.x + 30.0 - task.absolute_position().x,
        delta_y: 0.0,
    });
    paint(&ui);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1000));
    paint(&ui);
    assert!(task.absolute_position().x >= strip_origin.x - 1.0);
    assert!(
        task.absolute_position().x + task.size().width <= strip_origin.x + strip.size().width + 1.0
    );
    theme::apply_theme(&ui, STUDIO_THEME);
    settle(&ui);
    let strip = ElementHandle::find_by_element_id(&ui, "AppWindow::studio-task-tabs-scroll")
        .next()
        .expect("Studio task strip");
    let task = ElementHandle::find_by_accessible_label(&ui, "Search task: Task 2")
        .next()
        .unwrap();
    let origin = strip.absolute_position();
    assert!(task.absolute_position().x >= origin.x - 1.0);
    assert!(task.absolute_position().x + task.size().width <= origin.x + strip.size().width + 1.0);
    assert_eq!(ui.get_active_search_task_index(), 2);
    ui.hide().unwrap();
}
