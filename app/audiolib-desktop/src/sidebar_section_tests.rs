// app/audiolib-desktop/src/sidebar_section_tests.rs
//! Input regressions for independent sidebar disclosure and adjustable list heights.
use crate::{AlbumRow, AppWindow, FolderRow};
use i_slint_backend_testing::{ElementHandle, TestingBackend, TestingBackendOptions};
use slint::{
    ComponentHandle, ModelRc, VecModel, platform::PointerEventButton, platform::WindowEvent,
};
use std::{cell::Cell, rc::Rc, time::Duration};

#[test]
fn software_rendered_chevrons_follow_each_section_disclosure_state() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    for scale in [1.0, 1.5] {
        let ui = window(false, 1440.0 * scale, 900.0 * scale, scale);
        for libraries in [false, true] {
            let (width, height) = chevron_ink_extent(&ui, libraries);
            assert!(width > height, "Expanded chevron must point down");
        }
        save_sidebar_snapshot(&ui, &format!("sections-expanded-x{scale}.png"));

        // Accessibility activation keeps pointer/focus styling unchanged so the
        // rendered direction, rather than a hover or focus color, is verified.
        header(&ui, false).invoke_accessible_default_action();
        settle();
        let (width, height) = chevron_ink_extent(&ui, false);
        assert!(height > width, "Collapsed folder chevron must point right");
        let (width, height) = chevron_ink_extent(&ui, true);
        assert!(width > height, "Library chevron must remain expanded");

        header(&ui, true).invoke_accessible_default_action();
        settle();
        for libraries in [false, true] {
            let (width, height) = chevron_ink_extent(&ui, libraries);
            assert!(height > width, "Collapsed chevron must point right");
        }
        save_sidebar_snapshot(&ui, &format!("sections-collapsed-x{scale}.png"));
        for libraries in [false, true] {
            header(&ui, libraries).invoke_accessible_default_action();
            settle();
            let (width, height) = chevron_ink_extent(&ui, libraries);
            assert!(width > height, "Reopened chevron must point down");
        }
        ui.global::<crate::Theme>().set_ui_scale(1.0);
        ui.hide().unwrap();
    }
}

#[test]
fn sections_collapse_independently_and_restore_their_shared_height_without_searching() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = window(chinese, 1440.0, 900.0, 1.0);
        ui.set_selected_folder(0);
        ui.set_selected_folder_path("C:/Fixture/Folder".into());
        ui.set_album_filter_id("71".into());
        let saves = count_layout_changes(&ui);
        let searches = Rc::new(Cell::new(0));
        let counted = searches.clone();
        ui.on_folder_requested(move |_| counted.set(counted.get() + 1));
        let counted = searches.clone();
        ui.on_album_selected(move |_| counted.set(counted.get() + 1));
        let counted = searches.clone();
        ui.on_filter_requested(move || counted.set(counted.get() + 1));
        let folders = list_height(&ui, false);
        let albums = list_height(&ui, true);
        let ratio = ui.get_folders_section_ratio();

        click(&ui, &header(&ui, false));
        assert!(!ui.get_folders_section_expanded());
        assert_eq!(header(&ui, false).accessible_checked(), Some(false));
        near(list_height(&ui, false), 0.0);
        assert!(list_height(&ui, true) > albums + folders - 2.0);

        click(&ui, &header(&ui, true));
        assert!(!ui.get_libraries_section_expanded());
        near(list_height(&ui, false), 0.0);
        near(list_height(&ui, true), 0.0);
        for libraries in [false, true] {
            assert!(header(&ui, libraries).size().height > 0.0);
        }

        header(&ui, false).invoke_accessible_default_action();
        settle();
        assert!(ui.get_folders_section_expanded());
        assert!(list_height(&ui, false) > folders + albums - 2.0);
        near(list_height(&ui, true), 0.0);
        click(&ui, &header(&ui, true));
        near(list_height(&ui, false), folders);
        near(list_height(&ui, true), albums);
        assert_eq!(ui.get_folders_section_ratio(), ratio);
        assert_eq!(header(&ui, true).accessible_checked(), Some(true));
        assert_eq!(saves.get(), 4);
        assert_eq!(searches.get(), 0);
        assert_eq!(ui.get_selected_folder(), 0);
        assert_eq!(ui.get_selected_folder_path().as_str(), "C:/Fixture/Folder");
        assert_eq!(ui.get_album_filter_id().as_str(), "71");
        ui.hide().unwrap();
    }
}

#[test]
fn vertical_drag_and_arrow_keys_resize_in_visual_direction_and_save_only_finished_changes() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = window(chinese, 1440.0, 1100.0, 1.0);
        let saves = count_layout_changes(&ui);
        let folders = list_height(&ui, false);
        let albums = list_height(&ui, true);
        let start = midpoint(&divider(&ui));
        pointer(&ui, start, true);
        move_vertical(&ui, start, 48.0);
        near(list_height(&ui, false), folders + 48.0);
        near(list_height(&ui, true), albums - 48.0);
        assert_eq!(saves.get(), 0);
        pointer(&ui, offset(start, 48.0), false);
        assert_eq!(saves.get(), 1);

        let adjusted = list_height(&ui, false);
        key(&ui, slint::platform::Key::UpArrow.into());
        near(list_height(&ui, false), adjusted - 16.0);
        key(&ui, slint::platform::Key::DownArrow.into());
        near(list_height(&ui, false), adjusted);
        assert_eq!(saves.get(), 3);

        drag(&ui, 10000.0);
        assert!(ui.get_folders_section_ratio() <= 0.9);
        assert!(list_height(&ui, true) >= 60.0 - 1.0);
        drag(&ui, -10000.0);
        assert!(ui.get_folders_section_ratio() >= 0.1);
        assert!(list_height(&ui, false) >= 68.0 - 1.0);
        ui.hide().unwrap();
    }
}

#[test]
fn section_header_keyboard_activation_and_modal_boundaries_preserve_layout() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = window(false, 1440.0, 900.0, 1.0);
    click(&ui, &header(&ui, false));
    assert!(!ui.get_folders_section_expanded());
    key(&ui, " ".into());
    assert!(ui.get_folders_section_expanded());
    key(&ui, slint::platform::Key::Return.into());
    assert!(!ui.get_folders_section_expanded());
    click(&ui, &header(&ui, false));

    let ratio = ui.get_folders_section_ratio();
    let start = midpoint(&divider(&ui));
    pointer(&ui, start, true);
    ui.set_library_busy(true);
    settle();
    assert_eq!(divider(&ui).accessible_enabled(), Some(false));
    for libraries in [false, true] {
        assert_eq!(header(&ui, libraries).accessible_enabled(), Some(false));
        header(&ui, libraries).invoke_accessible_default_action();
        click(&ui, &header(&ui, libraries));
    }
    key(&ui, slint::platform::Key::DownArrow.into());
    move_vertical(&ui, start, 48.0);
    assert_eq!(ui.get_folders_section_ratio(), ratio);
    assert!(ui.get_folders_section_expanded());
    assert!(ui.get_libraries_section_expanded());
    ui.set_library_busy(false);
    move_vertical(&ui, start, 64.0);
    assert_eq!(ui.get_folders_section_ratio(), ratio);
    pointer(&ui, offset(start, 64.0), false);
    drag(&ui, 16.0);
    assert!(ui.get_folders_section_ratio() > ratio);
    ui.hide().unwrap();
}

#[test]
fn small_scaled_windows_keep_lists_usable_and_retain_preferences_across_hidden_sections() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        for scale in [1.0, 1.25, 1.5] {
            let ui = window(chinese, 1440.0 * scale, 1000.0 * scale, scale);
            ui.set_folders_section_ratio(0.82);
            settle();
            ui.window()
                .set_size(slint::LogicalSize::new(1080.0 * scale, 700.0 * scale));
            settle();
            assert_eq!(ui.get_folders_section_ratio(), 0.82);
            assert!(list_height(&ui, false) >= 68.0 * scale - 1.0);
            assert!(list_height(&ui, true) >= 60.0 * scale - 1.0);
            assert_lists_inside_sidebar(&ui);

            ui.set_library_sidebar_page(1);
            settle();
            ui.set_library_sidebar_page(0);
            settle();
            assert_eq!(ui.get_folders_section_ratio(), 0.82);
            click(&ui, &header(&ui, false));
            assert!(list_height(&ui, true) > 100.0 * scale);
            assert_eq!(ui.get_folders_section_ratio(), 0.82);
            click(&ui, &header(&ui, false));

            let folders_with_albums = list_height(&ui, false);
            ui.set_albums(ModelRc::new(VecModel::from(Vec::<AlbumRow>::new())));
            settle();
            assert!(list_height(&ui, false) > folders_with_albums);
            assert_eq!(ui.get_folders_section_ratio(), 0.82);
            ui.global::<crate::Theme>().set_ui_scale(1.0);
            ui.hide().unwrap();
        }
    }
}

#[test]
fn collapsing_and_reopening_folders_preserves_real_wheel_scroll_position() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = window(chinese, 1440.0, 900.0, 1.0);
        ui.set_folders(ModelRc::new(VecModel::from(
            (0..100)
                .map(|index| FolderRow {
                    name: format!("Folder {index:03}").into(),
                    path: format!("C:/Fixture/Folder{index:03}").into(),
                    count: "120".into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )));
        settle();
        scroll_folders(&ui, -170.0);
        let before = ui.get_task_folder_scroll_y();
        assert!(
            before < -100.0,
            "The wheel must scroll the folder list: {before}"
        );

        click(&ui, &header(&ui, false));
        near(list_height(&ui, false), 0.0);
        click(&ui, &header(&ui, false));
        near(ui.get_task_folder_scroll_y(), before);
        // The restored viewport must still accept new input, rather than hold a
        // saved offset over the underlying ListView's live scroll position.
        scroll_folders(&ui, -68.0);
        assert!(ui.get_task_folder_scroll_y() < before - 30.0);
        let after = ui.get_task_folder_scroll_y();
        click(&ui, &header(&ui, false));
        click(&ui, &header(&ui, true));
        click(&ui, &header(&ui, false));
        click(&ui, &header(&ui, true));
        near(ui.get_task_folder_scroll_y(), after);
        ui.hide().unwrap();
    }
}

fn window(chinese: bool, width: f32, height: f32, scale: f32) -> AppWindow {
    let ui = AppWindow::new().unwrap();
    ui.global::<crate::Theme>().set_ui_scale(scale);
    ui.set_chinese(chinese);
    ui.window().set_size(slint::LogicalSize::new(width, height));
    ui.set_folders(ModelRc::new(VecModel::from(vec![FolderRow {
        name: "Fixture folder".into(),
        path: "C:/Fixture/Folder".into(),
        count: "120".into(),
        ..Default::default()
    }])));
    ui.set_albums(ModelRc::new(VecModel::from(vec![AlbumRow {
        id: "71".into(),
        name: "Fixture library".into(),
        count: "120".into(),
        ..Default::default()
    }])));
    ui.show().unwrap();
    settle();
    ui
}

fn chevron_ink_extent(ui: &AppWindow, libraries: bool) -> (u32, u32) {
    let icon = header(ui, libraries)
        .query_descendants()
        .match_id("SidebarSectionHeader::disclosure-icon")
        .find_first()
        .expect("Sidebar disclosure icon");
    let pixels = ui.window().take_snapshot().unwrap();
    let scale = ui.window().scale_factor();
    let origin = icon.absolute_position();
    let size = icon.size();
    let left = (origin.x * scale).floor() as u32;
    let top = (origin.y * scale).floor() as u32;
    let width = (size.width * scale).ceil() as u32;
    let height = (size.height * scale).ceil() as u32;
    assert!(width > 0 && height > 0);
    assert!(left + width <= pixels.width() && top + height <= pixels.height());
    let bytes = pixels.as_bytes();
    let stride = pixels.width() as usize * 4;
    let background = &bytes[top as usize * stride + left as usize * 4..][..3];
    let mut bounds = None::<(u32, u32, u32, u32)>;
    for y in 0..height {
        for x in 0..width {
            let pixel = &bytes[(top + y) as usize * stride + (left + x) as usize * 4..][..3];
            let contrast: u16 = pixel
                .iter()
                .zip(background)
                .map(|(channel, background)| u16::from(channel.abs_diff(*background)))
                .sum();
            if contrast > 80 {
                bounds = Some(match bounds {
                    Some((min_x, min_y, max_x, max_y)) => {
                        (min_x.min(x), min_y.min(y), max_x.max(x), max_y.max(y))
                    }
                    None => (x, y, x, y),
                });
            }
        }
    }
    let (min_x, min_y, max_x, max_y) = bounds.expect("Visible disclosure chevron ink");
    (max_x - min_x + 1, max_y - min_y + 1)
}

fn save_sidebar_snapshot(ui: &AppWindow, name: &str) {
    if let Some(directory) = std::env::var_os("AUDIOLIB_SIDEBAR_SNAPSHOT_DIR") {
        let directory = std::path::Path::new(&directory);
        std::fs::create_dir_all(directory).unwrap();
        let pixels = ui.window().take_snapshot().unwrap();
        image::save_buffer(
            directory.join(name),
            pixels.as_bytes(),
            pixels.width(),
            pixels.height(),
            image::ColorType::Rgba8,
        )
        .unwrap();
    }
}

fn count_layout_changes(ui: &AppWindow) -> Rc<Cell<usize>> {
    let saves = Rc::new(Cell::new(0));
    let counted = saves.clone();
    ui.on_pane_layout_changed(move || counted.set(counted.get() + 1));
    saves
}

fn header(ui: &AppWindow, libraries: bool) -> ElementHandle {
    labelled(
        ui,
        match (ui.get_chinese(), libraries) {
            (false, false) => "Folders section",
            (false, true) => "Libraries section",
            (true, false) => "文件夹分区",
            (true, true) => "音效库分区",
        },
    )
}

fn divider(ui: &AppWindow) -> ElementHandle {
    labelled(
        ui,
        if ui.get_chinese() {
            "调整文件夹和音效库高度"
        } else {
            "Resize folder and library heights"
        },
    )
}

fn labelled(ui: &AppWindow, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(ui, label)
        .next()
        .unwrap_or_else(|| panic!("Missing accessible element: {label}"))
}

fn list(ui: &AppWindow, libraries: bool) -> ElementHandle {
    ElementHandle::find_by_element_id(ui, list_id(libraries))
        .next()
        .expect("expanded sidebar list")
}

fn list_height(ui: &AppWindow, libraries: bool) -> f32 {
    let expanded = if libraries {
        ui.get_libraries_section_expanded()
    } else {
        ui.get_folders_section_expanded()
    };
    if !expanded {
        assert!(
            ElementHandle::find_by_element_id(ui, list_id(libraries))
                .next()
                .is_none(),
            "A collapsed sidebar list must be hidden from introspection"
        );
        return 0.0;
    }
    list(ui, libraries).size().height
}

fn list_id(libraries: bool) -> &'static str {
    if libraries {
        "AppWindow::album-list"
    } else {
        "AppWindow::folder-list"
    }
}

fn assert_lists_inside_sidebar(ui: &AppWindow) {
    let sidebar = ElementHandle::find_by_element_id(ui, "AppWindow::library-sidebar")
        .next()
        .unwrap();
    let top = sidebar.absolute_position().y;
    let bottom = top + sidebar.size().height;
    for libraries in [false, true] {
        let list = list(ui, libraries);
        let y = list.absolute_position().y;
        assert!(y >= top - 1.0 && y + list.size().height <= bottom + 1.0);
    }
}

fn midpoint(element: &ElementHandle) -> slint::LogicalPosition {
    let origin = element.absolute_position();
    let size = element.size();
    slint::LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0)
}

fn offset(start: slint::LogicalPosition, delta: f32) -> slint::LogicalPosition {
    slint::LogicalPosition::new(start.x, start.y + delta)
}

fn click(ui: &AppWindow, element: &ElementHandle) {
    let point = midpoint(element);
    pointer(ui, point, true);
    pointer(ui, point, false);
}

fn pointer(ui: &AppWindow, position: slint::LogicalPosition, pressed: bool) {
    if pressed {
        ui.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
    } else {
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
    }
    settle();
}

fn move_vertical(ui: &AppWindow, start: slint::LogicalPosition, delta: f32) {
    ui.window().dispatch_event(WindowEvent::PointerMoved {
        position: offset(start, delta),
    });
    settle();
}

fn drag(ui: &AppWindow, delta: f32) {
    let start = midpoint(&divider(ui));
    pointer(ui, start, true);
    move_vertical(ui, start, delta);
    pointer(ui, offset(start, delta), false);
}

fn scroll_folders(ui: &AppWindow, delta_y: f32) {
    let origin = list(ui, false).absolute_position();
    let position = slint::LogicalPosition::new(origin.x + 24.0, origin.y + 24.0);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x: 0.0,
        delta_y,
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(250));
}

fn key(ui: &AppWindow, key: slint::SharedString) {
    crate::ui_tests::send_keys(ui, &[key]);
    settle();
}

fn settle() {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

fn near(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() <= 0.6,
        "Expected {expected}, got {actual}"
    );
}
