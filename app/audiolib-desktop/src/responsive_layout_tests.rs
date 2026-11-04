// app/audiolib-desktop/src/responsive_layout_tests.rs
//! Pointer and accessibility regressions for resizable panes and source Metadata disclosure.
use crate::{AlbumRow, AppWindow, AudioRow, FolderRow};
use i_slint_backend_testing::ElementHandle;
use slint::{ComponentHandle, ModelRc, VecModel, platform::WindowEvent};
use std::{cell::RefCell, rc::Rc, time::Duration};

#[test]
fn folder_and_album_indexes_stay_visible_and_clickable_in_small_windows() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        // Cover the reported small client area, the declared minimum, and the
        // same compact content budget at fractional interface scales.
        for (width, height, scale) in [
            (1080.0, 690.0, 1.0),
            (1080.0, 700.0, 1.0),
            (1350.0, 875.0, 1.25),
            (1620.0, 1050.0, 1.5),
        ] {
            let ui = AppWindow::new().unwrap();
            ui.global::<crate::Theme>().set_ui_scale(scale);
            ui.set_chinese(chinese);
            ui.window().set_size(slint::LogicalSize::new(width, height));
            let folder_names = ["BOOM root", "Alien Life child"];
            ui.set_folders(ModelRc::new(VecModel::from(vec![
                FolderRow {
                    name: folder_names[0].into(),
                    count: "761".into(),
                    path: "C:/Fixture/BOOM".into(),
                    expanded: true,
                    has_children: true,
                    ..Default::default()
                },
                FolderRow {
                    name: folder_names[1].into(),
                    count: "143".into(),
                    path: "C:/Fixture/BOOM/Alien Life".into(),
                    depth: 1,
                    ..Default::default()
                },
            ])));
            let album_names = ["Alien Life library", "Cinematic Metal library"];
            ui.set_albums(ModelRc::new(VecModel::from(vec![
                AlbumRow {
                    id: "71".into(),
                    name: album_names[0].into(),
                    count: "143".into(),
                    ..Default::default()
                },
                AlbumRow {
                    id: "92".into(),
                    name: album_names[1].into(),
                    count: "618".into(),
                    ..Default::default()
                },
            ])));
            let folders = Rc::new(RefCell::new(Vec::new()));
            let observed = folders.clone();
            ui.on_folder_requested(move |index| observed.borrow_mut().push(index));
            let albums = Rc::new(RefCell::new(Vec::new()));
            let observed = albums.clone();
            ui.on_album_selected(move |identity| observed.borrow_mut().push(identity.to_string()));
            ui.show().unwrap();
            settle();

            for (index, name) in folder_names.into_iter().enumerate() {
                let entry = labelled(&ui, name);
                assert_sidebar_entry_visible(&ui, &entry, "AppWindow::folder-list", name);
                click_element(&ui, &entry);
                assert_eq!(
                    folders.borrow().last().copied(),
                    Some(index as i32),
                    "Pointer input must reach folder {name} at {width}x{height}, scale={scale}, chinese={chinese}"
                );
            }
            for (name, identity) in album_names.into_iter().zip(["71", "92"]) {
                let entry = labelled(&ui, name);
                assert_sidebar_entry_visible(&ui, &entry, "AppWindow::album-list", name);
                click_element(&ui, &entry);
                assert_eq!(
                    albums.borrow().last().map(String::as_str),
                    Some(identity),
                    "Pointer input must reach Album {name} at {width}x{height}, scale={scale}, chinese={chinese}"
                );
            }
            assert_eq!(&*folders.borrow(), &[0, 1]);
            assert_eq!(&*albums.borrow(), &["71", "92"]);
            ui.global::<crate::Theme>().set_ui_scale(1.0);
            ui.hide().unwrap();
        }
    }
}

fn assert_sidebar_entry_visible(ui: &AppWindow, entry: &ElementHandle, list_id: &str, name: &str) {
    let viewport = ui.window().size().to_logical(ui.window().scale_factor());
    let mut left: f32 = 0.0;
    let mut top: f32 = 0.0;
    let mut right = viewport.width;
    let mut bottom = viewport.height;
    for id in [
        "AppWindow::main-scroll",
        "AppWindow::library-sidebar",
        list_id,
    ] {
        let clip = element(ui, id);
        let origin = clip.absolute_position();
        let size = clip.size();
        assert!(
            size.width > 0.0 && size.height > 0.0,
            "{name} has a collapsed viewport {id}: {origin:?} + {size:?} in {viewport:?}"
        );
        left = left.max(origin.x);
        top = top.max(origin.y);
        right = right.min(origin.x + size.width);
        bottom = bottom.min(origin.y + size.height);
    }
    let origin = entry.absolute_position();
    let size = entry.size();
    assert!(
        size.width > 0.0
            && size.height > 0.0
            && origin.x >= left - 1.0
            && origin.y >= top - 1.0
            && origin.x + size.width <= right + 1.0
            && origin.y + size.height <= bottom + 1.0,
        "{name} is clipped: {origin:?} + {size:?}, visible sidebar/list intersection ({left}, {top})..({right}, {bottom}), window {viewport:?}"
    );
}

#[test]
fn folder_counts_keep_a_scrollbar_gutter_and_remain_clickable_when_sidebar_resizes() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        for sidebar_width in [198.0, 170.0, 400.0] {
            for overflow in [true, false] {
                let ui = window(chinese, 1440.0, 900.0);
                ui.set_sidebar_preferred_width(sidebar_width);
                let names = if chinese {
                    ["飞机与交通工具音效文件夹", "连续录制的航空发动机素材"]
                } else {
                    [
                        "Aircraft and vehicle sound effects",
                        "Continuous aviation engine recordings",
                    ]
                };
                // Synthetic five- and seven-digit counts exercise the scrollbar gutter.
                let counts = ["54321", "1000000"];
                ui.set_folders(ModelRc::new(VecModel::from(
                    (0..if overflow { 80 } else { 2 })
                        .map(|index| FolderRow {
                            name: if index < 2 {
                                names[index].into()
                            } else {
                                format!("Scroll folder {index}").into()
                            },
                            count: if index < 2 {
                                counts[index].into()
                            } else {
                                "9".into()
                            },
                            depth: if index == 1 { 30 } else { 0 },
                            ..Default::default()
                        })
                        .collect::<Vec<_>>(),
                )));
                let selections = Rc::new(RefCell::new(Vec::new()));
                let observed = selections.clone();
                ui.on_folder_requested(move |index| observed.borrow_mut().push(index));
                settle();
                near(ui.get_sidebar_width(), sidebar_width);
                let list = element(&ui, "AppWindow::folder-list");
                let list_origin = list.absolute_position();
                let list_right = list_origin.x + list.size().width;
                for hovered in [false, true] {
                    if hovered {
                        ui.window().dispatch_event(WindowEvent::PointerMoved {
                            position: slint::LogicalPosition::new(
                                list_right - 7.0,
                                list_origin.y + 8.0,
                            ),
                        });
                        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
                    }
                    let mut count_widths = Vec::new();
                    for (index, (name, count)) in names.into_iter().zip(counts).enumerate() {
                        let name_text = labelled(&ui, name);
                        let count_text = labelled(&ui, count);
                        let name_origin = name_text.absolute_position();
                        let count_origin = count_text.absolute_position();
                        let count_size = count_text.size();
                        let gutter = list_right - count_origin.x - count_size.width;
                        assert!(
                            name_origin.x >= list_origin.x - 1.0
                                && name_text.size().width >= 40.0 - 1.0
                                && name_origin.x + name_text.size().width <= count_origin.x - 3.0,
                            "Folder name overlaps its count or has less than 40px at {sidebar_width}px, chinese={chinese}, overflow={overflow}, scrollbar_hovered={hovered}"
                        );
                        assert!(
                            gutter >= 18.0 - 1.0,
                            "Folder count {count} has only {gutter}px of scrollbar gutter at {sidebar_width}px, chinese={chinese}, overflow={overflow}, scrollbar_hovered={hovered}; count {count_origin:?} + {count_size:?}, list right {list_right}"
                        );
                        assert!(count_size.width >= 28.0 - 1.0);
                        count_widths.push(count_size.width);
                        click_element(&ui, &count_text);
                        assert_eq!(
                            selections.borrow().last().copied(),
                            Some(index as i32),
                            "Clicking folder count {count} must select its folder"
                        );
                    }
                    assert!(
                        count_widths[1] >= count_widths[0] + 8.0,
                        "Seven-digit folder counts need more room than five-digit counts"
                    );
                }
                assert_eq!(&*selections.borrow(), &[0, 1, 0, 1]);
                let second_count = labelled(&ui, counts[1]);
                let before_scroll = second_count.absolute_position();
                ui.window().dispatch_event(WindowEvent::PointerScrolled {
                    position: slint::LogicalPosition::new(
                        before_scroll.x + second_count.size().width / 2.0,
                        before_scroll.y + second_count.size().height / 2.0,
                    ),
                    delta_x: 0.0,
                    delta_y: -34.0,
                });
                settle();
                let after_scroll = labelled(&ui, counts[1]).absolute_position();
                if overflow {
                    assert!(
                        after_scroll.y < before_scroll.y - 10.0,
                        "Wheel input over folder counts must scroll overflowing folders"
                    );
                } else {
                    near(after_scroll.y, before_scroll.y);
                }
                ui.hide().unwrap();
            }
        }
    }
}

#[test]
fn pane_dividers_follow_absolute_pointer_movement_in_both_languages() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = window(chinese, 2200.0, 900.0);
        near(ui.get_sidebar_width(), 198.0);
        near(ui.get_inspector_width(), 320.0);
        let results_before = element(&ui, "AppWindow::results-column").size().width;

        let left = begin_drag(&ui, divider_label(chinese, false));
        move_drag(&ui, left, 24.0);
        near(ui.get_sidebar_preferred_width(), 222.0);
        near(ui.get_sidebar_width(), 222.0);
        move_drag(&ui, left, 48.0);
        near(ui.get_sidebar_preferred_width(), 246.0);
        near(ui.get_sidebar_width(), 246.0);
        move_drag(&ui, left, 36.0);
        near(ui.get_sidebar_preferred_width(), 234.0);
        end_drag(&ui, left, 36.0);
        near(
            element(&ui, "AppWindow::results-column").size().width,
            results_before - 36.0,
        );

        let right = begin_drag(&ui, divider_label(chinese, true));
        move_drag(&ui, right, -24.0);
        near(ui.get_inspector_preferred_width(), 344.0);
        near(ui.get_inspector_width(), 344.0);
        move_drag(&ui, right, -48.0);
        near(ui.get_inspector_preferred_width(), 368.0);
        near(ui.get_inspector_width(), 368.0);
        move_drag(&ui, right, -36.0);
        near(ui.get_inspector_preferred_width(), 356.0);
        end_drag(&ui, right, -36.0);
        near(
            element(&ui, "AppWindow::results-column").size().width,
            results_before - 72.0,
        );
        assert_panes_fit(&ui, 2200.0);
        ui.hide().unwrap();
    }
}

#[test]
fn pane_drag_limits_preserve_the_results_width() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = window(chinese, 2200.0, 900.0);
        drag(&ui, divider_label(chinese, false), 2000.0);
        near(ui.get_sidebar_preferred_width(), 480.0);
        drag(&ui, divider_label(chinese, true), -2000.0);
        near(ui.get_inspector_preferred_width(), 600.0);
        drag(&ui, divider_label(chinese, false), -2000.0);
        near(ui.get_sidebar_preferred_width(), 170.0);
        drag(&ui, divider_label(chinese, true), 2000.0);
        near(ui.get_inspector_preferred_width(), 240.0);

        ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
        settle();
        drag(&ui, divider_label(chinese, false), 2000.0);
        assert_panes_fit(&ui, 1080.0);
        assert!(ui.get_sidebar_preferred_width() < 480.0);
        drag(&ui, divider_label(chinese, true), -2000.0);
        assert_panes_fit(&ui, 1080.0);
        assert!(ui.get_inspector_preferred_width() < 600.0);
        ui.hide().unwrap();
    }
}

#[test]
fn pane_preferences_restore_after_window_resize_and_details_toggle() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = window(chinese, 2200.0, 900.0);
        drag(&ui, divider_label(chinese, false), 2000.0);
        drag(&ui, divider_label(chinese, true), -2000.0);
        near(ui.get_sidebar_width(), 480.0);
        near(ui.get_inspector_width(), 600.0);

        ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
        settle();
        assert_panes_fit(&ui, 1080.0);
        assert!(ui.get_sidebar_width() < 480.0 || ui.get_inspector_width() < 600.0);
        near(ui.get_sidebar_preferred_width(), 480.0);
        near(ui.get_inspector_preferred_width(), 600.0);
        let narrowed_results = element(&ui, "AppWindow::results-column").size().width;

        click_label(&ui, if chinese { "详情" } else { "Details" });
        assert!(!ui.get_inspector_visible());
        assert!(
            ElementHandle::find_by_element_id(&ui, "AppWindow::details-sidebar")
                .next()
                .is_none()
        );
        assert!(element(&ui, "AppWindow::results-column").size().width > narrowed_results);
        near(ui.get_inspector_preferred_width(), 600.0);
        click_label(&ui, if chinese { "详情" } else { "Details" });
        assert!(ui.get_inspector_visible());
        assert_panes_fit(&ui, 1080.0);

        ui.window().set_size(slint::LogicalSize::new(2200.0, 900.0));
        settle();
        near(ui.get_sidebar_width(), 480.0);
        near(ui.get_inspector_width(), 600.0);
        near(ui.get_sidebar_preferred_width(), 480.0);
        near(ui.get_inspector_preferred_width(), 600.0);
        ui.hide().unwrap();
    }
}

#[test]
fn filter_page_and_result_tags_adapt_to_the_remaining_widths() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        let ui = window(chinese, 2200.0, 900.0);
        let tag = "Responsive results tag";
        ui.set_rows(ModelRc::new(VecModel::from(vec![AudioRow {
            id: "42".into(),
            name: "Responsive result.wav".into(),
            tags: tag.into(),
            ..Default::default()
        }])));
        let details_list = element(&ui, "AppWindow::results-panel").size().height;
        ui.set_filters_visible(true);
        settle();
        // Filters share the sidebar instead of pushing the sound list down.
        near(
            element(&ui, "AppWindow::results-panel").size().height,
            details_list,
        );
        drag(&ui, divider_label(chinese, false), 2000.0);
        drag(&ui, divider_label(chinese, true), -2000.0);
        assert!(
            ElementHandle::find_by_accessible_label(&ui, tag)
                .next()
                .is_some()
        );

        ui.window().set_size(slint::LogicalSize::new(1080.0, 700.0));
        settle();
        assert_panes_fit(&ui, 1080.0);
        assert!(
            ElementHandle::find_by_accessible_label(&ui, tag)
                .next()
                .is_none()
        );
        let bounds = |id: &str| {
            let pane = element(&ui, id);
            let origin = pane.absolute_position();
            (origin.x, origin.x + pane.size().width)
        };
        let (left, right) = bounds("AppWindow::results-column");
        let toolbar_labels = if chinese {
            ["筛选", "详情"]
        } else {
            ["Filters", "Details"]
        };
        for label in toolbar_labels {
            let control = labelled(&ui, label);
            let origin = control.absolute_position();
            assert!(
                origin.x >= left - 1.0 && origin.x + control.size().width <= right + 1.0,
                "{label} extends outside the narrowed results column"
            );
        }
        let (left, right) = bounds("AppWindow::details-sidebar");
        let filter_labels = if chinese {
            [
                "标签筛选：+ 包含，- 排除，回车添加",
                "颜色 #RRGGBB",
                "有标签",
                "重置",
                "应用",
            ]
        } else {
            [
                "Tag filter: + include, - exclude, Enter adds",
                "Color #RRGGBB",
                "Tagged only",
                "Reset",
                "Apply",
            ]
        };
        scroll_to_bottom(&ui, "AppWindow::filters-scroll");
        for label in filter_labels {
            let control = labelled(&ui, label);
            let origin = control.absolute_position();
            assert!(
                origin.x >= left - 1.0 && origin.x + control.size().width <= right + 1.0,
                "{label} extends outside the narrowed sidebar"
            );
        }
        // Selecting a sound keeps the filter page; the toolbar switch toggles the sidebar.
        ui.set_has_selection(true);
        ui.set_selected_id("42".into());
        settle();
        assert!(ui.get_inspector_visible() && ui.get_filters_visible());
        click_label(&ui, toolbar_labels[0]);
        assert!(!ui.get_inspector_visible());
        click_label(&ui, toolbar_labels[1]);
        assert!(ui.get_inspector_visible() && !ui.get_filters_visible());
        click_label(&ui, toolbar_labels[0]);
        assert!(ui.get_inspector_visible() && ui.get_filters_visible());
        ui.window().set_size(slint::LogicalSize::new(2200.0, 900.0));
        settle();
        assert!(
            ElementHandle::find_by_accessible_label(&ui, tag)
                .next()
                .is_some()
        );
        ui.hide().unwrap();
    }
}

#[test]
fn extracted_fields_precede_collapsed_raw_metadata_and_selection_resets_disclosure() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        for (width, height) in [(1440.0, 900.0), (1080.0, 700.0)] {
            let ui = window(chinese, width, height);
            ui.set_has_selection(true);
            ui.set_selected_id("42".into());
            ui.set_selected_name("Aircraft takeoff.wav".into());
            ui.set_selected_description("Merlin engine takeoff, exterior stereo recording".into());
            ui.set_selected_source_tags("aircraft, engine, takeoff".into());
            ui.set_selected_attributes("{\"SMED\":{\"Category\":\"Aircraft\"}}".into());
            settle();
            let tags = if chinese {
                "提取的标签"
            } else {
                "Extracted tags"
            };
            let description = if chinese {
                "源文件描述"
            } else {
                "Source description"
            };
            let raw = if chinese {
                "源文件 Metadata"
            } else {
                "Source Metadata"
            };
            let collapsed = if chinese {
                "+  原始 Metadata（含 SMED）"
            } else {
                "+  Source Metadata / SMED"
            };
            let expanded = if chinese {
                "−  原始 Metadata（含 SMED）"
            } else {
                "−  Source Metadata / SMED"
            };
            assert!(!ui.get_source_metadata_visible());
            assert!(
                ElementHandle::find_by_accessible_label(&ui, raw)
                    .next()
                    .is_none()
            );
            let scroll = element(&ui, "AppWindow::inspector-scroll");
            let scroll_origin = scroll.absolute_position();
            let scroll_size = scroll.size();
            let mut source_fields = Vec::new();
            for (label, value) in [
                (
                    description,
                    "Merlin engine takeoff, exterior stereo recording",
                ),
                (tags, "aircraft, engine, takeoff"),
            ] {
                let field = labelled_value(&ui, label);
                assert_eq!(field.accessible_value().as_deref(), Some(value));
                let origin = field.absolute_position();
                let size = field.size();
                assert!(
                    origin.x >= scroll_origin.x - 1.0
                        && origin.x + size.width <= scroll_origin.x + scroll_size.width + 1.0
                        && origin.y >= scroll_origin.y - 1.0
                        && origin.y + size.height <= scroll_origin.y + scroll_size.height + 1.0,
                    "{label} is clipped before scrolling at {width}x{height}: {origin:?} + {size:?}, viewport {scroll_origin:?} + {scroll_size:?}"
                );
                click_element(&ui, &field);
                crate::ui_tests::send_keys(
                    &ui,
                    &[slint::platform::Key::Control.into(), "a".into()],
                );
                crate::ui_tests::send_keys(&ui, &["replacement".into()]);
                assert_eq!(
                    labelled_value(&ui, label).accessible_value().as_deref(),
                    Some(value)
                );
                source_fields.push(field);
            }
            assert!(
                !ui.get_metadata_dirty(),
                "Viewing source fields must not create an edit"
            );
            scroll_to_bottom(&ui, "AppWindow::inspector-scroll");
            let disclosure = labelled(&ui, collapsed);
            for field in source_fields {
                assert!(
                    field.absolute_position().y + field.size().height
                        <= disclosure.absolute_position().y,
                    "Extracted source fields must precede the raw Metadata disclosure"
                );
            }
            click_label(&ui, collapsed);
            assert!(ui.get_source_metadata_visible());
            scroll_to_bottom(&ui, "AppWindow::inspector-scroll");
            assert!(
                ElementHandle::find_by_accessible_label(&ui, expanded)
                    .next()
                    .is_some()
            );
            assert_eq!(
                labelled_value(&ui, raw).accessible_value().as_deref(),
                Some("{\"SMED\":{\"Category\":\"Aircraft\"}}")
            );
            click_label(&ui, expanded);
            assert!(!ui.get_source_metadata_visible());
            assert!(
                ElementHandle::find_by_accessible_label(&ui, raw)
                    .next()
                    .is_none()
            );
            click_label(&ui, collapsed);
            assert!(ui.get_source_metadata_visible());
            ui.set_selected_id("43".into());
            settle();
            assert!(!ui.get_source_metadata_visible());
            assert!(
                ElementHandle::find_by_accessible_label(&ui, raw)
                    .next()
                    .is_none()
            );
            scroll_to_bottom(&ui, "AppWindow::inspector-scroll");
            assert!(
                ElementHandle::find_by_accessible_label(&ui, collapsed)
                    .next()
                    .is_some()
            );
            ui.hide().unwrap();
        }
    }
}

#[test]
fn selecting_another_file_resets_extracted_tags_scroll() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        for (width, height) in [(1440.0, 900.0), (1080.0, 700.0)] {
            for same_text in [false, true] {
                let ui = window(chinese, width, height);
                ui.set_has_selection(true);
                ui.set_selected_id("42".into());
                ui.set_selected_source_tags(long_tags("Aircraft engine").into());
                settle();
                let field = labelled_value(&ui, source_tags_label(chinese));
                let initial = field.absolute_position();
                ui.window().dispatch_event(WindowEvent::PointerScrolled {
                    position: slint::LogicalPosition::new(initial.x + 10.0, initial.y + 8.0),
                    delta_x: 0.0,
                    delta_y: -200.0,
                });
                settle();
                assert!(field.absolute_position().y < initial.y - 100.0);
                ui.set_selected_id("43".into());
                ui.set_selected_source_tags(
                    long_tags(if same_text {
                        "Aircraft engine"
                    } else {
                        "New recording"
                    })
                    .into(),
                );
                settle();
                near(field.absolute_position().y, initial.y);
                assert!(!ui.get_metadata_dirty());
                ui.hide().unwrap();
            }
        }
    }
}

#[test]
fn keyboard_navigation_keeps_long_extracted_tags_visible() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        for (width, height) in [(1440.0, 900.0), (1080.0, 700.0)] {
            let ui = window(chinese, width, height);
            ui.set_has_selection(true);
            ui.set_selected_id("42".into());
            let tags = long_tags("Aircraft engine");
            ui.set_selected_source_tags(tags.clone().into());
            settle();
            let field = labelled_value(&ui, source_tags_label(chinese));
            let viewport = element(&ui, "AppWindow::source-tags");
            let initial = field.absolute_position();
            // Native text navigation may also scroll the enclosing details pane.
            // Measure the tag input inside its own viewport, independently of it.
            let initial_offset = initial.y - viewport.absolute_position().y;
            click_at(
                &ui,
                slint::LogicalPosition::new(initial.x + 10.0, initial.y + 8.0),
            );
            crate::ui_tests::send_keys(
                &ui,
                &[
                    slint::platform::Key::Control.into(),
                    text_document_end_key(),
                ],
            );
            settle();
            assert!(
                field.absolute_position().y - viewport.absolute_position().y
                    < initial_offset - 100.0
            );
            crate::ui_tests::send_keys(
                &ui,
                &[
                    slint::platform::Key::Control.into(),
                    text_document_start_key(),
                ],
            );
            settle();
            near(
                field.absolute_position().y - viewport.absolute_position().y,
                initial_offset,
            );
            crate::ui_tests::send_keys(
                &ui,
                &[
                    slint::platform::Key::Control.into(),
                    slint::platform::Key::Shift.into(),
                    text_document_end_key(),
                ],
            );
            settle();
            assert!(
                field.absolute_position().y - viewport.absolute_position().y
                    < initial_offset - 100.0
            );
            ui.set_selected_id("43".into());
            settle();
            near(
                field.absolute_position().y - viewport.absolute_position().y,
                initial_offset,
            );
            assert_eq!(field.accessible_value().as_deref(), Some(tags.as_str()));
            assert!(!ui.get_metadata_dirty());
            ui.hide().unwrap();
        }
    }
}

#[test]
fn selecting_another_file_resets_description_scroll_and_keyboard_selection() {
    i_slint_backend_testing::init_no_event_loop();
    for chinese in [false, true] {
        for (width, height) in [(1440.0, 900.0), (1080.0, 700.0)] {
            let ui = window(chinese, width, height);
            ui.set_has_selection(true);
            ui.set_selected_id("42".into());
            ui.set_selected_description(long_tags("Long source description").into());
            settle();
            let field = element(&ui, "AppWindow::source-description");
            let input = field
                .query_descendants()
                .match_type_name("TextInput")
                .find_first()
                .unwrap();
            let initial = input.absolute_position();
            click_at(
                &ui,
                slint::LogicalPosition::new(initial.x + 10.0, initial.y + 8.0),
            );
            crate::ui_tests::send_keys(
                &ui,
                &[
                    slint::platform::Key::Control.into(),
                    slint::platform::Key::Shift.into(),
                    text_document_end_key(),
                ],
            );
            settle();
            assert!(input.absolute_position().y < initial.y - 100.0);
            ui.set_selected_id("43".into());
            settle();
            near(input.absolute_position().y, initial.y);
            assert!(!ui.get_metadata_dirty());
            ui.hide().unwrap();
        }
    }
}

// Slint's normalized Control modifier uses vertical arrows for text boundaries
// on macOS; other native platforms use Home/End for the same editing actions.
fn text_document_start_key() -> slint::SharedString {
    if cfg!(target_os = "macos") {
        slint::platform::Key::UpArrow
    } else {
        slint::platform::Key::Home
    }
    .into()
}

fn text_document_end_key() -> slint::SharedString {
    if cfg!(target_os = "macos") {
        slint::platform::Key::DownArrow
    } else {
        slint::platform::Key::End
    }
    .into()
}

fn long_tags(prefix: &str) -> String {
    (0..60)
        .map(|index| format!("{prefix} tag {index}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn source_tags_label(chinese: bool) -> &'static str {
    if chinese {
        "提取的标签"
    } else {
        "Extracted tags"
    }
}

fn window(chinese: bool, width: f32, height: f32) -> AppWindow {
    let ui = AppWindow::new().unwrap();
    ui.set_chinese(chinese);
    ui.window().set_size(slint::LogicalSize::new(width, height));
    ui.show().unwrap();
    settle();
    ui
}

fn settle() {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

fn near(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() <= 1.0,
        "Expected {expected}px, got {actual}px"
    );
}

fn element(ui: &AppWindow, id: &str) -> ElementHandle {
    ElementHandle::find_by_element_id(ui, id)
        .next()
        .unwrap_or_else(|| panic!("Missing element: {id}"))
}

fn labelled(ui: &AppWindow, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(ui, label)
        .next()
        .unwrap_or_else(|| panic!("Missing accessible element: {label}"))
}

fn labelled_value(ui: &AppWindow, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(ui, label)
        .find(|element| element.accessible_value().is_some())
        .unwrap_or_else(|| panic!("Missing source field: {label}"))
}

fn divider_label(chinese: bool, right: bool) -> &'static str {
    match (chinese, right) {
        (false, false) => "Resize library sidebar",
        (false, true) => "Resize sound details",
        (true, false) => "调整素材库侧栏宽度",
        (true, true) => "调整声音详情宽度",
    }
}

fn begin_drag(ui: &AppWindow, label: &str) -> slint::LogicalPosition {
    let divider = labelled(ui, label);
    near(divider.size().width, 12.0);
    let origin = divider.absolute_position();
    let size = divider.size();
    let position =
        slint::LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    position
}

fn move_drag(ui: &AppWindow, start: slint::LogicalPosition, delta: f32) {
    ui.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(start.x + delta, start.y),
    });
    settle();
}

fn end_drag(ui: &AppWindow, start: slint::LogicalPosition, delta: f32) {
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position: slint::LogicalPosition::new(start.x + delta, start.y),
        button: slint::platform::PointerEventButton::Left,
    });
    settle();
}

fn drag(ui: &AppWindow, label: &str, delta: f32) {
    let start = begin_drag(ui, label);
    move_drag(ui, start, delta);
    end_drag(ui, start, delta);
}

fn assert_panes_fit(ui: &AppWindow, width: f32) {
    let left = element(ui, "AppWindow::library-sidebar");
    let center = element(ui, "AppWindow::results-column");
    let right = element(ui, "AppWindow::details-sidebar");
    assert!(left.size().width >= 170.0 - 1.0);
    assert!(right.size().width >= 240.0 - 1.0);
    assert!(center.size().width >= 520.0 - 1.0);
    assert!(left.absolute_position().x >= 0.0);
    assert!(left.absolute_position().x + left.size().width <= center.absolute_position().x);
    assert!(center.absolute_position().x + center.size().width <= right.absolute_position().x);
    assert!(right.absolute_position().x + right.size().width <= width);
    near(left.size().width, ui.get_sidebar_width());
    near(right.size().width, ui.get_inspector_width());
}

fn click_label(ui: &AppWindow, label: &str) {
    click_element(ui, &labelled(ui, label));
}

fn click_element(ui: &AppWindow, element: &ElementHandle) {
    let origin = element.absolute_position();
    let size = element.size();
    let position =
        slint::LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0);
    click_at(ui, position);
}

fn click_at(ui: &AppWindow, position: slint::LogicalPosition) {
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: slint::platform::PointerEventButton::Left,
    });
    settle();
}

fn scroll_to_bottom(ui: &AppWindow, id: &str) {
    let scroll = element(ui, id);
    let origin = scroll.absolute_position();
    let position =
        slint::LogicalPosition::new(origin.x + scroll.size().width - 2.0, origin.y + 8.0);
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x: 0.0,
        delta_y: -5000.0,
    });
    settle();
}
