// app/audiolib-desktop/src/search_task_ui_tests.rs
//! Search-task controls are exercised with an isolated Slint backend and owned models.
use crate::{AppWindow, AudioRow, FolderRow, SavedSearchTaskRow, SearchTaskRow};
use i_slint_backend_testing::{ElementHandle, TestingBackend, TestingBackendOptions};
use slint::{
    ComponentHandle, ModelRc, VecModel,
    platform::{Key, PointerEventButton, WindowEvent},
};
use std::{cell::RefCell, rc::Rc, time::Duration};

fn tasks(count: usize) -> ModelRc<SearchTaskRow> {
    ModelRc::new(VecModel::from(
        (0..count)
            .map(|index| SearchTaskRow {
                id: index.to_string().into(),
                name: format!("Task {index}").into(),
                active: index == 0,
                favorite: false,
                modified: index == 1,
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    ))
}

fn window() -> AppWindow {
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_search_tasks(tasks(2));
    ui.show().unwrap();
    settle();
    ui
}

fn click(ui: &AppWindow, label: &str) {
    ElementHandle::find_by_accessible_label(ui, label)
        .next()
        .unwrap_or_else(|| panic!("Missing task control: {label}"))
        .mock_single_click(PointerEventButton::Left);
    settle();
}

fn settle() {
    // Declarative Timer.running changes start their native timer in the change
    // handler pass, after the mock backend's timer activation pass. Flush that
    // pass before advancing time so one-shot view restoration can actually fire.
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(10));
}

fn snapshot_if_requested(ui: &AppWindow, name: &str) {
    let Some(directory) = std::env::var_os("AUDIOLIB_MULTI_SEARCH_SNAPSHOT_DIR") else {
        return;
    };
    let directory = std::path::Path::new(&directory);
    std::fs::create_dir_all(directory).unwrap();
    let pixels = ui.window().take_snapshot().unwrap();
    image::save_buffer(
        directory.join(format!("{name}.png")),
        pixels.as_bytes(),
        pixels.width(),
        pixels.height(),
        image::ColorType::Rgba8,
    )
    .unwrap();
}

fn open_saved_menu(ui: &AppWindow) {
    let control = ElementHandle::find_by_accessible_label(ui, "Manage saved search: Forest")
        .next()
        .unwrap();
    control.mock_single_click(PointerEventButton::Left);
    settle();
}

fn activate_popup(ui: &AppWindow, label: &str, steps_from_end: usize) {
    ElementHandle::find_by_accessible_label(ui, label)
        .next()
        .unwrap_or_else(|| panic!("Missing saved-search menu item: {label}"));
    // Context popups use their own coordinate origin. Keyboard navigation drives
    // the actual menu without assuming MenuBar popup positioning conventions.
    for key in std::iter::repeat_n(Key::UpArrow, steps_from_end).chain([Key::Return]) {
        ui.window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        ui.window()
            .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    }
    settle();
}

#[test]
fn active_task_toolbar_favorite_and_background_close_do_not_select_a_task() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = window();
    let events = Rc::new(RefCell::new(Vec::new()));
    let observed = events.clone();
    ui.on_search_task_selected(move |id| observed.borrow_mut().push(format!("select:{id}")));
    let observed = events.clone();
    ui.on_search_task_favorite_toggled(move |id| {
        observed.borrow_mut().push(format!("favorite:{id}"));
    });
    let observed = events.clone();
    ui.on_search_task_closed(move |id| observed.borrow_mut().push(format!("close:{id}")));
    click(&ui, "Favorite search task: Task 0");
    click(&ui, "Close search task: Task 1");
    assert_eq!(&*events.borrow(), &["favorite:0", "close:1"]);
    click(&ui, "Search task: Task 1");
    assert_eq!(&*events.borrow(), &["favorite:0", "close:1", "select:1"]);
    ui.set_active_search_task_index(1);
    settle();
    let toolbar_favorite = ElementHandle::find_by_accessible_label(
        &ui,
        "Favorite search task: Task 1 (changed; update from saved search menu)",
    )
    .next()
    .unwrap();
    toolbar_favorite.mock_single_click(PointerEventButton::Left);
    settle();
    assert_eq!(
        &*events.borrow(),
        &["favorite:0", "close:1", "select:1", "favorite:1"]
    );
    ui.hide().unwrap();
}

#[test]
fn new_task_control_follows_tabs_and_docks_when_resizing_both_themes() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    for theme in audiolib_core::config::THEME_IDS {
        for scale in [1.0, 2.0] {
            let ui = AppWindow::new().unwrap();
            crate::theme::apply_theme(&ui, theme);
            ui.global::<crate::Theme>().set_ui_scale(scale);
            ui.window()
                .set_size(slint::LogicalSize::new(2400.0 * scale, 900.0 * scale));
            ui.set_search_tasks(tasks(1));
            ui.on_search_task_selected(|_| panic!("Resizing and paging must not select a task"));
            let created = Rc::new(RefCell::new(0));
            let observed = created.clone();
            ui.on_search_task_created(move || *observed.borrow_mut() += 1);
            ui.show().unwrap();
            settle();

            let control = |label: &str| {
                ElementHandle::find_by_accessible_label(&ui, label)
                    .next()
                    .unwrap_or_else(|| {
                        panic!("Missing control: {label}, theme={theme}, scale={scale}")
                    })
            };
            let follows_last_tab = |count| {
                let last = control(&format!("Search task: Task {}", count - 1));
                let plus = control("New search task");
                let gap =
                    plus.absolute_position().x - (last.absolute_position().x + last.size().width);
                assert!(
                    (-1.0..plus.size().width / 4.0).contains(&gap),
                    "New must follow the last tab: gap={gap}, theme={theme}, scale={scale}"
                );
                assert!(
                    ElementHandle::find_by_accessible_label(&ui, "Scroll search tasks right")
                        .next()
                        .is_none()
                );
                plus.absolute_position().x
            };

            let first_x = follows_last_tab(1);
            ui.set_search_tasks(tasks(3));
            settle();
            assert!(follows_last_tab(3) > first_x);
            ui.set_search_tasks(tasks(1));
            settle();
            assert!((follows_last_tab(1) - first_x).abs() < 1.0);

            // Tabs first share the available width instead of immediately scrolling.
            ui.set_search_tasks(tasks(4));
            settle();
            follows_last_tab(4);
            let wide_tab_width = control("Search task: Task 0").size().width;
            let bar = control("Search tasks");
            // Studio's strip is inside the result pane. Include that theme's
            // surrounding panes when choosing a width that can fit four minimum tabs.
            let outside_strip = ui
                .window()
                .size()
                .to_logical(ui.window().scale_factor())
                .width
                - bar.size().width;
            let adaptive_window_width = (outside_strip + 712.0 * scale).max(1080.0 * scale);
            if scale == 1.0 {
                snapshot_if_requested(&ui, &format!("task-add-following-{theme}"));
            }
            ui.window().set_size(slint::LogicalSize::new(
                adaptive_window_width,
                900.0 * scale,
            ));
            settle();
            follows_last_tab(4);
            let narrow_tab_width = control("Search task: Task 0").size().width;
            assert!(
                narrow_tab_width < wide_tab_width - 1.0,
                "Tabs must shrink before scrolling: wide={wide_tab_width}, narrow={narrow_tab_width}, theme={theme}, scale={scale}"
            );
            assert!(narrow_tab_width >= 144.0 * scale - 1.0);
            for index in 1..4 {
                assert!(
                    (control(&format!("Search task: Task {index}")).size().width
                        - narrow_tab_width)
                        .abs()
                        < 1.0,
                    "Available width must be shared between tabs, theme={theme}, scale={scale}"
                );
            }
            if scale == 1.0 {
                snapshot_if_requested(&ui, &format!("task-tabs-adaptive-{theme}"));
            }

            // Only a strip that cannot fit minimum-width tabs gains scrolling.
            ui.set_search_tasks(tasks(12));
            ui.window()
                .set_size(slint::LogicalSize::new(1080.0 * scale, 700.0 * scale));
            settle();
            let first_tab = control("Search task: Task 0");
            assert!(
                (first_tab.size().width - 144.0 * scale).abs() < 1.0,
                "Overflow must retain readable minimum tabs, theme={theme}, scale={scale}"
            );
            let strip_id = if ui.global::<crate::Theme>().get_modern_style() {
                "AppWindow::studio-task-tabs-scroll"
            } else {
                "AppWindow::task-tabs-scroll"
            };
            let strip = ElementHandle::find_by_element_id(&ui, strip_id)
                .next()
                .unwrap();
            let scrollbar_lane = strip.absolute_position().y + strip.size().height
                - (first_tab.absolute_position().y + first_tab.size().height);
            assert!(
                scrollbar_lane >= 14.0 * scale - 1.0,
                "The horizontal scrollbar needs a lane below tab content: lane={scrollbar_lane}, theme={theme}, scale={scale}"
            );
            let plus = control("New search task");
            let bar = control("Search tasks");
            let right_gap = bar.absolute_position().x + bar.size().width
                - (plus.absolute_position().x + plus.size().width);
            assert!(
                (-1.0..plus.size().width / 4.0).contains(&right_gap),
                "Overflow must dock New at the bar's right edge: gap={right_gap}, theme={theme}, scale={scale}"
            );
            assert_eq!(plus.accessible_enabled(), Some(true));
            let fixed_x = plus.absolute_position().x;
            click(&ui, "Scroll search tasks right");
            assert!((control("New search task").absolute_position().x - fixed_x).abs() < 1.0);
            click(&ui, "New search task");
            assert_eq!(*created.borrow(), 1);
            if scale == 1.0 {
                snapshot_if_requested(&ui, &format!("task-add-docked-{theme}"));
            }

            ui.window()
                .set_size(slint::LogicalSize::new(2400.0 * scale, 900.0 * scale));
            ui.set_search_tasks(tasks(4));
            settle();
            follows_last_tab(4);
            assert!(
                (control("Search task: Task 0").size().width - wide_tab_width).abs() < 1.0,
                "Tabs must regain their preferred width when room returns, theme={theme}, scale={scale}"
            );
            ui.hide().unwrap();
        }
    }
}

#[test]
fn thirty_task_tabs_page_in_both_directions_with_a_fixed_new_task_control() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        for scale in [1.0, 2.0] {
            let ui = AppWindow::new().unwrap();
            ui.global::<crate::Theme>().set_ui_scale(scale);
            ui.global::<crate::Theme>().set_modern_style(modern);
            ui.global::<crate::Theme>().set_modern_player(modern);
            ui.window()
                .set_size(slint::LogicalSize::new(1440.0 * scale, 900.0 * scale));
            ui.set_search_tasks(tasks(2));
            let created = Rc::new(RefCell::new(0));
            let observed = created.clone();
            ui.on_search_task_created(move || *observed.borrow_mut() += 1);
            let selected = Rc::new(RefCell::new(Vec::new()));
            let observed = selected.clone();
            ui.on_search_task_selected(move |id| observed.borrow_mut().push(id.to_string()));
            ui.show().unwrap();
            settle();

            assert!(
                ElementHandle::find_by_accessible_label(&ui, "Scroll search tasks right")
                    .next()
                    .is_none()
            );
            // New remains available at the start of a long strip without scrolling to its end.
            ui.window()
                .set_size(slint::LogicalSize::new(1080.0 * scale, 700.0 * scale));
            ui.set_search_tasks(tasks(29));
            settle();
            click(&ui, "New search task");
            assert_eq!(*created.borrow(), 1);
            ui.set_search_tasks(tasks(30));
            settle();
            let plus = ElementHandle::find_by_accessible_label(&ui, "New search task")
                .next()
                .unwrap();
            let fixed_x = plus.absolute_position().x;
            assert_eq!(plus.accessible_enabled(), Some(false));
            plus.mock_single_click(PointerEventButton::Left);
            assert_eq!(*created.borrow(), 1);
            assert!(selected.borrow().is_empty());
            let strip_id = if modern {
                "AppWindow::studio-task-tabs-scroll"
            } else {
                "AppWindow::task-tabs-scroll"
            };
            let strip = ElementHandle::find_by_element_id(&ui, strip_id)
                .next()
                .unwrap();
            assert!(
                plus.absolute_position().x
                    >= strip.absolute_position().x + strip.size().width - 1.0
            );
            let left = ElementHandle::find_by_accessible_label(&ui, "Scroll search tasks left")
                .next()
                .unwrap();
            assert_eq!(left.accessible_enabled(), Some(false));
            for _ in 0..30 {
                let right =
                    ElementHandle::find_by_accessible_label(&ui, "Scroll search tasks right")
                        .next()
                        .unwrap();
                if right.accessible_enabled() == Some(false) {
                    break;
                }
                right.mock_single_click(PointerEventButton::Left);
                settle();
            }
            assert_eq!(
                ElementHandle::find_by_accessible_label(&ui, "Scroll search tasks right")
                    .next()
                    .unwrap()
                    .accessible_enabled(),
                Some(false)
            );
            let last = ElementHandle::find_by_accessible_label(&ui, "Search task: Task 29")
                .next()
                .unwrap();
            assert!(last.absolute_position().x >= strip.absolute_position().x - 1.0);
            assert!(
                last.absolute_position().x + last.size().width
                    <= strip.absolute_position().x + strip.size().width + 1.0
            );
            assert_eq!(
                ElementHandle::find_by_accessible_label(&ui, "New search task")
                    .next()
                    .unwrap()
                    .absolute_position()
                    .x,
                fixed_x
            );
            for _ in 0..30 {
                let left = ElementHandle::find_by_accessible_label(&ui, "Scroll search tasks left")
                    .next()
                    .unwrap();
                if left.accessible_enabled() == Some(false) {
                    break;
                }
                left.mock_single_click(PointerEventButton::Left);
                settle();
            }
            assert_eq!(
                ElementHandle::find_by_accessible_label(&ui, "Scroll search tasks left")
                    .next()
                    .unwrap()
                    .accessible_enabled(),
                Some(false)
            );
            assert!(selected.borrow().is_empty(), "Paging must not switch tasks");
            // Native horizontal wheel input still reaches the end independently of the arrows.
            ui.window().dispatch_event(WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(
                    strip.absolute_position().x + strip.size().width / 2.0,
                    strip.absolute_position().y + strip.size().height / 2.0,
                ),
                delta_x: -100_000.0,
                delta_y: 0.0,
            });
            settle();
            assert!(
                ElementHandle::find_by_accessible_label(&ui, "Search task: Task 29")
                    .next()
                    .is_some()
            );

            // Active-task restoration reveals the last tab even when the strip starts left.
            ui.invoke_reveal_search_task(0);
            settle();
            ui.invoke_reveal_search_task(29);
            settle();
            click(&ui, "Search task: Task 29");
            assert_eq!(&*selected.borrow(), &["29"]);
            ui.set_active_search_task_index(29);
            ui.window()
                .set_size(slint::LogicalSize::new(1440.0 * scale, 900.0 * scale));
            settle();
            ui.window()
                .set_size(slint::LogicalSize::new(1080.0 * scale, 700.0 * scale));
            settle();
            let last = ElementHandle::find_by_accessible_label(&ui, "Search task: Task 29")
                .next()
                .unwrap();
            assert!(last.absolute_position().x >= strip.absolute_position().x - 1.0);
            assert!(
                last.absolute_position().x + last.size().width
                    <= strip.absolute_position().x + strip.size().width + 1.0
            );
            ui.set_active_search_task_index(0);
            ui.set_search_tasks(tasks(2));
            settle();
            assert!(
                ElementHandle::find_by_accessible_label(&ui, "Scroll search tasks right")
                    .next()
                    .is_none()
            );
            assert!(
                ElementHandle::find_by_accessible_label(&ui, "Search task: Task 0")
                    .next()
                    .is_some()
            );
            ui.hide().unwrap();
        }
    }
}

#[test]
fn minimum_width_tabs_keep_keywords_ahead_of_auxiliary_labels_in_both_languages() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    for theme in audiolib_core::config::THEME_IDS {
        for chinese in [false, true] {
            let ui = AppWindow::new().unwrap();
            crate::theme::apply_theme(&ui, theme);
            ui.set_chinese(chinese);
            ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
            let summary = "Search: distant thunderstorm rolling\nDirectory: D:/Sound Library/Weather/Atmospheres";
            ui.set_search_tasks(ModelRc::new(VecModel::from(
                (0..12)
                    .map(|index| SearchTaskRow {
                        id: index.to_string().into(),
                        name: format!("Task {index}").into(),
                        active: index == 0,
                        searched: index == 0,
                        keywords: if index == 0 {
                            ModelRc::new(VecModel::from(vec![
                                "distant thunderstorm".into(),
                                "rolling".into(),
                            ]))
                        } else {
                            ModelRc::default()
                        },
                        folder: "Atmospheres".into(),
                        summary: summary.into(),
                        ..Default::default()
                    })
                    .collect::<Vec<_>>(),
            )));
            ui.show().unwrap();
            settle();
            let tab_label = if chinese {
                "搜索任务：Task 0"
            } else {
                "Search task: Task 0"
            };
            let tab = ElementHandle::find_by_accessible_label(&ui, tab_label)
                .next()
                .unwrap();
            let keyword = ElementHandle::find_by_element_id(&ui, "SearchTaskTab::keyword-label")
                .next()
                .expect("Minimum tabs must keep their search keywords visible");
            let close_label = if chinese {
                "关闭搜索任务：Task 0"
            } else {
                "Close search task: Task 0"
            };
            let close = ElementHandle::find_by_accessible_label(&ui, close_label)
                .next()
                .unwrap();
            assert!((tab.size().width - 144.0).abs() < 1.0);
            assert!(keyword.size().width >= 90.0);
            assert!(keyword.absolute_position().x >= tab.absolute_position().x);
            assert!(
                keyword.absolute_position().x + keyword.size().width <= close.absolute_position().x,
                "Keyword text must stop before the close control: theme={theme}, chinese={chinese}"
            );
            for id in [
                "SearchTaskTab::directory-separator",
                "SearchTaskTab::directory-label",
            ] {
                assert!(
                    ElementHandle::find_by_element_id(&ui, id).next().is_none(),
                    "Auxiliary labels must yield space to long keywords: {id}, theme={theme}, chinese={chinese}"
                );
            }
            assert!(tab.accessible_description().unwrap().contains(summary));
            let language = if chinese { "cn" } else { "en" };
            snapshot_if_requested(&ui, &format!("task-keywords-minimum-{theme}-{language}"));

            // Short terms stay together instead of spreading across the tab.
            ui.set_search_tasks(ModelRc::new(VecModel::from(vec![SearchTaskRow {
                id: "0".into(),
                name: "Task 0".into(),
                active: true,
                searched: true,
                keywords: ModelRc::new(VecModel::from(vec!["wind".into()])),
                folder: "Atmospheres".into(),
                summary: summary.into(),
                ..Default::default()
            }])));
            settle();
            let keyword = ElementHandle::find_by_element_id(&ui, "SearchTaskTab::keyword-label")
                .next()
                .unwrap();
            let separator =
                ElementHandle::find_by_element_id(&ui, "SearchTaskTab::directory-separator")
                    .next()
                    .expect("A fitting directory must have its separator");
            let directory =
                ElementHandle::find_by_element_id(&ui, "SearchTaskTab::directory-label")
                    .next()
                    .expect("A short query must retain its directory context");
            let keyword_end = keyword.absolute_position().x + keyword.size().width;
            let separator_end = separator.absolute_position().x + separator.size().width;
            assert!(
                (0.0..=8.0).contains(&(separator.absolute_position().x - keyword_end)),
                "The separator must follow the keyword closely: theme={theme}, chinese={chinese}"
            );
            assert!(
                (0.0..=8.0).contains(&(directory.absolute_position().x - separator_end)),
                "The directory must follow the separator closely: theme={theme}, chinese={chinese}"
            );
            assert!((keyword.absolute_position().y - directory.absolute_position().y).abs() < 1.0);
            snapshot_if_requested(&ui, &format!("task-keywords-compact-{theme}-{language}"));

            // A directory-only task keeps its marker and the same left alignment.
            let keyword_start = keyword.absolute_position().x;
            ui.set_search_tasks(ModelRc::new(VecModel::from(vec![SearchTaskRow {
                id: "0".into(),
                name: "Task 0".into(),
                active: true,
                searched: true,
                folder: "Atmospheres".into(),
                ..Default::default()
            }])));
            settle();
            assert!(
                ElementHandle::find_by_element_id(&ui, "SearchTaskTab::keyword-label")
                    .next()
                    .is_none()
            );
            let separator =
                ElementHandle::find_by_element_id(&ui, "SearchTaskTab::directory-separator")
                    .next()
                    .unwrap();
            let directory =
                ElementHandle::find_by_element_id(&ui, "SearchTaskTab::directory-label")
                    .next()
                    .unwrap();
            assert!((separator.absolute_position().x - keyword_start).abs() < 1.0);
            assert!(directory.absolute_position().x > separator.absolute_position().x);

            // Filter-only conditions have no directory marker or reserved gap.
            ui.set_search_tasks(ModelRc::new(VecModel::from(vec![SearchTaskRow {
                id: "0".into(),
                name: "Task 0".into(),
                active: true,
                searched: true,
                ..Default::default()
            }])));
            settle();
            assert!(
                ElementHandle::find_by_element_id(&ui, "SearchTaskTab::directory-separator")
                    .next()
                    .is_none()
            );
            let filters = ElementHandle::find_by_element_id(&ui, "SearchTaskTab::directory-label")
                .next()
                .unwrap();
            assert!((filters.absolute_position().x - keyword_start).abs() < 1.0);
            ui.hide().unwrap();
        }
    }
}

#[test]
fn search_tab_hover_exposes_the_complete_query_and_directory_without_selecting_the_task() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    for chinese in [false, true] {
        let ui = window();
        ui.set_chinese(chinese);
        let summary = "Search: hard \"metal hit\" glass\nDirectory: D:/Sound Library/Aircraft/P-51 Mustang\nInclude subfolders: Yes";
        ui.set_search_tasks(ModelRc::new(VecModel::from(vec![SearchTaskRow {
            id: "hover-task".into(),
            name: "Mustang impacts".into(),
            keywords: ModelRc::new(VecModel::from(vec![
                "hard".into(),
                "metal hit".into(),
                "glass".into(),
            ])),
            folder: "P-51 Mustang".into(),
            summary: summary.into(),
            searched: true,
            active: true,
            ..Default::default()
        }])));
        ui.on_search_task_selected(|_| panic!("Hover must not select a task"));
        settle();
        let label = if chinese {
            "搜索任务：Mustang impacts"
        } else {
            "Search task: Mustang impacts"
        };
        let tab = ElementHandle::find_by_accessible_label(&ui, label)
            .next()
            .unwrap();
        let accessible_summary = tab.accessible_description().unwrap();
        assert!(accessible_summary.contains("hard \"metal hit\" glass"));
        assert!(accessible_summary.contains("D:/Sound Library/Aircraft/P-51 Mustang"));
        assert!(
            ElementHandle::find_by_accessible_label(&ui, summary)
                .next()
                .is_none()
        );
        let language = if chinese { "cn" } else { "en" };
        snapshot_if_requested(&ui, &format!("task-before-hover-{language}"));
        ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(
                tab.absolute_position().x + tab.size().width / 2.0,
                tab.absolute_position().y + tab.size().height / 2.0,
            ),
        });
        settle();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(600));
        settle();
        snapshot_if_requested(&ui, &format!("task-after-hover-{language}"));
        assert!(
            ElementHandle::find_by_accessible_label(&ui, summary)
                .next()
                .is_some(),
            "Hover should open the full task tooltip, chinese={chinese}"
        );
        ui.hide().unwrap();
    }
}

#[test]
fn task_creation_obeys_the_configured_limit_and_workspace_loading_guard() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = window();
    let calls = Rc::new(RefCell::new(0));
    let observed = calls.clone();
    ui.on_search_task_created(move || *observed.borrow_mut() += 1);
    ui.set_max_search_tasks(2);
    click(&ui, "New search task");
    assert_eq!(*calls.borrow(), 0);
    ui.set_max_search_tasks(3);
    click(&ui, "New search task");
    assert_eq!(*calls.borrow(), 1);
    ui.set_search_workspace_loading(true);
    click(&ui, "New search task");
    assert_eq!(*calls.borrow(), 1);
    ui.set_search_workspace_loading(false);
    click(&ui, "New search task");
    assert_eq!(*calls.borrow(), 2);
    ui.hide().unwrap();
}

#[test]
fn rename_keeps_task_and_saved_search_targets_separate_and_blocks_background_actions() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = window();
    let events = Rc::new(RefCell::new(Vec::new()));
    let observed = events.clone();
    ui.on_search_task_renamed(move |id, name| {
        observed.borrow_mut().push(format!("task:{id}:{name}"));
    });
    let observed = events.clone();
    ui.on_saved_search_renamed(move |id, name| {
        observed.borrow_mut().push(format!("saved:{id}:{name}"));
    });
    ui.on_search_task_created(|| panic!("Rename must block background task creation"));
    ui.invoke_begin_search_task_rename("1".into(), "Task 1".into(), false);
    settle();
    click(&ui, "New search task");
    ui.set_search_task_rename_name("Metal doors".into());
    click(&ui, "Save name");
    assert!(!ui.get_search_task_rename_visible());
    ui.invoke_begin_search_task_rename("saved-7".into(), "Old name".into(), true);
    settle();
    ui.set_search_task_rename_name("Interior rain".into());
    click(&ui, "Save name");
    assert_eq!(
        &*events.borrow(),
        &["task:1:Metal doors", "saved:saved-7:Interior rain"]
    );
    ui.hide().unwrap();
}

#[test]
fn favorites_categories_keep_audio_filtering_and_saved_search_opening_distinct() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = window();
    ui.set_saved_search_tasks(ModelRc::new(VecModel::from(vec![SavedSearchTaskRow {
        id: "saved-9".into(),
        name: "Forest".into(),
        summary: "wind · Ambience".into(),
    }])));
    let searches = Rc::new(RefCell::new(0));
    let observed = searches.clone();
    ui.on_all_library_requested(move || *observed.borrow_mut() += 1);
    let opened = Rc::new(RefCell::new(Vec::new()));
    let observed = opened.clone();
    ui.on_saved_search_opened(move |id| observed.borrow_mut().push(id.to_string()));
    click(&ui, "Favorites page");
    assert_eq!(ui.get_library_sidebar_page(), 1);
    assert!(ui.get_favorites_only());
    assert_eq!(*searches.borrow(), 1);
    click(&ui, "Search task favorites category");
    assert_eq!(ui.get_favorites_category(), 1);
    assert_eq!(*searches.borrow(), 1);
    click(&ui, "Open saved search: Forest");
    assert_eq!(&*opened.borrow(), &["saved-9"]);
    click(&ui, "Audio favorites category");
    assert_eq!(ui.get_favorites_category(), 0);
    assert_eq!(*searches.borrow(), 2);
    ui.hide().unwrap();
}

#[test]
fn saved_search_update_and_delete_use_the_identity_captured_when_the_menu_opened() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = window();
    ui.set_library_sidebar_page(1);
    ui.set_favorites_category(1);
    let saved = || {
        ModelRc::new(VecModel::from(vec![SavedSearchTaskRow {
            id: "saved-9".into(),
            name: "Forest".into(),
            summary: "wind".into(),
        }]))
    };
    ui.set_saved_search_tasks(saved());
    settle();
    let events = Rc::new(RefCell::new(Vec::new()));
    let observed = events.clone();
    ui.on_saved_search_updated(move |id| observed.borrow_mut().push(format!("update:{id}")));
    let observed = events.clone();
    ui.on_saved_search_deleted(move |id| observed.borrow_mut().push(format!("delete:{id}")));
    open_saved_menu(&ui);
    ui.set_saved_search_tasks(ModelRc::new(VecModel::default()));
    activate_popup(&ui, "Update from current task", 2);
    ui.set_saved_search_tasks(saved());
    settle();
    open_saved_menu(&ui);
    ui.set_saved_search_tasks(ModelRc::new(VecModel::default()));
    activate_popup(&ui, "Delete saved search", 1);
    assert_eq!(&*events.borrow(), &["update:saved-9", "delete:saved-9"]);
    ui.hide().unwrap();
}

#[test]
fn task_viewport_restore_survives_model_and_selection_replacement() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = window();
    ui.set_rows(ModelRc::new(VecModel::from(
        (0..100)
            .map(|index| AudioRow {
                id: index.to_string().into(),
                name: format!("Sound {index}").into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_folders(ModelRc::new(VecModel::from(
        (0..100)
            .map(|index| FolderRow {
                name: format!("Folder {index}").into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    settle();
    ui.set_task_restoring(true);
    ui.set_selected_row(90);
    ui.invoke_restore_search_task_viewport(-220.0, -80.0, -180.0, 0.0, 0.0);
    assert_eq!(ui.get_task_result_scroll_y(), -220.0);
    assert!(ui.get_task_restoring());
    // Restoration waits for the arriving virtual rows to be laid out on a frame.
    // Keep a bounded deadline while allowing that asynchronous measurement turn.
    for _ in 0..10 {
        settle();
        if !ui.get_task_restoring() {
            break;
        }
    }
    assert!(!ui.get_task_restoring());
    assert!((ui.get_task_result_scroll_y() + 220.0).abs() < 1.0);
    assert!((ui.get_task_folder_scroll_y() + 180.0).abs() < 1.0);
    assert!(ui.get_task_result_row_height() > 0.0);
    ui.hide().unwrap();
}

#[test]
fn restored_task_allows_independent_result_and_folder_scrolling() {
    i_slint_backend_testing::init_no_event_loop();
    for modern in [false, true] {
        let ui = window();
        ui.global::<crate::Theme>().set_modern_style(modern);
        ui.window()
            .set_size(slint::LogicalSize::new(1440.0, 1560.0));
        ui.set_rows(ModelRc::new(VecModel::from(
            (0..200)
                .map(|index| AudioRow {
                    id: index.to_string().into(),
                    name: format!("Sound {index}").into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )));
        ui.set_folders(ModelRc::new(VecModel::from(
            (0..200)
                .map(|index| FolderRow {
                    name: format!("Folder {index}").into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        )));
        settle();

        // Give the declarative timer a native handle before restarting it, as
        // happens when creating a second search task after startup restoration.
        ui.invoke_restore_search_task_viewport(0.0, 0.0, 0.0, 0.0, 0.0);
        settle();
        // A fresh default task restores zero, while an existing task restores its
        // saved position. Neither restoration may retain ownership of user input.
        for restored_y in [0.0, -2618.0, 0.0] {
            ui.invoke_restore_search_task_viewport(restored_y, 0.0, -440.0, 0.0, 0.0);
            // Unlike settle(), expire the restarted native timer before flushing
            // running's deferred change handler, matching a busy UI event loop.
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(10));
            // The 0.8.7 virtualized list restores over two 16 ms layout passes.
            // Let those passes finish before handing the view to real wheel input.
            for _ in 0..12 {
                settle();
                if !ui.get_task_restoring() {
                    break;
                }
            }
            assert!(
                !ui.get_task_restoring(),
                "restore did not finish: modern={modern}"
            );
            let result_before = ui.get_task_result_scroll_y();
            let folder_before = ui.get_task_folder_scroll_y();
            let lists = if modern {
                [
                    "AppWindow::studio-result-list",
                    "AppWindow::studio-folder-list",
                ]
            } else {
                ["AppWindow::result-list", "AppWindow::folder-list"]
            };
            for id in lists {
                let list = ElementHandle::find_by_element_id(&ui, id).next().unwrap();
                ui.window().dispatch_event(WindowEvent::PointerScrolled {
                    position: slint::LogicalPosition::new(
                        list.absolute_position().x + 24.0,
                        list.absolute_position().y + 24.0,
                    ),
                    delta_x: 0.0,
                    delta_y: -220.0,
                });
                settle();
            }
            assert!(ui.get_task_result_scroll_y() < result_before - 100.0);
            assert!(ui.get_task_folder_scroll_y() < folder_before - 100.0);
            let result_after = ui.get_task_result_scroll_y();
            let folder_after = ui.get_task_folder_scroll_y();
            for _ in 0..60 {
                settle();
            }
            assert!((ui.get_task_result_scroll_y() - result_after).abs() < 1.0);
            assert!((ui.get_task_folder_scroll_y() - folder_after).abs() < 1.0);
        }
        // The controller releases the saved-viewport guard for an explicit
        // user selection; merely publishing a selected index must not do so.
        ui.set_preserve_result_scroll(false);
        ui.set_selected_row(199);
        settle();
        assert!(ui.get_task_result_scroll_y() < -3000.0);
        ui.hide().unwrap();
    }
}

#[test]
fn thirty_task_tabs_and_saved_searches_render_at_all_supported_layout_extremes() {
    slint::platform::set_platform(Box::new(TestingBackend::new(TestingBackendOptions {
        mock_time: true,
        threading: false,
        renderer_name: Some("software".into()),
    })))
    .unwrap();
    for modern in [false, true] {
        for scale in [1.0, 1.5, 2.0] {
            for chinese in [false, true] {
                let ui = AppWindow::new().unwrap();
                ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
                ui.global::<crate::Theme>().set_ui_scale(scale);
                ui.global::<crate::Theme>().set_modern_style(modern);
                ui.global::<crate::Theme>().set_modern_player(modern);
                ui.set_chinese(chinese);
                ui.set_search_tasks(tasks(30));
                ui.set_library_sidebar_page(1);
                ui.set_favorites_category(1);
                ui.set_saved_search_tasks(ModelRc::new(VecModel::from(vec![SavedSearchTaskRow {
                    id: "saved-1".into(),
                    name: "森林环境声音 / Forest ambience".into(),
                    summary: "wind -interior · Ambience/Forest".into(),
                }])));
                ui.show().unwrap();
                let pixels = ui.window().take_snapshot().unwrap();
                assert!(pixels.width() >= 1080 && pixels.height() >= 700);
                if let Some(directory) = std::env::var_os("AUDIOLIB_MULTI_SEARCH_SNAPSHOT_DIR") {
                    let directory = std::path::Path::new(&directory);
                    std::fs::create_dir_all(directory).unwrap();
                    let language = if chinese { "cn" } else { "en" };
                    let style = if modern { "studio" } else { "classic" };
                    image::save_buffer(
                        directory.join(format!("tasks-{style}-{language}-x{scale}.png")),
                        pixels.as_bytes(),
                        pixels.width(),
                        pixels.height(),
                        image::ColorType::Rgba8,
                    )
                    .unwrap();
                }
                ui.hide().unwrap();
            }
        }
    }
}
