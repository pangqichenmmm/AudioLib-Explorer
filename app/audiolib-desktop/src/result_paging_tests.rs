// app/audiolib-desktop/src/result_paging_tests.rs
//! Native Slint scrolling loads adjacent result ranges while preserving visible content.
use crate::{AppWindow, AudioRow, FolderRow};
use i_slint_backend_testing::ElementHandle;
use slint::{ComponentHandle, Model, ModelRc, VecModel, platform::WindowEvent};
use std::{cell::RefCell, rc::Rc, time::Duration};

#[test]
fn downward_scroll_prefetches_before_the_bottom_and_coalesces_pending_requests() {
    let ui = show(200);
    let requests = observe_ranges(&ui, true);
    ui.set_can_next(true);
    wheel(&ui, 0.0, -180.0);
    assert!(ui.get_task_result_scroll_y() < -40.0);
    assert!(
        requests.borrow().is_empty(),
        "Scrolling far from the next range stays local"
    );

    let height = result_height(&ui);
    let bottom = bottom_y(&ui);
    let target = bottom + height;
    wheel(&ui, 0.0, target - ui.get_task_result_scroll_y());
    let requested_at = ui.get_task_result_scroll_y();
    assert!(
        requested_at > bottom + 40.0,
        "Prefetch must begin before the visible boundary"
    );
    assert_eq!(*requests.borrow(), [1]);
    assert!(ui.get_searching());
    for _ in 0..3 {
        wheel(&ui, 0.0, -80.0);
    }
    assert_eq!(
        *requests.borrow(),
        [1],
        "A pending range coalesces continued wheel input"
    );
    assert!(ui.get_task_result_scroll_y() < requested_at - 40.0);
    ui.hide().unwrap();
}

#[test]
fn upward_scroll_prefetches_previous_rows_before_reaching_the_top() {
    let ui = show(400);
    let requests = observe_ranges(&ui, true);
    ui.set_can_previous(true);
    let height = result_height(&ui);
    ui.invoke_restore_result_window_viewport(-height * 3.0);
    settle(20);
    assert!(
        requests.borrow().is_empty(),
        "A restored position cannot start a range request"
    );
    wheel(&ui, 0.0, height * 1.8);
    assert!(
        ui.get_task_result_scroll_y() < -40.0,
        "Prefetch must precede the top boundary"
    );
    assert_eq!(*requests.borrow(), [-1]);
    ui.hide().unwrap();
}

#[test]
fn first_last_empty_and_horizontal_scrolling_do_not_request_ranges() {
    let ui = show(200);
    let requests = observe_ranges(&ui, false);
    wheel(&ui, 0.0, -100_000.0);
    wheel(&ui, 0.0, -120.0);
    wheel(&ui, 0.0, 100_000.0);
    wheel(&ui, 0.0, 120.0);
    assert!(
        requests.borrow().is_empty(),
        "First and last ranges cannot load past the library"
    );

    ui.set_can_next(true);
    ui.set_can_previous(true);
    wheel(&ui, -180.0, 0.0);
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Shift.into(),
    });
    wheel(&ui, 0.0, -120.0);
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: slint::platform::Key::Shift.into(),
    });
    assert!(
        requests.borrow().is_empty(),
        "Horizontal and Shift+wheel input must stay horizontal"
    );

    ui.set_rows(ModelRc::new(VecModel::from(Vec::<AudioRow>::new())));
    settle(20);
    wheel(&ui, 0.0, -120.0);
    wheel(&ui, 0.0, 120.0);
    assert!(
        requests.borrow().is_empty(),
        "An empty view cannot extend a result window"
    );
    ui.hide().unwrap();
}

#[test]
fn range_prefetch_waits_for_search_modal_and_task_restore_guards() {
    let ui = show(200);
    let requests = observe_ranges(&ui, true);
    ui.set_can_next(true);
    let target = bottom_y(&ui) + result_height(&ui) * 1.8;
    ui.invoke_restore_result_window_viewport(target);
    settle(20);
    assert!(requests.borrow().is_empty());

    ui.set_searching(true);
    wheel(&ui, 0.0, -80.0);
    assert!(requests.borrow().is_empty());
    ui.set_searching(false);
    ui.set_library_busy(true);
    wheel(&ui, 0.0, -80.0);
    assert!(requests.borrow().is_empty());
    ui.set_library_busy(false);
    ui.set_settings_visible(true);
    wheel(&ui, 0.0, -80.0);
    assert!(requests.borrow().is_empty());
    ui.set_settings_visible(false);
    ui.set_task_restoring(true);
    wheel(&ui, 0.0, -80.0);
    assert!(requests.borrow().is_empty());
    ui.set_task_restoring(false);
    wheel(&ui, 0.0, -80.0);
    assert_eq!(
        *requests.borrow(),
        [1],
        "Prefetch resumes after the guards clear"
    );
    ui.hide().unwrap();
}

#[test]
fn appending_rows_keeps_the_visible_content_and_other_pane_scrolls_in_place() {
    let ui = show(200);
    let requests = observe_ranges(&ui, true);
    ui.set_can_next(true);
    ui.set_table_name_width(1_000.0);
    ui.set_folders(ModelRc::new(VecModel::from(
        (0..80)
            .map(|index| FolderRow {
                name: format!("Folder {index:03}").into(),
                path: format!("C:/Sounds/{index:03}").into(),
                count: "1".into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    settle(20);
    ui.invoke_restore_search_task_viewport(-600.0, -200.0, -120.0, -80.0, -60.0);
    settle(20);
    let target = bottom_y(&ui) + result_height(&ui);
    wheel(&ui, 0.0, target - ui.get_task_result_scroll_y());
    assert_eq!(*requests.borrow(), [1]);
    let old_y = ui.get_task_result_scroll_y();
    let old_visible_id = first_visible_id(&ui);
    let other_scrolls = (
        ui.get_task_result_scroll_x(),
        ui.get_task_folder_scroll_y(),
        ui.get_task_filter_scroll_y(),
        ui.get_task_inspector_scroll_y(),
    );
    assert!(other_scrolls.0 < -40.0);
    assert!(other_scrolls.1 < -40.0);
    ui.set_task_restoring(true);
    ui.set_rows(ModelRc::new(VecModel::from(rows(0, 400))));
    ui.invoke_restore_result_window_viewport(old_y);
    ui.set_searching(false);
    settle(20);
    near(ui.get_task_result_scroll_y(), old_y);
    assert_eq!(first_visible_id(&ui), old_visible_id);
    near(ui.get_task_result_scroll_x(), other_scrolls.0);
    near(ui.get_task_folder_scroll_y(), other_scrolls.1);
    near(ui.get_task_filter_scroll_y(), other_scrolls.2);
    near(ui.get_task_inspector_scroll_y(), other_scrolls.3);
    assert_eq!(
        *requests.borrow(),
        [1],
        "Publishing rows cannot start another range request"
    );
    wheel(&ui, 0.0, -120.0);
    let continued_y = ui.get_task_result_scroll_y();
    assert!(
        continued_y < old_y - 40.0,
        "The same scroll continues into the appended rows"
    );
    settle(300);
    near(ui.get_task_result_scroll_y(), continued_y);
    assert_eq!(*requests.borrow(), [1]);
    ui.hide().unwrap();
}

#[test]
fn wheel_input_takes_over_an_appended_windows_pending_viewport_restore() {
    let ui = show(200);
    wheel(&ui, 0.0, -1_200.0);
    let old_y = ui.get_task_result_scroll_y();
    assert!(old_y < -1_000.0);
    ui.set_task_restoring(true);
    ui.set_rows(ModelRc::new(VecModel::from(rows(0, 400))));
    ui.invoke_restore_result_window_viewport(old_y);
    // The append's restore is still waiting for its layout timer. Real input
    // must take over before that timer can reapply the older scroll position.
    wheel(&ui, 0.0, -180.0);
    let continued_y = ui.get_task_result_scroll_y();
    assert!(
        continued_y < old_y - 40.0,
        "Wheel input must continue beyond the pending restored position"
    );
    settle(300);
    near(ui.get_task_result_scroll_y(), continued_y);
    ui.hide().unwrap();
}

#[test]
fn a_reindexed_selection_cannot_reveal_over_an_appended_windows_restored_viewport() {
    let ui = show(200);
    wheel(&ui, 0.0, -1_200.0);
    let old_y = ui.get_task_result_scroll_y();
    assert!(old_y < -1_000.0);
    ui.set_task_restoring(true);
    ui.set_rows(ModelRc::new(VecModel::from(rows(0, 400))));
    ui.invoke_restore_result_window_viewport(old_y);
    ui.set_selected_row(0);
    settle(20);
    near(ui.get_task_result_scroll_y(), old_y);
    settle(300);
    near(ui.get_task_result_scroll_y(), old_y);
    ui.hide().unwrap();
}

#[test]
fn removing_the_first_200_rows_compensates_the_viewport_without_changing_visible_content() {
    let ui = show(600);
    let requests = observe_ranges(&ui, false);
    ui.set_can_next(true);
    ui.set_can_previous(true);
    let row_height = ui.get_task_result_row_height();
    ui.invoke_restore_result_window_viewport(-350.0 * row_height - 7.0);
    settle(20);
    let old_y = ui.get_task_result_scroll_y();
    let visible_id = first_visible_id(&ui);
    assert_eq!(visible_id, "350");
    let compensated_y = old_y + 200.0 * row_height;
    ui.set_task_restoring(true);
    ui.set_rows(ModelRc::new(VecModel::from(rows(200, 600))));
    ui.invoke_restore_result_window_viewport(compensated_y);
    settle(20);
    near(ui.get_task_result_scroll_y(), compensated_y);
    assert_eq!(first_visible_id(&ui), visible_id);
    assert!(
        requests.borrow().is_empty(),
        "Window eviction and compensation cannot prefetch by themselves"
    );
    wheel(&ui, 0.0, -120.0);
    let continued_y = ui.get_task_result_scroll_y();
    assert!(continued_y < compensated_y - 40.0);
    settle(300);
    near(ui.get_task_result_scroll_y(), continued_y);
    ui.hide().unwrap();
}

#[test]
fn replacing_models_and_restoring_boundaries_do_not_request_ranges() {
    let ui = show(200);
    let requests = observe_ranges(&ui, false);
    ui.set_can_next(true);
    ui.set_can_previous(true);
    for y in [bottom_y(&ui), 0.0, -100_000.0] {
        ui.invoke_restore_result_window_viewport(y);
        settle(20);
        assert!(
            requests.borrow().is_empty(),
            "Restoring a boundary requires no user scrolling"
        );
    }
    ui.set_rows(ModelRc::new(VecModel::from(rows(0, 400))));
    settle(20);
    assert!(
        requests.borrow().is_empty(),
        "Model replacement cannot start a range request"
    );
    ui.hide().unwrap();
}

#[test]
fn a_short_result_window_does_not_prefetch_when_the_wheel_cannot_scroll_it() {
    let ui = show(5);
    let requests = observe_ranges(&ui, false);
    ui.set_can_next(true);
    ui.set_can_previous(true);
    wheel(&ui, 0.0, -120.0);
    wheel(&ui, 0.0, 120.0);
    assert!(requests.borrow().is_empty());
    near(ui.get_task_result_scroll_y(), 0.0);
    ui.hide().unwrap();
}

fn show(count: usize) -> AppWindow {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_list_waveforms_server(false);
    ui.set_rows(ModelRc::new(VecModel::from(rows(0, count))));
    ui.show().unwrap();
    settle(20);
    ui
}

fn rows(start: usize, count: usize) -> Vec<AudioRow> {
    (start..start + count)
        .map(|index| AudioRow {
            id: index.to_string().into(),
            name: format!("Sound {index:03}.wav").into(),
            ..Default::default()
        })
        .collect()
}

fn observe_ranges(ui: &AppWindow, mark_searching: bool) -> Rc<RefCell<Vec<i32>>> {
    let requests = Rc::new(RefCell::new(Vec::new()));
    let observed = requests.clone();
    let weak = ui.as_weak();
    ui.on_result_range_requested(move |direction| {
        observed.borrow_mut().push(direction);
        if mark_searching {
            weak.upgrade().unwrap().set_searching(true);
        }
    });
    requests
}

fn result_view(ui: &AppWindow) -> ElementHandle {
    ElementHandle::find_by_element_id(ui, "AppWindow::results-focus")
        .next()
        .expect("result view")
}

fn result_height(ui: &AppWindow) -> f32 {
    result_view(ui).size().height
}

fn bottom_y(ui: &AppWindow) -> f32 {
    (result_height(ui) - ui.get_rows().row_count() as f32 * ui.get_task_result_row_height())
        .min(0.0)
}

fn first_visible_id(ui: &AppWindow) -> String {
    let index = (-ui.get_task_result_scroll_y() / ui.get_task_result_row_height()).floor() as usize;
    ui.get_rows()
        .row_data(index)
        .expect("visible row")
        .id
        .to_string()
}

fn wheel(ui: &AppWindow, delta_x: f32, delta_y: f32) {
    let origin = result_view(ui).absolute_position();
    let position = slint::LogicalPosition::new(origin.x + 30.0, origin.y + 30.0);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x,
        delta_y,
    });
    settle(250);
}

fn settle(ms: u64) {
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(ms));
}

fn near(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() < 2.0,
        "Expected {expected}, got {actual}"
    );
}
