// app/audiolib-desktop/src/client_review_tests.rs
//! UI controller regressions use owned empty libraries and no playback device.
use super::*;

fn fixture() -> (
    tempfile::TempDir,
    AppWindow,
    audiolib_audio::AudioEngine,
    Arc<Controller>,
) {
    i_slint_backend_testing::init_no_event_loop();
    make_fixture()
}

fn make_fixture() -> (
    tempfile::TempDir,
    AppWindow,
    audiolib_audio::AudioEngine,
    Arc<Controller>,
) {
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig::load_in(root.path().join("preferences")).unwrap();
    let provider = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
    let playback = PlaybackConfig::default();
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
    (root, ui, engine, controller)
}

#[test]
fn clearing_filters_leaves_missing_view_and_restores_present_results() {
    let (_root, ui, _engine, controller) = fixture();
    ui.set_visibility_index(1);
    ui.set_favorites_only(true);
    ui.set_duration_min("3".into());
    ui.invoke_reset_filters();
    assert_eq!(ui.get_visibility_index(), 0);
    assert!(!ui.get_favorites_only());
    assert!(ui.get_duration_min().is_empty());
    assert_eq!(
        controller.request.lock().unwrap().filters.visibility,
        FileVisibility::Present
    );
    controller.shutdown();
}

#[test]
fn persistent_playback_error_does_not_replace_later_feedback() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "controller::client_review_tests::child_playback_feedback",
            "--ignored",
            "--test-threads=1",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "status worker feedback uses an isolated event loop"]
fn child_playback_feedback() {
    use std::{cell::Cell, rc::Rc};
    i_slint_backend_testing::init_integration_test_with_system_time();
    let (_root, ui, _engine, controller) = make_fixture();
    let stage = Rc::new(Cell::new(0));
    let observed = stage.clone();
    let feedback_time = Rc::new(Cell::new(Instant::now()));
    let since = feedback_time.clone();
    let started = Instant::now();
    let c = controller.clone();
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(10),
        move || {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "playback feedback timed out at stage {}",
                observed.get()
            );
            let ui = weak.upgrade().unwrap();
            match observed.get() {
                // Row paging becomes idle before the initial directory model.
                // Settle both publications before testing persistent audio errors.
                0 if !ui.get_searching()
                    && !ui.get_search_workspace_loading()
                    && !ui.get_task_restoring()
                    && c.search_publication
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|state| {
                            state.page == SearchPagePhase::Published && state.model_published
                        }) =>
                {
                    c.command(AudioCommand::Seek(f64::NAN));
                    observed.set(1);
                }
                1 if c
                    .audio
                    .status()
                    .error
                    .as_ref()
                    .is_some_and(|error| ui.get_status().as_str() == error) =>
                {
                    ui.set_status("Later Metadata feedback".into());
                    since.set(Instant::now());
                    observed.set(2);
                }
                2 if since.get().elapsed() > Duration::from_millis(300) => {
                    assert_eq!(ui.get_status(), "Later Metadata feedback");
                    assert!(
                        c.audio.status().error.is_some(),
                        "the original engine error remains active"
                    );
                    observed.set(3);
                    c.shutdown();
                    ui.hide().unwrap();
                    slint::quit_event_loop().unwrap();
                }
                _ => {}
            }
        },
    );
    ui.run().unwrap();
    timer.stop();
    assert_eq!(stage.get(), 3);
}

#[test]
fn reopening_output_form_restores_acknowledged_configuration() {
    let (_root, ui, _engine, controller) = fixture();
    let active = PlaybackConfig {
        backend: AudioBackend::Asio,
        rate_policy: RatePolicy::Native,
        buffer_frames: 256,
        output_pair: 3,
        source_pair: 2,
        ..Default::default()
    };
    *controller.playback.lock().unwrap() = active;
    ui.set_backend_index(0);
    ui.set_rate_index(0);
    ui.set_buffer_frames("bad draft".into());
    ui.set_pair_index("9".into());
    ui.set_source_pair("8".into());
    ui.set_output_dirty(true);
    controller.restore_output_form(&ui);
    assert_eq!(ui.get_backend_index(), 1);
    assert_eq!(ui.get_rate_index(), 1);
    assert_eq!(ui.get_buffer_frames(), "256");
    assert_eq!(ui.get_pair_index(), "3");
    assert_eq!(ui.get_source_pair(), "2");
    assert!(!ui.get_output_dirty());
    controller.shutdown();
}

#[test]
fn rejected_output_draft_stays_open_and_stops_the_close_save_chain() {
    let (_root, ui, _engine, controller) = fixture();
    let saved_playback = controller.config.lock().unwrap().playback.clone();
    ui.set_settings_visible(true);
    ui.set_settings_page(0);
    ui.set_output_dirty(true);
    ui.set_shared_sync_dirty(true);
    ui.set_settings_close_pending(true);
    ui.set_settings_exit_pending(true);
    ui.set_buffer_frames("invalid buffer draft".into());

    controller.apply_output(&ui, false);

    assert!(ui.get_settings_visible());
    assert_eq!(ui.get_settings_page(), 0);
    assert_eq!(ui.get_buffer_frames(), "invalid buffer draft");
    assert!(ui.get_output_dirty());
    assert!(ui.get_shared_sync_dirty());
    assert!(!ui.get_settings_close_pending());
    assert!(!ui.get_settings_exit_pending());
    assert!(!ui.get_output_applying());
    assert!(ui.get_device_info().contains("Buffer"));
    assert_eq!(controller.config.lock().unwrap().playback, saved_playback);
    controller.shutdown();
}

#[test]
fn repeated_audio_apply_while_busy_preserves_the_existing_close_chain() {
    let (_root, ui, _engine, controller) = fixture();
    ui.set_settings_visible(true);
    ui.set_output_dirty(true);
    ui.set_output_applying(true);
    ui.set_settings_close_pending(true);
    ui.set_settings_exit_pending(true);
    controller.apply_output(&ui, false);
    assert!(ui.get_settings_close_pending());
    assert!(ui.get_settings_exit_pending());
    assert!(ui.get_output_dirty());
    ui.set_output_applying(false);
    controller.shutdown();
}

#[test]
fn application_close_waits_for_global_settings_drafts_and_active_saves() {
    let (_root, ui, _engine, controller) = fixture();
    ui.set_settings_visible(true);
    for (audio_dirty, sync_dirty, audio_busy, sync_busy) in [
        (true, false, false, false),
        (false, true, false, false),
        (false, false, true, false),
        (false, false, false, true),
    ] {
        ui.set_output_dirty(audio_dirty);
        ui.set_shared_sync_dirty(sync_dirty);
        ui.set_output_applying(audio_busy);
        ui.set_shared_sync_busy(sync_busy);
        ui.set_settings_exit_pending(false);

        controller.request_close(&ui);

        assert!(ui.get_settings_visible());
        assert!(ui.get_settings_exit_pending());
        assert!(!ui.get_closing());
        assert!(!controller.closing.load(Ordering::Acquire));
        assert!(
            !ui.get_settings_close_pending(),
            "app Quit must not grant consent to apply settings"
        );
    }
    controller.shutdown();
}

#[test]
fn completed_scan_and_output_validation_follow_runtime_language() {
    let (_root, ui, _engine, controller) = fixture();
    let progress = audiolib_core::ScanProgress {
        processed: 12,
        added: 3,
        ..Default::default()
    };
    controller.publish_scan_completion(&ui, 0, 0, false, false, Ok(progress));
    assert!(ui.get_scan_summary().contains("3 added"));
    controller.chinese.store(true, Ordering::Release);
    ui.set_chinese(true);
    controller.refresh_generated_text(&ui);
    assert!(ui.get_scan_summary().contains("新增 3"));
    assert!(!ui.get_scan_summary().contains("added"));
    ui.set_buffer_frames("invalid".into());
    controller.apply_output(&ui, false);
    assert!(ui.get_device_info().contains("缓冲"));
    controller.shutdown();
}

#[test]
fn preview_volume_label_follows_slider_while_dragging() {
    use i_slint_backend_testing::{AccessibleRole, ElementQuery};
    use slint::LogicalPosition;
    use slint::platform::{PointerEventButton, WindowEvent};

    let (_root, ui, _engine, controller) = fixture();
    ui.set_audio_ready(true);
    ui.set_has_selection(true);
    ui.show().unwrap();
    let slider = ElementQuery::from_root(&ui)
        .match_descendants()
        .match_accessible_role(AccessibleRole::Slider)
        .find_all()
        .into_iter()
        .find(|item| item.accessible_value_minimum() == Some(-60.0))
        .expect("volume slider");
    let origin = slider.absolute_position();
    let size = slider.size();
    let y = origin.y + size.height / 2.0;
    let at = |fraction: f32| LogicalPosition::new(origin.x + size.width * fraction, y);
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position: at(0.5),
        button: PointerEventButton::Left,
    });
    let mut labels = Vec::new();
    for fraction in [0.2, 0.4, 0.6, 0.8] {
        ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: at(fraction),
        });
        // Still mid-drag: the poll would not publish while adjusting, so the
        // label must come from the slider callback itself.
        let label = ui.get_volume_label().to_string();
        assert_eq!(
            label,
            volume_label(gain_from_db(ui.get_volume_db())).as_str(),
            "label lagged the dragged value at {fraction}"
        );
        labels.push(label);
    }
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position: at(0.8),
        button: PointerEventButton::Left,
    });
    labels.dedup();
    assert_eq!(
        labels.len(),
        4,
        "dB label did not change during drag: {labels:?}"
    );
    controller.shutdown();
}

#[test]
fn focused_volume_reflects_ipc_acknowledgement_without_replaying_stale_drag_status() {
    use audiolib_core::config::{DEFAULT_THEME, STUDIO_THEME};
    use i_slint_backend_testing::{AccessibleRole, ElementHandle, ElementQuery};
    use slint::{
        LogicalPosition,
        platform::{PointerEventButton, WindowEvent},
    };

    i_slint_backend_testing::init_no_event_loop();
    for theme in [DEFAULT_THEME, STUDIO_THEME] {
        let (_root, ui, _engine, controller) = make_fixture();
        ui.window()
            .set_size(slint::LogicalSize::new(1600.0, 1000.0));
        crate::theme::apply_theme(&ui, theme);
        ui.set_audio_ready(true);
        ui.show().unwrap();
        let slider = ElementQuery::from_root(&ui)
            .match_descendants()
            .match_accessible_role(AccessibleRole::Slider)
            .find_all()
            .into_iter()
            .find(|item| item.accessible_value_minimum() == Some(-60.0))
            .expect("active volume slider");
        let origin = slider.absolute_position();
        let size = slider.size();
        let position =
            LogicalPosition::new(origin.x + size.width * 0.8, origin.y + size.height / 2.0);
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        assert!(ui.get_volume_adjusting());
        let draft_db = ui.get_volume_db();
        controller.publish_confirmed_volume(&ui, 1.0);
        assert_eq!(
            ui.get_volume_db(),
            draft_db,
            "{theme}: an old snapshot replaced the drag draft"
        );
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
        assert!(
            ui.get_volume_adjusting(),
            "{theme}: slider must retain keyboard focus after release"
        );
        // Delay consumption of IPC A's exact completion until GUI B is admitted.
        // The channel retains A's result even if the engine has already applied B,
        // making this ordering regression deterministic without scheduling sleeps.
        let late_ipc = controller.enqueue_volume(0.1).unwrap();
        slider.set_accessible_value("-6");
        let latest = gain_from_db(-6.0);
        assert!((controller.playback.lock().unwrap().volume - latest).abs() < 0.000_01);
        let old = late_ipc.wait(Duration::from_secs(2)).unwrap();
        assert!((old.volume - 0.1).abs() < 0.000_01);
        assert!(
            (controller.playback.lock().unwrap().volume - latest).abs() < 0.000_01,
            "{theme}: late IPC completion replaced the newer admitted GUI level"
        );
        controller.publish_confirmed_volume(&ui, old.volume);
        assert!((ui.get_volume_db() + 6.0).abs() < 0.001);
        let confirmed = controller
            .audio
            .send_and_wait(AudioCommand::Synchronize, Duration::from_secs(2))
            .unwrap();
        assert!((confirmed.volume - latest).abs() < 0.000_01);
        controller.publish_confirmed_volume(&ui, confirmed.volume);
        assert!((ui.get_volume_db() + 6.0).abs() < 0.001);
        assert!(controller.enqueue_volume(f32::NAN).is_err());
        assert!((controller.playback.lock().unwrap().volume - latest).abs() < 0.000_01);
        let reply = controller.ipc(serde_json::json!({"command":"volume","volume":0.1}));
        assert_eq!(reply["success"], true, "{reply}");
        controller.publish_confirmed_volume(&ui, controller.audio.status().volume);
        assert!(
            (ui.get_volume_db() + 20.0).abs() < 0.001,
            "{theme}: acknowledged IPC level did not reach the focused slider"
        );
        assert_eq!(ui.get_volume_label().as_str(), "-20.0 dB");
        if theme == STUDIO_THEME {
            for label in ["Mute", "Unmute"] {
                ElementHandle::find_by_accessible_label(&ui, label)
                    .next()
                    .expect(label)
                    .invoke_accessible_default_action();
            }
            assert!(
                (controller.playback.lock().unwrap().volume - 0.1).abs() < 0.000_01,
                "unmute must restore the confirmed level"
            );
            let status = controller
                .audio
                .send_and_wait(AudioCommand::Stop, Duration::from_secs(2))
                .unwrap();
            assert!((status.volume - 0.1).abs() < 0.000_01);
        }
        controller.shutdown();
        ui.hide().unwrap();
    }
}
