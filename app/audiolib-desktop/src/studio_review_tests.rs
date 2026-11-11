// app/audiolib-desktop/src/studio_review_tests.rs
//! Pointer regressions use a synthetic Studio timeline without an audio device.
use crate::AppWindow;
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use slint::{
    ComponentHandle, LogicalPosition,
    platform::{Key, PointerEventButton, WindowEvent},
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

fn flush() {
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
}

fn window() -> AppWindow {
    let ui = AppWindow::new().unwrap();
    crate::theme::apply_theme(&ui, audiolib_core::config::STUDIO_THEME);
    ui.window()
        .set_size(slint::LogicalSize::new(1600.0, 1000.0));
    ui.global::<crate::Theme>().set_motion(0.0);
    ui.set_audio_ready(true);
    ui.set_playback_state(4);
    ui.set_playback_clock("1:0".into());
    ui.set_playback_name("Synthetic source.wav".into());
    ui.set_play_duration(100.0);
    ui.set_view_start(0.0);
    ui.set_view_end(100.0);
    ui.set_wave_target_start(0.0);
    ui.set_wave_target_end(100.0);
    ui.set_waveform_ready(true);
    ui.show().unwrap();
    flush();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(20));
    flush();
    ui
}

fn position(ui: &AppWindow, fraction: f32) -> LogicalPosition {
    let wave = ElementHandle::find_by_accessible_label(ui, "Waveform timeline")
        .next()
        .expect("Studio Waveform timeline");
    let origin = wave.absolute_position();
    let size = wave.size();
    assert!(size.width > 20.0 && size.height > 10.0);
    LogicalPosition::new(
        origin.x + size.width * fraction,
        origin.y + size.height * 0.55,
    )
}

fn begin(ui: &AppWindow, fraction: f32) {
    let position = position(ui, fraction);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    flush();
}

fn release(ui: &AppWindow, fraction: f32) {
    let position = position(ui, fraction);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
    flush();
}

#[test]
fn studio_cancelled_scrubs_never_seek_and_the_next_drag_still_works() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = window();
    let seeks = Rc::new(RefCell::new(Vec::new()));
    let observed = seeks.clone();
    ui.on_seek_play_requested(move |seconds| observed.borrow_mut().push(seconds));

    for gate in ["window-exit", "not-ready", "maintenance", "missing"] {
        let before = seeks.borrow().len();
        begin(&ui, 0.2);
        match gate {
            "window-exit" => ui.window().dispatch_event(WindowEvent::PointerExited),
            "not-ready" => ui.set_audio_ready(false),
            "maintenance" => ui.set_library_busy(true),
            "missing" => ui.set_selected_missing(true),
            _ => unreachable!(),
        }
        flush();
        // A disabled TouchArea emits the same button=Other cancellation as a
        // window exit when the next pointer event reaches its old mouse grab.
        if gate != "window-exit" {
            ui.window().dispatch_event(WindowEvent::PointerMoved {
                position: position(&ui, 0.4),
            });
        }
        ui.set_audio_ready(true);
        ui.set_library_busy(false);
        ui.set_selected_missing(false);
        flush();
        release(&ui, 0.7);
        assert_eq!(
            seeks.borrow().len(),
            before,
            "{gate}: an unpaired release must not revive the cancelled gesture"
        );

        begin(&ui, 0.25);
        release(&ui, 0.65);
        assert_eq!(seeks.borrow().len(), before + 1);
        assert!(
            (seeks.borrow()[before] - 65.0_f32).abs() < 0.05,
            "{gate}: a fresh drag must still seek to the pointer's source time"
        );
    }
    ui.hide().unwrap();
}

#[test]
fn studio_scrub_survives_progressive_images_but_not_an_engine_discontinuity() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = window();
    let seeks = Rc::new(RefCell::new(Vec::new()));
    let observed = seeks.clone();
    ui.on_seek_play_requested(move |seconds| observed.borrow_mut().push(seconds));

    begin(&ui, 0.2);
    let pixels =
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&[255_u8; 8 * 8 * 4], 8, 8);
    ui.set_waveform_image(slint::Image::from_rgba8(pixels.clone()));
    ui.set_waveform_mask(slint::Image::from_rgba8(pixels));
    ui.set_waveform_progress(0.5);
    flush();
    release(&ui, 0.6);
    assert_eq!(seeks.borrow().len(), 1);
    assert!((seeks.borrow()[0] - 60.0_f32).abs() < 0.05);

    for clock in ["2:0", "2:1"] {
        begin(&ui, 0.3);
        // The production controller publishes generation:transport-revision:
        // source replacement and external seeks must cancel the old gesture.
        ui.set_playback_clock(clock.into());
        flush();
        release(&ui, 0.7);
        assert_eq!(
            seeks.borrow().len(),
            1,
            "Clock {clock} allowed a stale drag to seek the replacement transport"
        );
    }
    begin(&ui, 0.3);
    release(&ui, 0.75);
    assert_eq!(seeks.borrow().len(), 2);
    assert!((seeks.borrow()[1] - 75.0_f32).abs() < 0.05);
    ui.hide().unwrap();
}

fn key(ui: &AppWindow, text: slint::SharedString) {
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
    flush();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(20));
}

#[test]
fn default_tab_navigation_reveals_the_scaled_player_before_keyboard_play() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        crate::theme::apply_theme(&ui, audiolib_core::config::DEFAULT_THEME);
        ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
        ui.global::<crate::Theme>().set_ui_scale(2.0);
        ui.global::<crate::Theme>().set_motion(0.0);
        ui.set_chinese(chinese);
        ui.set_sidebar_visible(false);
        ui.set_inspector_visible(false);
        ui.set_audio_ready(true);
        let starts = Rc::new(Cell::new(0));
        let observed = starts.clone();
        ui.on_start_requested(move || observed.set(observed.get() + 1));
        ui.show().unwrap();
        flush();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(20));
        assert!(
            ui.get_main_last_bottom() > 700.0 || ui.get_main_last_right() > 1080.0,
            "Calibrate the scaled main view as overflowing: bottom={}, right={}",
            ui.get_main_last_bottom(),
            ui.get_main_last_right()
        );

        // ElementHandle has no focus setter/getter. Follow native Tab traversal,
        // then use Space and the transport callback to identify the Play owner.
        // Space on the result list has a separate callback, so it cannot falsely
        // satisfy this check. No product-only test focus hook is needed.
        // The testing query prunes fully clipped items, unlike native Tab, which
        // visits items that their Flickable ancestors can reveal. Query after
        // each focus move rather than requiring an offscreen accessibility item.
        let mut activated = None;
        let mut last_seen = None;
        for step in 0..160 {
            key(&ui, Key::Tab.into());
            let Some(play) = ElementHandle::find_by_element_type_name(&ui, "AuditionPanel")
                .flat_map(|panel| {
                    panel
                        .query_descendants()
                        .match_predicate(move |element| {
                            element.accessible_role() == Some(AccessibleRole::Button)
                                && element.accessible_label().is_some_and(|label| {
                                    label == if chinese { "播放" } else { "Play" }
                                })
                        })
                        .find_all()
                })
                .next()
            else {
                continue;
            };
            let origin = play.absolute_position();
            let size = play.size();
            last_seen = Some((step, origin, size));
            if origin.x >= 0.0
                && origin.y >= 0.0
                && origin.x + size.width <= 1080.5
                && origin.y + size.height <= 700.5
            {
                key(&ui, " ".into());
                if starts.get() > 0 {
                    activated = Some(play);
                    break;
                }
            }
        }
        assert!(
            starts.get() > 0,
            "Tab never revealed and activated the scaled Play control: last-seen={last_seen:?}, main-bottom={}, main-right={}",
            ui.get_main_last_bottom(),
            ui.get_main_last_right()
        );
        let play = activated.expect("Keyboard activation identified the Play control");
        let origin = play.absolute_position();
        let size = play.size();
        assert!(
            origin.x >= 0.0
                && origin.y >= 0.0
                && origin.x + size.width <= 1080.5
                && origin.y + size.height <= 700.5,
            "Keyboard activation left Play outside its scroll viewport: {origin:?}, {size:?}"
        );
        ui.hide().unwrap();
    }
}

#[test]
fn studio_mute_survives_a_real_presenter_round_trip() {
    use audiolib_core::config::{DEFAULT_THEME, STUDIO_THEME};
    i_slint_backend_testing::init_no_event_loop();
    let ui = window();
    ui.set_volume_db(-18.0);
    let activate = |label: &str| {
        ElementHandle::find_by_accessible_label(&ui, label)
            .next()
            .unwrap_or_else(|| panic!("Missing {label}"))
            .invoke_accessible_default_action();
        flush();
    };
    activate("Mute");
    assert_eq!(ui.get_volume_db(), -60.0);
    // Force presenter destruction between styles, rather than coalescing the two
    // assignments into one event-loop turn that would leave the old child alive.
    crate::theme::apply_theme(&ui, DEFAULT_THEME);
    flush();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(40));
    crate::theme::apply_theme(&ui, STUDIO_THEME);
    flush();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(40));
    activate("Unmute");
    assert_eq!(
        ui.get_volume_db(),
        -18.0,
        "the incoming facade lost the muted level"
    );
    ui.hide().unwrap();
}
