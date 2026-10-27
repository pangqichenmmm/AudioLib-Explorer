// app/audiolib-desktop/src/ui_review_tests.rs
//! Interaction regressions for the client review; all data belongs to the test backend.
use crate::ui_tests::{click_connection_field, click_label, send_keys};
use crate::{AppWindow, FolderRow, SourceRow};
use i_slint_backend_testing::ElementHandle;
use slint::{ComponentHandle, Model, ModelRc, VecModel, platform::Key};
use std::{cell::RefCell, rc::Rc, time::Duration};

fn window() -> AppWindow {
    let ui = AppWindow::new().unwrap();
    ui.window()
        .set_size(slint::LogicalSize::new(1440.0, 1000.0));
    ui.set_query("background query".into());
    ui.show().unwrap();
    ui
}

#[test]
fn update_consent_blocks_background_keys_and_disables_install_while_downloading() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = window();
        ui.set_chinese(chinese);
        ui.set_update_available(true);
        ui.set_update_visible(true);
        let installs = Rc::new(RefCell::new(0));
        let observed = installs.clone();
        ui.on_update_install_requested(move || *observed.borrow_mut() += 1);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(5));
        send_keys(&ui, &["z".into()]);
        assert_eq!(ui.get_query(), "background query");
        let label = if chinese {
            "下载、安装并重启"
        } else {
            "Download, install & restart"
        };
        ui.set_update_busy(true);
        click_label(&ui, label);
        assert_eq!(*installs.borrow(), 0);
        ui.set_update_busy(false);
        ui.set_update_enabled(false);
        click_label(&ui, label);
        assert_eq!(*installs.borrow(), 0);
        ui.set_update_enabled(true);
        click_label(&ui, label);
        assert_eq!(*installs.borrow(), 1);
        send_keys(&ui, &[Key::Escape.into()]);
        assert!(!ui.get_update_visible());
        assert_eq!(*installs.borrow(), 1, "Dismissing never confirms an update");
        ui.hide().unwrap();
    }
}

#[test]
fn offline_metadata_keeps_revert_and_recovery_exit_reachable() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = window();
        ui.set_chinese(chinese);
        ui.set_has_selection(true);
        ui.set_metadata_dirty(true);
        ui.set_metadata_writes_blocked(true);
        ui.set_close_recovery_available(true);
        let actions = Rc::new(RefCell::new(Vec::new()));
        let observed = actions.clone();
        ui.on_restore_metadata(move || observed.borrow_mut().push("revert"));
        let observed = actions.clone();
        ui.on_save_metadata_requested(move || observed.borrow_mut().push("save"));
        let observed = actions.clone();
        ui.on_recover_metadata_and_close(move || observed.borrow_mut().push("recover"));
        click_label(&ui, if chinese { "立即保存" } else { "Save now" });
        click_label(&ui, if chinese { "还原" } else { "Revert" });
        click_label(
            &ui,
            if chinese {
                "保存恢复文件并退出"
            } else {
                "Save recovery file and exit"
            },
        );
        assert_eq!(&*actions.borrow(), &["revert", "recover"]);
        ui.hide().unwrap();
    }
}

#[test]
fn shared_sync_edits_do_not_mark_a_server_profile_dirty() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = window();
    ui.set_sm_sources(ModelRc::new(VecModel::from(vec![
        SourceRow {
            name: "Local".into(),
            kind: 0,
            ..Default::default()
        },
        SourceRow {
            name: "Server".into(),
            kind: 1,
            ..Default::default()
        },
    ])));
    ui.invoke_open_data_source();
    ui.set_sm_selected(1);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    click_connection_field(&ui, "Revision check interval (seconds)");
    send_keys(&ui, &[Key::Control.into(), "a".into()]);
    send_keys(&ui, &["90".into()]);
    assert_eq!(ui.get_ds_poll_interval(), "90");
    assert!(ui.get_shared_sync_dirty());
    assert!(
        !ui.get_ds_dirty(),
        "shared preferences have their own save action"
    );
    let saves = Rc::new(RefCell::new(0));
    let observed = saves.clone();
    ui.on_shared_sync_save_requested(move || *observed.borrow_mut() += 1);
    click_label(&ui, "Save sync settings");
    assert_eq!(*saves.borrow(), 1);
    assert!(!ui.get_ds_dirty());
    ui.invoke_close_library_manager();
    assert!(ui.get_library_visible() && ui.get_shared_sync_close_pending());
    click_label(&ui, "Keep open");
    assert!(ui.get_library_visible() && !ui.get_shared_sync_close_pending());
    assert_eq!(ui.get_ds_poll_interval(), "90");
    ui.invoke_close_library_manager();
    click_label(&ui, "Discard sync edits");
    assert!(!ui.get_library_visible());
    ui.hide().unwrap();
}

#[test]
fn folder_tree_supports_keyboard_selection_and_expansion() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = window();
    ui.set_folders(ModelRc::new(VecModel::from(vec![
        FolderRow {
            name: "One".into(),
            has_children: true,
            ..Default::default()
        },
        FolderRow {
            name: "Two".into(),
            has_children: true,
            expanded: true,
            ..Default::default()
        },
    ])));
    let selections = Rc::new(RefCell::new(Vec::new()));
    let observed = selections.clone();
    let weak = ui.as_weak();
    ui.on_folder_requested(move |index| {
        weak.upgrade().unwrap().set_selected_folder(index);
        observed.borrow_mut().push(index);
    });
    let toggles = Rc::new(RefCell::new(Vec::new()));
    let observed = toggles.clone();
    ui.on_folder_toggle_requested(move |index| observed.borrow_mut().push(index));
    click_label(&ui, "One");
    send_keys(&ui, &[Key::RightArrow.into()]);
    send_keys(&ui, &[Key::DownArrow.into()]);
    send_keys(&ui, &[Key::LeftArrow.into()]);
    send_keys(&ui, &[Key::UpArrow.into()]);
    assert_eq!(&*selections.borrow(), &[0, 1, 0]);
    assert_eq!(&*toggles.borrow(), &[0, 1]);
    assert_eq!(ui.get_query(), "background query");
    ui.hide().unwrap();
}

#[test]
fn folder_double_click_expands_and_collapses_without_toggling_leaves_or_arrow_twice() {
    i_slint_backend_testing::init_no_event_loop();
    for scale in [1.0, 2.0] {
        let ui = window();
        ui.global::<crate::Theme>().set_ui_scale(scale);
        ui.window()
            .set_size(slint::LogicalSize::new(1440.0 * scale, 1000.0 * scale));
        let folders = Rc::new(VecModel::from(vec![
            FolderRow {
                name: "Parent".into(),
                has_children: true,
                ..Default::default()
            },
            FolderRow {
                name: "Leaf".into(),
                ..Default::default()
            },
        ]));
        ui.set_folders(ModelRc::from(folders.clone()));
        let weak = ui.as_weak();
        ui.on_folder_requested(move |index| weak.upgrade().unwrap().set_selected_folder(index));
        let toggles = Rc::new(RefCell::new(Vec::new()));
        let observed = toggles.clone();
        let rows = folders.clone();
        ui.on_folder_toggle_requested(move |index| {
            observed.borrow_mut().push(index);
            assert_eq!(index, 0, "Only the parent has child folders");
            let mut parent = rows.row_data(0).unwrap();
            parent.expanded = !parent.expanded;
            if parent.expanded {
                rows.insert(
                    1,
                    FolderRow {
                        name: "Child".into(),
                        depth: 1,
                        ..Default::default()
                    },
                );
            } else {
                rows.remove(1);
            }
            rows.set_row_data(0, parent);
        });
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1000));

        click_label(&ui, "Parent");
        assert_eq!(ui.get_selected_folder(), 0);
        assert!(toggles.borrow().is_empty(), "A single click only selects");
        assert_eq!(folders.row_count(), 2);

        for expanded in [true, false] {
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1000));
            click_label(&ui, "Parent");
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
            click_label(&ui, "Parent");
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(10));
            assert_eq!(folders.row_data(0).unwrap().expanded, expanded);
            assert_eq!(folders.row_count(), if expanded { 3 } else { 2 });
            assert_eq!(ui.get_selected_folder(), 0);
        }
        assert_eq!(&*toggles.borrow(), &[0, 0]);

        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1000));
        click_label(&ui, "Leaf");
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        click_label(&ui, "Leaf");
        assert_eq!(ui.get_selected_folder(), 1);
        assert_eq!(&*toggles.borrow(), &[0, 0], "Leaves only select");

        // The disclosure control handles each click itself. The name's
        // double-click handler must not also receive those same clicks.
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1000));
        let disclosure = |label: &'static str| {
            ElementHandle::find_by_accessible_label(&ui, "Folder: Parent")
                .next()
                .unwrap()
                .query_descendants()
                .match_predicate(move |item| item.accessible_label().as_deref() == Some(label))
                .find_first()
                .unwrap()
        };
        let plus = disclosure("+");
        plus.mock_single_click(slint::platform::PointerEventButton::Left);
        assert!(folders.row_data(0).unwrap().expanded);
        let minus = disclosure("−");
        minus.mock_single_click(slint::platform::PointerEventButton::Left);
        assert!(!folders.row_data(0).unwrap().expanded);
        assert_eq!(&*toggles.borrow(), &[0, 0, 0, 0]);
        assert_eq!(ui.get_query(), "background query");
        ui.hide().unwrap();
    }
}

#[test]
fn review_modals_set_initial_focus_and_keep_tab_input_inside() {
    i_slint_backend_testing::init_no_event_loop();
    for kind in 0..5 {
        for reverse in [false, true] {
            let ui = window();
            match kind {
                0 => ui.set_settings_visible(true),
                1 => ui.set_batch_visible(true),
                2 => ui.set_about_visible(true),
                3 => ui.set_peaks_visible(true),
                _ => ui.set_debug_visible(true),
            }
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
            if kind == 1 {
                send_keys(&ui, &[" ".into()]);
                assert!(
                    ui.get_batch_tags_enabled(),
                    "batch first field receives initial focus"
                );
            }
            if kind == 2 || kind == 3 || kind == 4 {
                send_keys(&ui, &[Key::Return.into()]);
                assert!(
                    !ui.get_about_visible() && !ui.get_peaks_visible() && !ui.get_debug_visible(),
                    "modal Close receives initial focus"
                );
                match kind {
                    2 => ui.set_about_visible(true),
                    3 => ui.set_peaks_visible(true),
                    _ => ui.set_debug_visible(true),
                }
                i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
            }
            for _ in 0..40 {
                let keys = if reverse {
                    vec![Key::Shift.into(), Key::Tab.into()]
                } else {
                    vec![Key::Tab.into()]
                };
                send_keys(&ui, &keys);
                send_keys(&ui, &["z".into()]);
                assert_eq!(
                    ui.get_query(),
                    "background query",
                    "kind {kind}, reverse {reverse}"
                );
            }
            send_keys(&ui, &[Key::Escape.into()]);
            assert!(
                !ui.get_settings_visible()
                    && !ui.get_batch_visible()
                    && !ui.get_about_visible()
                    && !ui.get_peaks_visible()
                    && !ui.get_debug_visible()
            );
            ui.hide().unwrap();
        }
    }
}

#[test]
fn modal_tab_boundaries_return_to_operable_controls() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = window();
    let applied = Rc::new(RefCell::new(0));
    let observed = applied.clone();
    ui.on_output_apply_requested(move || *observed.borrow_mut() += 1);
    ui.set_settings_visible(true);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    send_keys(&ui, &[Key::Shift.into(), Key::Tab.into()]);
    send_keys(&ui, &[Key::Return.into()]);
    assert_eq!(
        *applied.borrow(),
        1,
        "reverse Tab from Driver wraps to Apply"
    );
    ui.set_settings_visible(false);
    ui.set_batch_visible(true);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    send_keys(&ui, &[Key::Shift.into(), Key::Tab.into()]);
    send_keys(&ui, &[Key::Return.into()]);
    assert!(
        !ui.get_batch_visible(),
        "reverse Tab from Tags wraps to Cancel when Apply is disabled"
    );
    ui.set_peaks_visible(true);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    let before = ui.get_peak_variance();
    send_keys(&ui, &[Key::Shift.into(), Key::Tab.into()]);
    send_keys(&ui, &[Key::RightArrow.into()]);
    assert!(
        ui.get_peak_variance() > before,
        "reverse Tab from Close wraps to the variance slider"
    );
    ui.set_peaks_visible(false);
    let debug = Rc::new(RefCell::new(0));
    let observed = debug.clone();
    ui.on_debug_mode_requested(move |_| *observed.borrow_mut() += 1);
    ui.set_debug_visible(true);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
    send_keys(&ui, &[Key::Tab.into()]);
    send_keys(&ui, &[Key::Return.into()]);
    assert_eq!(
        *debug.borrow(),
        1,
        "forward Tab past disabled GitHub action wraps to Debug capture"
    );
    ui.set_debug_visible(false);
    for reverse in [false, true] {
        ui.set_about_visible(true);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
        for _ in 0..41 {
            send_keys(
                &ui,
                &if reverse {
                    vec![Key::Shift.into(), Key::Tab.into()]
                } else {
                    vec![Key::Tab.into()]
                },
            );
        }
        send_keys(&ui, &[Key::Return.into()]);
        assert!(
            !ui.get_about_visible(),
            "About's single button remains focused after repeated Tab"
        );
    }
    ui.hide().unwrap();
}
