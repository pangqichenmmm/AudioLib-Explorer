// app/audiolib-desktop/src/ui_tests.rs
//! UI regression checks use an isolated Slint backend, with no audio or user database.
use crate::{AppWindow, AudioRow, FolderRow, SourceRow};
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::{ComponentHandle, Model, ModelRc, VecModel, platform::WindowEvent};
use std::{cell::RefCell, rc::Rc, time::Duration};

#[test]
fn mirror_folder_action_requires_an_existing_cache_and_remains_available_during_sync() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window()
            .set_size(slint::LogicalSize::new(1400.0, 1000.0));
        ui.set_chinese(chinese);
        ui.set_sm_sources(source_rows(true));
        ui.set_remote_mode(true);
        ui.invoke_open_library(0);
        ui.set_sm_selected(1);
        ui.set_sm_page(2);
        let calls = Rc::new(RefCell::new(0));
        let observed = calls.clone();
        ui.on_sm_open_cache_requested(move || *observed.borrow_mut() += 1);
        ui.show().unwrap();
        let label = if chinese {
            "打开镜像目录"
        } else {
            "Open mirror folder"
        };
        click_label(&ui, label);
        assert_eq!(*calls.borrow(), 0, "a missing cache cannot be opened");
        ui.set_sm_cache_available(true);
        ui.set_ds_syncing(true);
        click_label(&ui, label);
        assert_eq!(*calls.borrow(), 1);
        ui.set_sm_cache_available(false);
        click_label(&ui, label);
        assert_eq!(
            *calls.borrow(),
            1,
            "changing to a missing cache disables the action"
        );
        ui.hide().unwrap();
    }
}

#[test]
fn snapshot_sync_controls_pause_once_and_offer_continue_and_restart_on_both_pages() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        for page in [0, 2] {
            let ui = AppWindow::new().unwrap();
            ui.window()
                .set_size(slint::LogicalSize::new(1400.0, 1000.0));
            ui.set_chinese(chinese);
            ui.set_sm_sources(source_rows(true));
            ui.set_remote_mode(true);
            ui.invoke_open_library(0);
            ui.set_sm_selected(1);
            ui.set_sm_page(page);
            ui.set_ds_syncing(true);
            let calls = Rc::new(RefCell::new(Vec::new()));
            let observed = calls.clone();
            let weak = ui.as_weak();
            ui.on_data_source_sync_pause_requested(move || {
                observed.borrow_mut().push("pause");
                weak.upgrade().unwrap().set_ds_sync_pausing(true);
            });
            let observed = calls.clone();
            ui.on_data_source_sync_requested(move || observed.borrow_mut().push("continue"));
            let observed = calls.clone();
            ui.on_data_source_sync_restart_requested(move || observed.borrow_mut().push("restart"));
            ui.show().unwrap();
            let pause = if chinese {
                "暂停同步"
            } else {
                "Pause sync"
            };
            click_label(&ui, pause);
            assert!(calls.borrow().is_empty(), "publication cannot be paused");
            ui.set_ds_sync_can_pause(true);
            if page == 0 {
                // The first source activation keeps its existing settings gate;
                // cancellation must remain reachable while its provider is pending.
                ui.set_library_busy(true);
                ui.set_ds_applying(true);
            }
            click_label(&ui, pause);
            click_label(
                &ui,
                if chinese {
                    "正在暂停…"
                } else {
                    "Pausing…"
                },
            );
            assert_eq!(&*calls.borrow(), &["pause"]);
            ui.set_library_busy(false);
            ui.set_ds_applying(false);
            ui.set_ds_syncing(false);
            ui.set_ds_sync_pausing(false);
            ui.set_ds_sync_paused(true);
            ui.set_ds_sync_pending(true);
            click_label(
                &ui,
                if chinese {
                    "继续同步"
                } else {
                    "Continue sync"
                },
            );
            click_label(
                &ui,
                if chinese {
                    "重新下载"
                } else {
                    "Restart download"
                },
            );
            assert_eq!(&*calls.borrow(), &["pause", "continue", "restart"]);
            ui.hide().unwrap();
        }
    }
}

#[test]
fn server_cache_page_hides_local_scan_buttons_and_keeps_audition_hold_option() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window()
            .set_size(slint::LogicalSize::new(1400.0, 1000.0));
        ui.set_chinese(chinese);
        ui.set_sm_sources(source_rows(true));
        ui.set_remote_mode(true);
        ui.invoke_open_library(0);
        ui.set_sm_selected(1);
        ui.set_sm_page(2);
        ui.show().unwrap();

        let (add, incr, full, hold, scan_note, audition) = if chinese {
            (
                "添加文件夹…",
                "增量扫描…",
                "全量扫描…",
                "试听开始后 3 秒内暂缓列表波形批量请求",
                "扫描请到「服务器音效库」页。",
                "试听/性能",
            )
        } else {
            (
                "Add folder…",
                "Incremental scan…",
                "Full scan…",
                "Hold list-waveform batch requests during first 3 s after audition start",
                "Scans are on the Server library page.",
                "Audition / performance",
            )
        };
        for label in [add, incr, full] {
            assert!(
                !has_label(&ui, label),
                "remote cache page must hide dead scan control: {label}"
            );
        }
        assert!(
            !has_label(
                &ui,
                if chinese {
                    "扫描与更新"
                } else {
                    "Scan and update"
                }
            ),
            "remote cache page must not keep a fake Scan and update section"
        );
        assert!(
            has_label(&ui, scan_note),
            "missing short scan redirect note"
        );
        assert!(
            has_label(&ui, audition),
            "missing Audition / performance section"
        );
        assert!(
            has_label(&ui, hold),
            "hold list-waveform option must remain"
        );

        // Local settings still expose the scan controls unchanged.
        ui.set_remote_mode(false);
        ui.set_sm_selected(0);
        ui.set_sm_page(0);
        for label in [add, incr, full] {
            assert!(
                has_label(&ui, label),
                "local settings must keep scan control: {label}"
            );
        }
        assert!(
            has_label(
                &ui,
                if chinese {
                    "扫描与更新"
                } else {
                    "Scan and update"
                }
            ),
            "local settings must keep Scan and update"
        );
        assert!(
            !has_label(&ui, hold),
            "hold list-waveform option is server-mode only"
        );
        ui.hide().unwrap();
    }
}

#[test]
fn keyboard_selection_keeps_compact_result_rows_visible() {
    i_slint_backend_testing::init_no_event_loop();
    for (width, height) in [(1080.0, 700.0), (1440.0, 900.0)] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(width, height));
        ui.set_rows(ModelRc::new(VecModel::from(
            (0..120)
                .map(|index| AudioRow {
                    id: index.to_string().into(),
                    name: format!("Scroll {index}.wav").into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )));
        ui.set_selected_row(0);
        let weak = ui.as_weak();
        ui.on_selection_step_requested(move |step| {
            let ui = weak.upgrade().unwrap();
            ui.set_selected_row((ui.get_selected_row() + step).clamp(0, 119));
        });
        ui.show().unwrap();
        click_label(&ui, "Scroll 0.wav");
        for (key, targets) in [
            (
                slint::platform::Key::DownArrow,
                (1..=95).collect::<Vec<_>>(),
            ),
            (slint::platform::Key::UpArrow, (0..95).rev().collect()),
        ] {
            for target in targets {
                send_keys(&ui, &[key.into()]);
                i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
                assert_eq!(ui.get_selected_row(), target);
                let list = ElementHandle::find_by_element_id(&ui, "AppWindow::result-list")
                    .next()
                    .unwrap();
                let row =
                    ElementHandle::find_by_accessible_label(&ui, &format!("Scroll {target}.wav"))
                        .next()
                        .unwrap_or_else(|| {
                            panic!("Selected row {target} is outside the virtualized view")
                        });
                let top = row.absolute_position().y;
                let bottom = top + row.size().height;
                let view_top = list.absolute_position().y;
                let view_bottom = view_top + list.size().height;
                assert!(
                    top >= view_top - 1.0 && bottom <= view_bottom + 1.0,
                    "Selected row {target} spans {top}..{bottom}, outside {view_top}..{view_bottom} at {width}x{height}"
                );
            }
        }
        ui.hide().unwrap();
    }
}

#[test]
fn scan_progress_controls_pause_resume_cancel_and_guard_terminal_phases() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.set_chinese(chinese);
        ui.window().set_size(slint::LogicalSize::new(1100.0, 780.0));
        ui.set_scanning(true);
        ui.set_scan_summary("100000 sounds checked · 99872 unchanged".into());
        ui.set_scan_current_path("N:/Sounds/Large library/aircraft.wav".into());
        let calls = Rc::new(RefCell::new(Vec::new()));
        let observed = calls.clone();
        let weak = ui.as_weak();
        ui.on_scan_pause_requested(move || {
            let ui = weak.upgrade().unwrap();
            observed.borrow_mut().push("toggle");
            ui.set_scan_paused(!ui.get_scan_paused());
        });
        let observed = calls.clone();
        let weak = ui.as_weak();
        ui.on_scan_cancel_requested(move || {
            observed.borrow_mut().push("cancel");
            weak.upgrade().unwrap().set_scan_cancelling(true);
        });
        ui.show().unwrap();
        let pause = if chinese {
            "暂停扫描"
        } else {
            "Pause scan"
        };
        let resume = if chinese {
            "继续扫描"
        } else {
            "Resume scan"
        };
        let cancel = if chinese {
            "取消扫描"
        } else {
            "Cancel scan"
        };
        ui.set_scan_validating(true);
        click_label(&ui, pause);
        assert!(calls.borrow().is_empty());
        ui.set_scan_validating(false);
        click_label(&ui, pause);
        assert!(ui.get_scan_paused());
        click_label(&ui, resume);
        assert!(!ui.get_scan_paused());
        click_menu_item(&ui, if chinese { "扫描" } else { "Scan" }, pause);
        assert!(ui.get_scan_paused());
        click_label(&ui, cancel);
        click_label(&ui, resume);
        assert_eq!(&*calls.borrow(), &["toggle", "toggle", "toggle", "cancel"]);
        ui.set_scan_cancelling(false);
        ui.set_scan_finalizing(true);
        click_label(&ui, resume);
        click_label(&ui, cancel);
        assert_eq!(
            calls.borrow().len(),
            4,
            "final reconciliation disables control requests"
        );
        ui.set_scanning(false);
        assert!(
            ElementHandle::find_by_accessible_label(&ui, cancel)
                .next()
                .is_none()
        );
        ui.hide().unwrap();
    }
}

#[test]
fn audition_buttons_have_distinct_transport_actions_and_submit_transform_settings() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_audio_ready(true);
    ui.set_pitch_text("7.00".into());
    ui.set_speed_text("0.50".into());
    let actions = Rc::new(RefCell::new(Vec::new()));
    let copy = actions.clone();
    ui.on_start_requested(move || copy.borrow_mut().push("play".to_string()));
    let copy = actions.clone();
    ui.on_pause_requested(move || copy.borrow_mut().push("pause".to_string()));
    let copy = actions.clone();
    ui.on_transform_requested(move |pitch, rate, mono, repeat| {
        copy.borrow_mut()
            .push(format!("{pitch}/{rate}/{mono}/{repeat}"));
    });
    ui.show().unwrap();
    for label in ["Play", "Pause", "Mono monitoring", "Repeat sound"] {
        let item = ElementHandle::find_by_accessible_label(&ui, label)
            .next()
            .expect(label);
        let origin = item.absolute_position();
        let size = item.size();
        let position =
            slint::LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0);
        ui.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
    }
    assert_eq!(
        &*actions.borrow(),
        &[
            "play",
            "pause",
            "7.00/0.50/true/false",
            "7.00/0.50/true/true"
        ]
    );
    ui.hide().unwrap();
}

#[test]
fn single_click_play_checkbox_is_visible_and_clickable_in_both_languages_and_layouts() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        for (width, height) in [(1080.0, 700.0), (1440.0, 900.0)] {
            let ui = AppWindow::new().unwrap();
            ui.window().set_size(slint::LogicalSize::new(width, height));
            ui.set_chinese(chinese);
            assert!(!ui.get_audition_play_on_single_click());
            let changes = Rc::new(RefCell::new(Vec::new()));
            let observed = changes.clone();
            ui.on_single_click_play_requested(move |enabled| {
                observed.borrow_mut().push(enabled);
            });
            ui.show().unwrap();
            let label = if chinese {
                "单击播放"
            } else {
                "Single-click play"
            };
            let checkbox = ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .unwrap_or_else(|| panic!("Missing checkbox: {label}"));
            let origin = checkbox.absolute_position();
            let size = checkbox.size();
            assert!(size.width > 0.0 && size.height > 0.0, "{label} is hidden");
            assert!(
                origin.x >= 0.0 && origin.x + size.width <= width,
                "{label} is horizontally clipped at {width}x{height}"
            );
            assert!(
                origin.y >= 0.0 && origin.y + size.height <= height,
                "{label} is vertically clipped at {width}x{height}"
            );
            click_label(&ui, label);
            assert!(ui.get_audition_play_on_single_click());
            assert_eq!(&*changes.borrow(), &[true]);
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
            click_label(&ui, label);
            assert!(!ui.get_audition_play_on_single_click());
            assert_eq!(&*changes.borrow(), &[true, false]);
            ui.hide().unwrap();
        }
    }
}

#[test]
fn result_click_preview_mode_preserves_double_click_and_keyboard_selection() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_rows(ModelRc::new(VecModel::from(vec![
        AudioRow {
            id: "42".into(),
            name: "First click.wav".into(),
            ..Default::default()
        },
        AudioRow {
            id: "43".into(),
            name: "Next selection.wav".into(),
            ..Default::default()
        },
    ])));
    let clicks = Rc::new(RefCell::new(Vec::new()));
    let observed = clicks.clone();
    let weak = ui.as_weak();
    ui.on_row_clicked(move |index, toggle, range, play| {
        observed.borrow_mut().push((index, toggle, range, play));
        weak.upgrade().unwrap().set_selected_row(index);
    });
    let auditions = Rc::new(RefCell::new(Vec::new()));
    let observed = auditions.clone();
    ui.on_audition_requested(move |index| observed.borrow_mut().push(index));
    let steps = Rc::new(RefCell::new(Vec::new()));
    let observed = steps.clone();
    let weak = ui.as_weak();
    ui.on_selection_step_requested(move |step| {
        observed.borrow_mut().push(step);
        let ui = weak.upgrade().unwrap();
        ui.set_selected_row((ui.get_selected_row() + step).clamp(0, 1));
    });
    ui.show().unwrap();
    assert!(!ui.get_audition_play_on_single_click());
    let position = result_row_position(&ui, "First click.wav");
    click_at(&ui, position);
    assert_eq!(&*clicks.borrow(), &[(0, false, false, false)]);
    assert!(
        auditions.borrow().is_empty(),
        "One click only selects by default"
    );
    // The second release must occur inside the real double-click interval.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
    click_at(&ui, position);
    assert_eq!(&*auditions.borrow(), &[0]);
    assert!(clicks.borrow().iter().all(|request| !request.3));

    ui.set_audition_play_on_single_click(true);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
    clicks.borrow_mut().clear();
    auditions.borrow_mut().clear();
    click_at(&ui, position);
    assert_eq!(&*clicks.borrow(), &[(0, false, false, true)]);
    assert!(auditions.borrow().is_empty());
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
    click_at(&ui, position);
    assert!(
        auditions.borrow().is_empty(),
        "Single-click mode must not request extra double-click playback"
    );
    assert!(clicks.borrow().iter().all(|request| request.3));

    clicks.borrow_mut().clear();
    send_keys(&ui, &[slint::platform::Key::DownArrow.into()]);
    assert_eq!(ui.get_selected_row(), 1);
    send_keys(&ui, &[slint::platform::Key::UpArrow.into()]);
    assert_eq!(ui.get_selected_row(), 0);
    assert_eq!(&*steps.borrow(), &[1, -1]);
    assert!(clicks.borrow().is_empty());
    send_keys(
        &ui,
        &[
            slint::platform::Key::Shift.into(),
            slint::platform::Key::DownArrow.into(),
        ],
    );
    assert_eq!(&*clicks.borrow(), &[(1, false, true, false)]);
    assert!(
        auditions.borrow().is_empty(),
        "Keyboard selection remains silent"
    );
    ui.hide().unwrap();
}

#[test]
fn missing_result_rows_never_preview_in_either_click_mode() {
    i_slint_backend_testing::init_no_event_loop();
    for enabled in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
        ui.set_audition_play_on_single_click(enabled);
        ui.set_rows(ModelRc::new(VecModel::from(vec![AudioRow {
            id: "42".into(),
            name: "Missing preview.wav".into(),
            missing: true,
            ..Default::default()
        }])));
        let clicks = Rc::new(RefCell::new(Vec::new()));
        let observed = clicks.clone();
        ui.on_row_clicked(move |index, toggle, range, play| {
            observed.borrow_mut().push((index, toggle, range, play));
        });
        ui.on_audition_requested(|_| panic!("A missing source cannot be auditioned"));
        ui.show().unwrap();
        let position = result_row_position(&ui, "Missing preview.wav");
        click_at(&ui, position);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
        click_at(&ui, position);
        assert!(!clicks.borrow().is_empty());
        assert!(clicks.borrow().iter().all(|request| !request.3));
        ui.hide().unwrap();
    }
}

fn result_row_position(ui: &AppWindow, label: &str) -> slint::LogicalPosition {
    let row = ElementHandle::find_by_accessible_label(ui, label)
        .find(|element| element.accessible_role() == Some(AccessibleRole::ListItem))
        .unwrap_or_else(|| panic!("Missing result row: {label}"));
    let origin = row.absolute_position();
    slint::LogicalPosition::new(origin.x + 8.0, origin.y + row.size().height / 2.0)
}

pub(crate) fn click_label(ui: &AppWindow, label: &str) {
    let element = ElementHandle::find_by_accessible_label(ui, label)
        .next()
        .unwrap_or_else(|| panic!("Missing action: {label}"));
    let origin = element.absolute_position();
    let size = element.size();
    let position =
        slint::LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
}

fn click_at(ui: &AppWindow, position: slint::LogicalPosition) {
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
}

fn menu_bar_entry(ui: &AppWindow, title: &str) -> ElementHandle {
    ElementHandle::find_by_element_type_name(ui, "MenuBarItem")
        .find(|entry| {
            let title = title.to_owned();
            entry
                .query_descendants()
                .match_predicate(move |element| {
                    element
                        .accessible_label()
                        .is_some_and(|label| label == title)
                })
                .find_first()
                .is_some()
        })
        .unwrap_or_else(|| panic!("Missing menu bar entry: {title}"))
}

fn open_menu(ui: &AppWindow, title: &str) -> slint::LogicalPosition {
    let entry = menu_bar_entry(ui, title);
    let origin = entry.absolute_position();
    let size = entry.size();
    click_at(
        ui,
        slint::LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0),
    );
    // Slint's fallback popup positions are relative to the menu bar entry.
    slint::LogicalPosition::new(origin.x, origin.y + size.height)
}

fn click_menu_item(ui: &AppWindow, title: &str, label: &str) {
    let popup = open_menu(ui, title);
    let item = ElementHandle::find_by_accessible_label(ui, label)
        .next()
        .unwrap_or_else(|| panic!("Missing {title} menu item: {label}"));
    let origin = item.absolute_position();
    let size = item.size();
    click_at(
        ui,
        slint::LogicalPosition::new(
            popup.x + origin.x + size.width / 2.0,
            popup.y + origin.y + size.height / 2.0,
        ),
    );
}

// Fallback submenus open beside their entry; their items report popup-relative positions.
fn click_submenu_item(ui: &AppWindow, title: &str, submenu: &str, label: &str) {
    let popup = open_menu(ui, title);
    let entry = ElementHandle::find_by_accessible_label(ui, submenu)
        .next()
        .unwrap_or_else(|| panic!("Missing {title} submenu: {submenu}"));
    let (origin, size) = (entry.absolute_position(), entry.size());
    click_at(
        ui,
        slint::LogicalPosition::new(
            popup.x + origin.x + size.width / 2.0,
            popup.y + origin.y + size.height / 2.0,
        ),
    );
    let nested = slint::LogicalPosition::new(popup.x + origin.x + size.width, popup.y + origin.y);
    let item = ElementHandle::find_by_accessible_label(ui, label)
        .find(|item| item.absolute_position().x < origin.x + size.width / 2.0)
        .unwrap_or_else(|| panic!("Missing {submenu} item: {label}"));
    let (item_origin, item_size) = (item.absolute_position(), item.size());
    click_at(
        ui,
        slint::LogicalPosition::new(
            nested.x + item_origin.x + item_size.width / 2.0,
            nested.y + item_origin.y + item_size.height / 2.0,
        ),
    );
}

#[test]
fn library_menu_file_status_and_view_sort_show_a_footer_reset() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    let searches = Rc::new(RefCell::new(0));
    let observed = searches.clone();
    ui.on_filter_requested(move || *observed.borrow_mut() += 1);
    ui.show().unwrap();
    click_submenu_item(&ui, "Library", "Show files: Present files", "Missing files");
    assert_eq!(ui.get_visibility_index(), 1);
    assert_eq!(*searches.borrow(), 1);
    // The footer keeps the menu-only scope visible and restores it in one click.
    click_label(&ui, "Show present files");
    assert_eq!(ui.get_visibility_index(), 0);
    assert_eq!(*searches.borrow(), 2);
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "Show present files")
            .next()
            .is_none()
    );
    // Modification time has no column, so the View menu sorts by it.
    click_submenu_item(&ui, "View", "Sort: Default", "Modified (newest first)");
    assert_eq!((ui.get_sort_index(), ui.get_sort_descending()), (6, true));
    click_label(&ui, "Restore default order");
    assert_eq!((ui.get_sort_index(), ui.get_sort_descending()), (0, false));
    assert_eq!(*searches.borrow(), 4);
    ui.hide().unwrap();
}

pub(crate) fn send_keys(ui: &AppWindow, keys: &[slint::SharedString]) {
    for key in keys {
        ui.window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.clone() });
    }
    for key in keys.iter().rev() {
        ui.window()
            .dispatch_event(WindowEvent::KeyReleased { text: key.clone() });
    }
}

fn close_menu(ui: &AppWindow) {
    send_keys(ui, &[slint::platform::Key::Escape.into()]);
}

fn scroll_library_management(ui: &AppWindow, delta_y: f32) {
    let scroll = ElementHandle::find_by_element_id(ui, "AppWindow::library-scroll")
        .next()
        .expect("library management scroll area");
    let origin = scroll.absolute_position();
    let size = scroll.size();
    let position = slint::LogicalPosition::new(origin.x + size.width - 24.0, origin.y + 12.0);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x: 0.0,
        delta_y,
    });
}

#[test]
fn menu_bar_library_workflows_hide_technical_actions_in_both_languages() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
    let actions = Rc::new(RefCell::new(Vec::new()));
    let observed = actions.clone();
    ui.on_library_action(move |action| observed.borrow_mut().push(action));
    ui.show().unwrap();
    for chinese in [false, true] {
        ui.set_chinese(chinese);
        let titles = if chinese {
            ["文件", "编辑", "音效库", "扫描", "视图", "播放", "帮助"]
        } else {
            [
                "File", "Edit", "Library", "Scan", "View", "Playback", "Help",
            ]
        };
        for title in titles {
            let entry = menu_bar_entry(&ui, title);
            let origin = entry.absolute_position();
            let size = entry.size();
            assert!(origin.x >= 0.0 && origin.x + size.width <= 1080.0);
            assert!(origin.y >= 0.0 && origin.y + size.height <= 700.0);
            open_menu(&ui, title);
            close_menu(&ui);
        }
        let file = titles[0];
        let library = titles[2];
        let hidden_file_actions = if chinese {
            [
                "新建数据库…",
                "打开数据库…",
                "导入标注…",
                "JSONL…",
                "CSV…",
                "添加声音文件夹…",
                "数据源与音效库…",
                "数据源设置…",
            ]
        } else {
            [
                "Add sound folder…",
                "Data sources and library…",
                "Data source settings…",
                "New database…",
                "Open database…",
                "Import annotations…",
                "JSONL…",
                "CSV…",
            ]
        };
        open_menu(&ui, file);
        for label in hidden_file_actions {
            assert!(
                ElementHandle::find_by_accessible_label(&ui, label)
                    .next()
                    .is_none(),
                "Technical File action must be hidden: {label}"
            );
        }
        close_menu(&ui);
        open_menu(&ui, library);
        for label in if chinese {
            [
                "备份当前数据库…",
                "从备份恢复…",
                "校验数据库 / FTS5",
                "优化数据库",
                "重建 FTS5 搜索索引",
            ]
        } else {
            [
                "Backup current database…",
                "Restore from backup…",
                "Check database / FTS5",
                "Optimize database",
                "Rebuild FTS5 search index",
            ]
        } {
            assert!(
                ElementHandle::find_by_accessible_label(&ui, label)
                    .next()
                    .is_none(),
                "Technical Library action must be hidden: {label}"
            );
        }
        close_menu(&ui);
        actions.borrow_mut().clear();
        click_menu_item(
            &ui,
            library,
            if chinese {
                "音效库管理…"
            } else {
                "Manage sound library…"
            },
        );
        assert_eq!(&*actions.borrow(), &[0]);
        assert!(ui.get_library_visible());
        ui.set_library_visible(false);
        for (label, kind) in if chinese {
            [("扫描历史…", 0), ("最近播放…", 1)]
        } else {
            [("Scan history…", 0), ("Recent playback…", 1)]
        } {
            actions.borrow_mut().clear();
            click_menu_item(&ui, library, label);
            assert_eq!(&*actions.borrow(), &[0]);
            assert_eq!(ui.get_history_kind(), kind);
            assert!(ui.get_library_visible());
            ui.set_library_visible(false);
        }
    }
    ui.hide().unwrap();
}

#[test]
fn library_management_prioritizes_scanning_and_collapses_advanced_maintenance() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
        ui.set_chinese(chinese);
        let actions = Rc::new(RefCell::new(Vec::new()));
        let observed = actions.clone();
        ui.on_library_action(move |action| observed.borrow_mut().push(action));
        ui.show().unwrap();
        let manage = if chinese {
            "管理音效库"
        } else {
            "Manage library"
        };
        click_label(&ui, manage);
        assert!(ui.get_library_visible());
        assert_eq!(&*actions.borrow(), &[0]);
        let advanced = if chinese {
            "高级维护 ▸"
        } else {
            "Advanced maintenance ▸"
        };
        let expanded = if chinese {
            "高级维护 ▾"
        } else {
            "Advanced maintenance ▾"
        };
        let technical_actions = if chinese {
            [
                ("新建库…", 6),
                ("打开库…", 7),
                ("备份…", 1),
                ("恢复…", 2),
                ("校验 / FTS5", 3),
                ("优化", 4),
                ("重建搜索", 5),
                ("导出全库 JSONL…", 8),
                ("导出全库 CSV…", 9),
                ("导入标注…", 10),
            ]
        } else {
            [
                ("New…", 6),
                ("Open…", 7),
                ("Backup…", 1),
                ("Restore…", 2),
                ("Check / FTS5", 3),
                ("Optimize", 4),
                ("Rebuild FTS5", 5),
                ("Export all JSONL…", 8),
                ("Export all CSV…", 9),
                ("Import annotations…", 10),
            ]
        };
        for (label, _) in technical_actions {
            assert!(
                ElementHandle::find_by_accessible_label(&ui, label)
                    .next()
                    .is_none(),
                "Default management view exposes technical action: {label}"
            );
        }
        for label in if chinese {
            ["添加文件夹…", "增量扫描…", "全量扫描…"]
        } else {
            ["Add folder…", "Incremental scan…", "Full scan…"]
        } {
            let element = ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .expect(label);
            let origin = element.absolute_position();
            let size = element.size();
            assert!(
                origin.x >= 0.0 && origin.x + size.width <= 1080.0,
                "{label} is horizontally clipped"
            );
            assert!(
                origin.y >= 0.0 && origin.y + size.height <= 700.0,
                "{label} is vertically clipped"
            );
        }
        let remove_root = if chinese {
            "从库中移除此根目录…"
        } else {
            "Remove root from library…"
        };
        let remove = ElementHandle::find_by_accessible_label(&ui, remove_root)
            .next()
            .expect(remove_root);
        assert_eq!(
            remove.accessible_enabled(),
            Some(false),
            "Remove root stays disabled with an empty selection"
        );
        // Scan-roots list sits above repair; scroll so the maintenance action stays reachable.
        scroll_library_management(&ui, -400.0);
        let repair = if chinese {
            "修复文件引用"
        } else {
            "Repair file references"
        };
        ElementHandle::find_by_accessible_label(&ui, repair)
            .next()
            .expect(repair);
        scroll_library_management(&ui, -5000.0);
        click_label(&ui, advanced);
        scroll_library_management(&ui, -5000.0);
        actions.borrow_mut().clear();
        for (label, action) in technical_actions {
            click_label(&ui, label);
            assert_eq!(actions.borrow().last(), Some(&action), "{label}");
        }
        click_label(&ui, expanded);
        for (label, _) in technical_actions {
            assert!(
                ElementHandle::find_by_accessible_label(&ui, label)
                    .next()
                    .is_none(),
                "Collapsed maintenance retains action: {label}"
            );
        }
        scroll_library_management(&ui, 5000.0);
        for (label, mode) in if chinese {
            [("增量扫描…", 0), ("全量扫描…", 1)]
        } else {
            [("Incremental scan…", 0), ("Full scan…", 1)]
        } {
            click_label(&ui, label);
            assert!(!ui.get_library_visible());
            assert!(ui.get_import_visible());
            assert_eq!(ui.get_scan_mode(), mode);
            click_label(&ui, if chinese { "取消" } else { "Cancel" });
            assert!(!ui.get_import_visible());
            click_label(&ui, manage);
        }
        ui.hide().unwrap();
    }
}

#[test]
fn repair_references_shows_missing_files_across_the_library_in_both_languages() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
        ui.set_chinese(chinese);
        let searches = Rc::new(RefCell::new(Vec::new()));
        let observed = searches.clone();
        let weak = ui.as_weak();
        ui.on_all_library_requested(move || {
            let ui = weak.upgrade().unwrap();
            // The controller clears the folder scope before publishing the search.
            ui.set_selected_folder(-1);
            ui.set_selected_folder_path("".into());
            observed.borrow_mut().push((
                ui.get_query().to_string(),
                ui.get_visibility_index(),
                ui.get_favorites_only(),
            ));
        });
        ui.show().unwrap();
        for from_panel in [false, true] {
            ui.set_query("aircraft".into());
            ui.set_favorites_only(true);
            ui.set_selected_folder(4);
            ui.set_selected_folder_path("D:\\Sounds\\Aircraft".into());
            ui.set_sample_filter(4);
            ui.set_bit_filter(2);
            ui.set_channel_filter(1);
            ui.set_tagged_only(true);
            ui.set_tag_filter("engine".into());
            ui.set_color_filter("#ff0000".into());
            ui.set_duration_min("2".into());
            ui.set_duration_max("4".into());
            ui.set_visibility_index(0);
            ui.set_inspector_visible(false);
            if from_panel {
                click_label(
                    &ui,
                    if chinese {
                        "管理音效库"
                    } else {
                        "Manage library"
                    },
                );
                // The scan-roots list added in 0.7.7 moves repair below the
                // initial viewport. Reach it through the real scroll area.
                scroll_library_management(&ui, -400.0);
                click_label(
                    &ui,
                    if chinese {
                        "修复文件引用"
                    } else {
                        "Repair file references"
                    },
                );
            } else {
                click_menu_item(
                    &ui,
                    if chinese { "音效库" } else { "Library" },
                    if chinese {
                        "查看缺失文件 / 修复引用"
                    } else {
                        "Missing files / repair references"
                    },
                );
            }
            assert!(!ui.get_library_visible());
            assert!(ui.get_inspector_visible());
            assert!(ui.get_query().is_empty());
            assert!(!ui.get_favorites_only());
            assert_eq!(ui.get_visibility_index(), 1);
            assert_eq!(ui.get_selected_folder(), -1);
            assert!(ui.get_selected_folder_path().is_empty());
            assert_eq!(
                (
                    ui.get_sample_filter(),
                    ui.get_bit_filter(),
                    ui.get_channel_filter()
                ),
                (0, 0, 0)
            );
            assert!(!ui.get_tagged_only());
            assert!(ui.get_tag_filter().is_empty());
            assert!(ui.get_color_filter().is_empty());
            assert!(ui.get_duration_min().is_empty());
            assert!(ui.get_duration_max().is_empty());
            assert_eq!(
                searches.borrow().last(),
                Some(&(String::new(), 1, false)),
                "Search must see the missing-file scope after old filters are cleared"
            );
        }
        assert_eq!(searches.borrow().len(), 2);
        ui.hide().unwrap();
    }
}

#[test]
fn folder_menu_modal_focus_keeps_path_input_and_tabs_out_of_background_metadata() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
        ui.set_chinese(chinese);
        ui.set_has_selection(true);
        ui.set_selected_id("42".into());
        ui.set_edit_tags("original".into());
        let edits = Rc::new(RefCell::new(0));
        let observed = edits.clone();
        ui.on_metadata_edited(move || *observed.borrow_mut() += 1);
        let scans = Rc::new(RefCell::new(Vec::new()));
        let observed = scans.clone();
        ui.on_scan_requested(move |path| observed.borrow_mut().push(path.to_string()));
        ui.show().unwrap();
        let tags = if chinese {
            "标签 · 逗号分隔"
        } else {
            "Tags, comma separated"
        };
        click_label(&ui, tags);
        send_keys(&ui, &["x".into()]);
        let draft = ui.get_edit_tags();
        assert_ne!(draft, "original", "Calibrate the background editor focus");
        assert_eq!(*edits.borrow(), 1);
        ui.set_metadata_dirty(false);
        click_menu_item(
            &ui,
            if chinese { "扫描" } else { "Scan" },
            if chinese {
                "增量扫描文件夹…"
            } else {
                "Incremental folder scan…"
            },
        );
        assert!(ui.get_import_visible());
        // Even before the post-popup focus handoff, the former editor cannot edit.
        send_keys(&ui, &["blocked".into()]);
        assert_eq!(ui.get_edit_tags(), draft);
        assert_eq!(*edits.borrow(), 1);
        assert!(!ui.get_metadata_dirty());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        send_keys(&ui, &["N:\\New Sounds\\测试".into()]);
        assert_eq!(ui.get_scan_path(), "N:\\New Sounds\\测试");
        assert_eq!(ui.get_edit_tags(), draft);
        assert_eq!(*edits.borrow(), 1);
        assert!(!ui.get_metadata_dirty());
        send_keys(&ui, &[slint::platform::Key::Return.into()]);
        assert_eq!(&*scans.borrow(), &["N:\\New Sounds\\测试"]);
        // Both directions cycle the six scan controls and return to the path.
        for keys in [
            vec![slint::platform::Key::Tab.into()],
            vec![
                slint::platform::Key::Shift.into(),
                slint::platform::Key::Tab.into(),
            ],
        ] {
            for _ in 0..6 {
                send_keys(&ui, &keys);
            }
            send_keys(&ui, &["z".into()]);
            assert_eq!(ui.get_edit_tags(), draft);
            assert_eq!(*edits.borrow(), 1);
        }
        assert_eq!(ui.get_scan_path(), "N:\\New Sounds\\测试zz");
        send_keys(&ui, &[slint::platform::Key::Escape.into()]);
        assert!(!ui.get_import_visible());
        // With an empty path, reverse traversal must skip disabled Start to Cancel.
        click_menu_item(
            &ui,
            if chinese { "扫描" } else { "Scan" },
            if chinese {
                "增量扫描文件夹…"
            } else {
                "Incremental folder scan…"
            },
        );
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        send_keys(
            &ui,
            &[
                slint::platform::Key::Shift.into(),
                slint::platform::Key::Tab.into(),
            ],
        );
        send_keys(&ui, &[slint::platform::Key::Return.into()]);
        assert!(
            !ui.get_import_visible(),
            "Reverse Tab reaches Cancel for an empty path"
        );
        assert_eq!(*edits.borrow(), 1);
        ui.hide().unwrap();
    }
}

#[test]
fn library_menu_modal_blocks_background_audition_and_wraps_dynamic_keyboard_controls() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
        ui.set_chinese(chinese);
        ui.set_rows(ModelRc::new(VecModel::from(vec![AudioRow {
            id: "42".into(),
            name: "Modal focus.wav".into(),
            ..Default::default()
        }])));
        ui.set_selected_row(0);
        ui.set_has_selection(true);
        ui.set_selected_id("42".into());
        ui.set_audio_ready(true);
        ui.set_edit_tags("original".into());
        let calls = Rc::new(RefCell::new(Vec::new()));
        let observed = calls.clone();
        ui.on_selection_step_requested(move |step| {
            observed.borrow_mut().push(format!("step {step}"))
        });
        let observed = calls.clone();
        ui.on_audition_requested(move |index| {
            observed.borrow_mut().push(format!("audition {index}"))
        });
        let observed = calls.clone();
        ui.on_play_requested(move || observed.borrow_mut().push("play".into()));
        let observed = calls.clone();
        ui.on_metadata_edited(move || observed.borrow_mut().push("Metadata".into()));
        ui.show().unwrap();
        click_label(&ui, "Modal focus.wav");
        send_keys(&ui, &[slint::platform::Key::DownArrow.into()]);
        assert_eq!(&*calls.borrow(), &["step 1"]);
        calls.borrow_mut().clear();
        let menu = if chinese { "音效库" } else { "Library" };
        let manage = if chinese {
            "音效库管理…"
        } else {
            "Manage sound library…"
        };
        click_menu_item(&ui, menu, manage);
        assert!(ui.get_library_visible());
        send_keys(&ui, &[slint::platform::Key::Return.into()]);
        assert!(
            calls.borrow().is_empty(),
            "Immediate Return cannot audition the background list"
        );
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        send_keys(&ui, &[slint::platform::Key::Return.into()]);
        assert!(
            !ui.get_library_visible(),
            "Initial modal focus reaches Close"
        );
        assert!(calls.borrow().is_empty());
        click_menu_item(&ui, menu, manage);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        send_keys(
            &ui,
            &[
                slint::platform::Key::Shift.into(),
                slint::platform::Key::Tab.into(),
            ],
        );
        send_keys(&ui, &[slint::platform::Key::Return.into()]);
        assert!(
            ui.get_advanced_maintenance_visible(),
            "Reverse Tab reaches the last collapsed library control"
        );
        // The expanded maintenance report is the final focusable child; returning
        // forward from there must wrap to Close instead of the background.
        click_label(&ui, if chinese { "关闭" } else { "Close" });
        assert!(!ui.get_library_visible());
        click_menu_item(&ui, menu, manage);
        ui.set_advanced_maintenance_visible(true);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        send_keys(
            &ui,
            &[
                slint::platform::Key::Shift.into(),
                slint::platform::Key::Tab.into(),
            ],
        );
        send_keys(&ui, &["background must stay unchanged".into()]);
        assert_eq!(ui.get_edit_tags(), "original");
        assert!(calls.borrow().is_empty());
        send_keys(&ui, &[slint::platform::Key::Tab.into()]);
        send_keys(&ui, &[slint::platform::Key::Return.into()]);
        assert!(
            !ui.get_library_visible(),
            "Expanded maintenance reverse/forward traversal wraps to Close"
        );
        assert!(calls.borrow().is_empty());
        click_menu_item(&ui, menu, manage);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        send_keys(&ui, &[slint::platform::Key::Escape.into()]);
        assert!(!ui.get_library_visible());
        send_keys(&ui, &[slint::platform::Key::Return.into()]);
        assert_eq!(
            &*calls.borrow(),
            &["audition 0"],
            "Closing the modal restores list keyboard control"
        );
        ui.hide().unwrap();
    }
}

#[test]
fn pending_scan_dialog_guards_inputs_and_cancels_with_button_or_escape() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        for escape in [false, true] {
            let ui = AppWindow::new().unwrap();
            ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
            ui.set_chinese(chinese);
            let scans = Rc::new(RefCell::new(0));
            let observed = scans.clone();
            let weak = ui.as_weak();
            ui.on_scan_requested(move |_| {
                *observed.borrow_mut() += 1;
                let ui = weak.upgrade().unwrap();
                ui.set_scanning(true);
                ui.set_scan_validating(true);
            });
            let cancels = Rc::new(RefCell::new(0));
            let observed = cancels.clone();
            ui.on_scan_cancel_requested(move || *observed.borrow_mut() += 1);
            let browses = Rc::new(RefCell::new(0));
            let observed = browses.clone();
            ui.on_scan_browse_requested(move || *observed.borrow_mut() += 1);
            ui.show().unwrap();
            click_menu_item(
                &ui,
                if chinese { "扫描" } else { "Scan" },
                if chinese {
                    "增量扫描文件夹…"
                } else {
                    "Incremental folder scan…"
                },
            );
            ui.set_scan_path("\\\\server\\Sounds".into());
            click_label(
                &ui,
                if chinese {
                    "开始扫描"
                } else {
                    "Start scan"
                },
            );
            assert!(ui.get_scan_validating());
            assert!(ui.get_scanning());
            assert!(ui.get_import_visible());
            click_label(
                &ui,
                if chinese {
                    "正在检查…"
                } else {
                    "Checking…"
                },
            );
            click_label(
                &ui,
                if chinese {
                    "选择文件夹…"
                } else {
                    "Browse folders…"
                },
            );
            click_label(
                &ui,
                if chinese {
                    "全量扫描 · 重新检查"
                } else {
                    "Full scan · recheck folder"
                },
            );
            assert_eq!(*scans.borrow(), 1);
            assert_eq!(*browses.borrow(), 0);
            assert_eq!(ui.get_scan_mode(), 0);
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
            if escape {
                send_keys(&ui, &[slint::platform::Key::Escape.into()]);
            } else {
                send_keys(&ui, &[slint::platform::Key::Return.into()]);
            }
            assert_eq!(*cancels.borrow(), 1);
            assert!(!ui.get_import_visible());
            assert!(
                ui.get_scanning(),
                "UI remains busy until controller drains path validation"
            );
            assert!(ui.get_scan_validating());
            ui.hide().unwrap();
        }
    }
}

#[test]
fn nested_library_confirmation_keeps_cancel_and_continue_keyboard_reachable() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
        ui.set_chinese(chinese);
        let confirmed = Rc::new(RefCell::new(Vec::new()));
        let observed = confirmed.clone();
        let weak = ui.as_weak();
        ui.on_confirmation_requested(move |accepted| {
            observed.borrow_mut().push(accepted);
            weak.upgrade().unwrap().set_confirmation_visible(false);
        });
        ui.show().unwrap();
        click_menu_item(
            &ui,
            if chinese { "音效库" } else { "Library" },
            if chinese {
                "音效库管理…"
            } else {
                "Manage sound library…"
            },
        );
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        for (keys, expected) in [
            (vec![], false),
            (vec![slint::platform::Key::Tab.into()], true),
            (
                vec![
                    slint::platform::Key::Shift.into(),
                    slint::platform::Key::Tab.into(),
                ],
                true,
            ),
        ] {
            ui.set_confirmation_visible(true);
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
            send_keys(&ui, &keys);
            send_keys(&ui, &[slint::platform::Key::Return.into()]);
            assert_eq!(confirmed.borrow().last(), Some(&expected));
            assert!(!ui.get_confirmation_visible());
            assert!(
                ui.get_library_visible(),
                "Nested confirmation keeps the library panel open"
            );
        }
        ui.set_confirmation_visible(true);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        send_keys(&ui, &[slint::platform::Key::Escape.into()]);
        assert_eq!(confirmed.borrow().last(), Some(&false));
        assert!(!ui.get_confirmation_visible());
        assert!(ui.get_library_visible());
        ui.hide().unwrap();
    }
}

#[test]
fn folder_scan_dialog_browses_preserves_cancelled_draft_and_guards_submission() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
        ui.set_chinese(chinese);
        let browses = Rc::new(RefCell::new(0));
        let observed = browses.clone();
        ui.on_scan_browse_requested(move || *observed.borrow_mut() += 1);
        let scans = Rc::new(RefCell::new(Vec::new()));
        let observed = scans.clone();
        let weak = ui.as_weak();
        ui.on_scan_requested(move |path| {
            let ui = weak.upgrade().unwrap();
            if path == "D:\\Missing" {
                ui.set_scan_error("Choose an existing folder".into());
                return;
            }
            observed
                .borrow_mut()
                .push((path.to_string(), ui.get_scan_mode()));
            // Successful controller validation closes the scan dialog.
            ui.set_import_visible(false);
        });
        ui.show().unwrap();
        click_menu_item(
            &ui,
            if chinese { "扫描" } else { "Scan" },
            if chinese {
                "增量扫描文件夹…"
            } else {
                "Incremental folder scan…"
            },
        );
        let browse = if chinese {
            "选择文件夹…"
        } else {
            "Browse folders…"
        };
        let start = if chinese {
            "开始扫描"
        } else {
            "Start scan"
        };
        let cancel = if chinese { "取消" } else { "Cancel" };
        let path_input = if chinese {
            "音效文件夹路径"
        } else {
            "Sound folder path"
        };
        click_label(&ui, start);
        assert!(
            scans.borrow().is_empty(),
            "An empty folder must not start a scan"
        );
        assert!(ui.get_import_visible());
        click_label(&ui, path_input);
        send_keys(&ui, &[slint::platform::Key::Return.into()]);
        assert!(
            scans.borrow().is_empty(),
            "Enter must also reject an empty folder"
        );
        click_label(&ui, browse);
        assert_eq!(*browses.borrow(), 1);
        let incremental = if chinese {
            "增量扫描 · 日常更新"
        } else {
            "Incremental · daily update"
        };
        let full = if chinese {
            "全量扫描 · 重新检查"
        } else {
            "Full scan · recheck folder"
        };
        click_label(&ui, full);
        assert_eq!(ui.get_scan_mode(), 1);
        click_label(&ui, incremental);
        assert_eq!(ui.get_scan_mode(), 0);
        click_label(&ui, full);
        ui.set_scan_browse_busy(true);
        click_label(
            &ui,
            if chinese {
                "正在选择…"
            } else {
                "Choosing…"
            },
        );
        assert_eq!(
            *browses.borrow(),
            1,
            "Repeated folder browse must be blocked"
        );
        click_label(&ui, incremental);
        assert_eq!(
            ui.get_scan_mode(),
            1,
            "The mode must not change during folder browsing"
        );
        ui.set_scan_path("D:\\Sounds\\Aircraft".into());
        click_label(&ui, start);
        assert!(
            scans.borrow().is_empty(),
            "Folder browsing must finish before scan submission"
        );
        ui.set_scan_browse_busy(false);
        ui.set_scan_path("D:\\Missing".into());
        click_label(&ui, start);
        assert!(
            ui.get_import_visible(),
            "Rejected folders keep the dialog open for correction"
        );
        assert!(
            ElementHandle::find_by_accessible_label(&ui, "Choose an existing folder")
                .next()
                .is_some()
        );
        ui.set_scan_path("D:\\Sounds\\Aircraft".into());
        click_label(&ui, cancel);
        assert!(!ui.get_import_visible());
        assert_eq!(
            ui.get_scan_path(),
            "D:\\Sounds\\Aircraft",
            "Cancellation preserves the path draft"
        );
        assert!(scans.borrow().is_empty());
        click_menu_item(
            &ui,
            if chinese { "扫描" } else { "Scan" },
            if chinese {
                "全量扫描文件夹…"
            } else {
                "Full folder rescan…"
            },
        );
        ui.set_scan_path("D:\\Sounds\\Aircraft".into());
        click_label(&ui, path_input);
        ui.set_scanning(true);
        send_keys(&ui, &[slint::platform::Key::Return.into()]);
        assert!(
            scans.borrow().is_empty(),
            "Enter must respect an active scan"
        );
        click_label(&ui, start);
        assert!(
            scans.borrow().is_empty(),
            "A scan already in progress blocks another scan"
        );
        ui.set_scanning(false);
        ui.set_scan_error("".into());
        click_label(&ui, start);
        assert_eq!(&*scans.borrow(), &[("D:\\Sounds\\Aircraft".into(), 1)]);
        assert!(!ui.get_import_visible());
        ui.hide().unwrap();
    }
}

#[test]
fn menu_bar_record_actions_use_displayed_id_and_current_folder_path() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_has_selection(true);
    ui.set_selected_id("42".into());
    ui.set_selected_folder_path("D:\\Sounds\\Aircraft".into());
    // The result page may refresh while the inspector still displays record 42.
    ui.set_rows(ModelRc::new(VecModel::from(vec![AudioRow {
        id: "99".into(),
        name: "New result.wav".into(),
        ..Default::default()
    }])));
    ui.set_selected_row(0);
    let files = Rc::new(RefCell::new(Vec::new()));
    let observed = files.clone();
    ui.on_file_action_requested(move |id, action| {
        observed.borrow_mut().push((id.to_string(), action));
    });
    let folders = Rc::new(RefCell::new(Vec::new()));
    let observed = folders.clone();
    ui.on_folder_action_requested(move |path, action| {
        observed.borrow_mut().push((path.to_string(), action));
    });
    let library = Rc::new(RefCell::new(Vec::new()));
    let observed = library.clone();
    ui.on_library_action(move |action| observed.borrow_mut().push(action));
    ui.show().unwrap();
    for (label, action) in [
        ("Add to favorites", 5),
        ("Reveal in Explorer", 2),
        ("Repair selected file reference…", 20),
        ("Remove selected record (keep WAV)…", 21),
    ] {
        click_menu_item(&ui, "Edit", label);
        assert_eq!(files.borrow().last(), Some(&("42".to_owned(), action)));
    }
    click_menu_item(&ui, "Edit", "Remove current folder (keep WAV)…");
    assert_eq!(&*folders.borrow(), &[("D:\\Sounds\\Aircraft".into(), 1)]);
    ui.set_checked_count(2);
    click_menu_item(&ui, "Edit", "Remove selected records (keep WAV)…");
    assert_eq!(&*library.borrow(), &[11]);

    ui.set_has_selection(false);
    ui.set_checked_count(0);
    ui.set_selected_folder_path("".into());
    let file_count = files.borrow().len();
    for label in [
        "Remove selected record (keep WAV)…",
        "Remove selected records (keep WAV)…",
        "Remove current folder (keep WAV)…",
    ] {
        click_menu_item(&ui, "Edit", label);
        close_menu(&ui);
    }
    assert_eq!(files.borrow().len(), file_count);
    assert_eq!(folders.borrow().len(), 1);
    assert_eq!(&*library.borrow(), &[11]);
    ui.hide().unwrap();
}

#[test]
fn menu_bar_scan_modes_and_active_scan_guards_preserve_current_folder() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_selected_folder_path("D:\\Sounds\\Aircraft".into());
    let scans = Rc::new(RefCell::new(Vec::new()));
    let observed = scans.clone();
    let weak = ui.as_weak();
    ui.on_scan_requested(move |path| {
        let ui = weak.upgrade().unwrap();
        observed
            .borrow_mut()
            .push((path.to_string(), ui.get_scan_mode()));
        ui.set_import_visible(false);
    });
    let cancels = Rc::new(RefCell::new(0));
    let observed = cancels.clone();
    ui.on_scan_cancel_requested(move || *observed.borrow_mut() += 1);
    ui.show().unwrap();
    click_menu_item(&ui, "Scan", "Full folder rescan…");
    assert!(ui.get_import_visible());
    assert_eq!(ui.get_scan_mode(), 1);
    ui.set_import_visible(false);
    click_menu_item(&ui, "Scan", "Incremental folder scan…");
    assert!(ui.get_import_visible());
    assert_eq!(ui.get_scan_mode(), 0);
    ui.set_import_visible(false);
    click_menu_item(&ui, "Scan", "Full rescan of current folder");
    assert!(ui.get_import_visible());
    assert_eq!(ui.get_scan_path(), "D:\\Sounds\\Aircraft");
    assert_eq!(ui.get_scan_mode(), 1);
    assert!(
        scans.borrow().is_empty(),
        "Choosing a folder action must wait for scan confirmation"
    );
    click_label(&ui, "Start scan");
    assert!(!ui.get_import_visible());
    click_menu_item(&ui, "Scan", "Incremental scan of current folder");
    assert!(ui.get_import_visible());
    assert_eq!(ui.get_scan_path(), "D:\\Sounds\\Aircraft");
    assert_eq!(ui.get_scan_mode(), 0);
    click_label(&ui, "Start scan");
    assert_eq!(
        &*scans.borrow(),
        &[
            ("D:\\Sounds\\Aircraft".into(), 1),
            ("D:\\Sounds\\Aircraft".into(), 0)
        ]
    );
    click_menu_item(&ui, "Scan", "Cancel current scan");
    assert_eq!(*cancels.borrow(), 0);
    close_menu(&ui);
    ui.set_scanning(true);
    for (title, label) in [
        ("Scan", "Incremental folder scan…"),
        ("Scan", "Full folder rescan…"),
        ("Scan", "Full rescan of current folder"),
    ] {
        click_menu_item(&ui, title, label);
        assert!(!ui.get_import_visible());
        close_menu(&ui);
    }
    assert_eq!(scans.borrow().len(), 2);
    click_menu_item(&ui, "Scan", "Cancel current scan");
    assert_eq!(*cancels.borrow(), 1);
    ui.hide().unwrap();
}

#[test]
fn menu_bar_playback_view_and_help_preserve_selection_and_modal_guards() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_has_selection(false);
    ui.set_selected_id("42".into());
    ui.set_selected_missing(false);
    // Selecting a missing file clears readiness, while a previous source may still play.
    ui.set_audio_ready(false);
    ui.set_playing(true);
    ui.set_play_duration(20.0);
    let transport = Rc::new(RefCell::new(Vec::new()));
    let observed = transport.clone();
    ui.on_start_requested(move || observed.borrow_mut().push("play"));
    let observed = transport.clone();
    ui.on_pause_requested(move || observed.borrow_mut().push("pause"));
    let observed = transport.clone();
    ui.on_stop_requested(move || observed.borrow_mut().push("stop"));
    let files = Rc::new(RefCell::new(Vec::new()));
    let observed = files.clone();
    ui.on_file_action_requested(move |id, action| {
        observed.borrow_mut().push((id.to_string(), action));
    });
    ui.show().unwrap();
    click_menu_item(&ui, "Playback", "Audition selected file");
    close_menu(&ui);
    assert!(files.borrow().is_empty());
    ui.set_has_selection(true);
    ui.set_selected_missing(true);
    for label in ["Audition selected file", "Play"] {
        click_menu_item(&ui, "Playback", label);
        close_menu(&ui);
    }
    assert!(files.borrow().is_empty());
    assert!(transport.borrow().is_empty());
    click_menu_item(&ui, "Playback", "Pause");
    click_menu_item(&ui, "Playback", "Stop");
    assert_eq!(&*transport.borrow(), &["pause", "stop"]);

    ui.set_playing(false);
    ui.set_play_duration(0.0);
    for label in ["Pause", "Stop"] {
        click_menu_item(&ui, "Playback", label);
        close_menu(&ui);
    }
    assert_eq!(&*transport.borrow(), &["pause", "stop"]);
    ui.set_selected_missing(false);
    click_menu_item(&ui, "Playback", "Audition selected file");
    assert_eq!(&*files.borrow(), &[("42".into(), 0)]);
    ui.set_audio_ready(true);
    click_menu_item(&ui, "Playback", "Play");
    assert_eq!(&*transport.borrow(), &["pause", "stop", "play"]);

    let inspector = ui.get_inspector_visible();
    click_menu_item(&ui, "View", "Show side panel (filters / details)");
    assert_eq!(ui.get_inspector_visible(), !inspector);
    click_menu_item(&ui, "View", "Show side panel (filters / details)");
    assert_eq!(ui.get_inspector_visible(), inspector);
    // The page items open the sidebar on their page; the side panel item keeps the page.
    ui.set_inspector_visible(false);
    click_menu_item(&ui, "View", "Show filters");
    assert!(ui.get_inspector_visible() && ui.get_filters_visible());
    click_menu_item(&ui, "View", "Show side panel (filters / details)");
    click_menu_item(&ui, "View", "Show side panel (filters / details)");
    assert!(ui.get_inspector_visible() && ui.get_filters_visible());
    click_menu_item(&ui, "View", "Show sound details");
    assert!(ui.get_inspector_visible() && !ui.get_filters_visible());
    ui.set_inspector_visible(inspector);
    let libraries = Rc::new(RefCell::new(Vec::new()));
    let observed = libraries.clone();
    ui.on_library_action(move |action| observed.borrow_mut().push(action));
    click_menu_item(&ui, "Help", "Shortcuts and basics…");
    assert!(ui.get_help_visible());
    click_menu_item(&ui, "Library", "Manage sound library…");
    assert!(libraries.borrow().is_empty());
    assert!(!ui.get_library_visible());
    assert!(ui.get_help_visible());
    close_menu(&ui);
    ui.set_help_visible(false);
    click_menu_item(&ui, "Help", "About AudioLib Explorer");
    assert!(ui.get_about_visible());
    ui.hide().unwrap();
}

#[test]
fn menu_bar_shortcuts_dispatch_once_and_respect_all_modal_guards() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_has_selection(true);
    ui.set_metadata_dirty(true);
    let actions = Rc::new(RefCell::new(Vec::new()));
    let observed = actions.clone();
    ui.on_library_action(move |action| observed.borrow_mut().push(action));
    let saves = Rc::new(RefCell::new(0));
    let observed = saves.clone();
    ui.on_save_metadata_requested(move || *observed.borrow_mut() += 1);
    let exits = Rc::new(RefCell::new(0));
    let observed = exits.clone();
    ui.on_exit_requested(move || *observed.borrow_mut() += 1);
    let refreshes = Rc::new(RefCell::new(0));
    let observed = refreshes.clone();
    ui.on_refresh_requested(move || *observed.borrow_mut() += 1);
    ui.show().unwrap();
    for key in ["n", "o"] {
        send_keys(&ui, &[slint::platform::Key::Control.into(), key.into()]);
        assert!(actions.borrow().is_empty());
        assert!(!ui.get_library_visible());
    }
    send_keys(&ui, &[slint::platform::Key::Control.into(), "d".into()]);
    assert_eq!(&*actions.borrow(), &[0]);
    assert!(ui.get_library_visible());
    ui.set_library_visible(false);
    ui.set_scan_path("D:\\Old draft".into());
    send_keys(
        &ui,
        &[
            slint::platform::Key::Control.into(),
            slint::platform::Key::Shift.into(),
            "i".into(),
        ],
    );
    assert!(ui.get_import_visible());
    assert!(
        ui.get_scan_path().is_empty(),
        "Add folder shortcut starts a fresh folder choice"
    );
    assert_eq!(ui.get_scan_mode(), 0);
    ui.set_import_visible(false);
    send_keys(&ui, &[slint::platform::Key::Control.into(), "s".into()]);
    assert_eq!(*saves.borrow(), 1);
    let inspector = ui.get_inspector_visible();
    send_keys(&ui, &[slint::platform::Key::Control.into(), "i".into()]);
    assert_eq!(ui.get_inspector_visible(), !inspector);
    send_keys(&ui, &[slint::platform::Key::F5.into()]);
    assert_eq!(*refreshes.borrow(), 1);
    send_keys(&ui, &[slint::platform::Key::Control.into(), "q".into()]);
    assert_eq!(*exits.borrow(), 1);

    let modal_setters: [fn(&AppWindow, bool); 10] = [
        AppWindow::set_library_visible,
        AppWindow::set_confirmation_visible,
        AppWindow::set_batch_visible,
        AppWindow::set_import_visible,
        AppWindow::set_settings_visible,
        AppWindow::set_peaks_visible,
        AppWindow::set_about_visible,
        AppWindow::set_help_visible,
        AppWindow::set_library_busy,
        AppWindow::set_closing,
    ];
    for (index, set_modal) in modal_setters.into_iter().enumerate() {
        set_modal(&ui, true);
        for key in ["d", "s", "q", "i"] {
            send_keys(&ui, &[slint::platform::Key::Control.into(), key.into()]);
        }
        send_keys(&ui, &[slint::platform::Key::F5.into()]);
        assert_eq!(actions.borrow().len(), 1, "Guard {index}");
        assert_eq!(*saves.borrow(), 1, "Guard {index}");
        assert_eq!(*exits.borrow(), 1, "Guard {index}");
        assert_eq!(*refreshes.borrow(), 1, "Guard {index}");
        assert_eq!(ui.get_inspector_visible(), !inspector, "Guard {index}");
        set_modal(&ui, false);
    }
    ui.set_has_selection(false);
    send_keys(&ui, &[slint::platform::Key::Control.into(), "s".into()]);
    assert_eq!(*saves.borrow(), 1);
    ui.hide().unwrap();
}

#[test]
fn help_modal_blocks_result_keys_and_context_menu_in_both_languages() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
        ui.set_chinese(chinese);
        ui.set_rows(ModelRc::new(VecModel::from(vec![AudioRow {
            id: "42".into(),
            name: "Help focus.wav".into(),
            ..Default::default()
        }])));
        ui.set_selected_row(0);
        ui.set_has_selection(true);
        ui.set_selected_id("42".into());
        ui.set_audio_ready(true);
        let actions = Rc::new(RefCell::new(Vec::new()));
        let observed = actions.clone();
        ui.on_selection_step_requested(move |step| {
            observed.borrow_mut().push(format!("step {step}"));
        });
        let observed = actions.clone();
        ui.on_audition_requested(move |index| {
            observed.borrow_mut().push(format!("audition {index}"));
        });
        let observed = actions.clone();
        ui.on_play_requested(move || observed.borrow_mut().push("play".to_string()));
        let observed = actions.clone();
        ui.on_file_action_requested(move |id, action| {
            observed.borrow_mut().push(format!("file {id}/{action}"));
        });
        ui.show().unwrap();
        click_label(&ui, "Help focus.wav");
        send_keys(&ui, &[slint::platform::Key::DownArrow.into()]);
        assert_eq!(&*actions.borrow(), &["step 1"]);
        actions.borrow_mut().clear();
        for keys in [
            vec![slint::platform::Key::DownArrow.into()],
            vec![slint::platform::Key::UpArrow.into()],
            vec![slint::platform::Key::Return.into()],
            vec![" ".into()],
            vec![slint::platform::Key::Menu.into()],
            vec![
                slint::platform::Key::Shift.into(),
                slint::platform::Key::F10.into(),
            ],
        ] {
            // Each case reacquires list focus with a single click, not the
            // double-click audition that frozen test time would otherwise make.
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
            click_label(&ui, "Help focus.wav");
            click_menu_item(
                &ui,
                if chinese { "帮助" } else { "Help" },
                if chinese {
                    "快捷键与基本操作…"
                } else {
                    "Shortcuts and basics…"
                },
            );
            assert!(ui.get_help_visible());
            send_keys(&ui, &keys);
            assert!(
                actions.borrow().is_empty(),
                "Help key {keys:?}: {:?}",
                actions.borrow()
            );
            let context_label = if chinese {
                "复制完整路径"
            } else {
                "Copy full path"
            };
            assert!(
                ElementHandle::find_by_accessible_label(&ui, context_label)
                    .next()
                    .is_none(),
                "Help must not open the background file menu for {keys:?}"
            );
            // Return/Space can activate the focused Close button, but never
            // the list that held focus before Help opened.
            if ui.get_help_visible() {
                send_keys(&ui, &[slint::platform::Key::Escape.into()]);
            }
            assert!(!ui.get_help_visible());
        }
        for tab in [
            vec![slint::platform::Key::Tab.into()],
            vec![
                slint::platform::Key::Shift.into(),
                slint::platform::Key::Tab.into(),
            ],
        ] {
            click_menu_item(
                &ui,
                if chinese { "帮助" } else { "Help" },
                if chinese {
                    "快捷键与基本操作…"
                } else {
                    "Shortcuts and basics…"
                },
            );
            assert!(ui.get_help_visible());
            send_keys(&ui, &tab);
            send_keys(&ui, &[slint::platform::Key::Return.into()]);
            assert!(
                !ui.get_help_visible(),
                "Close remains reachable after {tab:?}"
            );
            assert!(actions.borrow().is_empty(), "Help tab {tab:?}");
        }
        // Closing Help returns keyboard control to the result list.
        for key in [
            slint::platform::Key::DownArrow.into(),
            slint::platform::Key::Return.into(),
            " ".into(),
        ] {
            send_keys(&ui, &[key]);
        }
        assert_eq!(&*actions.borrow(), &["step 1", "audition 0", "play"]);
        ui.hide().unwrap();
    }
}

#[test]
fn help_modal_blocks_background_metadata_input_in_both_languages() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
        ui.set_chinese(chinese);
        ui.set_has_selection(true);
        ui.set_selected_id("42".into());
        ui.set_edit_tags("original".into());
        let edits = Rc::new(RefCell::new(0));
        let observed = edits.clone();
        ui.on_metadata_edited(move || *observed.borrow_mut() += 1);
        ui.show().unwrap();
        click_label(
            &ui,
            if chinese {
                "标签 · 逗号分隔"
            } else {
                "Tags, comma separated"
            },
        );
        send_keys(&ui, &["x".into()]);
        assert_ne!(ui.get_edit_tags(), "original");
        assert_eq!(*edits.borrow(), 1);
        let draft = ui.get_edit_tags();
        ui.set_metadata_dirty(false);
        click_menu_item(
            &ui,
            if chinese { "帮助" } else { "Help" },
            if chinese {
                "快捷键与基本操作…"
            } else {
                "Shortcuts and basics…"
            },
        );
        assert!(ui.get_help_visible());
        for keys in [
            vec!["z".into()],
            vec!["测试".into()],
            vec![slint::platform::Key::Backspace.into()],
            vec![slint::platform::Key::Delete.into()],
            vec![slint::platform::Key::Control.into(), "a".into()],
            vec!["overwrite".into()],
        ] {
            send_keys(&ui, &keys);
            assert_eq!(ui.get_edit_tags(), draft, "Help input {keys:?}");
            assert_eq!(*edits.borrow(), 1, "Help input {keys:?}");
            assert!(!ui.get_metadata_dirty(), "Help input {keys:?}");
        }
        send_keys(&ui, &[slint::platform::Key::Escape.into()]);
        assert!(!ui.get_help_visible());
        for tab in [
            vec![slint::platform::Key::Tab.into()],
            vec![
                slint::platform::Key::Shift.into(),
                slint::platform::Key::Tab.into(),
            ],
        ] {
            click_label(
                &ui,
                if chinese {
                    "标签 · 逗号分隔"
                } else {
                    "Tags, comma separated"
                },
            );
            click_menu_item(
                &ui,
                if chinese { "帮助" } else { "Help" },
                if chinese {
                    "快捷键与基本操作…"
                } else {
                    "Shortcuts and basics…"
                },
            );
            assert!(ui.get_help_visible());
            send_keys(&ui, &tab);
            send_keys(&ui, &[slint::platform::Key::Return.into()]);
            assert!(
                !ui.get_help_visible(),
                "Help can close after opening from an editor: {tab:?}"
            );
            assert_eq!(ui.get_edit_tags(), draft);
            assert_eq!(*edits.borrow(), 1);
        }
        click_label(
            &ui,
            if chinese {
                "标签 · 逗号分隔"
            } else {
                "Tags, comma separated"
            },
        );
        send_keys(&ui, &["y".into()]);
        assert_ne!(ui.get_edit_tags(), draft);
        assert_eq!(*edits.borrow(), 2);
        assert!(ui.get_metadata_dirty());
        ui.hide().unwrap();
    }
}

#[test]
fn database_batch_and_confirmation_actions_are_reachable_in_both_languages() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
    let actions = Rc::new(RefCell::new(Vec::new()));
    let observed = actions.clone();
    ui.on_library_action(move |action| observed.borrow_mut().push(action));
    ui.set_library_visible(true);
    ui.show().unwrap();
    scroll_library_management(&ui, -5000.0);
    click_label(&ui, "Advanced maintenance ▸");
    scroll_library_management(&ui, -5000.0);
    click_label(&ui, "Backup…");
    assert_eq!(&*actions.borrow(), &[1]);
    ui.set_chinese(true);
    click_label(&ui, "重建搜索");
    assert_eq!(&*actions.borrow(), &[1, 5]);
    ui.set_confirmation_text("仅移除库记录；WAV 保留".into());
    ui.set_confirmation_visible(true);
    let confirmed = Rc::new(RefCell::new(None));
    let received = confirmed.clone();
    ui.on_confirmation_requested(move |accepted| *received.borrow_mut() = Some(accepted));
    click_label(&ui, "取消");
    assert_eq!(*confirmed.borrow(), Some(false));
    ui.set_confirmation_visible(false);
    ui.set_library_visible(false);
    ui.set_checked_count(2);
    ui.set_batch_visible(true);
    let applied = Rc::new(RefCell::new(false));
    let received = applied.clone();
    ui.on_batch_requested(move || *received.borrow_mut() = true);
    click_label(&ui, "应用到选择记录");
    assert!(*applied.borrow());
    ui.hide().unwrap();
}

#[test]
fn missing_records_and_library_freeze_disable_audition_and_batch_apply() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_audio_ready(true);
    ui.set_selected_missing(true);
    ui.show().unwrap();
    let play = ElementHandle::find_by_accessible_label(&ui, "Play")
        .next()
        .unwrap();
    assert_eq!(play.accessible_enabled(), Some(false));
    let row = AudioRow {
        id: "42".into(),
        name: "Offline.wav".into(),
        missing: true,
        ..Default::default()
    };
    let menu_position = slint::LogicalPosition::new(380.0, 280.0);
    let popup = menu_position;
    ui.invoke_show_file_menu(row, menu_position);
    let audition = ElementHandle::find_by_accessible_label(&ui, "Audition")
        .next()
        .unwrap();
    let actions = Rc::new(RefCell::new(Vec::new()));
    let observed = actions.clone();
    ui.on_file_action_requested(move |id, action| observed.borrow_mut().push((id, action)));
    let origin = audition.absolute_position();
    let size = audition.size();
    let position = slint::LogicalPosition::new(
        popup.x + origin.x + size.width / 2.0,
        popup.y + origin.y + size.height / 2.0,
    );
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    assert!(
        actions.borrow().is_empty(),
        "Missing record must not dispatch audition"
    );
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Escape.into(),
    });
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: slint::platform::Key::Escape.into(),
    });
    ui.set_selected_missing(false);
    ui.set_library_busy(true);
    assert_eq!(play.accessible_enabled(), Some(false));
    ui.set_checked_count(2);
    ui.set_batch_visible(true);
    let batch = ElementHandle::find_by_accessible_label(&ui, "Apply to selected")
        .next()
        .unwrap();
    assert_eq!(batch.accessible_enabled(), Some(false));
    ui.hide().unwrap();
}

#[test]
fn context_menu_keeps_target_after_result_refresh_and_escape_preserves_filters() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    let original = AudioRow {
        id: "42".into(),
        name: "Original.wav".into(),
        ..Default::default()
    };
    ui.set_rows(ModelRc::new(VecModel::from(vec![original.clone()])));
    let actions = Rc::new(RefCell::new(Vec::new()));
    let observed = actions.clone();
    ui.on_file_action_requested(move |id, action| {
        observed.borrow_mut().push((id.to_string(), action))
    });
    ui.show().unwrap();
    let menu_position = slint::LogicalPosition::new(400.0, 300.0);
    let popup = menu_position;
    ui.invoke_show_file_menu(original, menu_position);
    ui.set_rows(ModelRc::new(VecModel::from(vec![AudioRow {
        id: "99".into(),
        name: "Replacement.wav".into(),
        ..Default::default()
    }])));
    let favorite = ElementHandle::find_by_accessible_label(&ui, "Add to favorites")
        .next()
        .expect("favorite menu entry");
    // Popup element coordinates are relative to the popup, not the owner window.
    let item = favorite.absolute_position();
    let size = favorite.size();
    let position = slint::LogicalPosition::new(
        popup.x + item.x + size.width / 2.0,
        popup.y + item.y + size.height / 2.0,
    );
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    assert_eq!(&*actions.borrow(), &[("42".into(), 5)]);

    ui.set_filters_visible(true);
    ui.set_tag_filter("aircraft".into());
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: slint::platform::Key::Escape.into(),
    });
    ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: slint::platform::Key::Escape.into(),
    });
    assert!(!ui.get_filters_visible());
    assert_eq!(ui.get_tag_filter(), "aircraft");
    ui.hide().unwrap();
}

#[test]
fn folder_context_menu_uses_window_coordinates_and_retains_its_original_path() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    let original = FolderRow {
        name: "Original folder".into(),
        path: "D:\\Sounds\\Original".into(),
        ..Default::default()
    };
    ui.set_folders(ModelRc::new(VecModel::from(vec![original.clone()])));
    let actions = Rc::new(RefCell::new(Vec::new()));
    let observed = actions.clone();
    ui.on_folder_action_requested(move |path, action| {
        observed.borrow_mut().push((path.to_string(), action));
    });
    ui.show().unwrap();
    let owner = ElementHandle::find_by_element_id(&ui, "AppWindow::folder-menu")
        .next()
        .expect("folder context menu owner")
        .absolute_position();
    assert!(owner.y > 0.0, "Menu bar must offset the popup owner");
    let popup = slint::LogicalPosition::new(400.0, 300.0);
    ui.invoke_show_folder_menu(original.clone(), popup);
    ui.set_folders(ModelRc::new(VecModel::from(vec![FolderRow {
        name: "Replacement folder".into(),
        path: "D:\\Sounds\\Replacement".into(),
        ..Default::default()
    }])));
    let open = ElementHandle::find_by_accessible_label(&ui, "Open in Explorer")
        .next()
        .expect("open folder context action");
    let origin = open.absolute_position();
    let size = open.size();
    // Public show-folder-menu takes a window point. Adding the owner's menu-bar
    // offset again would click a different row and fail this routing assertion.
    click_at(
        &ui,
        slint::LogicalPosition::new(
            popup.x + origin.x + size.width / 2.0,
            popup.y + origin.y + size.height / 2.0,
        ),
    );
    assert_eq!(&*actions.borrow(), &[("D:\\Sounds\\Original".into(), 0)]);
    ui.invoke_show_folder_menu(original, popup);
    let rescan = ElementHandle::find_by_accessible_label(&ui, "Full scan this folder…")
        .next()
        .expect("full scan folder context action");
    let origin = rescan.absolute_position();
    let size = rescan.size();
    click_at(
        &ui,
        slint::LogicalPosition::new(
            popup.x + origin.x + size.width / 2.0,
            popup.y + origin.y + size.height / 2.0,
        ),
    );
    assert!(ui.get_import_visible());
    assert_eq!(ui.get_scan_mode(), 1);
    assert_eq!(ui.get_scan_path(), "D:\\Sounds\\Original");
    assert_eq!(
        actions.borrow().len(),
        1,
        "The context scan must wait for confirmation"
    );
    ui.hide().unwrap();
}

#[test]
fn compact_window_keeps_audition_controls_and_time_inside_viewport() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
    ui.set_filters_visible(true);
    ui.set_rows(ModelRc::new(VecModel::from(vec![AudioRow {
        name: "Compact.wav".into(),
        ..Default::default()
    }])));
    ui.show().unwrap();
    for has_rows in [true, false] {
        if !has_rows {
            ui.set_rows(ModelRc::new(VecModel::default()));
        }
        for label in ["Play", "Fit entire sound", "Audition time"] {
            let element = ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .expect(label);
            let origin = element.absolute_position();
            let size = element.size();
            assert!(
                origin.y >= 0.0 && origin.y + size.height <= 700.0,
                "{label} clipped at y={} height={} with rows={has_rows}",
                origin.y,
                size.height
            );
        }
    }
    ui.hide().unwrap();
}

fn timeline_position(ui: &AppWindow, label: &str, fraction: f32) -> slint::LogicalPosition {
    let element = ElementHandle::find_by_accessible_label(ui, label)
        .next()
        .unwrap_or_else(|| panic!("Missing timeline: {label}"));
    let origin = element.absolute_position();
    let size = element.size();
    slint::LogicalPosition::new(
        origin.x + size.width * fraction,
        origin.y + size.height / 2.0,
    )
}

fn scroll_timeline(ui: &AppWindow, label: &str, fraction: f32, delta_y: f32) {
    let position = timeline_position(ui, label, fraction);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x: 0.0,
        delta_y,
    });
}

#[test]
fn waveform_and_ruler_wheel_zoom_keep_pointer_anchor_and_precision_deltas() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_audio_ready(true);
    ui.set_play_duration(20.0);
    ui.set_view_start(5.0);
    ui.set_view_end(15.0);
    let requests = Rc::new(RefCell::new(Vec::new()));
    let observed = requests.clone();
    ui.on_waveform_wheel_zoom_requested(move |factor, fraction| {
        observed.borrow_mut().push((factor, fraction));
    });
    let seeks = Rc::new(RefCell::new(Vec::new()));
    let observed = seeks.clone();
    ui.on_seek_play_requested(move |seconds| observed.borrow_mut().push(seconds));
    ui.on_seek_requested(|_| panic!("Timeline clicks must seek and play, not only seek"));
    ui.show().unwrap();

    scroll_timeline(&ui, "Waveform timeline", 0.25, 120.0);
    scroll_timeline(&ui, "Waveform timeline", 0.75, -120.0);
    for fraction in [0.1, 0.9] {
        scroll_timeline(&ui, "Waveform time ruler", fraction, 1.0);
    }
    let expected = [
        (2.0, 0.25),
        (0.5, 0.75),
        (2.0_f32.powf(1.0 / 120.0), 0.1),
        (2.0_f32.powf(1.0 / 120.0), 0.9),
    ];
    assert_eq!(requests.borrow().len(), expected.len());
    for ((factor, fraction), (expected_factor, expected_fraction)) in
        requests.borrow().iter().zip(expected)
    {
        assert!((factor - expected_factor).abs() < 0.0001);
        assert!((fraction - expected_fraction).abs() < 0.001);
    }
    assert!(requests.borrow()[2].0 > 1.0 && requests.borrow()[2].0 < 1.01);
    assert!(
        seeks.borrow().is_empty(),
        "Wheel zoom must preserve playback position"
    );

    let request_count = requests.borrow().len();
    for label in ["Waveform timeline", "Waveform time ruler"] {
        scroll_timeline(&ui, label, 0.5, 0.0);
        ui.set_audio_ready(false);
        scroll_timeline(&ui, label, 0.5, 120.0);
        ui.set_audio_ready(true);
        ui.set_selected_missing(true);
        scroll_timeline(&ui, label, 0.5, 120.0);
        ui.set_selected_missing(false);
        ui.set_library_busy(true);
        scroll_timeline(&ui, label, 0.5, 120.0);
        ui.set_library_busy(false);
        ui.set_play_duration(0.0);
        scroll_timeline(&ui, label, 0.5, 120.0);
        ui.set_play_duration(20.0);
    }
    assert_eq!(requests.borrow().len(), request_count);

    for (label, fraction) in [("Waveform timeline", 0.25), ("Waveform time ruler", 0.8)] {
        let position = timeline_position(&ui, label, fraction);
        ui.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
    }
    assert_eq!(seeks.borrow().len(), 2);
    assert!((seeks.borrow()[0] - 7.5).abs() < 0.001);
    assert!((seeks.borrow()[1] - 13.0).abs() < 0.001);
    for label in ["Waveform timeline", "Waveform time ruler"] {
        ui.set_audio_ready(false);
        click_at(&ui, timeline_position(&ui, label, 0.5));
        ui.set_audio_ready(true);
        ui.set_selected_missing(true);
        click_at(&ui, timeline_position(&ui, label, 0.5));
        ui.set_selected_missing(false);
        ui.set_library_busy(true);
        click_at(&ui, timeline_position(&ui, label, 0.5));
        ui.set_library_busy(false);
    }
    assert_eq!(seeks.borrow().len(), 2);
    ui.hide().unwrap();
}

#[test]
fn full_width_rows_forward_ctrl_shift_selection_without_accidental_audition() {
    use slint::platform::Key;
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_audition_play_on_single_click(true);
    ui.set_rows(ModelRc::new(VecModel::from(vec![AudioRow {
        id: "42".into(),
        name: "Selection.wav".into(),
        ..Default::default()
    }])));
    let requests = Rc::new(RefCell::new(Vec::new()));
    let observed = requests.clone();
    ui.on_row_clicked(move |index, toggle, range, play| {
        observed.borrow_mut().push((index, toggle, range, play));
    });
    ui.on_audition_requested(|_| panic!("Modifier clicks must not audition"));
    ui.show().unwrap();
    // Settle the native window's responsive layout before caching pointer coordinates.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let row = ElementHandle::find_by_accessible_label(&ui, "Selection.wav")
        .find(|item| item.accessible_role() == Some(AccessibleRole::ListItem))
        .unwrap();
    let origin = row.absolute_position();
    let position = slint::LogicalPosition::new(origin.x + 8.0, origin.y + row.size().height / 2.0);
    for (keys, toggle, range) in [
        (vec![], false, false),
        (vec![Key::Control], true, false),
        (vec![Key::Shift], false, true),
        (vec![Key::Control, Key::Shift], true, true),
    ] {
        for key in &keys {
            ui.window().dispatch_event(WindowEvent::KeyPressed {
                text: (*key).into(),
            });
        }
        click_at(&ui, position);
        assert_eq!(
            requests.borrow().last(),
            Some(&(0, toggle, range, !toggle && !range))
        );
        for key in keys.into_iter().rev() {
            ui.window()
                .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
        }
    }
    assert_eq!(requests.borrow().len(), 4);
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Right,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Right,
    });
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "Add to favorites")
            .next()
            .is_some()
    );
    assert_eq!(requests.borrow().len(), 4);
    ui.hide().unwrap();
}

fn played_fraction(ui: &AppWindow) -> f32 {
    // Slint changed handlers run at the event-loop boundary, including at zero elapsed time.
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let waveform = ElementHandle::find_by_accessible_label(ui, "Waveform timeline")
        .next()
        .expect("Waveform timeline");
    let played = ElementHandle::find_by_accessible_label(ui, "Played waveform region")
        .next()
        .expect("Played waveform region");
    assert!(
        (played.absolute_position().x - waveform.absolute_position().x).abs() < 0.001,
        "Played region must fill from the timeline's left edge"
    );
    assert!(
        (played.absolute_position().y - waveform.absolute_position().y).abs() < 0.001
            && (played.size().height - waveform.size().height).abs() < 0.001,
        "Played region must cover the Waveform lanes"
    );
    played.size().width / waveform.size().width
}

#[test]
fn pending_waveform_zoom_uses_the_image_still_visible_under_the_pointer() {
    use crate::{controller::zoom_waveform_at_pointer, waveform_view::WaveView};
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_audio_ready(true);
    ui.set_play_duration(100.0);
    ui.set_view_start(0.0);
    ui.set_view_end(100.0);
    let view = Rc::new(RefCell::new(WaveView::default()));
    let requested_view = view.clone();
    let weak = ui.as_weak();
    ui.on_waveform_wheel_zoom_requested(move |factor, fraction| {
        let ui = weak.upgrade().unwrap();
        zoom_waveform_at_pointer(
            &ui,
            &mut requested_view.borrow_mut(),
            factor,
            fraction,
            f64::from(ui.get_play_duration()),
        );
    });
    ui.show().unwrap();
    scroll_timeline(&ui, "Waveform timeline", 0.25, 120.0);
    let first = view.borrow().range(100.0);
    assert!((first.0 - 12.5).abs() < 0.001 && (first.1 - 62.5).abs() < 0.001);
    // The first render has not completed; the image and ruler still show 0..100.
    scroll_timeline(&ui, "Waveform time ruler", 0.75, 120.0);
    let second = view.borrow().range(100.0);
    assert!((second.0 - 56.25).abs() < 0.001 && (second.1 - 81.25).abs() < 0.001);
    assert_eq!(view.borrow().zoom, 4.0);
    // Publish the completed image's range, then anchor subsequent events to it.
    ui.set_view_start(second.0 as f32);
    ui.set_view_end(second.1 as f32);
    scroll_timeline(&ui, "Waveform timeline", 0.5, -120.0);
    let third = view.borrow().range(100.0);
    assert!((third.0 - 43.75).abs() < 0.001 && (third.1 - 93.75).abs() < 0.001);
    ui.hide().unwrap();
}

#[test]
fn confirmed_small_seek_and_loop_reset_progress_through_controller_publication() {
    use crate::controller::{PlaybackProgress, publish_playback_progress};
    use audiolib_audio::{PlaybackState, PlaybackStatus};
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_audio_ready(true);
    ui.set_view_start(10.0);
    ui.set_view_end(10.2);
    ui.show().unwrap();
    let mut progress = PlaybackProgress::default();
    let mut status = PlaybackStatus {
        generation: 7,
        transport_revision: 3,
        play_session_id: 1,
        state: PlaybackState::Playing,
        duration: 20.0,
        position: 10.0,
        ..Default::default()
    };
    for _ in 0..2 {
        let continuous = progress.observe(&status);
        publish_playback_progress(&ui, &status, continuous);
        assert!(played_fraction(&ui).abs() < 0.001);
    }
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(65));
    assert!((played_fraction(&ui) - 0.325).abs() < 0.001);
    // Same source/session and a small forward step must still snap after Seek.
    status.position = 10.15;
    status.transport_revision += 1;
    let continuous = progress.observe(&status);
    assert!(!continuous);
    publish_playback_progress(&ui, &status, continuous);
    assert!((played_fraction(&ui) - 0.75).abs() < 0.001);
    assert!(!ui.get_playback_smooth());
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
    assert!((played_fraction(&ui) - 0.75).abs() < 0.001);
    status.position += 0.016;
    let continuous = progress.observe(&status);
    assert!(continuous);
    publish_playback_progress(&ui, &status, continuous);
    assert!(ui.get_playback_smooth());
    assert!((played_fraction(&ui) - 0.83).abs() < 0.001);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
    assert!(played_fraction(&ui) > 0.83);
    // A loop can finish and restart entirely between two status polls. Its next
    // position can be a small positive step, but the revision still resets it.
    status.position += 0.001;
    status.transport_revision += 1;
    let continuous = progress.observe(&status);
    assert!(!continuous);
    publish_playback_progress(&ui, &status, continuous);
    assert!((played_fraction(&ui) - 0.835).abs() < 0.001);
    ui.hide().unwrap();
}

#[test]
fn queued_playing_snapshots_do_not_merge_away_a_confirmed_seek_reset() {
    use crate::controller::{PlaybackProgress, publish_playback_progress};
    use audiolib_audio::{PlaybackState, PlaybackStatus};
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_audio_ready(true);
    ui.set_view_start(10.0);
    ui.set_view_end(10.2);
    ui.show().unwrap();
    let mut progress = PlaybackProgress::default();
    let mut status = PlaybackStatus {
        generation: 7,
        transport_revision: 3,
        state: PlaybackState::Playing,
        duration: 20.0,
        position: 10.0,
        ..Default::default()
    };
    for _ in 0..2 {
        let continuous = progress.observe(&status);
        publish_playback_progress(&ui, &status, continuous);
        assert!(played_fraction(&ui).abs() < 0.001);
    }
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(150));
    assert!((played_fraction(&ui) - 0.5).abs() < 0.001);
    // A busy UI processes the Seek confirmation and the following 65ms poll
    // together, before Slint's deferred changed handlers get to run.
    status.transport_revision += 1;
    status.position = 10.08;
    let continuous = progress.observe(&status);
    assert!(!continuous);
    publish_playback_progress(&ui, &status, continuous);
    status.position = 10.145;
    let continuous = progress.observe(&status);
    assert!(continuous);
    publish_playback_progress(&ui, &status, continuous);
    assert!(ui.get_playback_smooth());
    assert!((played_fraction(&ui) - 0.725).abs() < 0.001);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
    assert!(played_fraction(&ui) > 0.725);
    ui.hide().unwrap();
}

#[test]
fn played_waveform_region_clamps_to_viewport_and_tracks_source_position() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_audio_ready(true);
    ui.set_play_duration(20.0);
    ui.set_view_start(5.0);
    ui.set_view_end(15.0);
    ui.set_playback_smooth(true);
    ui.show().unwrap();
    for (seconds, fraction) in [
        (0.0, 0.0),
        (5.0, 0.0),
        (10.0, 0.5),
        (15.0, 1.0),
        (20.0, 1.0),
    ] {
        ui.set_play_position(seconds);
        assert!((played_fraction(&ui) - fraction).abs() < 0.001);
        let played = ElementHandle::find_by_accessible_label(&ui, "Played waveform region")
            .next()
            .unwrap();
        assert_eq!(
            played.accessible_role(),
            Some(AccessibleRole::ProgressIndicator)
        );
        assert_eq!(played.accessible_value_minimum(), Some(0.0));
        assert_eq!(played.accessible_value_maximum(), Some(20.0));
        let accessible_position = played.accessible_value().unwrap().parse::<f32>().unwrap();
        assert_eq!(accessible_position, seconds);
    }
    // Pause and Stop must update immediately even when forward-play smoothing is allowed.
    ui.set_play_position(12.0);
    assert!((played_fraction(&ui) - 0.7).abs() < 0.001);
    ui.set_play_position(0.0);
    assert!(played_fraction(&ui).abs() < 0.001);
    ui.set_play_duration(0.0);
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "Played waveform region")
            .next()
            .is_none()
    );
    ui.set_play_duration(20.0);
    ui.set_audio_ready(false);
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "Played waveform region")
            .next()
            .is_none()
    );
    ui.hide().unwrap();
}

#[test]
fn playback_progress_moves_each_frame_and_corrects_engine_snapshots_continuously() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_audio_ready(true);
    ui.set_play_duration(20.0);
    ui.set_view_start(5.0);
    ui.set_view_end(15.0);
    ui.set_play_position(6.0);
    ui.show().unwrap();
    assert!((played_fraction(&ui) - 0.1).abs() < 0.001);
    ui.set_playing(true);
    ui.set_playback_smooth(true);
    let mut previous = played_fraction(&ui);
    for elapsed_ms in [16, 32, 48, 64] {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
        let fraction = played_fraction(&ui);
        assert!(fraction > previous, "Playback should move on every frame");
        assert!((fraction - (0.1 + elapsed_ms as f32 / 10_000.0)).abs() < 0.0001);
        previous = fraction;
    }
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let before_snapshot = played_fraction(&ui);
    // A slightly delayed engine snapshot must preserve the displayed position.
    ui.set_play_position(6.06);
    assert!((played_fraction(&ui) - before_snapshot).abs() < 0.0001);
    let played = ElementHandle::find_by_accessible_label(&ui, "Played waveform region")
        .next()
        .unwrap();
    assert_eq!(
        played.accessible_value().unwrap().parse::<f32>().unwrap(),
        6.06
    );
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
    let after_snapshot = played_fraction(&ui);
    assert!(after_snapshot > before_snapshot);
    assert!((after_snapshot - 0.10802).abs() < 0.0001);

    // Identical snapshots must not restart prediction when output has stalled.
    ui.set_playback_smooth(false);
    ui.set_play_position(8.0);
    ui.set_playback_smooth(true);
    assert!((played_fraction(&ui) - 0.3).abs() < 0.0001);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(80));
    assert!((played_fraction(&ui) - 0.308).abs() < 0.0001);
    ui.set_play_position(8.0);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
    assert!((played_fraction(&ui) - 0.31).abs() < 0.0001);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    assert!((played_fraction(&ui) - 0.31).abs() < 0.0001);
    ui.hide().unwrap();
}

#[test]
fn playback_prediction_uses_confirmed_rate_and_transport_jumps_are_immediate() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_audio_ready(true);
    ui.set_play_duration(20.0);
    ui.set_view_start(5.0);
    ui.set_view_end(15.0);
    ui.set_playing(true);
    ui.show().unwrap();
    for rate in [0.25, 4.0] {
        ui.set_playback_smooth(false);
        ui.set_play_position(6.0);
        ui.set_confirmed_playback_rate(rate);
        ui.set_playback_smooth(true);
        assert!((played_fraction(&ui) - 0.1).abs() < 0.0001);
        for elapsed_ms in [16, 32, 48] {
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
            let expected = 0.1 + elapsed_ms as f32 / 10_000.0 * rate;
            assert!((played_fraction(&ui) - expected).abs() < 0.0001);
        }
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(52));
        let bounded = 0.1 + 0.01 * rate;
        assert!((played_fraction(&ui) - bounded).abs() < 0.0001);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        assert!((played_fraction(&ui) - bounded).abs() < 0.0001);
    }

    ui.set_playback_smooth(false);
    ui.set_play_position(14.0);
    assert!((played_fraction(&ui) - 0.9).abs() < 0.0001);
    ui.set_playback_smooth(true);
    assert!((played_fraction(&ui) - 0.9).abs() < 0.0001);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
    assert!(played_fraction(&ui) > 0.9);
    ui.set_playing(false);
    assert!((played_fraction(&ui) - 0.9).abs() < 0.0001);
    ui.set_play_position(7.0);
    assert!((played_fraction(&ui) - 0.2).abs() < 0.0001);
    ui.set_playing(true);
    assert!((played_fraction(&ui) - 0.2).abs() < 0.0001);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
    assert!(played_fraction(&ui) > 0.2);
    // Loop-back and Stop are discontinuities and must not retain extrapolation.
    ui.set_play_position(6.0);
    assert!((played_fraction(&ui) - 0.1).abs() < 0.0001);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
    assert!(played_fraction(&ui) > 0.1);
    ui.set_playing(false);
    ui.set_play_position(0.0);
    assert!(played_fraction(&ui).abs() < 0.0001);
    ui.set_playback_smooth(false);
    ui.set_playing(false);
    ui.set_play_position(0.0);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    assert!(played_fraction(&ui).abs() < 0.0001);
    ui.hide().unwrap();
}

#[test]
fn server_mode_edits_annotations_but_keeps_structural_writes_disabled() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_has_selection(true);
    ui.set_inspector_visible(true);
    ui.set_remote_mode(true);
    ui.show().unwrap();
    let enabled = |label: &str| {
        // Skip plain caption texts that share the control's label.
        ElementHandle::find_by_accessible_label(&ui, label)
            .find(|element| element.accessible_role() != Some(AccessibleRole::Text))
            .unwrap_or_else(|| panic!("missing {label}"))
            .accessible_enabled()
    };
    let inspector = ["Add to favorites", "Tags, comma separated"];
    // Annotations are saved to the server, so the inspector stays editable.
    for label in inspector {
        assert_eq!(enabled(label), Some(true), "{label}");
    }
    assert!(!ui.get_notes_visible());
    for label in ["Annotation color", "Notes"] {
        assert!(
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .is_none()
        );
    }
    assert!(!ui.get_source_metadata_visible());
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "Source Metadata")
            .next()
            .is_none()
    );
    for label in ["+  Source Metadata / SMED", "File path"] {
        assert!(
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .is_some()
        );
    }
    click_label(&ui, "Add to favorites");
    assert!(ui.get_edit_favorite());
    assert!(ui.get_metadata_dirty());
    click_label(&ui, "Remove from favorites");
    assert!(!ui.get_edit_favorite());
    let inspector_scroll = ElementHandle::find_by_element_id(&ui, "AppWindow::inspector-scroll")
        .next()
        .unwrap();
    let origin = inspector_scroll.absolute_position();
    let size = inspector_scroll.size();
    let inspector_position =
        slint::LogicalPosition::new(origin.x + size.width - 2.0, origin.y + 8.0);
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position: inspector_position,
        delta_x: 0.0,
        delta_y: -5000.0,
    });
    click_label(&ui, "+  Notes");
    assert!(ui.get_notes_visible());
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position: inspector_position,
        delta_x: 0.0,
        delta_y: -5000.0,
    });
    assert_eq!(enabled("Notes"), Some(true));
    ui.set_selected_id("42".into());
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    assert!(!ui.get_notes_visible());
    let row = AudioRow {
        id: "42".into(),
        name: "Remote.wav".into(),
        ..Default::default()
    };
    let popup = slint::LogicalPosition::new(380.0, 280.0);
    ui.invoke_show_file_menu(row.clone(), popup);
    let actions = Rc::new(RefCell::new(Vec::new()));
    let observed = actions.clone();
    ui.on_file_action_requested(move |id, action| observed.borrow_mut().push((id, action)));
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "Add to favorites (read-only in server mode)")
            .next()
            .is_none()
    );
    for label in [
        "Add to favorites",
        "Repair file reference… (read-only in server mode)",
        "Remove from library… (read-only in server mode)",
    ] {
        assert!(
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .is_some(),
            "{label}"
        );
    }
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "Remove from library…")
            .next()
            .is_none()
    );
    let remove = ElementHandle::find_by_accessible_label(
        &ui,
        "Remove from library… (read-only in server mode)",
    )
    .next()
    .unwrap();
    let origin = remove.absolute_position();
    let size = remove.size();
    let position = slint::LogicalPosition::new(
        popup.x + origin.x + size.width / 2.0,
        popup.y + origin.y + size.height / 2.0,
    );
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    assert!(
        actions.borrow().is_empty(),
        "disabled server-mode items must not dispatch"
    );
    let escape = || {
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: slint::platform::Key::Escape.into(),
        });
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: slint::platform::Key::Escape.into(),
        });
    };
    escape();
    // A server that does not let this PC manage: still greyed, with the server's reason.
    let refused = "Remove from library… (management is off on the server: manage_mode = off)";
    ui.set_server_manage_note("management is off on the server: manage_mode = off".into());
    ui.invoke_show_file_menu(row.clone(), popup);
    let click_item = |label: &str| {
        let item = ElementHandle::find_by_accessible_label(&ui, label)
            .next()
            .unwrap_or_else(|| panic!("missing {label}"));
        let origin = item.absolute_position();
        let size = item.size();
        let position = slint::LogicalPosition::new(
            popup.x + origin.x + size.width / 2.0,
            popup.y + origin.y + size.height / 2.0,
        );
        ui.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: slint::platform::PointerEventButton::Left,
        });
    };
    click_item(refused);
    assert!(
        actions.borrow().is_empty(),
        "still greyed without can_manage"
    );
    escape();
    // The active server lets this PC manage: the removal is offered and dispatched.
    ui.set_server_can_manage(true);
    ui.set_server_manage_note("".into());
    ui.invoke_show_file_menu(row, popup);
    click_item("Remove from library…");
    {
        let actions = actions.borrow();
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!((actions[0].0.as_str(), actions[0].1), ("42", 21));
    }
    ui.set_server_can_manage(false);
    ui.set_remote_mode(false);
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position: inspector_position,
        delta_x: 0.0,
        delta_y: 5000.0,
    });
    for label in inspector {
        assert_eq!(enabled(label), Some(true), "{label}");
    }
    ui.hide().unwrap();
}

#[test]
fn dragging_a_result_row_requests_drag_out_once_and_progress_can_be_cancelled() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_rows(ModelRc::new(VecModel::from(vec![AudioRow {
        id: "42".into(),
        name: "Drag me.wav".into(),
        ..Default::default()
    }])));
    ui.set_audition_play_on_single_click(true);
    let plays = Rc::new(RefCell::new(Vec::new()));
    let observed = plays.clone();
    ui.on_row_clicked(move |_, _, _, play| observed.borrow_mut().push(play));
    ui.on_audition_requested(|_| panic!("Dragging must not audition a result"));
    ui.show().unwrap();
    let requests = Rc::new(RefCell::new(Vec::new()));
    let observed = requests.clone();
    ui.on_drag_out_requested(move |id| observed.borrow_mut().push(id.to_string()));
    let name = ElementHandle::find_by_accessible_label(&ui, "Drag me.wav")
        .next()
        .expect("row name");
    let origin = name.absolute_position();
    let start = slint::LogicalPosition::new(origin.x + 4.0, origin.y + 4.0);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position: start });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position: start,
        button: slint::platform::PointerEventButton::Left,
    });
    // Below the threshold: still a click, no drag.
    ui.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(start.x + 3.0, start.y),
    });
    assert!(requests.borrow().is_empty());
    for step in 1..4 {
        ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(start.x + 10.0 * step as f32, start.y + 2.0),
        });
    }
    assert_eq!(
        *requests.borrow(),
        vec!["42".to_string()],
        "one request per press"
    );
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position: start,
        button: slint::platform::PointerEventButton::Left,
    });
    assert!(
        plays.borrow().iter().all(|play| !play),
        "Dragging must not play"
    );

    let cancelled = Rc::new(RefCell::new(0));
    let counter = cancelled.clone();
    ui.on_drag_cancel_requested(move || *counter.borrow_mut() += 1);
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "Cancel drag")
            .next()
            .is_none()
    );
    ui.set_drag_progress("Preparing drag 1/2: Drag me.wav 40%".into());
    ui.set_drag_preparing(true);
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "Preparing drag 1/2: Drag me.wav 40%")
            .next()
            .is_some()
    );
    ElementHandle::find_by_accessible_label(&ui, "Cancel drag")
        .next()
        .expect("cancel button")
        .invoke_accessible_default_action();
    assert_eq!(*cancelled.borrow(), 1);
    ui.hide().unwrap();
}

fn source_rows(server_active: bool) -> ModelRc<SourceRow> {
    ModelRc::new(VecModel::from(vec![
        SourceRow {
            id: "local".into(),
            name: "Local library".into(),
            subtitle: "D:/AudioLib/library.db".into(),
            kind: 0,
            active: !server_active,
            status: "".into(),
            status_kind: 0,
        },
        SourceRow {
            id: "home".into(),
            name: "Home Mac".into(),
            subtitle: "http://server.example.lan:8799".into(),
            kind: 1,
            active: server_active,
            status: "not tested".into(),
            status_kind: 0,
        },
    ]))
}

fn has_label(ui: &AppWindow, label: &str) -> bool {
    ElementHandle::find_by_accessible_label(ui, label)
        .next()
        .is_some()
}

#[cfg(windows)]
#[test]
#[ignore = "requires an interactive Windows desktop and the real Slint backend"]
fn real_windows_connection_editor_accepts_pointer_and_keyboard() {
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_sm_sources(source_rows(false));
    ui.show().unwrap();
    let opening_timer = slint::Timer::default();
    let opening_ui = ui.as_weak();
    opening_timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(500),
        move || {
            let ui = opening_ui.upgrade().unwrap();
            ui.invoke_open_data_source();
            ui.invoke_sm_select_row(-1);
        },
    );
    let pointer_ui = ui.as_weak();
    let pointer_timer = slint::Timer::default();
    pointer_timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(1000),
        move || {
            let ui = pointer_ui.upgrade().unwrap();
            click_connection_field(&ui, "Server profile name");
        },
    );
    // Real mouse and keyboard events arrive on separate event-loop turns.
    // Sending both in one callback hides a timer that repeatedly steals focus.
    let typing_ui = ui.as_weak();
    let typing_timer = slint::Timer::default();
    typing_timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(1100),
        move || {
            let ui = typing_ui.upgrade().unwrap();
            send_keys(&ui, &["Windows input".into()]);
        },
    );
    let closing_ui = ui.as_weak();
    let closing_timer = slint::Timer::default();
    closing_timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(1200),
        move || {
            let ui = closing_ui.upgrade().unwrap();
            ui.hide().unwrap();
            slint::quit_event_loop().unwrap();
        },
    );
    slint::run_event_loop().unwrap();
    assert_eq!(ui.get_ds_name(), "Windows input");
}

#[test]
fn source_connection_fields_accept_pointer_focus_and_keyboard_edits() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        for (width, height) in [(1080.0, 700.0), (1440.0, 900.0)] {
            let ui = AppWindow::new().unwrap();
            ui.window().set_size(slint::LogicalSize::new(width, height));
            ui.set_chinese(chinese);
            ui.set_sm_sources(source_rows(false));
            ui.set_query("Background query".into());
            ui.set_edit_tags("Background tags".into());
            let saves = Rc::new(RefCell::new(0));
            let observed = saves.clone();
            ui.on_sm_save_requested(move || *observed.borrow_mut() += 1);
            ui.show().unwrap();
            click_menu_item(
                &ui,
                if chinese { "音效库" } else { "Library" },
                if chinese {
                    "音效库管理…"
                } else {
                    "Manage sound library…"
                },
            );
            ui.invoke_sm_select_row(-1);
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
            click_connection_field(
                &ui,
                if chinese {
                    "服务器配置名称"
                } else {
                    "Server profile name"
                },
            );
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
            send_keys(&ui, &["Home server".into()]);
            assert_eq!(ui.get_ds_name(), "Home server");
            assert!(ui.get_ds_dirty());
            click_connection_field(
                &ui,
                if chinese {
                    "服务器地址"
                } else {
                    "Server URL"
                },
            );
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
            send_keys(&ui, &["http://127.0.0.1:18790".into()]);
            assert_eq!(ui.get_ds_server_url(), "http://127.0.0.1:18790");
            click_label(
                &ui,
                if chinese {
                    "域名 + 端口"
                } else {
                    "Domain + port"
                },
            );
            for (cn, en, text) in [
                ("服务器域名或 IP", "Server domain or IP", "127.0.0.1"),
                ("端口", "Port", "18790"),
                ("路径前缀（可选）", "Path prefix (optional)", "/sounds"),
                ("访问令牌", "Access token", "isolated-test-token"),
            ] {
                click_connection_field(&ui, if chinese { cn } else { en });
                i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
                send_keys(&ui, &[text.into()]);
            }
            assert_eq!(ui.get_ds_host(), "127.0.0.1");
            assert_eq!(ui.get_ds_port(), "18790");
            assert_eq!(ui.get_ds_prefix(), "/sounds");
            assert_eq!(ui.get_ds_token(), "isolated-test-token");
            click_connection_field(
                &ui,
                if chinese {
                    "版本检查间隔（秒）"
                } else {
                    "Revision check interval (seconds)"
                },
            );
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
            send_keys(&ui, &[slint::platform::Key::Control.into(), "a".into()]);
            send_keys(&ui, &["90".into()]);
            assert_eq!(ui.get_ds_poll_interval(), "90");
            ui.set_ds_applying(true);
            send_keys(&ui, &["ignored".into()]);
            assert_eq!(ui.get_ds_poll_interval(), "90");
            ui.set_ds_applying(false);
            click_label(
                &ui,
                if chinese {
                    "保存为新服务器"
                } else {
                    "Save new server"
                },
            );
            assert_eq!(*saves.borrow(), 1);
            assert_eq!(ui.get_query(), "Background query");
            assert_eq!(ui.get_edit_tags(), "Background tags");
            ui.hide().unwrap();
        }
    }
}

/// Scroll the connection field into view and click its visible center.
pub(crate) fn click_connection_field(ui: &AppWindow, label: &str) {
    let wrapper = ElementHandle::find_by_element_type_name(ui, "ThemedLineEdit")
        .find(|element| {
            let label = label.to_owned();
            element
                .query_descendants()
                .match_predicate(move |child| {
                    child.accessible_label().is_some_and(|value| value == label)
                })
                .find_first()
                .is_some()
        })
        .unwrap_or_else(|| panic!("Missing input wrapper: {label}"));
    let scroll = ElementHandle::find_by_element_id(ui, "AppWindow::connection-scroll")
        .next()
        .unwrap();
    let top = scroll.absolute_position().y + 4.0;
    let bottom = scroll.absolute_position().y + scroll.size().height - 4.0;
    let field_top = wrapper.absolute_position().y;
    let field_bottom = field_top + wrapper.size().height;
    if field_top < top {
        scroll.scroll(0.0, top - field_top);
    } else if field_bottom > bottom {
        scroll.scroll(0.0, bottom - field_bottom);
    }
    let origin = wrapper.absolute_position();
    let size = wrapper.size();
    click_at(
        ui,
        slint::LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0),
    );
}

#[test]
fn unified_source_window_lists_sources_and_shows_server_pages_in_both_languages() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
    let calls = Rc::new(RefCell::new(Vec::<String>::new()));
    let observed = calls.clone();
    ui.on_sm_open_requested(move |focus| observed.borrow_mut().push(format!("open {focus}")));
    let observed = calls.clone();
    ui.on_sm_source_selected(move |index| observed.borrow_mut().push(format!("select {index}")));
    let observed = calls.clone();
    ui.on_sm_page_selected(move |page| observed.borrow_mut().push(format!("page {page}")));
    let observed = calls.clone();
    ui.on_sm_switch_requested(move || observed.borrow_mut().push("switch".into()));
    let observed = calls.clone();
    ui.on_sm_delete_requested(move || observed.borrow_mut().push("delete".into()));
    let observed = calls.clone();
    ui.on_sm_add_requested(move || observed.borrow_mut().push("add".into()));
    ui.show().unwrap();
    for chinese in [false, true] {
        ui.set_chinese(chinese);
        ui.set_sm_sources(source_rows(false));
        ui.set_sm_selected(0);
        ui.set_sm_page(0);
        ui.set_ds_dirty(false);
        calls.borrow_mut().clear();
        let (file, item) = if chinese {
            ("音效库", "音效库管理…")
        } else {
            ("Library", "Manage sound library…")
        };
        click_menu_item(&ui, file, item);
        assert!(ui.get_library_visible());
        assert_eq!(&*calls.borrow(), &["open 1"]);
        // Local mode: the library management content shows for the local row.
        assert!(has_label(
            &ui,
            if chinese {
                "添加文件夹…"
            } else {
                "Add folder…"
            }
        ));
        assert!(!has_label(
            &ui,
            if chinese {
                "测试连接"
            } else {
                "Test connection"
            }
        ));
        // Server settings open the first saved profile without unimplemented WebDAV controls.
        click_label(
            &ui,
            if chinese {
                "服务器设置"
            } else {
                "Server settings"
            },
        );
        assert_eq!(ui.get_sm_selected(), 1);
        assert_eq!(ui.get_sm_page(), 0);
        assert!(has_label(
            &ui,
            if chinese {
                "测试连接"
            } else {
                "Test connection"
            }
        ));
        let webdav_user = if chinese {
            "WebDAV 用户名"
        } else {
            "WebDAV user"
        };
        assert!(!has_label(&ui, webdav_user));
        let placeholder = if chinese {
            "WebDAV 设置（功能未开发）"
        } else {
            "WebDAV settings (not developed)"
        };
        assert!(!has_label(&ui, placeholder));
        assert!(!ui.get_ds_dirty());
        // Not active: the switch action is offered; unsaved edits turn it into save + switch.
        click_label(
            &ui,
            if chinese {
                "切换到此数据源"
            } else {
                "Switch to this source"
            },
        );
        ui.set_ds_dirty(true);
        assert!(has_label(
            &ui,
            if chinese {
                "保存并切换"
            } else {
                "Save and switch"
            }
        ));
        ui.set_ds_dirty(false);
        // 服务器音效库 (management controls are covered by the gating test).
        click_label(
            &ui,
            if chinese {
                "服务器音效库"
            } else {
                "Server library"
            },
        );
        assert_eq!(ui.get_sm_page(), 1);
        assert!(has_label(
            &ui,
            if chinese {
                "开始扫描…"
            } else {
                "Start a scan…"
            }
        ));
        assert!(has_label(
            &ui,
            if chinese {
                "扫描历史（最近 20 次）"
            } else {
                "Scan history (latest 20)"
            }
        ));
        // Deleting asks inline first.
        click_label(
            &ui,
            if chinese {
                "删除此服务器配置…"
            } else {
                "Delete this server profile…"
            },
        );
        assert!(ui.get_sm_confirm_delete());
        click_label(
            &ui,
            if chinese {
                "确认删除"
            } else {
                "Delete profile"
            },
        );
        assert!(!ui.get_sm_confirm_delete());
        click_label(
            &ui,
            if chinese {
                "＋ 添加服务器"
            } else {
                "+ Add server"
            },
        );
        assert_eq!(ui.get_sm_selected(), -1);
        assert!(has_label(
            &ui,
            if chinese {
                "保存为新服务器"
            } else {
                "Save new server"
            }
        ));
        assert_eq!(
            &*calls.borrow(),
            &["open 1", "select 1", "switch", "page 1", "delete", "add"]
        );
        // The active profile cannot be deleted and offers no switch.
        ui.set_sm_sources(source_rows(true));
        ui.set_sm_selected(1);
        ui.set_sm_page(0);
        assert!(!has_label(
            &ui,
            if chinese {
                "切换到此数据源"
            } else {
                "Switch to this source"
            }
        ));
        click_label(
            &ui,
            if chinese {
                "删除此服务器配置…"
            } else {
                "Delete this server profile…"
            },
        );
        assert!(!ui.get_sm_confirm_delete(), "the active profile stays");
        send_keys(&ui, &[slint::platform::Key::Escape.into()]);
        assert!(!ui.get_library_visible());
    }
    ui.hide().unwrap();
}

#[test]
fn local_source_switch_remains_clickable_during_server_sync_in_both_languages() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
        ui.set_chinese(chinese);
        ui.set_sm_sources(source_rows(true));
        ui.set_sm_selected(1);
        ui.set_remote_mode(true);
        ui.set_ds_syncing(true);
        let switches = Rc::new(RefCell::new(0));
        let observed = switches.clone();
        ui.on_sm_switch_requested(move || *observed.borrow_mut() += 1);
        ui.show().unwrap();
        ui.invoke_open_library(0);
        click_label(
            &ui,
            if chinese {
                "本地设置"
            } else {
                "Local settings"
            },
        );
        assert_eq!(ui.get_sm_selected(), 0);
        let label = if chinese {
            "切换到此数据源"
        } else {
            "Switch to this source"
        };
        let action = ElementHandle::find_by_accessible_label(&ui, label)
            .next()
            .unwrap();
        assert_eq!(action.accessible_enabled(), Some(true));
        click_label(&ui, label);
        assert_eq!(*switches.borrow(), 1);
        // Other library operations and a settings write still own their gate.
        ui.set_library_busy(true);
        click_label(&ui, label);
        ui.set_library_busy(false);
        ui.set_ds_applying(true);
        click_label(&ui, label);
        assert_eq!(*switches.borrow(), 1);
        // A saved server activation can be abandoned before its provider opens.
        ui.set_library_busy(true);
        ui.set_ds_source_syncing(true);
        ui.set_sm_selected(1);
        click_label(
            &ui,
            if chinese {
                "本地设置"
            } else {
                "Local settings"
            },
        );
        assert_eq!(ui.get_sm_selected(), 0);
        click_label(&ui, label);
        assert_eq!(*switches.borrow(), 2);
        ui.hide().unwrap();
    }
}

#[test]
fn keyboard_can_escape_a_pending_server_activation_past_other_profiles() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
        ui.set_chinese(chinese);
        ui.set_remote_mode(true);
        let existing = source_rows(true);
        let mut sources = (0..existing.row_count())
            .map(|index| existing.row_data(index).unwrap())
            .collect::<Vec<_>>();
        sources[1].active = false;
        sources.push(SourceRow {
            id: "other".into(),
            name: "Other server".into(),
            kind: 1,
            active: true,
            ..Default::default()
        });
        ui.set_sm_sources(ModelRc::new(VecModel::from(sources)));
        ui.set_sm_selected(2);
        let switches = Rc::new(RefCell::new(0));
        let observed = switches.clone();
        ui.on_sm_switch_requested(move || *observed.borrow_mut() += 1);
        ui.show().unwrap();
        ui.invoke_open_library(0);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        click_label(&ui, "● Other server");
        ui.set_ds_syncing(true);
        ui.set_ds_applying(true);
        ui.set_library_busy(true);
        ui.set_ds_source_syncing(true);
        send_keys(&ui, &[slint::platform::Key::UpArrow.into()]);
        assert_eq!(ui.get_sm_selected(), 0, "skip unavailable server profiles");
        send_keys(&ui, &[slint::platform::Key::DownArrow.into()]);
        assert_eq!(ui.get_sm_selected(), 0);
        send_keys(&ui, &[slint::platform::Key::Tab.into()]);
        send_keys(&ui, &[slint::platform::Key::Return.into()]);
        assert_eq!(*switches.borrow(), 1, "keyboard activates local recovery");
        // Provider opening and settings persistence still block keyboard actions.
        ui.set_ds_source_syncing(false);
        send_keys(&ui, &[slint::platform::Key::Return.into()]);
        assert_eq!(*switches.borrow(), 1);
        ui.hide().unwrap();
    }
}

#[test]
fn library_manager_close_and_escape_preserve_unsaved_server_settings() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        for escape in [false, true] {
            let ui = AppWindow::new().unwrap();
            ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
            ui.set_chinese(chinese);
            ui.set_sm_sources(source_rows(false));
            ui.show().unwrap();
            ui.invoke_open_library(0);
            ui.set_sm_selected(1);
            ui.set_sm_page(0);
            ui.set_ds_name("Unsaved server".into());
            ui.set_ds_dirty(true);
            let saves = Rc::new(RefCell::new(Vec::new()));
            let observed = saves.clone();
            ui.on_sm_pending_save_requested(move |target| observed.borrow_mut().push(target));
            if escape {
                send_keys(&ui, &[slint::platform::Key::Escape.into()]);
            } else {
                click_label(&ui, if chinese { "关闭" } else { "Close" });
            }
            assert!(ui.get_library_visible());
            assert_eq!(ui.get_sm_pending_select(), -3);
            // Escape cancels the question, retaining the draft and its owner.
            send_keys(&ui, &[slint::platform::Key::Escape.into()]);
            assert_eq!(ui.get_sm_pending_select(), -2);
            assert!(ui.get_library_visible() && ui.get_ds_dirty());
            assert_eq!(ui.get_ds_name(), "Unsaved server");
            assert_eq!(ui.get_sm_selected(), 1);
            ui.invoke_close_library_manager();
            click_label(&ui, if chinese { "取消" } else { "Cancel" });
            assert_eq!(ui.get_sm_pending_select(), -2);
            assert!(ui.get_library_visible() && ui.get_ds_dirty());
            ui.invoke_close_library_manager();
            click_label(&ui, if chinese { "保存" } else { "Save" });
            assert_eq!(&*saves.borrow(), &[-3]);
            assert!(ui.get_library_visible(), "wait for the controller save");
            ui.set_ds_applying(true);
            click_label(&ui, if chinese { "放弃修改" } else { "Discard" });
            send_keys(&ui, &[slint::platform::Key::Escape.into()]);
            assert!(ui.get_library_visible() && ui.get_ds_dirty());
            ui.set_ds_applying(false);
            click_label(&ui, if chinese { "放弃修改" } else { "Discard" });
            assert!(!ui.get_library_visible());
            assert!(!ui.get_ds_dirty());
            assert_eq!(ui.get_sm_pending_select(), -2);
            assert_eq!(ui.get_sm_selected(), 1, "discard must not add a server");
            ui.hide().unwrap();
        }
    }
}

#[test]
fn unified_source_window_keeps_keyboard_focus_out_of_hidden_local_content() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
    ui.show().unwrap();
    // Server mode, active server on 服务器音效库: the local content is hidden.
    ui.set_remote_mode(true);
    ui.set_sm_sources(source_rows(true));
    ui.invoke_open_library(0);
    ui.set_sm_selected(1);
    ui.set_sm_page(1);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    send_keys(
        &ui,
        &[
            slint::platform::Key::Shift.into(),
            slint::platform::Key::Tab.into(),
        ],
    );
    send_keys(&ui, &[slint::platform::Key::Return.into()]);
    assert!(
        !ui.get_advanced_maintenance_visible(),
        "reverse Tab must not reach the hidden maintenance toggle"
    );
    ui.hide().unwrap();
}

#[test]
fn unified_source_window_prompts_for_unsaved_edits_and_moves_with_arrow_keys() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
    let calls = Rc::new(RefCell::new(Vec::<String>::new()));
    let observed = calls.clone();
    ui.on_sm_source_selected(move |index| observed.borrow_mut().push(format!("select {index}")));
    let observed = calls.clone();
    ui.on_sm_add_requested(move || observed.borrow_mut().push("add".into()));
    let observed = calls.clone();
    ui.on_sm_save_requested(move || observed.borrow_mut().push("save".into()));
    let observed = calls.clone();
    ui.on_sm_pending_save_requested(move |index| {
        observed.borrow_mut().push(format!("pending-save {index}"))
    });
    ui.show().unwrap();
    for chinese in [false, true] {
        ui.set_chinese(chinese);
        ui.set_remote_mode(false);
        ui.set_sm_sources(source_rows(false));
        ui.invoke_open_library(0);
        ui.set_sm_selected(1);
        ui.set_sm_page(0);
        ui.set_ds_dirty(false);
        calls.borrow_mut().clear();
        // Switching settings with unsaved connection edits asks first and stays put.
        ui.set_ds_dirty(true);
        click_label(
            &ui,
            if chinese {
                "本地设置"
            } else {
                "Local settings"
            },
        );
        assert_eq!(ui.get_sm_pending_select(), 0);
        assert_eq!(ui.get_sm_selected(), 1);
        assert!(has_label(
            &ui,
            if chinese {
                "连接页有未保存的修改。"
            } else {
                "The connection page has unsaved changes."
            }
        ));
        // [放弃修改] drops the edits and opens the row.
        click_label(&ui, if chinese { "放弃修改" } else { "Discard" });
        assert!(!ui.get_ds_dirty());
        assert_eq!(ui.get_sm_pending_select(), -2);
        assert_eq!(ui.get_sm_selected(), 0);
        // Without edits a row opens at once.
        ui.invoke_sm_select_row(1);
        assert_eq!(ui.get_sm_selected(), 1);
        // Up with edits asks too; [取消] keeps the edits and the row.
        ui.set_ds_dirty(true);
        ui.invoke_sm_step(false);
        assert_eq!(ui.get_sm_pending_select(), 0);
        click_label(&ui, if chinese { "取消" } else { "Cancel" });
        assert_eq!(ui.get_sm_pending_select(), -2);
        assert_eq!(ui.get_sm_selected(), 1);
        assert!(ui.get_ds_dirty());
        // 添加服务器 with edits; [保存] saves first, then opens the new server.
        ui.invoke_sm_select_row(-1);
        assert_eq!(ui.get_sm_pending_select(), -1);
        click_label(&ui, if chinese { "保存" } else { "Save" });
        ui.set_ds_dirty(false);
        ui.set_sm_pending_select(-2);
        assert_eq!(
            &*calls.borrow(),
            &["select 0", "select 1", "pending-save -1"]
        );
        // Up/Down move through the rows once the list has focus.
        calls.borrow_mut().clear();
        click_label(&ui, "○ Home Mac");
        send_keys(&ui, &[slint::platform::Key::UpArrow.into()]);
        assert_eq!(ui.get_sm_selected(), 0);
        send_keys(&ui, &[slint::platform::Key::UpArrow.into()]);
        assert_eq!(ui.get_sm_selected(), 0, "the first row stays");
        send_keys(&ui, &[slint::platform::Key::DownArrow.into()]);
        assert_eq!(ui.get_sm_selected(), 1);
        send_keys(&ui, &[slint::platform::Key::DownArrow.into()]);
        assert_eq!(ui.get_sm_selected(), 1, "the last row stays");
        assert_eq!(&*calls.borrow(), &["select 0", "select 1"]);
        send_keys(&ui, &[slint::platform::Key::Escape.into()]);
        assert!(!ui.get_library_visible());
    }
    ui.hide().unwrap();
}

#[test]
fn unified_source_window_gates_server_scans_and_removals_on_the_active_managed_server() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 760.0));
    let calls = Rc::new(RefCell::new(Vec::<String>::new()));
    let observed = calls.clone();
    ui.on_sm_scan_control_requested(move |action| {
        observed.borrow_mut().push(format!("control {action}"))
    });
    let observed = calls.clone();
    ui.on_sm_scan_start_requested(move || observed.borrow_mut().push("start".into()));
    let observed = calls.clone();
    ui.on_sm_remove_records_requested(move || observed.borrow_mut().push("remove".into()));
    let observed = calls.clone();
    ui.on_sm_remove_folder_requested(move || observed.borrow_mut().push("folder".into()));
    ui.show().unwrap();
    for chinese in [false, true] {
        ui.set_chinese(chinese);
        ui.set_remote_mode(true);
        ui.set_sm_sources(source_rows(true));
        ui.set_sm_root_names(ModelRc::new(VecModel::from(vec![
            slint::SharedString::from("SFX"),
        ])));
        ui.set_checked_count(2);
        ui.set_selected_folder_path("/V/SFX/Doors".into());
        ui.set_sm_scan_active(false);
        ui.set_sm_scan_paused(false);
        ui.set_sm_scan_form(false);
        ui.set_sm_confirm(0);
        calls.borrow_mut().clear();
        let start = if chinese {
            "开始扫描…"
        } else {
            "Start a scan…"
        };
        let remove = if chinese {
            "移除所选记录…"
        } else {
            "Remove checked records…"
        };
        let folder = if chinese {
            "删除目录记录…"
        } else {
            "Remove folder records…"
        };
        // The server does not allow management from here: grey, with the server's mode.
        ui.set_sm_can_manage(false);
        ui.set_sm_capabilities("manage_mode = lan".into());
        ui.invoke_open_library(0);
        ui.set_sm_selected(1);
        ui.set_sm_page(1);
        assert!(has_label(
            &ui,
            &format!(
                "{} manage_mode = lan",
                if chinese {
                    "服务器不允许本机管理，只能查看。"
                } else {
                    "The server does not allow management from this PC; view only."
                }
            )
        ));
        click_label(&ui, start);
        click_label(&ui, remove);
        assert!(!ui.get_sm_scan_form() && ui.get_sm_confirm() == 0);
        // Allowed: the start form and the inline removal confirmations open.
        ui.set_sm_page(0);
        ui.set_sm_can_manage(true);
        ui.set_sm_page(1);
        click_label(&ui, start);
        assert!(ui.get_sm_scan_form());
        ui.set_sm_scan_form(false);
        click_label(&ui, remove);
        assert_eq!(ui.get_sm_confirm(), 1);
        ui.set_sm_confirm(0);
        click_label(&ui, folder);
        assert_eq!(ui.get_sm_confirm(), 2);
        ui.set_sm_confirm(0);
        // The removal text says only records go and WAV files stay.
        assert!(has_label(
            &ui,
            if chinese {
                "只删除服务器音效库里的记录（含标签、备注等标注），服务器上的 WAV 文件不会改动；重新扫描会以新的 id 把它们加回。扫描进行中不能移除。"
            } else {
                "Only removes records (with their tags and notes) from the server library; WAV files on the server are untouched, and a rescan adds them back with new ids. Not possible while a scan runs."
            }
        ));
        // A running scan: pause / resume / cancel; no new scan or removal.
        ui.set_sm_page(0);
        ui.set_sm_scan_active(true);
        ui.set_sm_page(1);
        click_label(&ui, start);
        click_label(&ui, remove);
        assert!(!ui.get_sm_scan_form() && ui.get_sm_confirm() == 0);
        // 「暂停」/"Pause" is also the player's transport label behind the modal, so the
        // first match is not this button: pause goes through the callback it invokes.
        ui.invoke_sm_scan_control_requested(0);
        ui.set_sm_scan_paused(true);
        click_label(&ui, if chinese { "继续" } else { "Resume" });
        click_label(
            &ui,
            if chinese {
                "取消扫描"
            } else {
                "Cancel scan"
            },
        );
        assert_eq!(&*calls.borrow(), &["control 0", "control 1", "control 2"]);
        // Another (inactive) profile: viewable, managed only after switching to it.
        ui.set_sm_page(0);
        ui.set_sm_scan_active(false);
        ui.set_sm_scan_paused(false);
        ui.set_sm_sources(source_rows(false));
        ui.set_sm_page(1);
        assert!(has_label(
            &ui,
            if chinese {
                "只有当前数据源可以管理；请先切换到此数据源。"
            } else {
                "Only the active source can be managed; switch to this source first."
            }
        ));
        click_label(&ui, start);
        assert!(!ui.get_sm_scan_form());
        send_keys(&ui, &[slint::platform::Key::Escape.into()]);
        assert!(!ui.get_library_visible());
    }
    ui.hide().unwrap();
}

#[test]
fn appearance_settings_live_in_the_view_menu_in_both_languages() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
    ui.show().unwrap();
    for chinese in [false, true] {
        ui.set_chinese(chinese);
        let (view, playback, label) = if chinese {
            ("视图", "播放", "外观设置…")
        } else {
            ("View", "Playback", "Appearance settings…")
        };
        open_menu(&ui, playback);
        assert!(
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .is_none(),
            "Appearance settings must no longer be in the Playback menu"
        );
        close_menu(&ui);
        ui.set_settings_visible(false);
        ui.set_settings_page(0);
        click_menu_item(&ui, view, label);
        assert!(ui.get_settings_visible(), "View menu must open settings");
        assert_eq!(
            ui.get_settings_page(),
            1,
            "View menu must open the Appearance page"
        );
        ui.set_settings_visible(false);
    }
}
