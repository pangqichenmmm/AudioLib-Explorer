// app/audiolib-desktop/src/waveform_selection_ui_tests.rs
//! Waveform range gestures run against isolated Slint windows without audio or libraries.
use crate::{AppWindow, Theme};
use i_slint_backend_testing::ElementHandle;
use slint::{
    ComponentHandle, LogicalPosition,
    platform::{Key, PointerEventButton, WindowEvent},
};
use std::{cell::RefCell, rc::Rc, time::Duration};

fn window(modern: bool) -> AppWindow {
    let ui = AppWindow::new().unwrap();
    ui.global::<Theme>().set_modern_style(modern);
    ui.global::<Theme>().set_modern_player(modern);
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_audio_ready(true);
    ui.set_waveform_ready(true);
    ui.set_playback_state(2);
    ui.set_play_duration(30.0);
    ui.set_view_start(0.0);
    ui.set_view_end(20.0);
    ui.set_audition_preferred_height(114.0);
    ui.set_wave_selection_identity("library-a:file-42:version-1".into());
    ui.show().unwrap();
    settle();
    ui
}

fn settle() {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

fn point(ui: &AppWindow, fraction: f32) -> LogicalPosition {
    let wave = ElementHandle::find_by_accessible_label(ui, "Waveform timeline")
        .next()
        .or_else(|| ElementHandle::find_by_accessible_label(ui, "波形时间轴").next())
        .expect("Waveform timeline");
    let origin = wave.absolute_position();
    let size = wave.size();
    LogicalPosition::new(
        origin.x + size.width * fraction,
        origin.y + size.height * 0.45,
    )
}

fn moved(ui: &AppWindow, position: LogicalPosition) {
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    settle();
}

fn pressed(ui: &AppWindow, fraction: f32) {
    let position = point(ui, fraction);
    moved(ui, position);
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    settle();
}

fn released(ui: &AppWindow, fraction: f32) {
    let position = point(ui, fraction);
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
    settle();
}

fn select(ui: &AppWindow, start: f32, end: f32) {
    pressed(ui, start);
    moved(ui, point(ui, end));
    released(ui, end);
}

fn accept_ranges(ui: &AppWindow) -> Rc<RefCell<Vec<(f32, f32)>>> {
    let ranges = Rc::new(RefCell::new(Vec::new()));
    let observed = ranges.clone();
    let weak = ui.as_weak();
    ui.on_wave_selection_committed(move |start, end| {
        observed.borrow_mut().push((start, end));
        if let Some(ui) = weak.upgrade() {
            ui.set_wave_selection_start(start);
            ui.set_wave_selection_end(end);
            ui.set_wave_selection_active(true);
        }
    });
    ranges
}

fn near(actual: (f32, f32), expected: (f32, f32)) {
    assert!((actual.0 - expected.0).abs() < 0.025, "{actual:?}");
    assert!((actual.1 - expected.1).abs() < 0.025, "{actual:?}");
}

fn pointer(ui: &AppWindow, expected: i32) {
    assert_eq!(ui.get_wave_pointer_kind(), expected);
    if (1..=10).contains(&expected) {
        let image = ui.get_wave_pointer_image();
        assert_eq!((image.size().width, image.size().height), (20, 20));
        let scale = ui.get_wave_pointer_scale();
        assert_eq!(scale, 1.0, "GUI scaling must not enlarge a pointer");
        let hotspot_x = ui.get_wave_pointer_hotspot_x();
        let hotspot_y = ui.get_wave_pointer_hotspot_y();
        assert!(hotspot_x.is_finite() && hotspot_x >= 0.0 && hotspot_x < image.size().width as f32);
        assert!(
            hotspot_y.is_finite() && hotspot_y >= 0.0 && hotspot_y < image.size().height as f32
        );
        if expected <= 8 {
            assert_eq!((hotspot_x, hotspot_y), (10.0, 10.0));
        }
    }
}

fn center(element: &ElementHandle) -> LogicalPosition {
    let origin = element.absolute_position();
    let size = element.size();
    LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0)
}

fn pointer_press(ui: &AppWindow, position: LogicalPosition) {
    moved(ui, position);
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    settle();
}

fn pointer_release(ui: &AppWindow, position: LogicalPosition) {
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
    settle();
}

#[test]
fn both_players_create_reverse_ranges_resize_and_drag_exactly_once() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window(modern);
        let ranges = accept_ranges(&ui);
        ui.on_seek_play_requested(|_| panic!("Drawing a range must not seek"));
        let drags = Rc::new(RefCell::new(0));
        let observed = drags.clone();
        ui.on_wave_selection_drag_requested(move || *observed.borrow_mut() += 1);
        let ended = Rc::new(RefCell::new(0));
        let observed = ended.clone();
        ui.on_wave_selection_drag_ended(move || *observed.borrow_mut() += 1);

        moved(&ui, point(&ui, 0.7));
        pointer(&ui, 1);
        select(&ui, 0.7, 0.2);
        assert_eq!(ranges.borrow().len(), 1);
        near(ranges.borrow()[0], (4.0, 14.0));
        assert!(!ui.get_wave_selection_gesture());

        moved(&ui, point(&ui, 0.2));
        pointer(&ui, 4);
        select(&ui, 0.2, 0.3);
        near(*ranges.borrow().last().unwrap(), (6.0, 14.0));
        assert_eq!(*drags.borrow(), 0, "The boundary must win over drag-out");

        // Fine boundary adjustments do not inherit the export drag threshold.
        let left = point(&ui, 0.3);
        pressed(&ui, 0.3);
        moved(&ui, LogicalPosition::new(left.x + 3.0, left.y));
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position: LogicalPosition::new(left.x + 3.0, left.y),
            button: PointerEventButton::Left,
        });
        settle();
        assert_eq!(ranges.borrow().len(), 3);
        assert!(ranges.borrow().last().unwrap().0 > 6.0);

        moved(&ui, point(&ui, 0.5));
        pointer(&ui, 6);
        pressed(&ui, 0.5);
        pointer(&ui, 7);
        let start = point(&ui, 0.5);
        moved(&ui, LogicalPosition::new(start.x + 3.0, start.y));
        assert_eq!(*drags.borrow(), 0);
        for offset in [8.0, 18.0, 28.0] {
            moved(&ui, LogicalPosition::new(start.x + offset, start.y));
        }
        assert_eq!(*drags.borrow(), 1);
        released(&ui, 0.55);
        pointer(&ui, 6);
        assert_eq!(*ended.borrow(), 1);
        assert_eq!(ranges.borrow().len(), 3);
        ui.hide().unwrap();
    }
}

#[test]
fn shift_reselects_inside_existing_range_and_clicks_keep_seek_and_play() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window(modern);
        let ranges = accept_ranges(&ui);
        select(&ui, 0.2, 0.8);
        ui.on_wave_selection_drag_requested(|| unreachable!());
        // Shift creates a new range even when pressed inside the old selection.
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Shift.into(),
        });
        moved(&ui, point(&ui, 0.4));
        pointer(&ui, 1);
        select(&ui, 0.4, 0.6);
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Shift.into(),
        });
        near(*ranges.borrow().last().unwrap(), (8.0, 12.0));

        let seeks = Rc::new(RefCell::new(Vec::new()));
        let observed = seeks.clone();
        ui.on_seek_play_requested(move |time| observed.borrow_mut().push(time));
        for fraction in [0.1, 0.5] {
            pressed(&ui, fraction);
            released(&ui, fraction);
        }
        assert_eq!(seeks.borrow().len(), 2);
        assert!((seeks.borrow()[0] - 2.0).abs() < 0.025);
        assert!((seeks.borrow()[1] - 10.0).abs() < 0.025);
        assert_eq!(ranges.borrow().len(), 2);
        ui.hide().unwrap();
    }
}

#[test]
fn semantic_pointer_images_follow_gestures_and_leave_ownership_outside_the_plot() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        for scale in [1.0, 1.5, 2.0, 3.0] {
            let ui = window(modern);
            ui.global::<Theme>().set_ui_scale(scale);
            ui.window().dispatch_event(WindowEvent::ScaleFactorChanged {
                // Keep physical DPI fixed to isolate the user's GUI zoom from
                // native cursor rasterization, which has separate DPI tests.
                scale_factor: 1.0,
            });
            ui.window()
                .set_size(slint::LogicalSize::new(1440.0 * scale, 900.0 * scale));
            settle();
            let ranges = accept_ranges(&ui);
            let hover = point(&ui, 0.7);
            moved(&ui, hover);
            pointer(&ui, 1);
            assert!(!ui.get_wave_native_cursor());
            let cross_image = ui.get_wave_pointer_image();

            pressed(&ui, 0.7);
            moved(&ui, point(&ui, 0.2));
            pointer(&ui, 2);
            assert_ne!(ui.get_wave_pointer_image(), cross_image);
            released(&ui, 0.2);
            assert_eq!(ranges.borrow().len(), 1);
            near(ranges.borrow()[0], (4.0, 14.0));

            // Thin paint must retain the original generous edge hit target.
            let edge = point(&ui, 0.2);
            moved(&ui, LogicalPosition::new(edge.x + 4.0 * scale, edge.y));
            pointer(&ui, 4);
            moved(&ui, point(&ui, 0.5));
            pointer(&ui, 6);
            let export_image = ui.get_wave_pointer_image();
            pressed(&ui, 0.5);
            pointer(&ui, 7);
            assert_ne!(ui.get_wave_pointer_image(), export_image);
            ui.set_wave_selection_preparing(true);
            ui.set_wave_selection_export_state(1);
            settle();
            pointer(&ui, 7);
            released(&ui, 0.5);
            ui.set_wave_selection_preparing(false);
            ui.set_wave_selection_export_state(2);

            let wave = ElementHandle::find_by_accessible_label(&ui, "Waveform timeline")
                .next()
                .unwrap();
            let outside = LogicalPosition::new(point(&ui, 0.9).x, wave.absolute_position().y - 3.0);
            moved(&ui, outside);
            pointer(&ui, 0);
            ui.window().dispatch_event(WindowEvent::PointerExited);
            settle();
            ui.window().dispatch_event(WindowEvent::PointerReleased {
                position: outside,
                button: PointerEventButton::Left,
            });
            settle();
            ui.hide().unwrap();
        }
    }
}

#[test]
fn drawing_and_resizing_override_previous_preparation_only_during_the_active_gesture() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window(modern);
        let ranges = accept_ranges(&ui);
        select(&ui, 0.2, 0.6);
        ui.set_wave_selection_preparing(true);
        ui.set_wave_selection_export_state(1);
        moved(&ui, point(&ui, 0.8));
        pointer(&ui, 1);

        pressed(&ui, 0.8);
        moved(&ui, point(&ui, 0.9));
        pointer(&ui, 3);
        released(&ui, 0.9);
        assert_eq!(ranges.borrow().len(), 2);
        near(ranges.borrow()[1], (16.0, 18.0));
        pointer(&ui, 5);

        // Preparation never takes away the boundary's resize affordance.
        moved(&ui, point(&ui, 0.8));
        pointer(&ui, 4);
        pressed(&ui, 0.8);
        pointer(&ui, 4);
        moved(&ui, point(&ui, 0.7));
        pointer(&ui, 4);
        released(&ui, 0.7);
        assert_eq!(ranges.borrow().len(), 3);
        near(ranges.borrow()[2], (14.0, 18.0));
        pointer(&ui, 4);
        ui.hide().unwrap();
    }
}

#[test]
fn repeated_ranges_tolerate_synchronous_preview_and_source_publication_callbacks() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window(modern);
        let ranges = Rc::new(RefCell::new(Vec::new()));
        let observed = ranges.clone();
        let weak = ui.as_weak();
        ui.on_wave_selection_committed(move |start, end| {
            let ui = weak.unwrap();
            let revision = observed.borrow().len() + 1;
            // Match the controller's active/start/end publication order, and
            // exercise callbacks that arrive before the pointer handler returns.
            ui.set_wave_selection_active(true);
            ui.set_wave_selection_start(start);
            ui.set_wave_selection_end(end);
            ui.set_wave_selection_preparing(true);
            ui.set_playback_clock(format!("preview:{revision}").into());
            if revision % 4 == 0 {
                ui.set_wave_selection_identity(format!("library-a:file-42:{revision}").into());
            }
            ui.set_wave_selection_preparing(false);
            observed.borrow_mut().push((start, end));
        });
        let weak = ui.as_weak();
        ui.on_wave_selection_cleared(move || {
            let ui = weak.unwrap();
            ui.set_wave_selection_active(false);
            ui.set_wave_selection_preparing(false);
        });
        ui.on_wave_selection_drag_requested(|| panic!("Shift reselect must never drag out"));
        ui.on_seek_play_requested(|_| panic!("A committed range must not seek again"));
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Shift.into(),
        });
        for index in 0..24 {
            let start = 0.15 + (index % 3) as f32 * 0.05;
            let end = 0.65 + (index % 2) as f32 * 0.05;
            select(&ui, start, end);
            assert_eq!(ranges.borrow().len(), index + 1);
            near(ranges.borrow()[index], (start * 20.0, end * 20.0));
            assert!(!ui.get_wave_selection_gesture());
            if index % 4 == 3 {
                ElementHandle::find_by_accessible_label(&ui, "Clear waveform selection")
                    .next()
                    .unwrap()
                    .invoke_accessible_default_action();
                settle();
                assert!(!ui.get_wave_selection_active());
            }
        }
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Shift.into(),
        });
        ui.hide().unwrap();
    }
}

#[test]
fn gesture_freezes_pending_viewport_and_identity_changes_cancel_without_committing() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window(modern);
        let ranges = accept_ranges(&ui);
        ui.set_wave_target_start(8.0);
        ui.set_wave_target_end(18.0);
        pressed(&ui, 0.2);
        assert!(ui.get_wave_selection_gesture());
        ui.set_wave_target_start(12.0);
        ui.set_wave_target_end(22.0);
        ui.set_playback_clock("seek-and-play-revision".into());
        moved(&ui, point(&ui, 0.7));
        released(&ui, 0.7);
        near(ranges.borrow()[0], (10.0, 15.0));

        pressed(&ui, 0.8);
        moved(&ui, point(&ui, 0.9));
        ui.set_wave_selection_identity("library-b:file-42:version-1".into());
        settle();
        assert!(!ui.get_wave_selection_gesture());
        released(&ui, 0.9);
        assert_eq!(
            ranges.borrow().len(),
            1,
            "The old gesture cannot select the new source"
        );
        ui.hide().unwrap();
    }
}

#[test]
fn escape_cancels_gesture_then_preparation_then_selection_and_clear_is_accessible() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window(modern);
        let ranges = accept_ranges(&ui);
        select(&ui, 0.2, 0.6);
        ui.set_help_visible(true);
        settle();
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Escape.into(),
        });
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Escape.into(),
        });
        settle();
        assert!(
            !ui.get_help_visible(),
            "A modal receives Escape before the selection"
        );
        assert!(ui.get_wave_selection_active());
        pressed(&ui, 0.8);
        moved(&ui, point(&ui, 0.9));
        let escape = || {
            ui.window().dispatch_event(WindowEvent::KeyPressed {
                text: Key::Escape.into(),
            });
            ui.window().dispatch_event(WindowEvent::KeyReleased {
                text: Key::Escape.into(),
            });
            settle();
        };
        escape();
        released(&ui, 0.9);
        assert_eq!(ranges.borrow().len(), 1);
        assert!(ui.get_wave_selection_active());

        let cancels = Rc::new(RefCell::new(0));
        let observed = cancels.clone();
        let weak = ui.as_weak();
        ui.on_wave_selection_cancel_requested(move || {
            *observed.borrow_mut() += 1;
            weak.unwrap().set_wave_selection_preparing(false);
        });
        ui.set_wave_selection_preparing(true);
        moved(&ui, point(&ui, 0.4));
        pointer(&ui, 6);
        escape();
        assert_eq!(*cancels.borrow(), 1);
        assert!(ui.get_wave_selection_active());
        let clears = Rc::new(RefCell::new(0));
        let observed = clears.clone();
        let weak = ui.as_weak();
        ui.on_wave_selection_cleared(move || {
            *observed.borrow_mut() += 1;
            weak.unwrap().set_wave_selection_active(false);
        });
        escape();
        assert_eq!(*clears.borrow(), 1);
        assert!(!ui.get_wave_selection_active());
        ui.set_wave_selection_active(true);
        ElementHandle::find_by_accessible_label(&ui, "Clear waveform selection")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        assert_eq!(*clears.borrow(), 2);
        ui.hide().unwrap();
    }
}

#[test]
fn loaded_waveform_selection_survives_transport_reload_and_an_unrelated_missing_inspector() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window(modern);
        let ranges = accept_ranges(&ui);
        pressed(&ui, 0.2);
        ui.set_audio_ready(false);
        ui.set_playback_state(1);
        ui.set_selected_missing(true);
        moved(&ui, point(&ui, 0.6));
        assert!(ui.get_wave_selection_gesture());
        released(&ui, 0.6);
        assert_eq!(ranges.borrow().len(), 1);
        near(ranges.borrow()[0], (4.0, 12.0));
        ui.hide().unwrap();
    }
}

#[test]
fn source_view_selection_and_wheel_work_with_empty_or_shorter_transport() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        for expanded in [false, true] {
            for transport_duration in [0.0, 1.0] {
                let ui = window(modern);
                ui.set_audition_expanded(expanded);
                ui.set_audition_preferred_height(if expanded { 286.0 } else { 114.0 });
                if !expanded {
                    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
                }
                ui.set_play_duration(transport_duration);
                ui.set_audio_ready(transport_duration > 0.0);
                ui.set_playback_state(if transport_duration == 0.0 { 0 } else { 2 });
                ui.set_view_start(0.0);
                ui.set_view_end(6.0);
                settle();
                let zooms = Rc::new(RefCell::new(Vec::new()));
                let observed = zooms.clone();
                ui.on_waveform_wheel_zoom_requested(move |factor, anchor| {
                    observed.borrow_mut().push((factor, anchor));
                });
                let position = point(&ui, 0.5);
                moved(&ui, position);
                ui.window().dispatch_event(WindowEvent::PointerScrolled {
                    position,
                    delta_x: 0.0,
                    delta_y: 120.0,
                });
                settle();
                assert_eq!(zooms.borrow().len(), 1);
                near(zooms.borrow()[0], (2.0, 0.5));
                let ranges = accept_ranges(&ui);
                select(&ui, 0.25, 0.75);
                assert_eq!(ranges.borrow().len(), 1);
                near(ranges.borrow()[0], (1.5, 4.5));
                assert!(ranges.borrow()[0].1 > transport_duration);
                ui.hide().unwrap();
            }
        }
    }
}

#[test]
fn source_gesture_freezes_its_upper_bound_across_transport_and_view_changes() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window(modern);
        ui.set_play_duration(1.0);
        ui.set_view_start(0.0);
        ui.set_view_end(6.0);
        settle();
        let ranges = accept_ranges(&ui);
        pressed(&ui, 0.2);
        ui.set_play_duration(0.0);
        ui.set_view_start(6.0);
        ui.set_view_end(12.0);
        ui.set_wave_target_start(12.0);
        ui.set_wave_target_end(24.0);
        moved(&ui, point(&ui, 0.8));
        ui.set_play_duration(0.5);
        ui.set_view_start(0.0);
        ui.set_view_end(30.0);
        released(&ui, 0.8);
        assert_eq!(ranges.borrow().len(), 1);
        near(ranges.borrow()[0], (1.2, 4.8));
        ui.hide().unwrap();
    }
}

#[test]
fn legacy_seek_freezes_its_transport_duration_limit() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window(modern);
        ui.set_waveform_ready(false);
        ui.set_wave_selection_identity("".into());
        ui.set_play_duration(1.0);
        ui.set_view_start(0.0);
        ui.set_view_end(6.0);
        settle();
        let seeks = Rc::new(RefCell::new(Vec::new()));
        let observed = seeks.clone();
        ui.on_seek_play_requested(move |time| observed.borrow_mut().push(time));
        ui.on_wave_selection_committed(|_, _| {
            panic!("A legacy scrub cannot create a source range")
        });
        moved(&ui, point(&ui, 0.2));
        pointer(&ui, 9);
        pressed(&ui, 0.2);
        ui.set_play_duration(0.0);
        moved(&ui, point(&ui, 0.8));
        released(&ui, 0.8);
        assert_eq!(seeks.borrow().len(), 1);
        assert!((seeks.borrow()[0] - 1.0).abs() < 0.001);
        ui.set_audio_ready(false);
        settle();
        pointer(&ui, 10);
        ui.hide().unwrap();
    }
}

#[test]
fn narrow_selection_has_a_separate_drag_grip_without_covering_plot_or_ruler() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window(modern);
        ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
        ui.set_wave_selection_start(8.0);
        ui.set_wave_selection_end(8.001);
        ui.set_wave_selection_active(true);
        settle();
        let grip = ElementHandle::find_by_accessible_label(&ui, "Drag selection as WAV")
            .next()
            .expect("Narrow ranges retain a separate drag grip");
        let wave = ElementHandle::find_by_accessible_label(&ui, "Waveform timeline")
            .next()
            .unwrap();
        let origin = grip.absolute_position();
        let size = grip.size();
        let plot_origin = wave.absolute_position();
        let plot_size = wave.size();
        assert!(size.width > 0.0 && size.height > 0.0);
        assert!(
            origin.y + size.height <= plot_origin.y || origin.y >= plot_origin.y + plot_size.height,
            "modern={modern}, grip={origin:?}+{size:?}, plot={plot_origin:?}+{plot_size:?}"
        );
        let ruler = ElementHandle::find_by_accessible_label(&ui, "Waveform time ruler")
            .next()
            .unwrap();
        assert!(
            origin.y + size.height <= ruler.absolute_position().y
                || origin.y >= ruler.absolute_position().y + ruler.size().height
        );
        let start = LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0);
        let requests = Rc::new(RefCell::new(0));
        let observed = requests.clone();
        ui.on_wave_selection_drag_requested(move || *observed.borrow_mut() += 1);
        let endings = Rc::new(RefCell::new(0));
        let observed = endings.clone();
        ui.on_wave_selection_drag_ended(move || *observed.borrow_mut() += 1);
        ui.on_wave_selection_committed(|_, _| panic!("The export grip must not resize"));
        ui.on_seek_play_requested(|_| panic!("The export grip must not seek"));
        moved(&ui, start);
        pointer(&ui, 6);
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position: start,
            button: PointerEventButton::Left,
        });
        settle();
        assert!(ui.get_wave_selection_gesture());
        pointer(&ui, 7);
        moved(&ui, LogicalPosition::new(start.x + 3.0, start.y));
        assert_eq!(*requests.borrow(), 0);
        for offset in [8.0, 18.0] {
            moved(&ui, LogicalPosition::new(start.x + offset, start.y));
        }
        assert_eq!(*requests.borrow(), 1);
        assert_eq!(
            *endings.borrow(),
            0,
            "Keep drag intent while the button is held"
        );
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position: LogicalPosition::new(start.x + 18.0, start.y),
            button: PointerEventButton::Left,
        });
        settle();
        assert_eq!(*endings.borrow(), 1);
        assert!(!ui.get_wave_selection_gesture());
        ui.hide().unwrap();
    }
}

#[test]
fn stationary_export_feedback_updates_and_native_return_resets_only_the_gesture() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window(modern);
        let ranges = accept_ranges(&ui);
        select(&ui, 0.2, 0.8);
        let committed = ranges.borrow()[0];
        let changes = Rc::new(RefCell::new(Vec::new()));
        let observed = changes.clone();
        let weak = ui.as_weak();
        ui.on_wave_pointer_feedback_changed(move || {
            let ui = weak.unwrap();
            observed
                .borrow_mut()
                .push((ui.get_wave_pointer_kind(), ui.get_wave_pointer_image()));
        });
        let requests = Rc::new(RefCell::new(0));
        let observed = requests.clone();
        ui.on_wave_selection_drag_requested(move || *observed.borrow_mut() += 1);
        let grip = ElementHandle::find_by_accessible_label(&ui, "Drag selection as WAV")
            .next()
            .unwrap();
        let start = center(&grip);
        pointer_press(&ui, start);
        pointer(&ui, 7);
        moved(&ui, LogicalPosition::new(start.x + 5.0, start.y));
        assert_eq!(*requests.borrow(), 0);
        moved(&ui, LogicalPosition::new(start.x + 7.0, start.y));
        assert_eq!(*requests.borrow(), 1);
        for phase in [2, 4, 5] {
            ui.set_wave_selection_preparing(true);
            ui.set_wave_selection_export_state(1);
            settle();
            pointer(&ui, 8);
            let waiting_image = ui.get_wave_pointer_image();
            let owner = ElementHandle::find_by_accessible_label(&ui, "Waveform selection")
                .next()
                .unwrap();
            let waiting_hint = owner.accessible_description().unwrap();
            let before = changes.borrow().len();
            // No pointer event follows this asynchronous worker result.
            ui.set_wave_selection_preparing(false);
            ui.set_wave_selection_export_state(phase);
            settle();
            pointer(&ui, 7);
            assert_ne!(ui.get_wave_pointer_image(), waiting_image);
            assert_ne!(owner.accessible_description().unwrap(), waiting_hint);
            assert!(changes.borrow().len() > before);
            assert_eq!(changes.borrow().last().unwrap().0, 7);
            assert!(ui.get_wave_selection_active());
        }
        let before = changes.borrow().len();
        ui.set_wave_selection_export_state(3);
        settle();
        pointer(&ui, 11);
        assert!(changes.borrow().len() > before);
        ui.invoke_cancel_wave_selection_gesture();
        // The command invalidates a presenter revision; changed handlers then
        // cancel the overlay mode and publish the resulting gesture state.
        settle();
        assert!(!ui.get_wave_selection_gesture());
        assert_eq!(ui.get_wave_selection_export_state(), 3);
        assert!(ui.get_wave_selection_active());
        near(
            (ui.get_wave_selection_start(), ui.get_wave_selection_end()),
            committed,
        );
        ui.set_wave_selection_export_state(2);
        settle();
        pointer(&ui, 6);
        assert_eq!(*requests.borrow(), 1);
        assert_eq!(ranges.borrow().len(), 1);
        ui.hide().unwrap();
    }
}

#[test]
fn crossing_the_export_grip_cannot_take_over_a_draw_or_boundary_resize() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window(modern);
        let ranges = accept_ranges(&ui);
        select(&ui, 0.2, 0.6);
        ui.set_wave_selection_preparing(true);
        ui.set_wave_selection_export_state(1);
        ui.on_wave_selection_drag_requested(|| panic!("The grip cannot replace an active gesture"));
        let grip = ElementHandle::find_by_accessible_label(&ui, "Drag selection as WAV")
            .next()
            .unwrap();
        let grip_center = center(&grip);
        pressed(&ui, 0.8);
        moved(&ui, grip_center);
        assert!([2, 3].contains(&ui.get_wave_pointer_kind()));
        assert!(ui.get_wave_selection_gesture());
        pointer_release(&ui, grip_center);
        assert_eq!(ranges.borrow().len(), 2);
        ui.set_wave_selection_start(4.0);
        ui.set_wave_selection_end(12.0);
        settle();
        pressed(&ui, 0.2);
        moved(&ui, grip_center);
        pointer(&ui, 4);
        pointer_release(&ui, grip_center);
        assert_eq!(ranges.borrow().len(), 3);
        assert!(!ui.get_wave_selection_gesture());
        ui.hide().unwrap();
    }
}

#[test]
fn feedback_buttons_keep_cancel_and_clear_independent_across_themes_views_and_languages() {
    i_slint_backend_testing::init_no_event_loop();
    for modern_theme in [false, true] {
        for modern_player in [false, true] {
            for chinese in [false, true] {
                let ui = window(modern_theme);
                crate::theme::apply_theme(
                    &ui,
                    if modern_theme {
                        audiolib_core::config::STUDIO_THEME
                    } else {
                        audiolib_core::config::DEFAULT_THEME
                    },
                );
                ui.global::<Theme>().set_modern_player(modern_player);
                ui.set_chinese(chinese);
                ui.window().dispatch_event(WindowEvent::ScaleFactorChanged {
                    scale_factor: if modern_player { 1.5 } else { 2.0 },
                });
                let ranges = accept_ranges(&ui);
                select(&ui, 0.2, 0.7);
                let selected = ranges.borrow()[0];
                ui.set_wave_selection_preparing(true);
                ui.set_wave_selection_export_state(1);
                let cancels = Rc::new(RefCell::new(0));
                let observed = cancels.clone();
                let weak = ui.as_weak();
                ui.on_wave_selection_cancel_requested(move || {
                    *observed.borrow_mut() += 1;
                    let ui = weak.unwrap();
                    ui.set_wave_selection_preparing(false);
                    ui.set_wave_selection_export_state(4);
                });
                let clears = Rc::new(RefCell::new(0));
                let observed = clears.clone();
                let weak = ui.as_weak();
                ui.on_wave_selection_cleared(move || {
                    *observed.borrow_mut() += 1;
                    weak.unwrap().set_wave_selection_active(false);
                });
                settle();
                let cancel = ElementHandle::find_by_accessible_label(
                    &ui,
                    if chinese {
                        "取消切片准备"
                    } else {
                        "Cancel slice preparation"
                    },
                )
                .next()
                .unwrap();
                let cancel_center = center(&cancel);
                moved(&ui, cancel_center);
                i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(900));
                pointer(&ui, 0);
                pointer_press(&ui, cancel_center);
                pointer_release(&ui, cancel_center);
                assert_eq!(*cancels.borrow(), 1);
                assert_eq!(*clears.borrow(), 0);
                assert!(ui.get_wave_selection_active());
                near(
                    (ui.get_wave_selection_start(), ui.get_wave_selection_end()),
                    selected,
                );
                let clear = ElementHandle::find_by_accessible_label(
                    &ui,
                    if chinese {
                        "清除波形选区"
                    } else {
                        "Clear waveform selection"
                    },
                )
                .next()
                .unwrap();
                let clear_center = center(&clear);
                moved(&ui, clear_center);
                i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(900));
                pointer(&ui, 0);
                pointer_press(&ui, clear_center);
                pointer_release(&ui, clear_center);
                assert_eq!(*clears.borrow(), 1);
                assert_eq!(*cancels.borrow(), 1);
                assert!(!ui.get_wave_selection_active());
                moved(&ui, point(&ui, 0.9));
                pointer(&ui, 1);
                assert_eq!(ranges.borrow().len(), 1);
                ui.hide().unwrap();
            }
        }
    }
}

#[test]
fn predicted_playhead_does_not_advance_past_selection_end() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window(modern);
        ui.set_wave_selection_start(4.0);
        ui.set_wave_selection_end(8.0);
        ui.set_wave_selection_active(true);
        ui.set_play_position(7.9);
        ui.set_confirmed_playback_rate(4.0);
        ui.set_playback_smooth(true);
        ui.set_playing(true);
        ui.set_playback_clock("selected-range-play".into());
        let played_fraction = || {
            // Changed handlers establish the anchor at an event-loop boundary;
            // reading geometry alone does not flush the pending clock revision.
            i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
            let wave = ElementHandle::find_by_accessible_label(&ui, "Waveform timeline")
                .next()
                .unwrap();
            let played = ElementHandle::find_by_accessible_label(&ui, "Played waveform region")
                .next()
                .unwrap();
            played.size().width / wave.size().width
        };
        let initial = played_fraction();
        assert!((initial - 0.395).abs() < 0.0001, "{modern}: {initial}");
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
        let advancing = played_fraction();
        assert!(advancing > initial, "{modern}: prediction did not advance");
        assert!((advancing - 0.3982).abs() < 0.0001, "{modern}: {advancing}");
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
        let clipped = played_fraction();
        assert!((clipped - 0.4).abs() < 0.0001, "{modern}: {clipped}");
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
        assert!((played_fraction() - 0.4).abs() < 0.0001);
        // The same prediction exceeds the selected end once its clamp is removed.
        ui.set_wave_selection_active(false);
        let unrestricted = played_fraction();
        assert!(
            (unrestricted - 0.415).abs() < 0.0001,
            "{modern}: {unrestricted}"
        );
        ui.hide().unwrap();
    }
}

#[test]
fn export_folder_choice_is_reachable_in_both_languages() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = window(false);
        ui.set_chinese(chinese);
        ui.set_slice_export_directory("C:/Audio Slices".into());
        ui.invoke_open_global_settings(0);
        settle();
        ui.invoke_scroll_main_to_end();
        settle();
        let choices = Rc::new(RefCell::new(0));
        let observed = choices.clone();
        ui.on_slice_export_directory_requested(move || *observed.borrow_mut() += 1);
        ElementHandle::find_by_accessible_label(
            &ui,
            if chinese {
                "选择保存目录…"
            } else {
                "Choose export folder…"
            },
        )
        .next()
        .expect("Export folder chooser")
        .invoke_accessible_default_action();
        assert_eq!(*choices.borrow(), 1);
        ui.hide().unwrap();
    }
}

/// Export focused pointer crops at GUI zoom 1/1.5/2 with physical DPI fixed at 1.
/// No audio source, library or device is opened; each theme gets one comparison
/// matrix with rows for GUI zoom and columns for hover/draw/resize/open/held/wait.
#[test]
#[ignore = "explicit PNG export for visual acceptance of the Waveform pointer"]
fn export_waveform_pointer_visual_previews() {
    use crate::waveform_view::{WaveView, render_waveform, render_waveform_mask};
    use audiolib_audio::{WaveformData, WaveformPeak};
    use audiolib_core::config::{DEFAULT_THEME, STUDIO_THEME};
    use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
    use std::path::Path;

    let directory = std::env::var_os("AUDIOLIB_WAVE_SELECTION_SNAPSHOT_DIR")
        .expect("Set AUDIOLIB_WAVE_SELECTION_SNAPSHOT_DIR to an owned absolute output directory");
    let directory = Path::new(&directory);
    assert!(
        directory.is_absolute(),
        "The PNG output directory must be absolute"
    );
    std::fs::create_dir_all(directory).unwrap();
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .expect("platform already initialized");

    let waveform = WaveformData {
        sample_rate: 48_000,
        channels: 2,
        frames: 960_000,
        samples_per_pixel: 938,
        peaks: (0..2)
            .map(|channel| {
                (0..1024)
                    .map(|index| {
                        let phase = index as f32 / 1024.0;
                        let amplitude = 0.15 + (phase * 34.0 + channel as f32).sin().abs() * 0.65;
                        WaveformPeak {
                            min: -amplitude,
                            max: amplitude,
                            rms: amplitude * 0.5,
                            dominant_hz: 80.0 * 2.0_f32.powf(phase * 6.0),
                            tonality: 0.8,
                            ..Default::default()
                        }
                    })
                    .collect()
            })
            .collect(),
    };
    let pixels = render_waveform(
        &waveform,
        1440,
        160,
        WaveView {
            spectral: false,
            ..WaveView::default()
        },
    );
    let waveform_image = slint::Image::from_rgba8(
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&pixels, 1440, 160),
    );
    let mask = render_waveform_mask(&waveform, 1440, 160, WaveView::default());
    let waveform_mask = slint::Image::from_rgba8(
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&mask, 1440, 160),
    );
    const FOCUS: u32 = 64;
    const MAGNIFICATION: u32 = 3;
    const GAP: u32 = 8;
    const CELL: u32 = FOCUS * MAGNIFICATION;
    let states = [
        "hover",
        "draw",
        "resize",
        "open-hand",
        "held-hand",
        "waiting-hand",
    ];
    let gui_scales = [1.0, 1.5, 2.0];
    let mut reports = Vec::new();
    for (modern, theme) in [(false, DEFAULT_THEME), (true, STUDIO_THEME)] {
        let mut matrix = image::RgbaImage::from_pixel(
            CELL * states.len() as u32 + GAP * (states.len() as u32 + 1),
            CELL * gui_scales.len() as u32 + GAP * (gui_scales.len() as u32 + 1),
            image::Rgba([106, 112, 116, 255]),
        );
        for (row, gui_scale) in gui_scales.into_iter().enumerate() {
            let ui = window(modern);
            crate::theme::apply_theme(&ui, theme);
            ui.global::<Theme>().set_motion(0.0);
            ui.global::<Theme>().set_ui_scale(gui_scale);
            ui.window()
                .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 1.0 });
            ui.window().set_size(slint::LogicalSize::new(
                1440.0 * gui_scale,
                900.0 * gui_scale,
            ));
            ui.set_wave_native_cursor(false);
            ui.set_has_selection(true);
            ui.set_selected_id("visual-fixture".into());
            ui.set_playback_name("Synthetic Waveform pointer preview.wav".into());
            ui.set_waveform_info("2 ch · 48 kHz · 0:20.000".into());
            ui.set_play_duration(20.0);
            ui.set_waveform_image(waveform_image.clone());
            ui.set_waveform_mask(waveform_mask.clone());
            ui.set_peak_mode(0);
            ui.set_audition_expanded(modern);
            ui.set_audition_preferred_height((if modern { 286.0 } else { 114.0 }) * gui_scale);
            let ranges = accept_ranges(&ui);
            settle();
            let mut capture = |column: usize, position: LogicalPosition, kind: i32| {
                pointer(&ui, kind);
                let pixels = ui.window().take_snapshot().unwrap();
                let full = image::RgbaImage::from_raw(
                    pixels.width(),
                    pixels.height(),
                    pixels.as_bytes().to_vec(),
                )
                .unwrap();
                let x = (position.x.floor() as i64 - i64::from(FOCUS / 2))
                    .clamp(0, i64::from(full.width().saturating_sub(FOCUS)))
                    as u32;
                let y = (position.y.floor() as i64 - i64::from(FOCUS / 2))
                    .clamp(0, i64::from(full.height().saturating_sub(FOCUS)))
                    as u32;
                let focus = image::imageops::crop_imm(&full, x, y, FOCUS, FOCUS).to_image();
                // Nearest-neighbor enlargement reveals the real 20px raster;
                // it does not smooth away antialiasing or size differences.
                let enlarged = image::imageops::resize(
                    &focus,
                    CELL,
                    CELL,
                    image::imageops::FilterType::Nearest,
                );
                image::imageops::replace(
                    &mut matrix,
                    &enlarged,
                    i64::from(GAP + column as u32 * (CELL + GAP)),
                    i64::from(GAP + row as u32 * (CELL + GAP)),
                );
                reports.push(serde_json::json!({"theme": theme, "gui_scale": gui_scale, "physical_dpi_scale": 1.0, "state": states[column], "kind": kind, "logical_cursor_size": [20,20], "cursor_scale": ui.get_wave_pointer_scale(), "row": row, "column": column}));
            };
            let hover = point(&ui, 0.7);
            moved(&ui, hover);
            capture(0, hover, 1);
            pressed(&ui, 0.2);
            let drawing = point(&ui, 0.65);
            moved(&ui, drawing);
            capture(1, drawing, 3);
            released(&ui, 0.65);
            assert_eq!(ranges.borrow().len(), 1);
            ui.set_wave_selection_export_state(2);
            let edge = point(&ui, 0.2);
            moved(&ui, edge);
            capture(2, edge, 4);
            let inside = point(&ui, 0.5);
            moved(&ui, inside);
            capture(3, inside, 6);
            pressed(&ui, 0.5);
            capture(4, inside, 7);
            let waiting = LogicalPosition::new(inside.x + 8.0, inside.y);
            moved(&ui, waiting);
            ui.set_wave_selection_preparing(true);
            ui.set_wave_selection_export_state(1);
            settle();
            capture(5, waiting, 8);
            ui.invoke_cancel_wave_selection_gesture();
            ui.set_wave_selection_preparing(false);
            ui.set_wave_selection_export_state(2);
            pointer_release(&ui, waiting);
            ui.hide().unwrap();
        }
        let path = directory.join(format!("wave-pointer-{theme}-gui-scale-matrix.png"));
        matrix.save(&path).unwrap();
        println!("{}", path.display());
    }
    std::fs::write(
        directory.join("wave-pointer-gui-scale-matrix.json"),
        serde_json::to_vec_pretty(&serde_json::json!({"row_gui_scales": gui_scales, "columns": states, "crop_logical_size": FOCUS, "display_magnification": MAGNIFICATION, "samples": reports})).unwrap(),
    ).unwrap();
}
