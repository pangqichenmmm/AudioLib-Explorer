// app/audiolib-desktop/src/ui_scale_tests.rs
//! Headless software-rendered snapshots of synthetic UI states. With
//! `AUDIOLIB_UI_SCALE_SNAPSHOT_DIR` set, every rendered frame is also saved as
//! PNG so layouts can be compared pixel by pixel across revisions.
use crate::{AppWindow, AudioRow, FolderRow};
use i_slint_backend_testing::{TestingBackend, TestingBackendOptions};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

fn init_software_backend() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .expect("platform already initialized");
}

fn synthetic_rows() -> ModelRc<AudioRow> {
    let rows = (0..12)
        .map(|index| AudioRow {
            id: format!("{index}").into(),
            file_path: format!("C:/Fixture/Sounds/Synthetic {index:02}.wav").into(),
            name: format!("Synthetic tone {index:02}.wav").into(),
            folder: "Fixture".into(),
            duration: format!("0:0{}", index % 10).into(),
            format: "WAV 48 kHz 24-bit".into(),
            channels: "2".into(),
            tags: "fixture, tone".into(),
            favorite: index % 3 == 0,
            missing: index == 5,
            checked: index % 4 == 1,
            ..Default::default()
        })
        .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(rows))
}

fn synthetic_folders() -> ModelRc<FolderRow> {
    let folders = ["Fixture", "Ambience", "Foley", "Weapons"]
        .iter()
        .enumerate()
        .map(|(depth, name)| FolderRow {
            name: (*name).into(),
            count: "12".into(),
            path: format!("C:/Fixture/{name}").into(),
            depth: (depth.min(1)) as i32,
            expanded: true,
            has_children: depth == 0,
        })
        .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(folders))
}

type Setup = fn(&AppWindow);

const VIEWS: &[(&str, Setup)] = &[
    ("main", |_| {}),
    ("settings", |ui| ui.set_settings_visible(true)),
    ("appearance", |ui| {
        ui.set_settings_page(1);
        ui.set_settings_visible(true);
    }),
    ("search-settings", |ui| {
        ui.set_settings_page(2);
        ui.set_settings_visible(true);
    }),
    ("sync-settings", |ui| {
        ui.set_settings_page(3);
        ui.set_settings_visible(true);
    }),
    ("cache-settings", |ui| {
        ui.set_settings_page(4);
        ui.set_settings_visible(true);
    }),
    ("about", |ui| ui.set_about_visible(true)),
    ("help", |ui| ui.set_help_visible(true)),
    ("wide", |ui| ui.set_inspector_visible(false)),
];

fn render(scale: f32, chinese: bool, name: &str, setup: Setup) -> (u32, u32) {
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    if scale != 1.0 {
        ui.global::<crate::Theme>().set_ui_scale(scale);
    }
    ui.set_chinese(chinese);
    ui.set_rows(synthetic_rows());
    ui.set_folders(synthetic_folders());
    setup(&ui);
    ui.show().unwrap();
    let pixels = ui.window().take_snapshot().unwrap();
    assert!(pixels.width() > 0 && pixels.height() > 0);
    if let Some(directory) = std::env::var_os("AUDIOLIB_UI_SCALE_SNAPSHOT_DIR") {
        let directory = std::path::Path::new(&directory);
        std::fs::create_dir_all(directory).unwrap();
        let language = if chinese { "cn" } else { "en" };
        image::save_buffer(
            directory.join(format!("{name}-{language}-x{scale}.png")),
            pixels.as_bytes(),
            pixels.width(),
            pixels.height(),
            image::ColorType::Rgba8,
        )
        .unwrap();
    }
    ui.hide().unwrap();
    (pixels.width(), pixels.height())
}

#[test]
fn ui_scale_renders_without_panicking_at_1_0_1_5_and_2_0() {
    init_software_backend();
    for scale in [1.0, 1.5, 2.0] {
        for chinese in [false, true] {
            for (name, setup) in VIEWS {
                let (width, height) = render(scale, chinese, name, *setup);
                assert!(width >= 1440 && height >= 900, "{name} x{scale}");
            }
        }
    }
}

#[test]
fn table_column_widths_scale_proportionally_with_ui_scale() {
    init_software_backend();
    let total_at = |scale: f32| {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
        ui.global::<crate::Theme>().set_ui_scale(scale);
        ui.set_table_name_width(300.0);
        ui.set_table_tags_width(140.0);
        let total = ui.get_table_columns_total();
        // Stored widths stay logical: scaling must never rewrite them.
        assert_eq!(ui.get_table_name_width(), 300.0);
        assert_eq!(ui.get_table_tags_width(), 140.0);
        total
    };
    let base = total_at(1.0);
    assert!(base > 0.0);
    for scale in [1.25_f32, 1.5, 2.0] {
        let total = total_at(scale);
        assert!(
            (total - base * scale).abs() < 0.01,
            "columns {total} at x{scale}, expected {}",
            base * scale
        );
    }
}

fn save_snapshot(ui: &AppWindow, file: &str) {
    let pixels = ui.window().take_snapshot().unwrap();
    if let Some(directory) = std::env::var_os("AUDIOLIB_UI_SCALE_SNAPSHOT_DIR") {
        let directory = std::path::Path::new(&directory);
        std::fs::create_dir_all(directory).unwrap();
        image::save_buffer(
            directory.join(file),
            pixels.as_bytes(),
            pixels.width(),
            pixels.height(),
            image::ColorType::Rgba8,
        )
        .unwrap();
    }
}

fn long_remote_source_label(chinese: bool) -> &'static str {
    if chinese {
        "服务器模式 · 影视制作音效资料库 — 当前项目使用的远端扩展拟音合集 · 音效库：木门、机械装置与室内环境素材 · 同步状态：已更新完整资料与目录，可继续检索与试听 · 较长的无空格服务器标识：sound-archive-production-foley-current-project.example.test:8791"
    } else {
        "Server mode · Production archive — Extended Foley collection for the current project · Library: doors, mechanisms and interiors · Synchronization: complete metadata and directories, ready for search and audition · Long server identifier: sound-archive-production-foley-current-project.example.test:8791"
    }
}

fn remote_toolbar_window(
    theme: &str,
    chinese: bool,
    width: f32,
    height: f32,
    scale: f32,
) -> AppWindow {
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(width, height));
    crate::theme::apply_theme(&ui, theme);
    ui.global::<crate::Theme>().set_ui_scale(scale);
    ui.set_chinese(chinese);
    ui.set_remote_mode(true);
    ui.set_data_source_label(long_remote_source_label(chinese).into());
    ui.set_query("metal door".into());
    ui.set_search_scope_index(1);
    ui.set_rows(synthetic_rows());
    ui.set_folders(synthetic_folders());
    ui.show().unwrap();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    ui
}

#[test]
fn source_label_hover_reveals_full_remote_status_without_changing_search() {
    use i_slint_backend_testing::{AccessibleRole, ElementHandle};
    use slint::platform::WindowEvent;
    use std::time::Duration;

    init_software_backend();
    for theme in audiolib_core::config::THEME_IDS {
        for chinese in [false, true] {
            let ui = remote_toolbar_window(theme, chinese, 1080.0, 700.0, 1.0);
            ui.on_search_requested(|_| panic!("Source hover must not request a search"));
            ui.on_search_scope_changed(|| panic!("Source hover must not change search scope"));
            ui.on_sm_open_requested(|_| panic!("Source hover must not open source management"));
            ui.on_library_action(|_| panic!("Source hover must not open library management"));
            let label = long_remote_source_label(chinese);
            let source = ElementHandle::find_by_accessible_label(&ui, label)
                .find(|element| element.accessible_role() == Some(AccessibleRole::Button))
                .expect("current remote source control");
            let language = if chinese { "cn" } else { "en" };
            save_snapshot(&ui, &format!("source-before-hover-{theme}-{language}.png"));
            let before = ElementHandle::find_by_accessible_label(&ui, label).count();
            let (origin, size) = (source.absolute_position(), source.size());
            ui.window().dispatch_event(WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(
                    origin.x + size.width / 2.0,
                    origin.y + size.height / 2.0,
                ),
            });
            i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(600));
            i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(10));
            save_snapshot(&ui, &format!("source-after-hover-{theme}-{language}.png"));

            // The badge already exposes this label. A newly visible copy proves
            // that the tooltip also exposes the complete, unabridged summary.
            assert!(
                ElementHandle::find_by_accessible_label(&ui, label).count() > before,
                "source tooltip must expose the complete summary: {theme}/{language}"
            );
            assert_eq!(ui.get_data_source_label(), label);
            assert_eq!(ui.get_query(), "metal door");
            assert_eq!(ui.get_search_scope_index(), 1);
            assert!(!ui.get_library_visible());
            ui.hide().unwrap();
        }
    }
}

#[test]
fn search_and_library_actions_remain_reachable_with_long_source_labels() {
    use i_slint_backend_testing::{AccessibleRole, ElementHandle};
    use slint::platform::PointerEventButton;
    use std::{cell::RefCell, rc::Rc};

    init_software_backend();
    for theme in audiolib_core::config::THEME_IDS {
        for chinese in [false, true] {
            for (layout, width, height) in [("compact", 1080.0, 700.0), ("wide", 1440.0, 900.0)] {
                for scale in [1.0, 1.5, 2.0] {
                    let ui = remote_toolbar_window(theme, chinese, width, height, scale);
                    let language = if chinese { "cn" } else { "en" };
                    save_snapshot(
                        &ui,
                        &format!("search-source-{theme}-{language}-{layout}-x{scale}.png"),
                    );
                    // Larger scales are inspected in the snapshots; exercise
                    // pointer routing at the application's minimum window size.
                    if layout == "compact" && scale == 1.0 {
                        let searches = Rc::new(RefCell::new(Vec::new()));
                        let observed = searches.clone();
                        ui.on_search_requested(move |explicit| {
                            observed.borrow_mut().push(explicit);
                        });
                        ui.on_search_scope_changed(|| {
                            panic!("Toolbar actions must retain the search scope")
                        });
                        let managers = Rc::new(RefCell::new(Vec::new()));
                        let observed = managers.clone();
                        ui.on_sm_open_requested(move |page| observed.borrow_mut().push(page));
                        let libraries = Rc::new(RefCell::new(Vec::new()));
                        let observed = libraries.clone();
                        ui.on_library_action(move |action| observed.borrow_mut().push(action));
                        let search_label =
                            match (ui.global::<crate::Theme>().get_modern_style(), chinese) {
                                (false, false) => "Search",
                                (false, true) => "搜索",
                                (true, false) => "Run search",
                                (true, true) => "执行搜索",
                            };
                        ElementHandle::find_by_accessible_label(&ui, search_label)
                            .find(|element| {
                                element.accessible_role() == Some(AccessibleRole::Button)
                            })
                            .expect("visible search action")
                            .mock_single_click(PointerEventButton::Left);
                        assert_eq!(&*searches.borrow(), &[true]);
                        assert!(managers.borrow().is_empty());
                        ElementHandle::find_by_accessible_label(
                            &ui,
                            if chinese {
                                "管理音效库"
                            } else {
                                "Manage library"
                            },
                        )
                        .find(|element| element.accessible_role() == Some(AccessibleRole::Button))
                        .expect("visible library management action")
                        .mock_single_click(PointerEventButton::Left);
                        assert_eq!(&*managers.borrow(), &[1]);
                        assert_eq!(&*libraries.borrow(), &[0]);
                        assert!(ui.get_library_visible());
                        assert_eq!(ui.get_query(), "metal door");
                        assert_eq!(ui.get_search_scope_index(), 1);
                    }
                    ui.hide().unwrap();
                }
            }
        }
    }
}

#[test]
fn wide_table_at_1_5_scrolls_horizontally_to_the_last_column() {
    init_software_backend();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.global::<crate::Theme>().set_ui_scale(1.5);
    ui.set_rows(synthetic_rows());
    ui.set_folders(synthetic_folders());
    ui.show().unwrap();
    save_snapshot(&ui, "main-en-x1.5-scroll0.png");
    let content = ui.get_table_columns_total();
    let visible = ui.get_table_visible_width();
    assert_eq!(ui.get_table_scroll_x(), 0.0);
    assert!(
        content > visible,
        "content {content} should exceed viewport {visible}"
    );
    ui.invoke_scroll_table_to_end();
    let scroll = ui.get_table_scroll_x();
    assert!(scroll < 0.0, "table did not scroll: {scroll}");
    let right_edge = content + scroll;
    assert!(
        (right_edge - visible).abs() < 0.5,
        "last column right edge {right_edge} vs viewport {visible}"
    );
    save_snapshot(&ui, "main-en-x1.5-scrollmax.png");
    ui.hide().unwrap();
}

#[test]
fn appearance_scale_choices_map_to_theme_ui_scale() {
    use crate::controller::{apply_ui_scale, ui_scale_index, ui_scale_percent};
    init_software_backend();
    let ui = AppWindow::new().unwrap();
    for (index, percent, scale) in [
        (0, 0, 1.0_f32),
        (1, 100, 1.0),
        (2, 125, 1.25),
        (3, 150, 1.5),
        (4, 175, 1.75),
        (5, 200, 2.0),
    ] {
        assert_eq!(ui_scale_percent(index), percent);
        assert_eq!(ui_scale_index(percent), index);
        apply_ui_scale(&ui, percent);
        assert_eq!(ui.get_ui_scale_index(), index);
        assert_eq!(ui.global::<crate::Theme>().get_ui_scale(), scale);
    }
    assert_eq!(ui_scale_percent(-1), 0);
    assert_eq!(ui_scale_percent(6), 0);
    apply_ui_scale(&ui, 130);
    assert_eq!(ui.get_ui_scale_index(), 0);
    assert_eq!(ui.global::<crate::Theme>().get_ui_scale(), 1.0);
}

fn small_window_at_2_0(setup: Setup) -> AppWindow {
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
    ui.global::<crate::Theme>().set_ui_scale(2.0);
    ui.set_rows(synthetic_rows());
    ui.set_folders(synthetic_folders());
    setup(&ui);
    ui.show().unwrap();
    ui
}

#[test]
fn main_view_at_2_0_in_1080x700_scrolls_to_its_last_control() {
    init_software_backend();
    for chinese in [false, true] {
        let ui = small_window_at_2_0(|_| {});
        ui.set_chinese(chinese);
        let language = if chinese { "cn" } else { "en" };
        save_snapshot(&ui, &format!("main-{language}-x2-1080x700-scroll0.png"));
        // The content is larger than the window, so it must scroll rather than be cut off.
        assert!(ui.get_main_viewport_height() > 700.0);
        assert!(ui.get_main_viewport_width() > 1080.0);
        assert!(ui.get_main_last_bottom() > 700.0);
        assert_eq!(ui.get_results_panel_min_height(), 480.0);
        assert!(
            ui.get_results_panel_height() >= 480.0,
            "sound list squeezed to {}",
            ui.get_results_panel_height()
        );
        ui.invoke_scroll_main_to_end();
        save_snapshot(&ui, &format!("main-{language}-x2-1080x700-scrollmax.png"));
        let bottom = ui.get_main_last_bottom();
        let right = ui.get_main_last_right();
        assert!(
            bottom <= 700.0 && bottom > 600.0,
            "status row bottom {bottom}"
        );
        assert!(right <= 1080.0 && right > 980.0, "status row right {right}");
        ui.hide().unwrap();
    }
}

#[test]
fn settings_pages_at_2_0_in_1080x700_scroll_to_their_last_controls() {
    init_software_backend();
    for page in 0..5 {
        for chinese in [false, true] {
            let ui = small_window_at_2_0(|ui| ui.set_settings_visible(true));
            ui.set_settings_page(page);
            ui.set_chinese(chinese);
            let name = ["audio", "general", "search", "sync", "cache"][page as usize];
            let language = if chinese { "cn" } else { "en" };
            save_snapshot(&ui, &format!("{name}-{language}-x2-1080x700-scroll0.png"));
            let top = ui.get_settings_panel_top();
            let panel_bottom = ui.get_settings_panel_bottom();
            assert!(
                top >= 0.0 && panel_bottom <= 700.0,
                "panel {top}..{panel_bottom}"
            );
            ui.invoke_scroll_main_to_end();
            save_snapshot(&ui, &format!("{name}-{language}-x2-1080x700-scrollmax.png"));
            let last = ui.get_settings_last_bottom();
            assert!(
                last <= panel_bottom && last > top,
                "{name}: last {last}, panel {top}..{panel_bottom}"
            );
            ui.hide().unwrap();
        }
    }
}

#[test]
fn tab_never_reaches_controls_on_the_hidden_settings_page() {
    use slint::platform::{Key, WindowEvent};
    init_software_backend();
    for page in 0..5 {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
        ui.set_settings_visible(true);
        ui.set_settings_page(page);
        ui.show().unwrap();
        ui.invoke_focus_settings_first();
        assert!(ui.get_settings_visible_page_focused());
        for key in [Key::Tab, Key::Backtab] {
            for step in 0..30 {
                ui.window()
                    .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
                ui.window()
                    .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
                assert!(
                    !ui.get_settings_hidden_page_focused(),
                    "page {page}: {key:?} step {step} focused a hidden control"
                );
            }
        }
        ui.hide().unwrap();
    }
}

#[test]
fn list_font_size_changes_only_list_rows_and_row_height() {
    use crate::theme::apply_font_sizes;
    init_software_backend();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
    ui.set_rows(synthetic_rows());
    let theme = ui.global::<crate::Theme>();
    // Defaults (13 px) keep the design sizes.
    apply_font_sizes(&ui, 13, 13);
    assert_eq!(theme.get_ui_font_size(), 12.0);
    assert_eq!(theme.get_list_font_size(), 12.0);
    assert_eq!(ui.get_result_row_height_px(), 22.0);
    // A larger list font enlarges list text and rows only.
    apply_font_sizes(&ui, 13, 20);
    assert_eq!(ui.get_list_font_px(), 20);
    assert_eq!(theme.get_list_font_size(), 18.0); // round(12 * 20 / 13)
    assert_eq!(theme.get_ui_font_size(), 12.0);
    assert_eq!(ui.get_result_row_height_px(), 34.0); // round(22 * 20 / 13)
    // A smaller list font never shrinks the rows below the design height.
    apply_font_sizes(&ui, 13, 10);
    assert_eq!(theme.get_list_font_size(), 9.0);
    assert_eq!(ui.get_result_row_height_px(), 22.0);
    // The interface font does not touch list rows.
    apply_font_sizes(&ui, 24, 13);
    assert_eq!(theme.get_ui_font_size(), 22.0); // round(12 * 24 / 13)
    assert_eq!(theme.get_list_font_size(), 12.0);
    assert_eq!(ui.get_result_row_height_px(), 22.0);
    // Font sizes combine with the interface scale in whole pixels; out of range clamps.
    theme.set_ui_scale(1.5);
    apply_font_sizes(&ui, 0, 99);
    assert_eq!((ui.get_ui_font_px(), ui.get_list_font_px()), (10, 24));
    assert_eq!(theme.get_ui_font_size(), 14.0); // round(12 * 1.5 * 10 / 13)
    assert_eq!(theme.get_list_font_size(), 33.0); // round(12 * 1.5 * 24 / 13)
    assert_eq!(ui.get_result_row_height_px(), 61.0); // round(22 * 1.5 * 24 / 13)
    ui.show().unwrap();
    save_snapshot(&ui, "list-font-24-x1.5-1080x700.png");
    ui.hide().unwrap();
}

#[test]
fn default_theme_applied_from_rust_is_pixel_identical() {
    use crate::theme::{apply_font_sizes, apply_theme, theme_id, theme_index};
    init_software_backend();
    assert_eq!(theme_index("default"), 0);
    assert_eq!(theme_index("unknown"), 0);
    assert_eq!(theme_id(0), "default");
    assert_eq!(theme_id(7), "default");
    let shot = |apply: bool| {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
        ui.set_rows(synthetic_rows());
        ui.set_folders(synthetic_folders());
        if apply {
            apply_theme(&ui, "default");
            apply_font_sizes(&ui, 13, 13);
            assert_eq!(ui.get_theme_index(), 0);
        }
        ui.show().unwrap();
        let pixels = ui.window().take_snapshot().unwrap();
        ui.hide().unwrap();
        pixels.as_bytes().to_vec()
    };
    assert!(
        shot(false) == shot(true),
        "default theme changed the rendering"
    );
}

#[test]
fn theme_registry_round_trips_and_lists_both_languages() {
    use crate::theme::{apply_theme, theme_id, theme_index};
    use audiolib_core::config::{
        BUILTIN_THEMES, DEFAULT_THEME, PlayerStyle, THEME_IDS, WorkbenchStyle,
    };

    init_software_backend();
    let ui = AppWindow::new().unwrap();
    for (index, id) in THEME_IDS.iter().enumerate() {
        assert_eq!(theme_id(index as i32), *id);
        assert_eq!(theme_index(id), index as i32);
        apply_theme(&ui, id);
        assert_eq!(ui.get_theme_index(), index as i32);
        assert_eq!(
            ui.global::<crate::Theme>().get_modern_style(),
            BUILTIN_THEMES[index].workbench_style == WorkbenchStyle::Studio
        );
        assert_eq!(
            ui.global::<crate::Theme>().get_modern_player(),
            BUILTIN_THEMES[index].player_style == PlayerStyle::Studio
        );
        let english = ui.get_theme_names();
        let chinese = ui.get_theme_names_cn();
        assert_eq!(english.row_count(), THEME_IDS.len());
        assert_eq!(chinese.row_count(), THEME_IDS.len());
        for position in 0..THEME_IDS.len() {
            assert!(!english.row_data(position).unwrap().is_empty());
            assert!(!chinese.row_data(position).unwrap().is_empty());
            assert_ne!(english.row_data(position), chinese.row_data(position));
        }
    }
    for index in [-1, THEME_IDS.len() as i32, i32::MAX] {
        assert_eq!(theme_id(index), DEFAULT_THEME);
    }
    apply_theme(&ui, "removed-theme");
    assert_eq!(ui.get_theme_index(), theme_index(DEFAULT_THEME));
    assert!(!ui.global::<crate::Theme>().get_modern_style());
    assert!(!ui.global::<crate::Theme>().get_modern_player());
}

#[test]
fn theme_switch_preserves_transport_view_and_typography() {
    use crate::theme::{apply_font_sizes, apply_theme};
    use audiolib_core::config::{DEFAULT_THEME, DEFAULT_V2_THEME, STUDIO_THEME};

    init_software_backend();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_rows(synthetic_rows());
    ui.set_folders(synthetic_folders());
    ui.set_has_selection(true);
    ui.set_selected_id("42".into());
    ui.set_query("rain in forest".into());
    ui.set_edit_tags("rain, draft".into());
    ui.set_edit_notes("Unsaved Metadata note".into());
    ui.set_metadata_dirty(true);
    ui.set_audio_ready(true);
    ui.set_playing(false);
    ui.set_playback_state(4);
    ui.set_playback_name("Synthetic paused sound.wav".into());
    ui.set_play_position(12.5);
    ui.set_play_duration(30.0);
    ui.set_view_start(7.5);
    ui.set_view_end(15.0);
    ui.set_wave_target_start(7.5);
    ui.set_wave_target_end(15.0);
    ui.set_wave_zoom(4.0);
    ui.set_wave_scroll(0.25);
    ui.set_pitch_text("3.00".into());
    ui.set_speed_text("0.80".into());
    ui.set_audition_mono(true);
    ui.set_audition_repeat(true);
    ui.set_volume_db(-12.0);
    apply_font_sizes(&ui, 15, 17);
    let theme = ui.global::<crate::Theme>();
    theme.set_ui_scale(1.25);
    theme.set_motion(0.0);
    ui.show().unwrap();

    let mut frames = Vec::new();
    for id in [
        DEFAULT_THEME,
        STUDIO_THEME,
        DEFAULT_V2_THEME,
        STUDIO_THEME,
        DEFAULT_THEME,
    ] {
        apply_theme(&ui, id);
        assert_eq!(ui.get_selected_id().as_str(), "42");
        assert_eq!(ui.get_query(), "rain in forest");
        assert_eq!(ui.get_edit_tags(), "rain, draft");
        assert_eq!(ui.get_edit_notes(), "Unsaved Metadata note");
        assert!(ui.get_metadata_dirty());
        assert!(ui.get_audio_ready());
        assert!(!ui.get_playing());
        assert_eq!(ui.get_playback_state(), 4);
        assert_eq!(ui.get_play_position(), 12.5);
        assert_eq!(ui.get_play_duration(), 30.0);
        assert_eq!((ui.get_view_start(), ui.get_view_end()), (7.5, 15.0));
        assert_eq!(
            (ui.get_wave_target_start(), ui.get_wave_target_end()),
            (7.5, 15.0)
        );
        assert_eq!((ui.get_wave_zoom(), ui.get_wave_scroll()), (4.0, 0.25));
        assert_eq!(ui.get_pitch_text().as_str(), "3.00");
        assert_eq!(ui.get_speed_text().as_str(), "0.80");
        assert!(ui.get_audition_mono() && ui.get_audition_repeat());
        assert_eq!(ui.get_volume_db(), -12.0);
        assert_eq!((ui.get_ui_font_px(), ui.get_list_font_px()), (15, 17));
        assert_eq!(theme.get_ui_scale(), 1.25);
        frames.push(ui.window().take_snapshot().unwrap().as_bytes().to_vec());
        save_snapshot(&ui, &format!("theme-switch-{id}.png"));
    }
    assert!(
        frames[0] != frames[1],
        "The theme must switch the player presentation"
    );
    assert!(
        frames[0] == frames[4],
        "Switching back must restore the classic presentation"
    );
    ui.hide().unwrap();
}

#[test]
fn theme_switch_does_not_replay_expired_or_classic_input_rejections() {
    use crate::theme::apply_theme;
    use audiolib_core::config::{DEFAULT_THEME, DEFAULT_V2_THEME, STUDIO_THEME};
    use std::time::Duration;

    init_software_backend();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_audio_ready(true);
    ui.set_playback_state(2);
    ui.set_waveform_ready(true); // Keep the loading shimmer out of field comparisons.
    ui.global::<crate::Theme>().set_motion(0.0);
    apply_theme(&ui, STUDIO_THEME);
    ui.show().unwrap();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(2));
    let frame = || {
        i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
        ui.window().take_snapshot().unwrap().as_bytes().to_vec()
    };
    let baseline = frame();
    save_snapshot(&ui, "theme-feedback-baseline.png");

    // The controller restores valid values before sending this rejection event.
    // Confirm the current presenter still shows a newly rejected input.
    ui.invoke_audition_input_rejected(1);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    assert!(frame() != baseline, "a new rejection must remain visible");
    save_snapshot(&ui, "theme-feedback-active.png");
    i_slint_backend_testing::mock_elapsed_time(Duration::from_secs(2));
    let expired = frame();
    save_snapshot(&ui, "theme-feedback-expired.png");
    assert!(expired == baseline, "the rejection feedback must expire");

    for id in [DEFAULT_THEME, DEFAULT_V2_THEME] {
        apply_theme(&ui, id);
        let _classic = frame(); // Commit destruction before selecting Studio again.
        apply_theme(&ui, STUDIO_THEME);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
        save_snapshot(&ui, &format!("theme-feedback-restored-from-{id}.png"));
        assert!(
            frame() == baseline,
            "switching from {id} must not replay an expired event"
        );

        apply_theme(&ui, id);
        let _classic = frame();
        ui.invoke_audition_input_rejected(3);
        apply_theme(&ui, STUDIO_THEME);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
        assert!(
            frame() == baseline,
            "a classic event from {id} must not reach a new Studio"
        );
    }

    // Repeated events in the selected Studio still deliver after its reset.
    ui.invoke_audition_input_rejected(2);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    assert!(frame() != baseline, "new events must work after switching");
    ui.hide().unwrap();
}

#[test]
fn view_theme_menu_keyboard_selection_applies_the_requested_style() {
    use crate::theme::{apply_theme, theme_id, theme_index};
    use crate::ui_tests::{open_menu, send_keys};
    use audiolib_core::config::{DEFAULT_THEME, DEFAULT_V2_THEME, STUDIO_THEME};
    use slint::platform::Key;
    use std::{cell::Cell, rc::Rc};

    init_software_backend();
    for chinese in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
        ui.set_chinese(chinese);
        apply_theme(&ui, DEFAULT_THEME);
        let requests = Rc::new(Cell::new(0));
        ui.on_theme_requested({
            let weak = ui.as_weak();
            let requests = requests.clone();
            move |index| {
                requests.set(requests.get() + 1);
                apply_theme(&weak.upgrade().unwrap(), &theme_id(index));
            }
        });
        ui.show().unwrap();
        let language = if chinese { "cn" } else { "en" };
        for (step, id) in [STUDIO_THEME, DEFAULT_V2_THEME, DEFAULT_THEME]
            .into_iter()
            .enumerate()
        {
            open_menu(&ui, if chinese { "视图" } else { "View" });
            // Theme is the first View command; Right enters its registered choices.
            send_keys(&ui, &[Key::DownArrow.into()]);
            send_keys(&ui, &[Key::RightArrow.into()]);
            for _ in 0..=theme_index(id) {
                send_keys(&ui, &[Key::DownArrow.into()]);
            }
            send_keys(&ui, &[Key::Return.into()]);
            assert_eq!(requests.get(), step + 1);
            assert_eq!(ui.get_theme_index(), theme_index(id));
            assert_eq!(
                ui.global::<crate::Theme>().get_modern_style(),
                id != DEFAULT_THEME
            );
            assert_eq!(
                ui.global::<crate::Theme>().get_modern_player(),
                id == STUDIO_THEME
            );
            assert!(
                !ui.get_settings_visible(),
                "theme switching needs no dialog"
            );
            save_snapshot(&ui, &format!("menu-theme-{id}-{language}.png"));
        }
        ui.hide().unwrap();
    }
}

/// Render synthetic transport phases through the same controller publishers as
/// the application. No decoder, device, filesystem source or library is opened.
#[test]
fn player_bar_states_render_in_both_languages_and_small_scaled_windows() {
    use crate::controller::{publish_playback_progress, publish_player_state, publish_wave_target};
    use crate::waveform_view::{WaveView, render_waveform};
    use audiolib_audio::{AudioBackend, PlaybackState, PlaybackStatus, WaveformData, WaveformPeak};
    use audiolib_core::config::THEME_IDS;
    use std::time::Duration;

    init_software_backend();
    let peaks = (0..2)
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
        .collect();
    let waveform = WaveformData {
        sample_rate: 48_000,
        channels: 2,
        frames: 1_450_080,
        samples_per_pixel: 1417,
        peaks,
    };
    let pixels = render_waveform(&waveform, 1024, 120, WaveView::default());
    let waveform_image = slint::Image::from_rgba8(
        slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&pixels, 1024, 120),
    );
    let cases = [
        ("loading", PlaybackState::Loading),
        ("analyzing", PlaybackState::Ready),
        ("partial", PlaybackState::Ready),
        ("error", PlaybackState::Error),
        ("missing", PlaybackState::Ready),
        ("playing-zoomed", PlaybackState::Playing),
        ("paused", PlaybackState::Paused),
        ("ended", PlaybackState::Ended),
        ("buffering", PlaybackState::Playing),
        ("maintenance", PlaybackState::Ready),
    ];
    for (layout, width, height, scale) in [
        ("wide", 1440.0, 900.0, 1.0),
        ("small-scaled", 1080.0, 700.0, 2.0),
    ] {
        for (id, chinese) in THEME_IDS.iter().flat_map(|id| [(*id, false), (*id, true)]) {
            for (name, state) in cases {
                let ui = AppWindow::new().unwrap();
                ui.window().set_size(slint::LogicalSize::new(width, height));
                crate::theme::apply_theme(&ui, id);
                ui.global::<crate::Theme>().set_ui_scale(scale);
                ui.global::<crate::Theme>().set_motion(0.0);
                ui.set_chinese(chinese);
                ui.set_rows(synthetic_rows());
                ui.set_folders(synthetic_folders());
                ui.set_has_selection(true);
                ui.set_selected_row(0);
                ui.set_selected_id("42".into());
                ui.set_selected_missing(name == "missing");
                ui.set_library_busy(name == "maintenance");
                ui.set_playback_name("Synthetic engine pass · 合成引擎掠过.wav".into());
                ui.set_waveform_info("2 ch · 48 kHz · 0:30.210".into());
                ui.set_output_summary("Synthetic output · 48 kHz · 256 frames".into());
                let ready = !matches!(state, PlaybackState::Loading | PlaybackState::Error)
                    && name != "missing";
                let waveform_ready = ready && name != "analyzing";
                ui.set_audio_ready(ready);
                ui.set_waveform_ready(waveform_ready);
                ui.set_waveform_progress(if name == "partial" { 0.35 } else { 1.0 });
                if waveform_ready {
                    ui.set_waveform_image(waveform_image.clone());
                }
                let status = PlaybackStatus {
                    state,
                    duration: 30.21,
                    position: if state == PlaybackState::Ended {
                        30.21
                    } else {
                        12.45
                    },
                    error: (state == PlaybackState::Error)
                        .then(|| "Synthetic decoder error: unsupported WAV encoding".to_string()),
                    active_backend: ready.then_some(AudioBackend::Wasapi),
                    device_sample_rate: if ready { 48_000 } else { 0 },
                    underruns: u64::from(name == "buffering") * 3,
                    ..Default::default()
                };
                let view = WaveView {
                    zoom: if name == "playing-zoomed" || name == "buffering" {
                        4.0
                    } else {
                        1.0
                    },
                    scroll: 0.35,
                    ..Default::default()
                };
                let (start, end) = view.range(status.duration);
                ui.set_view_start(start as f32);
                ui.set_view_end(end as f32);
                publish_wave_target(&ui, view, status.duration);
                publish_playback_progress(&ui, &status, false);
                publish_player_state(&ui, &status, name == "buffering");
                ui.show().unwrap();
                i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(500));
                // The scaled app uses its existing main viewport scrolling.
                // Capture its player after scrolling, rather than a clipped top view.
                if scale > 1.0 {
                    ui.invoke_scroll_main_to_end();
                }
                let language = if chinese { "cn" } else { "en" };
                save_snapshot(&ui, &format!("player-{id}-{name}-{language}-{layout}.png"));
                ui.hide().unwrap();
            }
        }
    }
}
