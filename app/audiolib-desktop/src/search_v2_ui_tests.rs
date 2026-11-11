// app/audiolib-desktop/src/search_v2_ui_tests.rs
//! Migration/import modal and refresh-notice interaction tests on Slint's test backend.
use crate::{AppWindow, theme::apply_theme, ui_tests::click_label};
use slint::ComponentHandle;
use std::{cell::RefCell, rc::Rc};

#[test]
fn refresh_notice_text_and_button_share_the_action_and_busy_gate() {
    i_slint_backend_testing::init_no_event_loop();
    for theme in ["default", "studio"] {
        for chinese in [false, true] {
            let ui = AppWindow::new().unwrap();
            apply_theme(&ui, theme);
            ui.window()
                .set_size(slint::LogicalSize::new(1400.0, 1000.0));
            ui.set_chinese(chinese);
            ui.set_remote_mode(true);
            ui.set_remote_notice_kind(1);
            ui.set_remote_notice_refresh(true);
            let text = if chinese {
                "服务器音效库已更新，点击刷新"
            } else {
                "Server library updated, click to refresh"
            };
            ui.set_remote_notice(text.into());
            let clicks = Rc::new(RefCell::new(0));
            let seen = clicks.clone();
            ui.on_remote_notice_action(move || *seen.borrow_mut() += 1);
            ui.show().unwrap();
            click_label(&ui, text);
            click_label(&ui, if chinese { "刷新" } else { "Refresh" });
            assert_eq!(*clicks.borrow(), 2);
            ui.set_ds_syncing(true);
            click_label(&ui, text);
            assert_eq!(*clicks.borrow(), 2);
            ui.set_ds_syncing(false);
            ui.set_vi_state(1);
            assert!(ui.get_background_blocked());
            click_label(&ui, text);
            assert_eq!(*clicks.borrow(), 2);
            ui.hide().unwrap();
        }
    }
}

#[test]
fn migration_cancel_and_rollback_require_confirmation_and_manage_permission() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_chinese(false);
    ui.window()
        .set_size(slint::LogicalSize::new(1400.0, 1000.0));
    ui.set_migration_visible(true);
    ui.set_migration_can_manage(true);
    ui.set_migration_state("running".into());
    let actions = Rc::new(RefCell::new(Vec::<String>::new()));
    let seen = actions.clone();
    ui.on_migration_action(move |action| seen.borrow_mut().push(action.to_string()));
    ui.show().unwrap();
    assert!(ui.get_background_blocked());
    click_label(&ui, "Cancel migration");
    assert!(actions.borrow().is_empty());
    click_label(&ui, "Back");
    assert!(actions.borrow().is_empty());
    click_label(&ui, "Cancel migration");
    click_label(&ui, "Confirm");
    assert_eq!(&*actions.borrow(), &["cancel"]);
    ui.set_migration_state("done".into());
    click_label(&ui, "Roll back");
    click_label(&ui, "Confirm");
    assert_eq!(&*actions.borrow(), &["cancel", "rollback"]);
    ui.set_migration_can_manage(false);
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "Roll back")
            .next()
            .is_none()
    );
    click_label(&ui, "Refresh progress");
    assert_eq!(actions.borrow().last().map(String::as_str), Some(""));
    ui.hide().unwrap();
}

#[test]
fn vendor_preview_commit_and_rollback_are_distinct_busy_guarded_actions() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_chinese(false);
    ui.set_vi_available(true);
    ui.set_vi_state(3);
    ui.set_vi_has_preview(true);
    ui.set_vi_can_commit(true);
    let actions = Rc::new(RefCell::new(Vec::new()));
    let seen = actions.clone();
    ui.on_vi_commit(move || seen.borrow_mut().push("commit"));
    let seen = actions.clone();
    ui.on_vi_rollback(move || seen.borrow_mut().push("rollback"));
    ui.show().unwrap();
    click_label(&ui, "Commit");
    ui.set_vi_mapping_dirty(true);
    click_label(&ui, "Commit");
    assert_eq!(&*actions.borrow(), &["commit"]);
    ui.set_vi_state(2);
    assert!(ui.get_background_blocked());
    assert!(
        i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "Commit")
            .next()
            .is_none()
    );
    ui.set_vi_state(4);
    ui.set_vi_can_rollback(true);
    click_label(&ui, "Roll back");
    assert_eq!(&*actions.borrow(), &["commit"]);
    click_label(&ui, "Back");
    click_label(&ui, "Roll back");
    click_label(&ui, "Confirm rollback");
    assert_eq!(&*actions.borrow(), &["commit", "rollback"]);
    ui.hide().unwrap();
}

#[test]
fn vendor_progress_cancel_and_history_are_separate_actions() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_chinese(false);
    ui.window()
        .set_size(slint::LogicalSize::new(1400.0, 1000.0));
    ui.set_vi_available(true);
    ui.set_vi_state(2);
    ui.set_vi_cancellable(true);
    let calls = Rc::new(RefCell::new(Vec::<String>::new()));
    let seen = calls.clone();
    ui.on_vi_cancel(move || seen.borrow_mut().push("cancel".into()));
    let seen = calls.clone();
    ui.on_vi_load_history(move || seen.borrow_mut().push("history".into()));
    let seen = calls.clone();
    ui.on_vi_select_history(move |index| seen.borrow_mut().push(format!("batch {index}")));
    ui.set_vi_history(slint::ModelRc::new(slint::VecModel::from(vec![
        crate::VendorHistoryRow {
            id: "old-batch".into(),
            text: "Old committed batch".into(),
        },
    ])));
    ui.show().unwrap();
    click_label(&ui, "Cancel import");
    click_label(&ui, "Import history");
    assert_eq!(&*calls.borrow(), &["cancel"]);
    ui.set_vi_state(1);
    click_label(&ui, "Import history");
    click_label(&ui, "Old committed batch");
    assert_eq!(&*calls.borrow(), &["cancel", "history", "batch 0"]);
    ui.hide().unwrap();
}

#[test]
fn vendor_full_candidate_picker_keeps_selection_explicit_and_busy_guarded() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_chinese(false);
    ui.window()
        .set_size(slint::LogicalSize::new(1400.0, 1000.0));
    ui.set_vi_state(3);
    ui.set_vi_editable(true);
    ui.set_vi_candidates_visible(true);
    ui.set_vi_candidates(slint::ModelRc::new(slint::VecModel::from(vec![
        "Choose a candidate".into(),
        "41 · /complete/path/door.wav".into(),
    ])));
    ui.set_vi_candidates_range("33–64 / 75".into());
    ui.set_vi_candidates_next(true);
    let calls = Rc::new(RefCell::new(Vec::<String>::new()));
    let seen = calls.clone();
    ui.on_vi_candidates_page(move |direction| seen.borrow_mut().push(format!("page {direction}")));
    let seen = calls.clone();
    ui.on_vi_candidate_confirmed(move || seen.borrow_mut().push("confirm".into()));
    ui.show().unwrap();
    click_label(&ui, "Use selected candidate");
    assert!(calls.borrow().is_empty());
    click_label(&ui, "Next candidates");
    assert_eq!(&*calls.borrow(), &["page 1"]);
    ui.set_vi_candidate_index(1);
    ui.set_vi_state(2);
    click_label(&ui, "Use selected candidate");
    assert_eq!(&*calls.borrow(), &["page 1"]);
    ui.set_vi_state(3);
    click_label(&ui, "Use selected candidate");
    assert_eq!(&*calls.borrow(), &["page 1", "confirm"]);
    ui.hide().unwrap();
}

#[test]
fn declined_library_stays_blocked_after_closing_library_manager() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_chinese(false);
    ui.set_lu_state(6);
    ui.set_lu_accept_label("Upgrade".into());
    ui.set_lu_decline_label("Choose another library".into());
    ui.show().unwrap();
    ui.invoke_open_library(0);
    ui.set_library_visible(false);
    assert!(ui.get_background_blocked());
    let chosen = Rc::new(RefCell::new(false));
    let seen = chosen.clone();
    ui.on_lu_decline(move || *seen.borrow_mut() = true);
    click_label(&ui, "Choose another library");
    assert!(*chosen.borrow());
    assert_eq!(ui.get_lu_state(), 6);
    let navigated = Rc::new(RefCell::new(false));
    let seen = navigated.clone();
    ui.on_all_library_requested(move || *seen.borrow_mut() = true);
    i_slint_backend_testing::ElementHandle::find_by_accessible_label(&ui, "All sounds")
        .next()
        .unwrap()
        .invoke_accessible_default_action();
    assert!(!*navigated.borrow());
    ui.set_lu_state(0);
    click_label(&ui, "All sounds");
    assert!(*navigated.borrow());
    ui.hide().unwrap();
}
