// app/audiolib-desktop/src/table_tests.rs
//! Real table input and isolated controller checks cover paging and durable preferences.
use super::*;
use i_slint_backend_testing::ElementHandle;
use slint::platform::{PointerEventButton, WindowEvent};
use std::{cell::Cell, rc::Rc};

const ROOT_ENV: &str = "AUDIOLIB_TABLE_TEST_ROOT";

#[test]
fn table_headers_sort_and_dividers_track_absolute_pointer_with_limits_and_reset() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1800.0, 900.0));
    ui.show().unwrap();
    settle();
    let searches = Rc::new(Cell::new(0));
    let requested = searches.clone();
    ui.on_filter_requested(move || requested.set(requested.get() + 1));
    for (index, id, label) in [
        (1, "table-name-header", "Sort by filename"),
        (2, "table-duration-header", "Sort by duration"),
        (3, "table-format-header", "Sort by sample rate"),
        (4, "table-channels-header", "Sort by channels"),
        (5, "table-tags-header", "Sort by tags"),
    ] {
        let header = element(&ui, id);
        assert_eq!(header.accessible_label().as_deref(), Some(label));
        click(&ui, midpoint(&header));
        settle();
        assert_eq!(ui.get_sort_index(), index);
        assert!(!ui.get_sort_descending());
        assert_eq!(header.accessible_value().as_deref(), Some("ascending"));
        click(&ui, midpoint(&header));
        settle();
        assert!(ui.get_sort_descending());
        assert_eq!(header.accessible_value().as_deref(), Some("descending"));
        // A third click returns to the default order (relevance, else filename).
        click(&ui, midpoint(&header));
        settle();
        assert_eq!((ui.get_sort_index(), ui.get_sort_descending()), (0, false));
        assert_eq!(header.accessible_value().as_deref(), Some(""));
    }
    assert_eq!(
        searches.get(),
        15,
        "Each header click admits a new library sort"
    );

    // Filename must shrink even when the window leaves plenty of unused table space.
    let old_duration_x = element(&ui, "table-duration-header").absolute_position().x;
    drag(&ui, "table-name-divider", -100.0);
    settle();
    near(ui.get_table_name_width(), 260.0);
    near(element(&ui, "table-name-header").size().width, 252.0);
    near(
        element(&ui, "table-duration-header").absolute_position().x,
        old_duration_x - 100.0,
    );

    // Two moves retain the original absolute press coordinate as the divider moves.
    let start = midpoint(&element(&ui, "table-duration-divider"));
    press(&ui, start);
    move_to(&ui, slint::LogicalPosition::new(start.x + 35.0, start.y));
    settle();
    near(ui.get_table_duration_width(), 115.0);
    move_to(&ui, slint::LogicalPosition::new(start.x + 85.0, start.y));
    settle();
    near(ui.get_table_duration_width(), 165.0);
    release(&ui, slint::LogicalPosition::new(start.x + 85.0, start.y));

    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(600));
    drag(&ui, "table-duration-divider", -3000.0);
    settle();
    near(ui.get_table_duration_width(), 60.0);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(600));
    drag(&ui, "table-duration-divider", 3000.0);
    settle();
    near(ui.get_table_duration_width(), 2000.0);

    // The resized divider is outside this narrow viewport, so bring it back for reset.
    ui.set_table_duration_width(150.0);
    settle();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(600));
    let position = midpoint(&element(&ui, "table-duration-divider"));
    click(&ui, position);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(40));
    click(&ui, position);
    settle();
    near(ui.get_table_duration_width(), 80.0);
    assert_eq!(
        searches.get(),
        15,
        "Resizing must retain the current result order"
    );
    ui.hide().unwrap();
}

#[test]
fn resized_table_cells_follow_headers_during_horizontal_scroll_and_menu_reset() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(2400.0, 900.0));
    ui.set_rows(ModelRc::new(VecModel::from(
        (0..5)
            .map(|index| AudioRow {
                id: index.to_string().into(),
                name: format!("Long field recording {index}.wav").into(),
                duration: "12.34s".into(),
                format: "48.0k / 24b".into(),
                channels: "2".into(),
                tags: "metal, impact, 城市".into(),
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    ui.show().unwrap();
    settle();
    assert_cell_alignment(&ui);
    ui.set_table_name_width(1000.0);
    ui.set_table_tags_width(340.0);
    settle();
    assert_cell_alignment(&ui);
    // Introspection exposes only columns intersecting the viewport. First check
    // all resized columns in a wide window, then make the same table overflow.
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    settle();
    let before = element(&ui, "table-name-header").absolute_position().x;
    let viewport = element(&ui, "result-list");
    let origin = viewport.absolute_position();
    // Scroll at a point known to be inside the left edge of the visible results.
    // An overflowing ScrollView's introspected size may describe its content,
    // whose midpoint can lie beyond the clipped table and hit the inspector.
    let position = slint::LogicalPosition::new(origin.x + 24.0, origin.y + 24.0);
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x: -900.0,
        delta_y: 0.0,
    });
    // Flush viewport change handlers before inspecting the header geometry.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(250));
    let after = element(&ui, "table-name-header").absolute_position().x;
    assert!(
        after < before - 10.0,
        "Overflowing columns must be horizontally scrollable"
    );
    assert_cell_alignment(&ui);

    // The column menu restores every width and returns the viewport to the first column.
    let customize =
        ElementHandle::find_by_accessible_label(&ui, "Column widths, Waveforms and sorting")
            .next()
            .unwrap();
    click(&ui, midpoint(&customize));
    settle();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(350));
    let reset = ElementHandle::find_by_accessible_label(&ui, "Reset column widths")
        .next()
        .expect("Reset column widths");
    assert!(reset.size().width > 0.0 && reset.size().height > 0.0);
    // The testing API's pointer helpers use map_to_window, which omits child-popup
    // offsets. Route real keyboard input through the opened menu instead: its first
    // entry toggles Waveforms, so select the second entry (Reset widths) with two Down events.
    crate::ui_tests::send_keys(
        &ui,
        &[
            slint::platform::Key::DownArrow.into(),
            slint::platform::Key::DownArrow.into(),
            slint::platform::Key::Return.into(),
        ],
    );
    settle();
    near(ui.get_table_name_width(), 360.0);
    near(ui.get_table_duration_width(), 80.0);
    near(ui.get_table_format_width(), 100.0);
    near(ui.get_table_channels_width(), 64.0);
    near(ui.get_table_tags_width(), 160.0);
    near(
        element(&ui, "table-name-header").absolute_position().x,
        before,
    );
    assert_cell_alignment(&ui);
    ui.hide().unwrap();
}

#[test]
fn empty_table_keeps_horizontal_extent_and_right_columns_operable_across_model_changes() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    ui.set_table_name_width(1000.0);
    ui.set_table_tags_width(340.0);
    let rows = Rc::new(VecModel::<AudioRow>::default());
    ui.set_rows(ModelRc::new(rows.clone()));
    ui.show().unwrap();
    settle();
    let before = element(&ui, "table-name-header").absolute_position().x;
    // Saturate the scroll range: the album column extends beyond the Tags divider.
    scroll_table(&ui, "empty-result-list", -10_000.0);
    let empty_offset = element(&ui, "table-name-header").absolute_position().x - before;
    assert!(
        empty_offset < -600.0,
        "An initially empty table must scroll"
    );
    assert_eq!(ui.get_rows().row_count(), 0);

    // The rightmost real header and divider must accept pointer input without rows.
    click(&ui, midpoint(&element(&ui, "table-tags-header")));
    settle();
    assert_eq!(ui.get_sort_index(), 5);
    assert!(!ui.get_sort_descending());
    drag(&ui, "table-tags-divider", -40.0);
    settle();
    near(ui.get_table_tags_width(), 300.0);
    crate::ui_tests::send_keys(&ui, &[slint::platform::Key::LeftArrow.into()]);
    near(ui.get_table_tags_width(), 284.0);
    settle();
    crate::ui_tests::send_keys(&ui, &[slint::platform::Key::RightArrow.into()]);
    near(ui.get_table_tags_width(), 300.0);
    settle();
    scroll_table(&ui, "empty-result-list", -10_000.0);
    let resized_offset = element(&ui, "table-name-header").absolute_position().x - before;
    near(resized_offset, empty_offset + 40.0);

    // Mutate the same model to exercise length changes, not only model replacement.
    let row = AudioRow {
        id: "1".into(),
        name: "Field recording.wav".into(),
        duration: "12.34s".into(),
        format: "48.0k / 24b".into(),
        channels: "2".into(),
        tags: "metal, impact, 城市".into(),
        ..Default::default()
    };
    rows.push(row.clone());
    settle();
    near(
        element(&ui, "table-name-header").absolute_position().x - before,
        resized_offset,
    );
    assert_cell_alignment(&ui);
    scroll_table(&ui, "result-list", 240.0);
    let populated_offset = element(&ui, "table-name-header").absolute_position().x - before;
    assert!(populated_offset > resized_offset + 100.0);

    rows.set_vec(Vec::new());
    settle();
    near(
        element(&ui, "table-name-header").absolute_position().x - before,
        populated_offset,
    );
    assert_eq!(ui.get_rows().row_count(), 0);
    scroll_table(&ui, "empty-result-list", -10_000.0);
    near(
        element(&ui, "table-name-header").absolute_position().x - before,
        resized_offset,
    );
    click(&ui, midpoint(&element(&ui, "table-tags-header")));
    settle();
    assert!(ui.get_sort_descending());
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(600));
    drag(&ui, "table-tags-divider", -20.0);
    settle();
    near(ui.get_table_tags_width(), 280.0);

    rows.push(row.clone());
    settle();
    near(
        element(&ui, "table-name-header").absolute_position().x - before,
        resized_offset + 20.0,
    );
    assert_cell_alignment(&ui);
    scroll_table(&ui, "result-list", 10_000.0);
    near(
        element(&ui, "table-name-header").absolute_position().x,
        before,
    );
    ui.window().set_size(slint::LogicalSize::new(2400.0, 900.0));
    settle();
    assert_cell_alignment(&ui);

    // Fractional stored widths must survive the same switch and range checks.
    ui.set_table_name_width(1000.5);
    ui.set_table_tags_width(280.25);
    ui.window().set_size(slint::LogicalSize::new(1440.0, 900.0));
    rows.set_vec(Vec::new());
    settle();
    scroll_table(&ui, "empty-result-list", -10_000.0);
    let fractional_offset = element(&ui, "table-name-header").absolute_position().x - before;
    assert!(fractional_offset < -600.0);
    rows.push(row.clone());
    settle();
    near(
        element(&ui, "table-name-header").absolute_position().x - before,
        fractional_offset,
    );
    assert_cell_alignment(&ui);

    // Narrow columns have no horizontal range in either view. The fixed empty
    // overlay still accepts its clear action over the independent viewport.
    rows.set_vec(Vec::new());
    ui.set_table_name_width(120.5);
    ui.set_table_tags_width(160.25);
    settle();
    scroll_table(&ui, "empty-result-list", -10_000.0);
    near(
        element(&ui, "table-name-header").absolute_position().x,
        before,
    );
    let filters = Rc::new(Cell::new(0));
    let requested = filters.clone();
    ui.on_reset_filters(move || requested.set(requested.get() + 1));
    ui.set_query("empty".into());
    let clear = ElementHandle::find_by_accessible_label(&ui, "Clear search & filters")
        .next()
        .unwrap();
    click(&ui, midpoint(&clear));
    settle();
    assert_eq!(ui.get_query(), "");
    assert_eq!(filters.get(), 1);
    rows.push(row.clone());
    settle();
    scroll_table(&ui, "result-list", -10_000.0);
    near(
        element(&ui, "table-name-header").absolute_position().x,
        before,
    );
    assert_cell_alignment(&ui);
    ui.set_table_name_width(1000.5);
    ui.set_table_tags_width(280.25);
    rows.set_vec(Vec::new());
    settle();
    near(
        element(&ui, "table-name-header").absolute_position().x,
        before,
    );
    scroll_table_for(&ui, "empty-result-list", -10_000.0, 60);
    // The public PointerScrolled API uses Slint's immediate (Cancelled) phase.
    // Rapid model switches must preserve that input's offset in both views.
    let moving_offset = element(&ui, "table-name-header").absolute_position().x - before;
    near(moving_offset, fractional_offset);
    rows.push(row.clone());
    settle();
    near(
        element(&ui, "table-name-header").absolute_position().x - before,
        moving_offset,
    );
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(60));
    near(
        element(&ui, "table-name-header").absolute_position().x - before,
        moving_offset,
    );
    rows.set_vec(Vec::new());
    settle();
    near(
        element(&ui, "table-name-header").absolute_position().x - before,
        moving_offset,
    );
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(250));
    near(
        element(&ui, "table-name-header").absolute_position().x - before,
        moving_offset,
    );
    ui.hide().unwrap();
}

#[test]
fn table_controller_restores_full_library_sort_and_flushes_latest_layout() {
    let root = std::env::temp_dir().join(format!(
        "audiolib-table-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "controller::table_tests::child_table_controller_preferences",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(ROOT_ENV, &root)
        .env("AUDIOLIB_DATA_DIR", root.join("preferences"))
        .output()
        .unwrap();
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    if root.exists() {
        std::fs::remove_dir_all(&root).unwrap();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "Table controller regression failed: {}\n{stdout}\n{stderr}",
        output.status
    );
}

#[test]
#[ignore = "Slint event-loop/controller check invoked in an isolated child process"]
fn child_table_controller_preferences() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = PathBuf::from(std::env::var_os(ROOT_ENV).unwrap());
    let sounds = root.join("sounds");
    std::fs::create_dir_all(&sounds).unwrap();
    let mut wav = Vec::new();
    wav.extend(b"RIFF");
    wav.extend(38u32.to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(48_000u32.to_le_bytes());
    wav.extend(96_000u32.to_le_bytes());
    wav.extend(2u16.to_le_bytes());
    wav.extend(16u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(2u32.to_le_bytes());
    wav.extend(0i16.to_le_bytes());
    for index in 0..1005 {
        std::fs::write(sounds.join(format!("Sound {index:04}.wav")), &wav).unwrap();
    }
    let mut config = AppConfig::load_in(root.join("preferences")).unwrap();
    config.language = "cn".into();
    config.audition_play_on_single_click = true;
    config.audio_table = AudioTablePreferences {
        name_width: 280.0,
        duration_width: 70.0,
        format_width: 90.0,
        channels_width: 55.0,
        tags_width: 150.0,
        order_by: SortOrder::Filename,
        descending: true,
    };
    let playback = PlaybackConfig {
        volume: 0.25,
        ..Default::default()
    };
    config.playback = serde_json::to_value(&playback).unwrap();
    config.save().unwrap();
    let provider = Arc::new(SqliteProvider::open(&config.paths.database).unwrap());
    ScanService::new(provider.clone())
        .scan(
            &sounds,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    // This fixture exercises scrolling and preferences. On-demand Waveform
    // derivation would add unrelated provider revisions while facets settle.
    ui.set_list_waveforms_enabled(false);
    ui.window().set_size(slint::LogicalSize::new(1800.0, 900.0));
    let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
    assert!(ui.get_chinese());
    assert!(ui.get_audition_play_on_single_click());
    near(ui.get_table_name_width(), 280.0);
    near(ui.get_table_duration_width(), 70.0);
    assert_eq!(ui.get_sort_index(), 1);
    assert!(ui.get_sort_descending());

    let phase = Rc::new(Cell::new(0));
    let range_requests = Rc::new(Cell::new(0));
    let cancel_next_range = Rc::new(Cell::new(false));
    let range_superseded = Rc::new(Cell::new(false));
    // Keep the production range handler, and supersede the last queued range
    // synchronously so the test cannot miss a fast local query's pending phase.
    let range_controller = controller.clone();
    let range_ui = ui.as_weak();
    let observed_requests = range_requests.clone();
    let should_supersede = cancel_next_range.clone();
    let superseded = range_superseded.clone();
    ui.on_result_range_requested(move |direction| {
        let ui = range_ui.upgrade().unwrap();
        let before = range_controller.request.lock().unwrap().clone();
        if !range_controller.extend_result_window(&ui, direction) {
            return;
        }
        observed_requests.set(observed_requests.get() + 1);
        assert!(ui.get_searching());
        let displayed = range_controller.request.lock().unwrap().clone();
        assert_eq!(
            (displayed.offset, displayed.limit),
            (before.offset, before.limit)
        );
        if should_supersede.replace(false) {
            let pending_generation = range_controller.search_generation.load(Ordering::Acquire);
            click(&ui, midpoint(&element(&ui, "table-name-header")));
            click(&ui, midpoint(&element(&ui, "table-name-header")));
            assert!(
                range_controller.search_generation.load(Ordering::Acquire) > pending_generation,
                "Sorting must invalidate the pending continuous-scroll generation"
            );
            superseded.set(true);
        }
    });
    let anchor_before_wheel = Rc::new(Cell::new(0.0));
    let timer_phase = phase.clone();
    let timer_controller = controller.clone();
    let weak = ui.as_weak();
    let preferences = root.join("preferences");
    let saved_preferences = preferences.clone();
    let started = Instant::now();
    let mut layout_model: Option<(ModelRc<AudioRow>, usize)> = None;
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(10),
        move || {
            let ui = weak.upgrade().unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(25),
                "Table phase {} timed out: rows={}, request={:?}, searching={}, restoring={}, range_requests={}, publication={:?}, search_generation={}, search_queue={}, cancelled={:?}",
                timer_phase.get(),
                ui.get_rows().row_count(),
                {
                    let request = timer_controller.request.lock().unwrap();
                    (request.offset, request.limit)
                },
                ui.get_searching(),
                ui.get_task_restoring(),
                range_requests.get(),
                timer_controller.search_publication.lock().unwrap().as_ref().map(|state| {
                    let page = match state.page {
                        SearchPagePhase::Pending => "pending",
                        SearchPagePhase::Published => "published",
                        SearchPagePhase::Failed => "failed",
                    };
                    (page, state.model_published)
                }),
                timer_controller.search_generation.load(Ordering::Acquire),
                match timer_controller.search_job.try_lock() {
                    Ok(queued) => format!("{:?}", queued.as_ref().map(|job| {
                        (job.generation, job.request.offset, job.request.limit, job.cancel.load(Ordering::Acquire))
                    })),
                    Err(error) => error.to_string(),
                },
                timer_controller.search_cancel.try_lock()
                    .map(|cancel| cancel.load(Ordering::Acquire))
                    .map_err(|error| error.to_string())
            );
            // The system-time testing loop does not render. Instantiate each new
            // result range once; traversing the full tree every tick starves posted
            // worker replies behind the status timer's event-loop messages.
            let model = ui.get_rows();
            let count = model.row_count();
            if layout_model
                .as_ref()
                .is_none_or(|(previous, previous_count)| previous != &model || *previous_count != count)
            {
                let _ = ElementHandle::find_by_element_id(&ui, "AppWindow::table-name-cell").next();
                layout_model = Some((model, count));
            }
            if timer_phase.get() < 12
                && (!page_published(&timer_controller) || ui.get_task_restoring())
            {
                return;
            }
            match timer_phase.get() {
                0 => {
                    assert_names(&ui, (805..1005).rev());
                    click(&ui, midpoint(&element(&ui, "table-name-header")));
                    timer_phase.set(1);
                }
                1 => {
                    // From descending, the header returns to the default order, which
                    // without a query is filename ascending.
                    assert_eq!((ui.get_sort_index(), ui.get_sort_descending()), (0, false));
                    assert_names(&ui, 0..200);
                    prepare_result_window_edge(&ui, true);
                    timer_phase.set(2);
                }
                2 | 4 | 6 | 8 | 10 => {
                    // Programmatic restoration prepares the fixture; only real wheel
                    // movement may request the adjoining portion of the result range.
                    anchor_before_wheel.set(result_window_anchor(&ui, &timer_controller));
                    let direction = if timer_phase.get() == 8 { 1.0 } else { -1.0 };
                    for _ in 0..2 {
                        wheel_results(&ui, direction * 120.0);
                    }
                    timer_phase.set(timer_phase.get() + 1);
                }
                3 => {
                    if ui.get_rows().row_count() != 400 {
                        return;
                    }
                    assert_result_window(
                        &ui,
                        &timer_controller,
                        0,
                        400,
                        anchor_before_wheel.get(),
                        timer_phase.get(),
                    );
                    assert_eq!(
                        range_requests.get(),
                        1,
                        "A wheel burst must queue one range"
                    );
                    prepare_result_window_edge(&ui, true);
                    timer_phase.set(4);
                }
                5 => {
                    if ui.get_rows().row_count() != 600 {
                        return;
                    }
                    assert_result_window(
                        &ui,
                        &timer_controller,
                        0,
                        600,
                        anchor_before_wheel.get(),
                        timer_phase.get(),
                    );
                    assert_eq!(range_requests.get(), 2);
                    prepare_result_window_edge(&ui, true);
                    timer_phase.set(6);
                }
                7 => {
                    if timer_controller.request.lock().unwrap().offset != 200 {
                        return;
                    }
                    assert_result_window(
                        &ui,
                        &timer_controller,
                        200,
                        600,
                        anchor_before_wheel.get(),
                        timer_phase.get(),
                    );
                    assert_eq!(range_requests.get(), 3);
                    prepare_result_window_edge(&ui, false);
                    timer_phase.set(8);
                }
                9 => {
                    if timer_controller.request.lock().unwrap().offset != 0 {
                        return;
                    }
                    assert_result_window(
                        &ui,
                        &timer_controller,
                        0,
                        600,
                        anchor_before_wheel.get(),
                        timer_phase.get(),
                    );
                    assert_eq!(range_requests.get(), 4);
                    // The next wheel queues another downward window. Its handler
                    // immediately sorts twice before that generation can publish.
                    cancel_next_range.set(true);
                    prepare_result_window_edge(&ui, true);
                    timer_phase.set(10);
                }
                11 => {
                    if !range_superseded.get() {
                        return;
                    }
                    assert_eq!(range_requests.get(), 5);
                    assert!(ui.get_sort_descending());
                    let request = timer_controller.request.lock().unwrap().clone();
                    assert_eq!((request.offset, request.limit), (0, 200));
                    assert_names(&ui, (805..1005).rev());
                    drag(&ui, "table-duration-divider", 40.0);
                    near(ui.get_table_duration_width(), 110.0);
                    timer_phase.set(12);
                }
                12 => {
                    let saved = AppConfig::load_in(saved_preferences.clone()).unwrap();
                    if saved.audio_table.duration_width != 110.0
                        || saved.audio_table.order_by != SortOrder::Filename
                        || !saved.audio_table.descending
                    {
                        return;
                    }
                    assert_eq!(saved.language, "cn");
                    assert_eq!(saved.playback["volume"], serde_json::json!(0.25));
                    assert!(saved.audition_play_on_single_click);
                    assert_names(&ui, (805..1005).rev());
                    timer_phase.set(13);
                    slint::quit_event_loop().unwrap();
                }
                // Quit is queued; an already-ready timer callback can run once
                // more before the event loop returns. Completion is idempotent.
                13 => {}
                _ => unreachable!(),
            }
        },
    );
    ui.show().unwrap();
    slint::run_event_loop().unwrap();
    timer.stop();
    assert_eq!(
        phase.get(),
        13,
        "Sort and resize requests must save before shutdown"
    );

    // A busy disk/config writer must never make a table callback wait. The final
    // admitted layout still has to survive closing before the worker can save it.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let held = Arc::new(AtomicBool::new(false));
    let writer_held = held.clone();
    let writer = controller.clone();
    let slow_writer = std::thread::spawn(move || {
        let _config = writer.config.lock().unwrap();
        writer_held.store(true, Ordering::Release);
        ready_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !writer.shutdown.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        writer_held.store(false, Ordering::Release);
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    for width in [200.0, 240.0, 280.0] {
        ui.set_table_tags_width(width);
        ui.invoke_table_layout_changed();
    }
    let returned_while_locked = held.load(Ordering::Acquire);
    controller.shutdown();
    slow_writer.join().unwrap();
    let saved = AppConfig::load_in(preferences).unwrap();
    assert_eq!(saved.audio_table.tags_width, 280.0);
    assert_eq!(saved.audio_table.duration_width, 110.0);
    assert_eq!(saved.audio_table.order_by, SortOrder::Filename);
    assert!(saved.audio_table.descending);
    assert_eq!(saved.language, "cn");
    assert_eq!(saved.playback["volume"], serde_json::json!(0.25));
    assert!(saved.audition_play_on_single_click);
    assert!(
        returned_while_locked,
        "Table callback waited for configuration I/O"
    );
    ui.hide().unwrap();
}

fn assert_names(ui: &AppWindow, indices: impl Iterator<Item = usize>) {
    let expected = indices
        .map(|index| format!("Sound {index:04}.wav"))
        .collect::<Vec<_>>();
    let actual = ui
        .get_rows()
        .iter()
        .map(|row| row.name.to_string())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

fn prepare_result_window_edge(ui: &AppWindow, down: bool) {
    let height = element(ui, "result-list").size().height;
    let y = if down {
        -(ui.get_rows().row_count() as f32 * ui.get_task_result_row_height() - height * 2.0)
    } else {
        -height
    };
    ui.invoke_restore_search_task_viewport(
        y,
        ui.get_task_result_scroll_x(),
        ui.get_task_folder_scroll_y(),
        ui.get_task_filter_scroll_y(),
        ui.get_task_inspector_scroll_y(),
    );
}

fn wheel_results(ui: &AppWindow, delta_y: f32) {
    let origin = element(ui, "result-list").absolute_position();
    let position = slint::LogicalPosition::new(origin.x + 40.0, origin.y + 40.0);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x: 0.0,
        delta_y,
    });
}

fn result_window_anchor(ui: &AppWindow, controller: &Controller) -> f32 {
    controller.request.lock().unwrap().offset as f32 * ui.get_task_result_row_height()
        - ui.get_task_result_scroll_y()
}

fn assert_result_window(
    ui: &AppWindow,
    controller: &Controller,
    offset: usize,
    limit: usize,
    anchor_before: f32,
    phase: i32,
) {
    let request = controller.request.lock().unwrap().clone();
    assert_eq!((request.offset, request.limit), (offset, limit));
    assert_names(ui, offset..offset + limit);
    let y = ui.get_task_result_scroll_y();
    let anchor_after = result_window_anchor(ui, controller);
    assert!(
        y < -100.0,
        "Prefetch must retain a scrolled viewport: phase={phase}, offset={offset}, limit={limit}, y={y}, anchor_before={anchor_before}, anchor_after={anchor_after}, restoring={}, selected_row={}",
        ui.get_task_restoring(),
        ui.get_selected_row()
    );
    assert!(
        (result_window_anchor(ui, controller) - anchor_before).abs()
            < element(ui, "result-list").size().height,
        "Growing or sliding the window must retain the same visible sounds: phase={phase}, offset={offset}, limit={limit}, y={y}, anchor_before={anchor_before}, anchor_after={anchor_after}"
    );
}

fn page_published(controller: &Controller) -> bool {
    controller
        .search_publication
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|state| state.model_published)
}

fn assert_cell_alignment(ui: &AppWindow) {
    for column in ["name", "duration", "format", "channels", "tags"] {
        let header = element(ui, &format!("table-{column}-header"));
        let cell = element(ui, &format!("table-{column}-cell"));
        near(
            cell.absolute_position().x,
            header.absolute_position().x + 12.0,
        );
        near(cell.size().width, header.size().width - 16.0);
    }
}

fn scroll_table(ui: &AppWindow, viewport_id: &str, delta_x: f32) {
    scroll_table_for(ui, viewport_id, delta_x, 250);
}

fn scroll_table_for(ui: &AppWindow, viewport_id: &str, delta_x: f32, elapsed_ms: u64) {
    let origin = element(ui, viewport_id).absolute_position();
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position: slint::LogicalPosition::new(origin.x + 24.0, origin.y + 24.0),
        delta_x,
        delta_y: 0.0,
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(elapsed_ms));
}

fn element(ui: &AppWindow, id: &str) -> ElementHandle {
    let id = format!("AppWindow::{id}");
    ElementHandle::find_by_element_id(ui, &id)
        .next()
        .unwrap_or_else(|| panic!("Missing table element: {id}"))
}

fn midpoint(element: &ElementHandle) -> slint::LogicalPosition {
    let origin = element.absolute_position();
    let size = element.size();
    slint::LogicalPosition::new(origin.x + size.width / 2.0, origin.y + size.height / 2.0)
}

fn press(ui: &AppWindow, position: slint::LogicalPosition) {
    move_to(ui, position);
    ui.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
}

fn release(ui: &AppWindow, position: slint::LogicalPosition) {
    ui.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
}

fn move_to(ui: &AppWindow, position: slint::LogicalPosition) {
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
}

fn click(ui: &AppWindow, position: slint::LogicalPosition) {
    press(ui, position);
    release(ui, position);
}

fn drag(ui: &AppWindow, id: &str, delta: f32) {
    let start = midpoint(&element(ui, id));
    press(ui, start);
    let end = slint::LogicalPosition::new(start.x + delta, start.y);
    move_to(ui, end);
    release(ui, end);
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

#[test]
fn divider_drag_after_a_recent_click_keeps_the_dragged_width() {
    // Slint reports a second press within the double-click interval and 10 px of the
    // first as a double click, on the release of that press, even after a drag. A
    // click (or a tiny drag) followed by a real drag used to reset the column to its
    // default on release and persist the default.
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1800.0, 900.0));
    ui.show().unwrap();
    settle();
    let saves = Rc::new(Cell::new(0));
    let counted = saves.clone();
    ui.on_table_layout_changed(move || counted.set(counted.get() + 1));

    let start = midpoint(&element(&ui, "table-duration-divider"));
    click(&ui, start);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(150));
    press(&ui, start);
    for delta in [20.0, 60.0, 100.0] {
        move_to(&ui, slint::LogicalPosition::new(start.x + delta, start.y));
        settle();
    }
    release(&ui, slint::LogicalPosition::new(start.x + 100.0, start.y));
    settle();
    near(ui.get_table_duration_width(), 180.0);
    assert!(saves.get() > 0, "the dragged width must be saved");

    // Fine adjustment: a small drag and a quick second grab accumulate.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(600));
    let start = midpoint(&element(&ui, "table-tags-divider"));
    press(&ui, start);
    move_to(&ui, slint::LogicalPosition::new(start.x + 4.0, start.y));
    release(&ui, slint::LogicalPosition::new(start.x + 4.0, start.y));
    settle();
    near(ui.get_table_tags_width(), 164.0);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(150));
    let second = slint::LogicalPosition::new(start.x + 4.0, start.y);
    press(&ui, second);
    move_to(&ui, slint::LogicalPosition::new(second.x + 6.0, second.y));
    release(&ui, slint::LogicalPosition::new(second.x + 6.0, second.y));
    settle();
    near(ui.get_table_tags_width(), 170.0);

    // A genuine double click (two presses without dragging) still resets.
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(600));
    let position = midpoint(&element(&ui, "table-tags-divider"));
    click(&ui, position);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(40));
    click(&ui, position);
    settle();
    near(ui.get_table_tags_width(), 160.0);
    near(ui.get_table_duration_width(), 180.0);
    ui.hide().unwrap();
}
