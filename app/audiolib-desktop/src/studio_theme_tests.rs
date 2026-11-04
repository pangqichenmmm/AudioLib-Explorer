// app/audiolib-desktop/src/studio_theme_tests.rs
//! Studio dock regressions use synthetic images and an isolated software backend.
use crate::{AppWindow, AudioRow, FolderRow, theme::apply_theme};
use audiolib_core::config::{DEFAULT_THEME, STUDIO_THEME};
use i_slint_backend_testing::{ElementHandle, TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
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
    for id in [STUDIO_THEME, DEFAULT_THEME, STUDIO_THEME, DEFAULT_THEME] {
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
        if id == STUDIO_THEME {
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
