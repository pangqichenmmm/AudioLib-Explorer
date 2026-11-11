// app/audiolib-desktop/src/mode_switch_scroll_tests.rs
//! The result list shares one Slint ListView between server mode and the local library.
//! These tests check that a remote viewport, row count and selected row do not keep
//! pulling the local list back after the mode switch.
use crate::{AppWindow, AudioRow, FolderRow};
use i_slint_backend_testing::ElementHandle;
use slint::{ComponentHandle, Model, ModelRc, VecModel, platform::WindowEvent};
use std::{rc::Rc, time::Duration};

fn folders(count: usize) -> Vec<FolderRow> {
    (0..count)
        .map(|index| FolderRow {
            name: format!("Folder {index:03}").into(),
            count: "1".into(),
            path: format!("C:/Sounds/{index:03}").into(),
            depth: 0,
            expanded: false,
            has_children: false,
        })
        .collect()
}

fn rows(prefix: &str, count: usize) -> Vec<AudioRow> {
    (0..count)
        .map(|index| AudioRow {
            id: format!("{prefix}-{index}").into(),
            name: format!("{prefix} {index:03}.wav").into(),
            ..Default::default()
        })
        .collect()
}

fn show() -> AppWindow {
    show_at(1440.0, 900.0)
}

fn show_at(width: f32, height: f32) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(width, height));
    ui.set_list_waveforms_server(false);
    ui.show().unwrap();
    settle(1);
    ui
}

fn settle(ms: u64) {
    // Timer.running starts in the change pass after the mock timer pass.
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(ms));
}

fn result_list(ui: &AppWindow) -> ElementHandle {
    ElementHandle::find_by_element_id(ui, "AppWindow::result-list")
        .next()
        .expect("result list")
}

fn wheel(ui: &AppWindow, delta_y: f32) {
    let origin = result_list(ui).absolute_position();
    let position = slint::LogicalPosition::new(origin.x + 40.0, origin.y + 40.0);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x: 0.0,
        delta_y,
    });
    settle(250);
}

fn scroll_y(ui: &AppWindow) -> f32 {
    ui.get_task_result_scroll_y()
}

/// Remote page scrolled deep, then the same ListView receives the shorter local page.
#[test]
fn wheel_scroll_sticks_after_remote_rows_are_replaced_by_a_shorter_local_page() {
    let ui = show();
    ui.set_remote_mode(true);
    ui.set_rows(ModelRc::new(VecModel::from(rows("remote", 200))));
    ui.set_selected_row(160);
    settle(20);
    wheel(&ui, -4000.0);
    let remote_y = scroll_y(&ui);
    assert!(
        remote_y < -200.0,
        "remote page must scroll before the switch, y={remote_y}"
    );

    ui.set_remote_mode(false);
    ui.set_rows(ModelRc::new(VecModel::from(rows("local", 36))));
    ui.set_selected_row(3);
    settle(20);
    let switched = scroll_y(&ui);
    wheel(&ui, -600.0);
    let moved = scroll_y(&ui);
    assert!(
        moved < switched - 40.0,
        "local wheel must leave the post-switch offset {switched}, moved to {moved}, remote was {remote_y}"
    );
    settle(250);
    let held = scroll_y(&ui);
    assert!(
        (held - moved).abs() < 2.0,
        "local scroll was pulled back from {moved} to {held} (switch {switched}, remote {remote_y})"
    );
    ui.hide().unwrap();
}

/// Shrinking the live model (remote total still installed) must not pin the virtual offset.
#[test]
fn wheel_scroll_sticks_when_the_shared_model_shrinks_from_the_remote_page() {
    let ui = show();
    ui.set_remote_mode(true);
    let model = Rc::new(VecModel::from(rows("snap", 200)));
    ui.set_rows(ModelRc::new(model.clone()));
    ui.set_selected_row(150);
    settle(20);
    wheel(&ui, -4000.0);
    let remote_y = scroll_y(&ui);
    assert!(remote_y < -200.0, "remote viewport y={remote_y}");

    ui.set_remote_mode(false);
    model.set_vec(rows("snap", 36));
    ui.set_selected_row(2);
    settle(20);
    assert_eq!(ui.get_rows().row_count(), 36);
    let switched = scroll_y(&ui);
    wheel(&ui, -500.0);
    let moved = scroll_y(&ui);
    assert!(
        moved < switched - 40.0,
        "shrunk local page must accept the wheel: switch {switched}, moved {moved}, remote {remote_y}"
    );
    settle(250);
    let held = scroll_y(&ui);
    assert!(
        (held - moved).abs() < 2.0,
        "shrunk page snapped from {moved} back to {held}"
    );
    ui.hide().unwrap();
}

/// Minimum window: hiding the remote notice and showing the waveform column must
/// leave the result list itself scrolling, not a few pixels of the outer view.
#[test]
fn compact_window_wheel_scrolls_the_local_list_after_leaving_remote_mode() {
    let ui = show_at(1080.0, 700.0);
    ui.set_remote_mode(true);
    ui.set_remote_notice("Server library is offline".into());
    ui.set_remote_notice_kind(1);
    ui.set_rows(ModelRc::new(VecModel::from(rows("remote", 200))));
    ui.set_selected_row(40);
    settle(20);
    let list_height = result_list(&ui).size().height;
    wheel(&ui, -800.0);
    let remote_y = scroll_y(&ui);
    assert!(
        remote_y < -60.0,
        "compact remote list height {list_height} did not scroll, y={remote_y}"
    );

    ui.set_remote_notice("".into());
    ui.set_remote_mode(false);
    ui.set_rows(ModelRc::new(VecModel::from(rows("local", 80))));
    ui.set_selected_row(2);
    settle(20);
    let local_height = result_list(&ui).size().height;
    assert!(
        local_height > 80.0,
        "local result list collapsed to {local_height}"
    );
    let switched = scroll_y(&ui);
    wheel(&ui, -500.0);
    let moved = scroll_y(&ui);
    assert!(
        moved < switched - 40.0,
        "compact local wheel stuck: height {local_height}, switch {switched}, moved {moved}, remote {remote_y}"
    );
    settle(250);
    let held = scroll_y(&ui);
    assert!(
        (held - moved).abs() < 2.0,
        "compact local scroll snapped from {moved} to {held}"
    );
    ui.hide().unwrap();
}

/// Waveform cells arrive after local mode shows the column. Updating row data must not
/// rewind a scroll the user already made on the local page.
#[test]
fn waveform_row_updates_do_not_rewind_local_scroll_after_leaving_remote_mode() {
    let ui = show();
    ui.set_remote_mode(true);
    ui.set_rows(ModelRc::new(VecModel::from(rows("remote", 80))));
    settle(20);
    ui.set_remote_mode(false);
    let model = Rc::new(VecModel::from(rows("local", 80)));
    ui.set_rows(ModelRc::new(model.clone()));
    ui.set_selected_row(4);
    settle(20);
    wheel(&ui, -700.0);
    let scrolled = scroll_y(&ui);
    assert!(
        scrolled < -80.0,
        "local page must scroll before waveform updates, y={scrolled}"
    );
    for index in 0..model.row_count() {
        let mut row = model.row_data(index).unwrap();
        row.wave_state = 1;
        model.set_row_data(index, row);
    }
    settle(50);
    let after_waves = scroll_y(&ui);
    assert!(
        (after_waves - scrolled).abs() < 2.0,
        "waveform updates rewound scroll from {scrolled} to {after_waves}"
    );
    wheel(&ui, -300.0);
    let moved = scroll_y(&ui);
    assert!(
        moved < after_waves - 40.0,
        "wheel after waveform updates stuck at {after_waves}, moved {moved}"
    );
    settle(250);
    let held = scroll_y(&ui);
    assert!(
        (held - moved).abs() < 2.0,
        "scroll snapped from {moved} to {held} after waveform updates"
    );
    ui.hide().unwrap();
}

/// A restore started in server mode must not be applied after the list is local.
#[test]
fn leaving_remote_mode_drops_an_in_flight_remote_viewport_restore() {
    let ui = show();
    ui.set_rows(ModelRc::new(VecModel::from(rows("local", 48))));
    ui.set_selected_row(2);
    settle(20);
    wheel(&ui, -160.0);
    let local_y = scroll_y(&ui);
    assert!(local_y < -40.0, "local scroll y={local_y}");

    ui.set_remote_mode(true);
    ui.set_rows(ModelRc::new(VecModel::from(rows("remote", 200))));
    ui.set_selected_row(150);
    // Arm the one-shot restore, then leave server mode before it is applied.
    ui.invoke_restore_search_task_viewport(-8000.0, -240.0, 0.0, 0.0, 0.0);
    ui.set_remote_mode(false);
    ui.set_rows(ModelRc::new(VecModel::from(rows("local", 48))));
    ui.set_selected_row(2);
    settle(40);
    assert_eq!(ui.get_rows().row_count(), 48);
    let after = scroll_y(&ui);
    assert!(
        after > -400.0,
        "remote restore still owns the local list: local {local_y}, after {after}"
    );
    wheel(&ui, -280.0);
    let moved = scroll_y(&ui);
    assert!(
        moved < after - 40.0,
        "local wheel stuck after the cancelled restore: after {after}, moved {moved}"
    );
    settle(200);
    let held = scroll_y(&ui);
    assert!(
        (held - moved).abs() < 2.0,
        "local scroll snapped from {moved} to {held}"
    );
    ui.hide().unwrap();
}

/// An out-of-range remote viewport restore must not pin later local wheel steps.
#[test]
fn out_of_range_remote_restore_does_not_pin_local_wheel_steps() {
    let ui = show();
    ui.set_remote_mode(true);
    ui.set_rows(ModelRc::new(VecModel::from(rows("remote", 200))));
    ui.set_selected_row(180);
    ui.invoke_restore_search_task_viewport(-8000.0, -400.0, 0.0, 0.0, 0.0);
    settle(30);
    ui.set_remote_mode(false);
    ui.set_rows(ModelRc::new(VecModel::from(rows("local", 120))));
    ui.set_selected_row(8);
    ui.invoke_restore_search_task_viewport(-8000.0, -400.0, 0.0, 0.0, 0.0);
    settle(30);
    assert_eq!(ui.get_rows().row_count(), 120);
    let mut previous = scroll_y(&ui);
    assert!(
        previous > -120.0 * 22.0 - 8.0 && previous < -80.0,
        "remote row index was not clamped inside the local page: {previous}"
    );
    // The clamped restore sits at the bottom of the local page; the wheel must still move up.
    let mut moved_steps = 0;
    for step in 0..3 {
        if previous > -40.0 {
            break;
        }
        wheel(&ui, 80.0);
        let moved = scroll_y(&ui);
        assert!(
            moved > previous + 20.0,
            "step {step} pinned: {previous} -> {moved}"
        );
        settle(80);
        let held = scroll_y(&ui);
        assert!(
            (held - moved).abs() < 2.0,
            "step {step} snapped {moved} -> {held}"
        );
        previous = held;
        moved_steps += 1;
    }
    assert!(
        moved_steps >= 1,
        "local page had no room to leave the restored offset"
    );
    ui.hide().unwrap();
}

/// Dropping a result-list restore armed for the other source must leave the folder
/// list, filter area and inspector scroll in place.
#[test]
fn mismatched_mode_drops_only_the_result_list_restore() {
    let ui = show();
    ui.set_remote_mode(true);
    ui.set_rows(ModelRc::new(VecModel::from(rows("remote", 200))));
    ui.set_folders(ModelRc::new(VecModel::from(folders(80))));
    settle(20);
    // Arm while still remote, then leave that mode before the timer applies it.
    ui.invoke_restore_search_task_viewport(-800.0, -20.0, -180.0, -90.0, -70.0);
    ui.set_remote_mode(false);
    ui.set_rows(ModelRc::new(VecModel::from(rows("local", 48))));
    settle(40);
    let result = scroll_y(&ui);
    assert!(
        result > -80.0,
        "remote result offset landed on the local list: {result}"
    );
    let folder = ui.get_task_folder_scroll_y();
    let filter = ui.get_task_filter_scroll_y();
    let inspector = ui.get_task_inspector_scroll_y();
    assert!(
        folder < -40.0,
        "folder list scroll was discarded with the result list: {folder}"
    );
    assert!(
        filter < -40.0,
        "filter scroll was discarded with the result list: {filter}"
    );
    assert!(
        inspector < -30.0,
        "inspector scroll was discarded with the result list: {inspector}"
    );
    ui.hide().unwrap();
}

/// A restored remote list offset must not be reapplied over later local wheel input.
#[test]
fn restored_remote_viewport_does_not_override_later_local_wheel_input() {
    let ui = show();
    ui.set_rows(ModelRc::new(VecModel::from(rows("local", 48))));
    ui.set_selected_row(1);
    settle(20);
    wheel(&ui, -180.0);
    let local_y = scroll_y(&ui);
    assert!(local_y < -40.0, "local warmup scroll y={local_y}");

    ui.set_remote_mode(true);
    ui.set_rows(ModelRc::new(VecModel::from(rows("remote", 200))));
    ui.set_selected_row(90);
    ui.invoke_restore_search_task_viewport(-2200.0, 0.0, 0.0, 0.0, 0.0);
    settle(20);
    let remote_y = scroll_y(&ui);
    assert!(
        remote_y < local_y - 100.0,
        "remote restore should move the list, local {local_y}, remote {remote_y}"
    );

    ui.set_remote_mode(false);
    ui.set_rows(ModelRc::new(VecModel::from(rows("local", 48))));
    ui.set_selected_row(1);
    settle(20);
    wheel(&ui, -240.0);
    let moved = scroll_y(&ui);
    settle(250);
    let held = scroll_y(&ui);
    assert!(
        (held - moved).abs() < 2.0 && held < -40.0,
        "local wheel did not stick: moved {moved}, held {held}, remote restore {remote_y}"
    );
    ui.hide().unwrap();
}
