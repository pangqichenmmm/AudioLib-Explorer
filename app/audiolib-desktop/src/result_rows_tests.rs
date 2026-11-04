// app/audiolib-desktop/src/result_rows_tests.rs
//! Native pointer gestures must survive Metadata refreshes without crossing row identities.
use super::*;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::platform::{PointerEventButton, WindowEvent};
use std::{cell::RefCell, rc::Rc};

fn row(id: &str, name: &str) -> AudioRow {
    AudioRow {
        id: id.into(),
        name: name.into(),
        ..Default::default()
    }
}

fn row_position(ui: &AppWindow, name: &str) -> slint::LogicalPosition {
    let item = ElementHandle::find_by_accessible_label(ui, name)
        .find(|item| item.accessible_role() == Some(AccessibleRole::ListItem))
        .unwrap_or_else(|| panic!("Missing row: {name}"));
    let origin = item.absolute_position();
    slint::LogicalPosition::new(origin.x + 8.0, origin.y + item.size().height / 2.0)
}

fn click(ui: &AppWindow, position: slint::LogicalPosition) {
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
}

#[test]
fn metadata_refresh_and_append_preserve_native_double_click_for_existing_ids() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    let first = row("42", "Refresh target.wav");
    let other = row("43", "Other.wav");
    publish_audio_rows(&ui, vec![first.clone(), other.clone()]);
    let auditions = Rc::new(RefCell::new(Vec::new()));
    let observed = auditions.clone();
    ui.on_audition_requested(move |index| observed.borrow_mut().push(index));
    let weak = ui.as_weak();
    ui.on_row_clicked(move |index, _, _, play| {
        assert!(!play, "Default pointer clicks only select");
        weak.upgrade().unwrap().set_selected_row(index);
    });
    ui.show().unwrap();
    let position = row_position(&ui, "Refresh target.wav");
    click(&ui, position);
    assert!(auditions.borrow().is_empty());
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(40));
    let mut refreshed = first;
    refreshed.tags = "new annotation".into();
    publish_audio_rows(&ui, vec![refreshed, other, row("44", "Appended.wav")]);
    assert_eq!(ui.get_rows().row_data(0).unwrap().tags, "new annotation");
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(60));
    click(&ui, position);
    assert_eq!(
        &*auditions.borrow(),
        &[0],
        "Refreshing Metadata and appending results must retain the double-click"
    );
    ui.hide().unwrap();
}

#[test]
fn refreshed_row_identity_order_count_and_missing_state_prevent_wrong_audition() {
    i_slint_backend_testing::init_no_event_loop();
    for change in ["different id", "reordered", "removed row", "missing"] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
        let first = row("42", "Original.wav");
        let other = row("43", "Other.wav");
        publish_audio_rows(&ui, vec![first.clone(), other.clone()]);
        let auditions = Rc::new(RefCell::new(Vec::new()));
        let observed = auditions.clone();
        ui.on_audition_requested(move |index| observed.borrow_mut().push(index));
        ui.show().unwrap();
        let position = row_position(&ui, "Original.wav");
        click(&ui, position);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(40));
        let replacement = match change {
            "different id" => vec![row("44", "Replacement.wav"), other],
            "reordered" => vec![other, first],
            "removed row" => vec![first],
            "missing" => vec![
                AudioRow {
                    missing: true,
                    ..first
                },
                other,
            ],
            _ => unreachable!(),
        };
        publish_audio_rows(&ui, replacement);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(60));
        click(&ui, position);
        assert!(
            auditions.borrow().is_empty(),
            "A refresh with {change} must not audition an obsolete click target"
        );
        ui.hide().unwrap();
    }
}
