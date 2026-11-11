// app/audiolib-desktop/src/studio_theme_tests.rs
//! Studio dock regressions use synthetic images and an isolated software backend.
use crate::{AppWindow, AudioRow, FolderRow, theme::apply_theme};
use audiolib_core::config::{DEFAULT_THEME, DEFAULT_V2_THEME, STUDIO_THEME};
use i_slint_backend_testing::{ElementHandle, TestingBackend, TestingBackendOptions};
use slint::{
    ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel,
    platform::{Key, PointerEventButton, WindowEvent},
};
use std::{cell::RefCell, rc::Rc, time::Duration};

fn init_backend() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .expect("platform already initialized");
}

fn fixture(chinese: bool) -> AppWindow {
    let ui = AppWindow::new().unwrap();
    ui.window()
        .set_size(slint::LogicalSize::new(1600.0, 1000.0));
    ui.global::<crate::Theme>().set_motion(0.0);
    ui.set_chinese(chinese);
    ui.set_has_selection(true);
    ui.set_selected_id("42".into());
    ui.set_playback_path("C:/OwnedFixture/Synthetic.wav".into());
    ui.set_playback_name("Synthetic paused sound.wav".into());
    ui.set_audio_ready(true);
    ui.set_playback_state(4);
    ui.set_playing(false);
    ui.set_play_position(25.0);
    ui.set_play_duration(100.0);
    ui.set_play_time("00:25.000".into());
    ui.set_play_end("01:40.000".into());
    ui.set_view_start(0.0);
    ui.set_view_end(100.0);
    ui.set_wave_target_start(0.0);
    ui.set_wave_target_end(100.0);
    ui.set_wave_zoom(1.0);
    ui.set_wave_scroll(0.0);
    ui.set_waveform_ready(true);
    ui.set_waveform_progress(1.0);
    ui.set_peak_mode(1);
    ui.set_pitch_text("3.00".into());
    ui.set_speed_text("0.80".into());
    ui.set_audition_mono(true);
    ui.set_audition_repeat(true);
    ui.set_volume_db(-12.0);
    ui
}

fn frame(ui: &AppWindow, name: &str) -> SharedPixelBuffer<Rgba8Pixel> {
    // Drain deferred property changes before expiring transient name/zoom timers.
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1000));
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let pixels = ui.window().take_snapshot().unwrap();
    if let Some(directory) = std::env::var_os("AUDIOLIB_UI_SCALE_SNAPSHOT_DIR") {
        std::fs::create_dir_all(&directory).unwrap();
        image::save_buffer(
            std::path::Path::new(&directory).join(format!("{name}.png")),
            pixels.as_bytes(),
            pixels.width(),
            pixels.height(),
            image::ColorType::Rgba8,
        )
        .unwrap();
    }
    pixels
}

fn activate(ui: &AppWindow, label: &str) {
    let action = ElementHandle::find_by_accessible_label(ui, label)
        .next()
        .unwrap_or_else(|| panic!("Missing action: {label}"));
    assert_eq!(action.accessible_enabled(), Some(true), "{label}");
    action.invoke_accessible_default_action();
}

fn player_actions(ui: &AppWindow, label: &str) -> Vec<ElementHandle> {
    // In Chinese the menu bar's Playback entry is also named 播放. Scope the
    // transport query to the audition panel rather than invoking that menu.
    ElementHandle::find_by_element_type_name(ui, "AuditionPanel")
        .flat_map(|panel| {
            let label = label.to_owned();
            panel
                .query_descendants()
                .match_predicate(move |element| {
                    element
                        .accessible_label()
                        .is_some_and(|candidate| candidate == label)
                })
                .find_all()
        })
        .collect()
}

fn player_action(ui: &AppWindow, label: &str) -> ElementHandle {
    let mut actions = player_actions(ui, label);
    assert_eq!(actions.len(), 1, "One player action: {label}");
    actions.remove(0)
}

fn confirm_transport(ui: &AppWindow, state: audiolib_audio::PlaybackState, position: f64) {
    let status = audiolib_audio::PlaybackStatus {
        state,
        position,
        duration: 100.0,
        ..Default::default()
    };
    crate::controller::publish_playback_progress(ui, &status, false);
    crate::controller::publish_player_state(ui, &status, false);
    ui.set_audio_ready(!matches!(
        state,
        audiolib_audio::PlaybackState::Empty
            | audiolib_audio::PlaybackState::Loading
            | audiolib_audio::PlaybackState::Error
    ));
    // Drain the same property-change notifications as one real UI turn, including
    // the cached start-boundary used by transport controls during playback.
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
}

fn control_center(element: &ElementHandle) -> slint::LogicalPosition {
    let origin = element.absolute_position();
    let size = element.size();
    slint::LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0)
}

fn pointer_down(ui: &AppWindow, position: slint::LogicalPosition) {
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
}

fn pointer_up(ui: &AppWindow, position: slint::LogicalPosition) {
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

fn key(ui: &AppWindow, text: slint::SharedString) {
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

fn control_pixels(ui: &AppWindow, element: &ElementHandle) -> Vec<u8> {
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let pixels = ui.window().take_snapshot().unwrap();
    let origin = element.absolute_position();
    let size = element.size();
    let left = (origin.x.ceil() as u32).min(pixels.width());
    let top = (origin.y.ceil() as u32).min(pixels.height());
    let right = ((origin.x + size.width).floor() as u32).min(pixels.width());
    let bottom = ((origin.y + size.height).floor() as u32).min(pixels.height());
    assert!(right > left && bottom > top, "Control must be visible");
    let mut crop = Vec::new();
    for y in top..bottom {
        crop.extend_from_slice(
            &pixels.as_bytes()[((y * pixels.width() + left) * 4) as usize
                ..((y * pixels.width() + right) * 4) as usize],
        );
    }
    crop
}

fn timeline(ui: &AppWindow) -> ElementHandle {
    ElementHandle::find_by_accessible_label(
        ui,
        if ui.get_chinese() {
            "波形时间轴"
        } else {
            "Waveform timeline"
        },
    )
    .next()
    .expect("Waveform timeline")
}

fn timeline_pixels(ui: &AppWindow, name: &str) -> Vec<u8> {
    let pixels = frame(ui, name);
    let wave = timeline(ui);
    let origin = wave.absolute_position();
    let size = wave.size();
    // The testing backend uses a 1:1 logical-to-physical scale. Exclude the frame
    // hairline so this comparison measures image content rather than focus paint.
    let left = (origin.x.ceil() as u32 + 2).min(pixels.width());
    let top = (origin.y.ceil() as u32 + 2).min(pixels.height());
    let right = ((origin.x + size.width).floor() as u32)
        .saturating_sub(2)
        .min(pixels.width());
    let bottom = ((origin.y + size.height).floor() as u32)
        .saturating_sub(2)
        .min(pixels.height());
    assert!(right > left && bottom > top, "Timeline must be visible");
    let mut crop = Vec::new();
    for y in top..bottom {
        let start = ((y * pixels.width() + left) * 4) as usize;
        let end = ((y * pixels.width() + right) * 4) as usize;
        crop.extend_from_slice(&pixels.as_bytes()[start..end]);
    }
    crop
}

#[derive(Debug, PartialEq)]
struct AuditionState {
    id: String,
    path: String,
    ready: bool,
    state: i32,
    playing: bool,
    position: f32,
    duration: f32,
    range: (f32, f32, f32, f32),
    viewport: (f32, f32),
    pitch: String,
    rate: String,
    mono: bool,
    repeat: bool,
    volume: f32,
}

impl AuditionState {
    fn capture(ui: &AppWindow) -> Self {
        Self {
            id: ui.get_selected_id().to_string(),
            path: ui.get_playback_path().to_string(),
            ready: ui.get_audio_ready(),
            state: ui.get_playback_state(),
            playing: ui.get_playing(),
            position: ui.get_play_position(),
            duration: ui.get_play_duration(),
            range: (
                ui.get_view_start(),
                ui.get_view_end(),
                ui.get_wave_target_start(),
                ui.get_wave_target_end(),
            ),
            viewport: (ui.get_wave_zoom(), ui.get_wave_scroll()),
            pitch: ui.get_pitch_text().to_string(),
            rate: ui.get_speed_text().to_string(),
            mono: ui.get_audition_mono(),
            repeat: ui.get_audition_repeat(),
            volume: ui.get_volume_db(),
        }
    }
}

#[test]
fn studio_dock_expands_without_transport_commands_and_restores_classic_frame() {
    init_backend();
    for chinese in [false, true] {
        let language = if chinese { "cn" } else { "en" };
        let ui = fixture(chinese);
        let requests = Rc::new(RefCell::new(Vec::new()));
        let observed = requests.clone();
        ui.on_start_requested(move || observed.borrow_mut().push("play"));
        let observed = requests.clone();
        ui.on_pause_requested(move || observed.borrow_mut().push("pause"));
        let observed = requests.clone();
        ui.on_stop_requested(move || observed.borrow_mut().push("stop"));
        let observed = requests.clone();
        ui.on_transform_requested(move |_, _, _, _| observed.borrow_mut().push("transform"));
        let observed = requests.clone();
        ui.on_volume_db_requested(move |_| observed.borrow_mut().push("volume"));
        let observed = requests.clone();
        ui.on_seek_play_requested(move |_| observed.borrow_mut().push("seek"));
        apply_theme(&ui, DEFAULT_THEME);
        ui.show().unwrap();
        let expected = AuditionState::capture(&ui);
        let classic = frame(&ui, &format!("studio-classic-before-{language}"));
        let classic_height = timeline(&ui).size().height;

        apply_theme(&ui, STUDIO_THEME);
        let compact = frame(&ui, &format!("studio-compact-{language}"));
        let compact_height = timeline(&ui).size().height;
        assert!(!ui.get_audition_expanded());
        assert!(
            compact_height < classic_height,
            "Studio starts as a compact dock"
        );
        assert_ne!(classic.as_bytes(), compact.as_bytes());
        assert_eq!(AuditionState::capture(&ui), expected);
        activate(
            &ui,
            if chinese {
                "展开试听波形"
            } else {
                "Expand audition waveform"
            },
        );
        frame(&ui, &format!("studio-expanded-{language}"));
        assert!(ui.get_audition_expanded());
        assert!(timeline(&ui).size().height > compact_height * 2.0);
        assert_eq!(AuditionState::capture(&ui), expected);
        activate(
            &ui,
            if chinese {
                "收起试听波形"
            } else {
                "Compact audition waveform"
            },
        );
        frame(&ui, &format!("studio-recompacted-{language}"));
        assert!(!ui.get_audition_expanded());
        assert_eq!(AuditionState::capture(&ui), expected);

        apply_theme(&ui, DEFAULT_THEME);
        let restored = frame(&ui, &format!("studio-classic-restored-{language}"));
        assert_eq!(classic.as_bytes(), restored.as_bytes());
        assert_eq!(AuditionState::capture(&ui), expected);
        assert!(
            requests.borrow().is_empty(),
            "Appearance changes must not issue audio commands"
        );
        ui.hide().unwrap();
    }
}

#[test]
fn default_v2_keeps_classic_transport_in_the_studio_workbench() {
    init_backend();
    for chinese in [false, true] {
        let language = if chinese { "cn" } else { "en" };
        let ui = fixture(chinese);
        let requests = Rc::new(RefCell::new(Vec::new()));
        let observed = requests.clone();
        let weak = ui.as_weak();
        ui.on_start_requested(move || {
            observed.borrow_mut().push("play");
            let ui = weak.upgrade().unwrap();
            ui.set_playback_state(3);
            ui.set_playing(true);
        });
        let observed = requests.clone();
        let weak = ui.as_weak();
        ui.on_pause_requested(move || {
            observed.borrow_mut().push("pause");
            let ui = weak.upgrade().unwrap();
            ui.set_playback_state(4);
            ui.set_playing(false);
        });
        let observed = requests.clone();
        ui.on_stop_requested(move || observed.borrow_mut().push("stop"));
        let observed = requests.clone();
        ui.on_transform_requested(move |_, _, _, _| observed.borrow_mut().push("transform"));
        let observed = requests.clone();
        ui.on_volume_db_requested(move |_| observed.borrow_mut().push("volume"));
        apply_theme(&ui, DEFAULT_THEME);
        ui.show().unwrap();
        frame(&ui, &format!("default-v2-classic-reference-{language}"));
        let classic_height = timeline(&ui).size().height;
        let expected = AuditionState::capture(&ui);

        // Switching between Studio and Default v2 keeps the workbench, but must
        // replace its player without replaying transport or transform commands.
        for id in [DEFAULT_V2_THEME, STUDIO_THEME, DEFAULT_V2_THEME] {
            apply_theme(&ui, id);
            frame(&ui, &format!("default-v2-switch-{id}-{language}"));
            assert!(ui.global::<crate::Theme>().get_modern_style());
            assert_eq!(AuditionState::capture(&ui), expected);
            if id == DEFAULT_V2_THEME {
                assert!(!ui.global::<crate::Theme>().get_modern_player());
                assert!((timeline(&ui).size().height - classic_height).abs() < 1.0);
                for label in if chinese {
                    ["播放", "暂停"]
                } else {
                    ["Play", "Pause"]
                } {
                    assert_eq!(
                        player_actions(&ui, label).len(),
                        1,
                        "Classic transport must expose separate {label} controls"
                    );
                }
                assert!(
                    ElementHandle::find_by_accessible_label(
                        &ui,
                        if chinese {
                            "调整试听播放器高度"
                        } else {
                            "Resize audition dock"
                        }
                    )
                    .next()
                    .is_none(),
                    "Default v2 retains the classic player's fixed-height layout"
                );
            } else {
                assert!(ui.global::<crate::Theme>().get_modern_player());
                assert!(timeline(&ui).size().height < classic_height);
            }
        }
        assert!(
            requests.borrow().is_empty(),
            "Appearance changes must not issue audio commands"
        );
        for label in if chinese {
            ["播放", "暂停"]
        } else {
            ["Play", "Pause"]
        } {
            player_actions(&ui, label)[0].invoke_accessible_default_action();
        }
        assert_eq!(*requests.borrow(), ["play", "pause"]);

        // The modern sidebar remains operational alongside the classic player.
        activate(
            &ui,
            if chinese {
                "最近使用"
            } else {
                "Recent playback"
            },
        );
        assert!(ui.get_library_visible());
        assert_eq!(ui.get_history_kind(), 1);
        ui.set_library_visible(false);
        frame(&ui, &format!("default-v2-{language}"));
        ui.hide().unwrap();
    }
}

#[test]
fn classic_transport_uses_confirmed_phases_and_guards_unavailable_actions() {
    use audiolib_audio::PlaybackState;

    init_backend();
    for id in [DEFAULT_THEME, DEFAULT_V2_THEME] {
        for chinese in [false, true] {
            let ui = fixture(chinese);
            apply_theme(&ui, id);
            let actions = Rc::new(RefCell::new(Vec::new()));
            let observed = actions.clone();
            ui.on_start_requested(move || observed.borrow_mut().push("play"));
            let observed = actions.clone();
            ui.on_pause_requested(move || observed.borrow_mut().push("pause"));
            let observed = actions.clone();
            ui.on_stop_requested(move || observed.borrow_mut().push("stop"));
            let observed = actions.clone();
            ui.on_seek_requested(move |_| observed.borrow_mut().push("start"));
            ui.show().unwrap();
            frame(&ui, "classic-transport-phase-fixture");
            let labels = if chinese {
                ["播放", "暂停", "停止", "回到开头"]
            } else {
                ["Play", "Pause", "Stop", "Go to start"]
            };
            let mut play_hints = Vec::new();
            for (state, position, enabled) in [
                (PlaybackState::Empty, 0.0, [false; 4]),
                (PlaybackState::Loading, 0.0, [false; 4]),
                (PlaybackState::Ready, 0.0, [true, false, false, false]),
                (PlaybackState::Playing, 25.0, [false, true, true, true]),
                (PlaybackState::Paused, 25.0, [true, false, true, true]),
                (PlaybackState::Playing, 25.0, [false, true, true, true]),
                (PlaybackState::Ended, 100.0, [true, false, true, true]),
                (PlaybackState::Stopped, 0.0, [true, false, false, false]),
                (PlaybackState::Error, 0.0, [false; 4]),
            ] {
                confirm_transport(&ui, state, position);
                for (index, active) in [
                    state == PlaybackState::Playing,
                    state == PlaybackState::Paused,
                    state == PlaybackState::Stopped,
                ]
                .into_iter()
                .enumerate()
                {
                    let action = player_action(&ui, labels[index]);
                    assert_eq!(action.accessible_checkable(), Some(true));
                    assert_eq!(
                        action.accessible_checked(),
                        Some(active),
                        "{id}/{chinese:?}/{state:?}: {} reflects the confirmed phase",
                        labels[index]
                    );
                }
                actions.borrow_mut().clear();
                for (index, label) in labels.iter().enumerate() {
                    let action = player_action(&ui, label);
                    assert_eq!(action.accessible_enabled(), Some(enabled[index]));
                    let before = actions.borrow().len();
                    action.invoke_accessible_default_action();
                    assert_eq!(
                        actions.borrow().len() - before,
                        usize::from(enabled[index]),
                        "{id}/{state:?}: {label} must guard its accessibility action"
                    );
                }
                if matches!(
                    state,
                    PlaybackState::Ready | PlaybackState::Paused | PlaybackState::Ended
                ) {
                    let hint = player_action(&ui, labels[0])
                        .accessible_description()
                        .expect("Play exposes its current action hint");
                    assert!(!hint.is_empty());
                    play_hints.push(hint);
                }
            }
            assert_ne!(play_hints[0], play_hints[1], "Paused offers resume");
            assert_ne!(play_hints[1], play_hints[2], "Ended offers replay");

            for gate in ["missing", "maintenance", "loading", "error"] {
                confirm_transport(&ui, PlaybackState::Playing, 25.0);
                ui.set_selected_missing(gate == "missing");
                ui.set_library_busy(gate == "maintenance");
                if matches!(gate, "loading" | "error") {
                    // Confirmed transitional phases must win over a stale ready flag.
                    confirm_transport(
                        &ui,
                        if gate == "loading" {
                            PlaybackState::Loading
                        } else {
                            PlaybackState::Error
                        },
                        25.0,
                    );
                    ui.set_audio_ready(true);
                }
                actions.borrow_mut().clear();
                for label in labels {
                    let action = player_action(&ui, label);
                    assert_eq!(action.accessible_enabled(), Some(false), "{gate}: {label}");
                    assert!(
                        !action
                            .accessible_description()
                            .unwrap_or_default()
                            .is_empty(),
                        "{gate}: disabled {label} explains why"
                    );
                    action.invoke_accessible_default_action();
                }
                assert!(actions.borrow().is_empty(), "{gate} dispatched transport");
                ui.set_selected_missing(false);
                ui.set_library_busy(false);
            }
            confirm_transport(&ui, PlaybackState::Ready, 0.0);
            for (label, mono) in if chinese {
                [("循环整个声音", false), ("单声道监听", true)]
            } else {
                [("Repeat sound", false), ("Mono monitoring", true)]
            } {
                let action = player_action(&ui, label);
                assert_eq!(action.accessible_checkable(), Some(true));
                for _ in 0..2 {
                    let previous = action.accessible_checked().unwrap();
                    action.invoke_accessible_default_action();
                    assert_eq!(action.accessible_checked(), Some(!previous));
                    assert_eq!(
                        if mono {
                            ui.get_audition_mono()
                        } else {
                            ui.get_audition_repeat()
                        },
                        !previous
                    );
                }
            }
            ui.hide().unwrap();
        }
    }
}

#[test]
fn classic_transport_pointer_feedback_cancels_cleanly_and_preserves_input_focus() {
    init_backend();
    for id in [DEFAULT_THEME, DEFAULT_V2_THEME] {
        for motion in [0.0, 1.0] {
            let ui = fixture(false);
            apply_theme(&ui, id);
            ui.global::<crate::Theme>().set_motion(motion);
            let plays = Rc::new(RefCell::new(0));
            let observed = plays.clone();
            ui.on_start_requested(move || *observed.borrow_mut() += 1);
            ui.show().unwrap();
            frame(&ui, "classic-pointer-fixture");
            ui.set_perf_marker_enabled(true);
            ui.invoke_perf_focus_search();
            let action = player_action(&ui, "Play");
            let origin = action.absolute_position();
            let size = action.size();
            let position = control_center(&action);
            let away = slint::LogicalPosition::new(position.x, origin.y + size.height * 2.0);
            ui.window()
                .dispatch_event(WindowEvent::PointerMoved { position: away });
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(300));
            let idle = control_pixels(&ui, &action);
            ui.window()
                .dispatch_event(WindowEvent::PointerMoved { position });
            let _ = control_pixels(&ui, &action);
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(150));
            let hover = control_pixels(&ui, &action);
            assert_ne!(idle, hover, "{id}/{motion}: hover is visible");
            pointer_down(&ui, position);
            assert_eq!(*plays.borrow(), 0, "Press waits for release");
            let _ = control_pixels(&ui, &action);
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(150));
            assert_ne!(hover, control_pixels(&ui, &action), "Press is visible");
            ui.window()
                .dispatch_event(WindowEvent::PointerMoved { position: away });
            pointer_up(&ui, away);
            assert_eq!(*plays.borrow(), 0, "Release outside cancels activation");
            pointer_down(&ui, position);
            pointer_up(&ui, position);
            assert_eq!(*plays.borrow(), 1, "One click invokes Play once");
            assert_eq!(action.absolute_position(), origin);
            assert_eq!(action.size(), size);
            key(&ui, "x".into());
            assert_eq!(
                ui.get_query(),
                "x",
                "Player click retains search input focus"
            );
            ui.window()
                .dispatch_event(WindowEvent::PointerMoved { position: away });
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1000));
            let settled = control_pixels(&ui, &action);
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1000));
            assert_eq!(settled, control_pixels(&ui, &action), "Feedback settles");
            ui.hide().unwrap();
        }
    }
}

#[test]
fn classic_transport_pointer_gesture_cannot_cross_disabled_state_transitions() {
    use audiolib_audio::PlaybackState;

    init_backend();
    for id in [DEFAULT_THEME, DEFAULT_V2_THEME] {
        for motion in [0.0, 1.0] {
            let ui = fixture(false);
            apply_theme(&ui, id);
            ui.global::<crate::Theme>().set_motion(motion);
            let plays = Rc::new(RefCell::new(0));
            let observed = plays.clone();
            ui.on_start_requested(move || *observed.borrow_mut() += 1);
            ui.show().unwrap();
            frame(&ui, "classic-pointer-transition-fixture");

            for (disabled, position, enabled, next_position) in [
                (PlaybackState::Loading, 0.0, PlaybackState::Ready, 0.0),
                (PlaybackState::Playing, 25.0, PlaybackState::Ended, 100.0),
            ] {
                *plays.borrow_mut() = 0;
                confirm_transport(&ui, disabled, position);
                let action = player_action(&ui, "Play");
                assert_eq!(action.accessible_enabled(), Some(false));
                assert!(
                    !action
                        .accessible_description()
                        .unwrap_or_default()
                        .is_empty(),
                    "A disabled control retains its explanatory hover hint"
                );
                let position = control_center(&action);
                pointer_down(&ui, position);
                confirm_transport(&ui, enabled, next_position);
                assert_eq!(action.accessible_enabled(), Some(true));
                pointer_up(&ui, position);
                assert_eq!(
                    *plays.borrow(),
                    0,
                    "{id}/{motion}/{disabled:?}: enabling during a held pointer cannot arm Play"
                );
                pointer_down(&ui, position);
                pointer_up(&ui, position);
                assert_eq!(
                    *plays.borrow(),
                    1,
                    "A fresh enabled click fires exactly once"
                );
            }

            *plays.borrow_mut() = 0;
            confirm_transport(&ui, PlaybackState::Ready, 0.0);
            let action = player_action(&ui, "Play");
            let position = control_center(&action);
            pointer_down(&ui, position);
            confirm_transport(&ui, PlaybackState::Loading, 0.0);
            assert_eq!(action.accessible_enabled(), Some(false));
            confirm_transport(&ui, PlaybackState::Ready, 0.0);
            assert_eq!(action.accessible_enabled(), Some(true));
            pointer_up(&ui, position);
            assert_eq!(
                *plays.borrow(),
                0,
                "{id}/{motion}: disabling invalidates an already armed pointer gesture"
            );
            pointer_down(&ui, position);
            pointer_up(&ui, position);
            assert_eq!(
                *plays.borrow(),
                1,
                "A fresh enabled click still fires exactly once"
            );
            ui.hide().unwrap();
        }
    }
}

#[test]
fn classic_transport_keyboard_releases_once_and_cancels_on_focus_loss() {
    init_backend();
    for id in [DEFAULT_THEME, DEFAULT_V2_THEME] {
        for chinese in [false, true] {
            let ui = fixture(chinese);
            apply_theme(&ui, id);
            let changes = Rc::new(RefCell::new(Vec::new()));
            let observed = changes.clone();
            ui.on_transform_requested(move |_, _, _, repeat| {
                observed.borrow_mut().push(repeat);
            });
            ui.show().unwrap();
            frame(&ui, "classic-keyboard-fixture");
            let field = player_action(
                &ui,
                if chinese {
                    "变调半音，回车或离开时应用"
                } else {
                    "Pitch semitones, applied on Enter or focus loss"
                },
            );
            let position = control_center(&field);
            pointer_down(&ui, position);
            pointer_up(&ui, position);
            // Repeat immediately precedes Pitch in the transport's tab order.
            key(&ui, Key::Backtab.into());
            changes.borrow_mut().clear();
            for activation in [slint::SharedString::from(" "), Key::Return.into()] {
                let previous = ui.get_audition_repeat();
                for _ in 0..3 {
                    ui.window().dispatch_event(WindowEvent::KeyPressed {
                        text: activation.clone(),
                    });
                }
                assert_eq!(ui.get_audition_repeat(), previous, "Wait for key release");
                let before = changes.borrow().len();
                ui.window()
                    .dispatch_event(WindowEvent::KeyReleased { text: activation });
                assert_eq!(ui.get_audition_repeat(), !previous);
                assert_eq!(
                    changes.borrow().len(),
                    before + 1,
                    "Ignore repeated key-down"
                );
            }
            let previous = ui.get_audition_repeat();
            ui.window()
                .dispatch_event(WindowEvent::KeyPressed { text: " ".into() });
            pointer_down(&ui, position);
            pointer_up(&ui, position);
            ui.window()
                .dispatch_event(WindowEvent::KeyReleased { text: " ".into() });
            assert_eq!(
                ui.get_audition_repeat(),
                previous,
                "Focus loss cancels the key"
            );
            ui.hide().unwrap();
        }
    }
}

fn image_from(color: [u8; 4], mask: bool) -> Image {
    let mut bytes = vec![0; 64 * 32 * 4];
    for y in 0..32 {
        for x in 0..64 {
            if !mask || (12..20).contains(&y) {
                bytes[(y * 64 + x) * 4..(y * 64 + x + 1) * 4].copy_from_slice(&color);
            }
        }
    }
    Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        &bytes, 64, 32,
    ))
}

#[test]
fn studio_compact_mask_and_expanded_spectral_images_remain_separate() {
    init_backend();
    let ui = fixture(false);
    let spectral_a = image_from([210, 35, 80, 255], false);
    let spectral_b = image_from([25, 165, 220, 255], false);
    let mask = image_from([255, 255, 255, 255], true);
    let empty_mask = image_from([0, 0, 0, 0], true);
    ui.set_waveform_image(spectral_a.clone());
    ui.set_waveform_mask(mask.clone());
    apply_theme(&ui, STUDIO_THEME);
    ui.show().unwrap();
    let compact = timeline_pixels(&ui, "studio-mask-original");
    ui.set_waveform_image(spectral_b.clone());
    assert_eq!(
        compact,
        timeline_pixels(&ui, "studio-mask-spectral-changed")
    );
    ui.set_waveform_mask(empty_mask.clone());
    assert_ne!(compact, timeline_pixels(&ui, "studio-mask-cleared"));
    ui.set_waveform_mask(mask);
    assert_eq!(compact, timeline_pixels(&ui, "studio-mask-restored"));

    activate(&ui, "Expand audition waveform");
    let expanded = timeline_pixels(&ui, "studio-spectral-original");
    ui.set_waveform_mask(empty_mask);
    assert_eq!(
        expanded,
        timeline_pixels(&ui, "studio-spectral-mask-changed")
    );
    ui.set_waveform_image(spectral_a.clone());
    let changed = timeline_pixels(&ui, "studio-spectral-changed");
    assert_ne!(expanded, changed);
    assert_eq!(
        ui.get_waveform_image().to_rgba8().unwrap().as_bytes(),
        spectral_a.to_rgba8().unwrap().as_bytes(),
        "The compact mask must not replace the renderer's original image"
    );
    apply_theme(&ui, DEFAULT_THEME);
    frame(&ui, "studio-spectral-classic");
    assert_eq!(
        ui.get_waveform_image().to_rgba8().unwrap().as_bytes(),
        spectral_a.to_rgba8().unwrap().as_bytes()
    );
    apply_theme(&ui, STUDIO_THEME);
    assert!(
        ui.get_audition_expanded(),
        "The Studio dock retains its expanded state"
    );
    assert_eq!(changed, timeline_pixels(&ui, "studio-spectral-returned"));
    ui.hide().unwrap();
}

fn settle_viewport(ui: &AppWindow) {
    // The restore timer waits until the virtual rows have real layout bounds.
    // Exercise that asynchronous boundary instead of merely reading wanted-Y.
    for _ in 0..10 {
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        ui.window().take_snapshot().unwrap();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        ui.window().take_snapshot().unwrap();
        if !ui.get_task_restoring() {
            return;
        }
    }
    panic!("The active result viewport did not finish restoration");
}

#[test]
fn live_theme_switch_keeps_record_anchor_and_reports_only_the_active_viewport() {
    init_backend();
    let ui = fixture(false);
    ui.set_rows(ModelRc::new(VecModel::from(
        (0..200)
            .map(|index| AudioRow {
                id: index.to_string().into(),
                name: format!("Synthetic sound {index:03}.wav").into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_folders(ModelRc::new(VecModel::from(
        (0..40)
            .map(|index| FolderRow {
                name: format!("Synthetic folder {index:02}").into(),
                path: format!("C:/OwnedFixture/{index:02}").into(),
                count: "5".into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_selected_row(42);
    ui.set_edit_tags("unsaved synthetic annotation".into());
    ui.set_metadata_dirty(true);
    ui.set_list_waveforms_enabled(true);
    ui.set_can_next(true);
    ui.set_can_previous(true);
    apply_theme(&ui, DEFAULT_THEME);
    ui.show().unwrap();
    frame(&ui, "studio-viewport-initial");
    let anchor = 31.5;
    let classic_row_height = ui.get_task_result_row_height();
    ui.invoke_restore_search_task_viewport(-classic_row_height * anchor, 0.0, -120.0, 0.0, 0.0);
    settle_viewport(&ui);
    assert!(
        (ui.get_task_result_scroll_y() / classic_row_height + anchor).abs() < 0.01,
        "Initial Default restore: y={}, row-height={classic_row_height}, wanted-y={}, restoring={}",
        ui.get_task_result_scroll_y(),
        -classic_row_height * anchor,
        ui.get_task_restoring()
    );
    assert!(
        (ui.get_task_folder_scroll_y() + 120.0).abs() < 1.0,
        "Initial folder restore: y={}, wanted=-120",
        ui.get_task_folder_scroll_y()
    );

    let windows = Rc::new(RefCell::new(Vec::new()));
    let observed = windows.clone();
    ui.on_result_window_changed(move |first, count| observed.borrow_mut().push((first, count)));
    let ranges = Rc::new(RefCell::new(Vec::new()));
    let observed = ranges.clone();
    ui.on_result_range_requested(move |direction| observed.borrow_mut().push(direction));
    let expected = AuditionState::capture(&ui);
    for id in [
        STUDIO_THEME,
        DEFAULT_V2_THEME,
        STUDIO_THEME,
        DEFAULT_THEME,
        DEFAULT_V2_THEME,
        DEFAULT_THEME,
    ] {
        windows.borrow_mut().clear();
        apply_theme(&ui, id);
        let scheduled_y = ui.get_task_result_scroll_y();
        let scheduled_row_height = ui.get_task_result_row_height();
        settle_viewport(&ui);
        let settled_y = ui.get_task_result_scroll_y();
        let settled_folder_y = ui.get_task_folder_scroll_y();
        frame(&ui, &format!("studio-viewport-{id}"));
        let row_height = ui.get_task_result_row_height();
        let actual_y = ui.get_task_result_scroll_y();
        if id != DEFAULT_THEME {
            assert!(row_height > classic_row_height);
        } else {
            assert_eq!(row_height, classic_row_height);
        }
        assert!(
            (actual_y / row_height + anchor).abs() < 0.01,
            "{id} changed the record anchor: actual-y={actual_y}, row-height={row_height}, wanted-y={}, actual-anchor={}, wanted-anchor={anchor}, scheduled=({scheduled_y}, row-height={scheduled_row_height}), settled-y={settled_y}, settled-folder-y={settled_folder_y}, visible-height={}, restoring={}, callbacks={:?}",
            -row_height * anchor,
            -actual_y / row_height,
            ui.get_task_result_visible_height(),
            ui.get_task_restoring(),
            windows.borrow()
        );
        assert!(
            (ui.get_task_folder_scroll_y() + 120.0).abs() < 1.0,
            "{id} discarded the folder viewport: actual={}, wanted=-120, settled={settled_folder_y}",
            ui.get_task_folder_scroll_y()
        );
        assert_eq!(ui.get_selected_row(), 42);
        assert_eq!(ui.get_edit_tags().as_str(), "unsaved synthetic annotation");
        assert!(ui.get_metadata_dirty());
        assert_eq!(AuditionState::capture(&ui), expected);
        let windows = windows.borrow();
        assert!(
            !windows.is_empty(),
            "The restored active viewport must request its visible Waveform range"
        );
        assert!(
            windows
                .iter()
                .all(|(first, count)| *first == 31 && *count > 0),
            "{id} reported a hidden or pre-restore result viewport: {windows:?}"
        );
        assert!(
            ranges.borrow().is_empty(),
            "Appearance changes must not request search paging"
        );
    }
    ui.hide().unwrap();
}
