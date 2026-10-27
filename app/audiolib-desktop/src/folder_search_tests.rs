// app/audiolib-desktop/src/folder_search_tests.rs
//! Exercise directory facets through the real workers and Slint event loop.
use super::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[test]
fn search_updates_navigation_counts_and_preserves_cross_folder_choices() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "controller::folder_search_tests::child_search_navigation",
            "--ignored",
            "--test-threads=1",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "Navigation regression failed: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Remove a fixture directory, retrying briefly while Windows finishes closing
/// handles released an instant ago (an Arc reaches zero strong references just
/// before its SQLite connection is closed). Cleanup never fails the test.
fn remove_test_dir(root: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::remove_dir_all(root) {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) if Instant::now() >= deadline => {
                eprintln!("warning: could not remove {}: {error}", root.display());
                return;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[test]
#[ignore = "Slint event-loop proxy requires an isolated child process"]
fn child_search_navigation() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = std::env::temp_dir().join(format!(
        "audiolib-search-navigation-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let sounds = root.join("sounds");
    std::fs::create_dir_all(sounds.join("gore/deep")).unwrap();
    std::fs::create_dir_all(sounds.join("wind")).unwrap();
    let mut wav = Vec::new();
    wav.extend(b"RIFF");
    wav.extend(38u32.to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(48000u32.to_le_bytes());
    wav.extend(96000u32.to_le_bytes());
    wav.extend(2u16.to_le_bytes());
    wav.extend(16u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(2u32.to_le_bytes());
    wav.extend(0i16.to_le_bytes());
    for path in [
        "gore/gore one.wav",
        "gore/gore two.wav",
        "gore/deep/gore three.wav",
        "wind/wind.wav",
    ] {
        std::fs::write(sounds.join(path), &wav).unwrap();
    }
    let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
    ScanService::new(provider.clone())
        .scan(
            &sounds,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    // Navigation reuses facets only while the database revision stays unchanged.
    // The list Waveform worker now fills missing local caches in the background;
    // those real SQLite writes correctly invalidate the conservative revision token.
    // Finish fixture derivation first, keeping the production worker enabled and the
    // strict Arc reuse assertion below intact for navigation without concurrent writes.
    for file in provider.search(&SearchRequest::default()).unwrap().results {
        assert!(matches!(
            provider
                .compute_list_waveform(file.id, Arc::new(AtomicBool::new(false)))
                .unwrap(),
            audiolib_core::ListWaveformState::Ready { .. }
        ));
    }
    let favorite = provider
        .search(&SearchRequest {
            query: "gore one".into(),
            ..Default::default()
        })
        .unwrap()
        .results[0]
        .id;
    provider
        .update_metadata(
            favorite,
            &MetadataPatch {
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    let engine = audiolib_audio::AudioEngine::new(PlaybackConfig::default()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(
        &ui,
        provider.clone(),
        engine.handle(),
        AppConfig::load_in(root.join("preferences")).unwrap(),
        PlaybackConfig::default(),
    );
    // A validation failure must supersede both queued and already-running searches.
    for invalid_duration in [false, true] {
        ui.set_query("gore".into());
        ui.set_duration_min("".into());
        controller.request_search(&ui, false, true);
        let previous_cancel = controller.search_cancel.lock().unwrap().clone();
        let previous_generation = controller.search_generation.load(Ordering::Acquire);
        if invalid_duration {
            ui.set_duration_min("invalid".into());
        } else {
            ui.set_query("x".repeat(501).into());
        }
        controller.request_search(&ui, true, true);
        assert!(previous_cancel.load(Ordering::Acquire));
        assert!(controller.search_job.lock().unwrap().is_none());
        assert!(controller.search_generation.load(Ordering::Acquire) > previous_generation);
        assert!(!ui.get_searching());
    }
    ui.set_duration_min("".into());
    // Typing and then backspacing within the debounce window must replace both
    // workers' generations. Returning to one letter cancels the last valid
    // request without dispatching a broad single-letter search.
    for query in ["go", "gor", "gore", "go", "g"] {
        let previous_cancel = controller.search_cancel.lock().unwrap().clone();
        let previous_generation = controller.search_generation.load(Ordering::Acquire);
        ui.set_query(query.into());
        controller.request_search(&ui, false, true);
        assert!(previous_cancel.load(Ordering::Acquire));
        assert!(controller.search_generation.load(Ordering::Acquire) > previous_generation);
    }
    assert!(
        controller
            .search_cancel
            .lock()
            .unwrap()
            .load(Ordering::Acquire)
    );
    assert!(controller.search_job.lock().unwrap().is_none());
    assert!(controller.facet_job.lock().unwrap().is_none());
    assert!(controller.search_publication.lock().unwrap().is_none());
    assert!(!ui.get_searching());
    assert!(ui.get_status().contains("at least 2"));
    ui.set_query("".into());
    // Simulate out-of-order background completions, including stale errors and a
    // read invalidated by maintenance before its library generation changes.
    let generation = controller.library_generation.load(Ordering::Acquire);
    let current = controller
        .library_info_generation
        .fetch_add(2, Ordering::AcqRel)
        + 2;
    let info = provider.library_info().unwrap();
    controller.publish_library_info(&ui, generation, current, Ok(info));
    let summary = ui.get_library_summary();
    let report = ui.get_management_report();
    let mut stale = provider.library_info().unwrap();
    stale.stats.audio_files = 999;
    controller.publish_library_info(&ui, generation, current - 1, Ok(stale));
    controller.publish_library_info(
        &ui,
        generation,
        current - 1,
        Err(audiolib_core::Error::Validation("stale error".into())),
    );
    assert_eq!(ui.get_library_summary(), summary);
    assert_eq!(ui.get_management_report(), report);
    assert_eq!(
        controller
            .tree
            .lock()
            .unwrap()
            .stats
            .as_ref()
            .unwrap()
            .audio_files,
        4
    );
    controller.invalidate_library_reads();
    controller.publish_library_info(
        &ui,
        generation,
        current,
        Err(audiolib_core::Error::Validation("invalidated error".into())),
    );
    assert_eq!(ui.get_management_report(), report);
    controller.request_search(&ui, true, true);
    let cached_folders = Rc::new(RefCell::new(None::<Arc<Vec<Folder>>>));
    let stage = Rc::new(Cell::new(0));
    let failure = Rc::new(RefCell::new(None::<String>));
    let check_stage = stage.clone();
    let errors = failure.clone();
    let window = ui.as_weak();
    let c = controller.clone();
    let started = Instant::now();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(15),
        move || {
            let Some(ui) = window.upgrade() else { return };
            let folders: Vec<_> = ui
                .get_folders()
                .iter()
                .map(|f| (f.name.to_string(), f.count.to_string()))
                .collect();
            let expected = |items: &[(&str, &str)]| {
                folders
                    == items
                        .iter()
                        .map(|(n, c)| (n.to_string(), c.to_string()))
                        .collect::<Vec<_>>()
            };
            if started.elapsed() > Duration::from_secs(15) {
                *errors.borrow_mut() = Some(format!(
                    "stage {} timed out: {folders:?}; {}",
                    check_stage.get(),
                    ui.get_status()
                ));
                slint::quit_event_loop().ok();
                return;
            }
            if ui.get_searching() {
                return;
            }
            let step = check_stage.get();
            match step {
                0 if expected(&[("sounds", "4")]) => ui.invoke_folder_toggle_requested(0),
                1 if expected(&[("sounds", "4"), ("gore", "3"), ("wind", "1")]) => {
                    ui.invoke_folder_requested(2);
                    ui.set_query("gore".into());
                    ui.invoke_search_requested(false);
                }
                2 if expected(&[("sounds", "3"), ("gore", "3")])
                    && ui.get_rows().row_count() == 0 =>
                {
                    if !ui.get_selected_folder_path().ends_with("wind")
                        || ui.get_selected_folder() != -1
                    {
                        *errors.borrow_mut() =
                            Some("Zero-hit selected folder lost its breadcrumb scope".into());
                        slint::quit_event_loop().ok();
                        return;
                    }
                    ui.invoke_folder_toggle_requested(1);
                }
                3 if expected(&[("sounds", "3"), ("gore", "3"), ("deep", "1")]) => {
                    *cached_folders.borrow_mut() = Some(c.tree.lock().unwrap().matches.clone());
                    ui.invoke_folder_requested(1)
                }
                4 if expected(&[("sounds", "3"), ("gore", "3"), ("deep", "1")])
                    && ui.get_rows().row_count() == 3 =>
                {
                    assert!(
                        Arc::ptr_eq(
                            cached_folders.borrow().as_ref().unwrap(),
                            &c.tree.lock().unwrap().matches
                        ),
                        "Folder navigation must reuse query facets"
                    );
                    ui.set_favorites_only(true);
                    ui.invoke_filter_requested();
                }
                5 if expected(&[("sounds", "1"), ("gore", "1")])
                    && ui.get_rows().row_count() == 1 =>
                {
                    if ui.get_folders().row_data(1).unwrap().has_children {
                        *errors.borrow_mut() =
                            Some("Empty descendant retained an expand control".into());
                        slint::quit_event_loop().ok();
                        return;
                    }
                    ui.set_query("no-such-sound".into());
                    ui.invoke_search_requested(true);
                }
                6 if folders.is_empty() && ui.get_rows().row_count() == 0 => {
                    ui.set_query("".into());
                    ui.invoke_reset_filters();
                }
                7 if expected(&[("sounds", "4"), ("gore", "3"), ("deep", "1"), ("wind", "1")]) => {
                    // Queue an old tree publication, then replace several pending queries.
                    c.reload_tree();
                    for query in ["gore", "no-such-sound", "wind"] {
                        ui.set_query(query.into());
                        ui.invoke_search_requested(false);
                    }
                    ui.invoke_all_library_requested();
                }
                8 if expected(&[("sounds", "1"), ("wind", "1")])
                    && ui.get_rows().row_count() == 1 =>
                {
                    ui.invoke_folder_toggle_requested(0)
                }
                9 if expected(&[("sounds", "1")]) => ui.invoke_folder_toggle_requested(0),
                10 if expected(&[("sounds", "1"), ("wind", "1")]) => {
                    ui.set_favorites_only(true);
                    ui.invoke_filter_requested();
                }
                11 if folders.is_empty() && ui.get_rows().row_count() == 0 => {
                    let file = c
                        .provider()
                        .search(&SearchRequest {
                            query: "wind".into(),
                            ..Default::default()
                        })
                        .unwrap()
                        .results[0]
                        .id;
                    c.provider()
                        .update_metadata(
                            file,
                            &MetadataPatch {
                                favorite: Some(true),
                                ..Default::default()
                            },
                        )
                        .unwrap();
                    // A write invalidation must survive a immediately superseding navigation.
                    c.request_search(&ui, true, false);
                    ui.invoke_all_library_requested();
                }
                12 if expected(&[("sounds", "1"), ("wind", "1")])
                    && ui.get_rows().row_count() == 1 =>
                {
                    // A separate provider models a CLI/other instance committing an edit.
                    let external =
                        SqliteProvider::open_existing(c.provider().database_path()).unwrap();
                    let file = external
                        .search(&SearchRequest {
                            query: "wind".into(),
                            ..Default::default()
                        })
                        .unwrap()
                        .results[0]
                        .id;
                    external
                        .update_metadata(
                            file,
                            &MetadataPatch {
                                favorite: Some(false),
                                ..Default::default()
                            },
                        )
                        .unwrap();
                    ui.invoke_all_library_requested();
                }
                13 if folders.is_empty() && ui.get_rows().row_count() == 0 => {
                    check_stage.set(14);
                    slint::quit_event_loop().ok();
                    return;
                }
                _ => return,
            }
            check_stage.set(step + 1);
        },
    );
    ui.show().unwrap();
    slint::run_event_loop().unwrap();
    timer.stop();
    controller.shutdown();
    ui.hide().unwrap();
    let released = Arc::downgrade(&controller);
    let provider_released = Arc::downgrade(&provider);
    drop(timer);
    drop(ui);
    drop(controller);
    drop(engine);
    drop(provider);
    let deadline = Instant::now() + Duration::from_secs(3);
    // Detached workers hold their own provider clones; wait for the SQLite
    // connection itself, not just the controller, before deleting its directory.
    while (released.strong_count() > 0 || provider_released.strong_count() > 0)
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let error = failure.borrow().clone();
    remove_test_dir(&root);
    assert!(error.is_none(), "{error:?}");
    assert_eq!(stage.get(), 14);
}

#[test]
fn directory_cache_key_tracks_filters_and_refresh_but_not_navigation() {
    let root = std::env::temp_dir().join(format!(
        "audiolib-facet-identity-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
    let another_provider =
        Arc::new(SqliteProvider::open_read_only(&root.join("library.db")).unwrap());
    let mut request = SearchRequest {
        query: "metal".into(),
        ..Default::default()
    };
    let cache = FolderFacets {
        provider: Arc::downgrade(&provider),
        revision: 7,
        database_revision: Some(10),
        query: request.query.clone(),
        filters: request.filters.clone(),
        folders: Arc::new(Vec::new()),
    };
    assert!(!cache.matches(&request, 7, None, &provider));
    assert!(!cache.matches(&request, 7, Some(11), &provider));
    assert!(!cache.matches(&request, 7, Some(10), &another_provider));
    request.folder_id = Some(42);
    request.include_subfolders = false;
    request.offset = 200;
    request.order_by = SortOrder::Filename;
    assert!(cache.matches(&request, 7, Some(10), &provider));
    assert!(!cache.matches(&request, 8, Some(10), &provider));
    request.filters.favorites_only = true;
    assert!(!cache.matches(&request, 7, Some(10), &provider));
    request.filters.favorites_only = false;
    request.query = "wind".into();
    assert!(!cache.matches(&request, 7, Some(10), &provider));
    drop(provider);
    assert!(
        cache.provider.upgrade().is_none(),
        "The cache must not retain the old library"
    );
    drop(another_provider);
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn asynchronous_page_and_facets_publish_in_order_and_preserve_failures() {
    let root = std::env::temp_dir().join(format!(
        "audiolib-facet-publication-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "controller::folder_search_tests::child_facet_publication",
            "--ignored",
            "--test-threads=1",
        ])
        .env("AUDIOLIB_FACET_PUBLICATION_ROOT", &root)
        .output()
        .unwrap();
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    if root.exists() {
        std::fs::remove_dir_all(root).unwrap();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn pending_publication(controller: &Controller, ui: &AppWindow) -> u64 {
    controller.invalidate_library_reads();
    let generation = controller.search_generation.load(Ordering::Acquire);
    *controller.search_publication.lock().unwrap() = Some(SearchPublication {
        generation,
        page: SearchPagePhase::Pending,
        pending_folders: None,
        expected_folders: None,
        model_published: false,
        retained_status: None,
    });
    ui.set_status("Page pending".into());
    ui.set_searching(true);
    generation
}

#[test]
#[ignore = "isolated Slint page/facets completion ordering helper"]
fn child_facet_publication() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = PathBuf::from(std::env::var_os("AUDIOLIB_FACET_PUBLICATION_ROOT").unwrap());
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    std::fs::create_dir_all(&root).unwrap();
    let provider = Arc::new(SqliteProvider::open(&root.join("library.db")).unwrap());
    let engine = audiolib_audio::AudioEngine::new(PlaybackConfig::default()).unwrap();
    let ui = AppWindow::new().unwrap();
    let mut config = AppConfig::load_in(root.join("preferences")).unwrap();
    config.language = "en".into();
    let controller = Controller::new(
        &ui,
        provider,
        engine.handle(),
        config,
        PlaybackConfig::default(),
    );
    let folders = |path: &str, count: u64| {
        Arc::new(vec![Folder {
            id: 1,
            parent_id: None,
            name: path.into(),
            path: path.into(),
            file_count: count,
        }])
    };
    let failed = pending_publication(&controller, &ui);
    let old_model = ui.get_folders();
    controller.search_folders_computed(&ui, failed, Ok(folders("unpublished", 9)));
    assert_eq!(ui.get_folders(), old_model);
    assert_eq!(ui.get_status(), "Page pending");
    assert!(controller.tree.lock().unwrap().matches.is_empty());
    ui.set_status("Page failed".into());
    controller.search_page_published(&ui, failed, false);
    controller.search_folders_computed(&ui, failed, Err("Later folder error".into()));
    controller.search_folders_computed(&ui, failed, Ok(folders("must stay hidden", 10)));
    assert_eq!(ui.get_status(), "Page failed");
    assert_eq!(ui.get_folders(), old_model);
    assert!(
        controller
            .search_publication
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .pending_folders
            .is_none()
    );

    // The first query finishes facets first. Ready must await the tree model, even
    // when another rebuild supersedes the first queued tree publication.
    let facets_first = pending_publication(&controller, &ui);
    controller.search_folders_computed(&ui, facets_first, Ok(folders("facets first", 2)));
    assert_eq!(ui.get_folders(), old_model);
    ui.set_status("Updating folders".into());
    controller.search_page_published(&ui, facets_first, true);
    assert_eq!(ui.get_folders(), old_model);
    assert_eq!(ui.get_status(), "Updating folders");
    controller.reload_tree();
    let c = controller.clone();
    let window = ui.as_weak();
    let stage = Rc::new(Cell::new(0));
    let next = stage.clone();
    let previous = Rc::new(RefCell::new((
        ui.get_folders(),
        controller.tree.lock().unwrap().visible.clone(),
    )));
    let failure = Rc::new(RefCell::new(None::<String>));
    let errors = failure.clone();
    let started = Instant::now();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(2),
        move || {
            let Some(ui) = window.upgrade() else { return };
            if started.elapsed() > Duration::from_secs(10) {
                *errors.borrow_mut() = Some(format!(
                    "publication stage {} timed out: {}",
                    next.get(),
                    ui.get_status()
                ));
                slint::quit_event_loop().unwrap();
                return;
            }
            match next.get() {
                0 if ui.get_status().starts_with("Ready") => {
                    assert_eq!(ui.get_folders().row_count(), 1);
                    assert_eq!(ui.get_folders().row_data(0).unwrap().path, "facets first");
                    assert_eq!(ui.get_folders().row_data(0).unwrap().count, "2");
                    let old_model = ui.get_folders();
                    let page_first = pending_publication(&c, &ui);
                    ui.set_status("Updating folders".into());
                    c.search_page_published(&ui, page_first, true);
                    assert_eq!(ui.get_folders(), old_model);
                    assert_eq!(ui.get_status(), "Updating folders");
                    // Failure from the previous query cannot overwrite this query.
                    c.search_folders_computed(&ui, facets_first, Err("Stale failed facets".into()));
                    assert_eq!(ui.get_status(), "Updating folders");
                    c.search_folders_computed(&ui, page_first, Ok(folders("page first", 3)));
                    assert_eq!(ui.get_folders(), old_model);
                    assert_eq!(ui.get_status(), "Updating folders");
                    next.set(1);
                }
                1 if ui.get_status().starts_with("Ready") => {
                    assert_eq!(ui.get_folders().row_data(0).unwrap().path, "page first");
                    assert_eq!(ui.get_folders().row_data(0).unwrap().count, "3");
                    assert!(
                        c.search_publication
                            .lock()
                            .unwrap()
                            .as_ref()
                            .unwrap()
                            .model_published
                    );
                    ui.set_status("Preserve subsequent error".into());
                    *previous.borrow_mut() =
                        (ui.get_folders(), c.tree.lock().unwrap().visible.clone());
                    c.reload_tree();
                    next.set(2);
                }
                2 if !Arc::ptr_eq(&c.tree.lock().unwrap().visible, &previous.borrow().1) => {
                    // The worker has published, but identical rows deliberately
                    // retain their pointer targets for directory double-clicks.
                    assert_eq!(ui.get_folders(), previous.borrow().0);
                    assert_eq!(ui.get_status(), "Preserve subsequent error");
                    next.set(3);
                    slint::quit_event_loop().unwrap();
                }
                _ => {}
            }
        },
    );
    ui.show().unwrap();
    slint::run_event_loop().unwrap();
    timer.stop();
    controller.invalidate_library_reads();
    assert!(controller.search_job.lock().unwrap().is_none());
    assert!(controller.facet_job.lock().unwrap().is_none());
    assert!(controller.search_publication.lock().unwrap().is_none());
    controller.search_folders_computed(
        &ui,
        facets_first,
        Err("Invalidated library failure".into()),
    );
    assert_eq!(ui.get_status(), "Preserve subsequent error");
    controller.shutdown();
    ui.hide().unwrap();
    assert!(failure.borrow().is_none(), "{:?}", failure.borrow());
    assert_eq!(stage.get(), 3);
}

/// Fresh process/provider per sample; measures publication on Slint's testing backend,
/// not native-window drawing. The supplied database is always opened read-only.
#[test]
#[ignore = "set AUDIOLIB_GUI_BENCHMARK_DB to an isolated benchmark database"]
fn million_record_gui_navigation_benchmark() {
    const SAMPLES: usize = 10;
    let database =
        PathBuf::from(std::env::var_os("AUDIOLIB_GUI_BENCHMARK_DB").expect("benchmark database"));
    let mut first_query = Vec::new();
    let mut failures = Vec::new();
    for sample in 1..=SAMPLES {
        let root = std::env::temp_dir().join(format!(
            "audiolib-gui-benchmark-{}-{sample}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "controller::folder_search_tests::child_million_record_gui_navigation_sample",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("AUDIOLIB_GUI_BENCHMARK_DB", &database)
            .env("AUDIOLIB_GUI_BENCHMARK_ROOT", &root)
            .env("AUDIOLIB_GUI_BENCHMARK_SAMPLE", sample.to_string())
            .output()
            .unwrap();
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        if root.exists() {
            std::fs::remove_dir_all(&root).unwrap();
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!("Fresh GUI sample {sample}:\n{stdout}");
        if let Some(report) = stdout.lines().find_map(|line| {
            line.split_once("GUI_BENCHMARK_RESULT ")
                .map(|(_, json)| json)
        }) {
            let report: serde_json::Value = serde_json::from_str(report).unwrap();
            if let Some(elapsed) = report["first_query_and_folders_ms"].as_f64() {
                first_query.push(elapsed);
            } else {
                failures.push(format!(
                    "sample {sample}: first publication never completed"
                ));
            }
        }
        if !output.status.success() || !stdout.contains("1 passed") {
            failures.push(format!("sample {sample}: {stderr}"));
        }
    }
    first_query.sort_by(f64::total_cmp);
    println!(
        "GUI first keyword + folders model publication (10 fresh processes/providers, Slint testing backend; OS cache is not flushed): {first_query:?}"
    );
    assert_eq!(first_query.len(), SAMPLES, "Missing samples: {failures:?}");
    let p95 = first_query[(SAMPLES * 95).div_ceil(100) - 1];
    let maximum = first_query[SAMPLES - 1];
    println!("GUI first keyword P95={p95:.3} ms, max={maximum:.3} ms, target <500ms");
    assert!(
        failures.is_empty(),
        "Fresh GUI regressions failed: {failures:?}"
    );
    assert!(
        p95 < 500.0 && maximum < 500.0,
        "First GUI result/folders exceeded 500ms"
    );
}

#[test]
#[ignore = "isolated helper for the explicit million-record GUI benchmark"]
fn child_million_record_gui_navigation_sample() {
    let database =
        PathBuf::from(std::env::var_os("AUDIOLIB_GUI_BENCHMARK_DB").expect("benchmark database"));
    let root =
        PathBuf::from(std::env::var_os("AUDIOLIB_GUI_BENCHMARK_ROOT").expect("owned preferences"));
    assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
    let sample = std::env::var("AUDIOLIB_GUI_BENCHMARK_SAMPLE").unwrap();
    let first_filter = std::env::var_os("AUDIOLIB_GUI_BENCHMARK_FILTER")
        .map(|value| {
            value
                .into_string()
                .expect("benchmark filter must be Unicode")
        })
        .unwrap_or_else(|| "none".into());
    let (sample_filter, bit_filter, first_filters) = match first_filter.as_str() {
        "none" => (0, 0, SearchFilters::default()),
        "sample_rate" => (
            2,
            0,
            SearchFilters {
                sample_rate: Some(48_000),
                ..Default::default()
            },
        ),
        "bit_depth" => (
            0,
            2,
            SearchFilters {
                bit_depth: Some(24),
                ..Default::default()
            },
        ),
        value => panic!("Unknown AUDIOLIB_GUI_BENCHMARK_FILTER: {value}"),
    };
    i_slint_backend_testing::init_integration_test_with_system_time();
    // Run every oracle read after timing, so it cannot prewarm the keyword's FTS
    // pages. Normal application startup still performs its empty search/info reads.
    struct FirstPublication {
        row_ids: Vec<String>,
        summary: String,
        facets: Vec<(i64, u64)>,
        root_rows: Vec<(String, String, String)>,
    }
    let provider = Arc::new(SqliteProvider::open_read_only(&database).unwrap());
    let engine = audiolib_audio::AudioEngine::new(PlaybackConfig::default()).unwrap();
    let ui = AppWindow::new().unwrap();
    let mut config = AppConfig::load_in(root.join("preferences")).unwrap();
    config.language = "en".into();
    let began = Instant::now();
    let controller = Controller::new(
        &ui,
        provider,
        engine.handle(),
        config,
        PlaybackConfig::default(),
    );
    let window = ui.as_weak();
    let c = controller.clone();
    let stage = Rc::new(Cell::new(0));
    let next_stage = stage.clone();
    let timings = Rc::new(RefCell::new(Vec::<f64>::new()));
    let results = timings.clone();
    let previous = Rc::new(RefCell::new(ui.get_folders()));
    let before_query = previous.clone();
    let previous_page = Rc::new(RefCell::new(ui.get_rows()));
    let before_page_query = previous_page.clone();
    let first_publication = Rc::new(RefCell::new(None::<FirstPublication>));
    let published = first_publication.clone();
    let toggle_target = Rc::new(Cell::new(0));
    let target = toggle_target.clone();
    let since = Rc::new(Cell::new(Instant::now()));
    let startup_ms = Rc::new(Cell::new(None));
    let startup = startup_ms.clone();
    let page_ms = Rc::new(Cell::new(None));
    let page_result = page_ms.clone();
    let first_ms = Rc::new(Cell::new(None));
    let first_result = first_ms.clone();
    let replacement_ms = Rc::new(Cell::new(None));
    let replacement = replacement_ms.clone();
    let broad_cancel = Rc::new(RefCell::new(None::<Arc<AtomicBool>>));
    let old_cancel = broad_cancel.clone();
    let failure = Rc::new(RefCell::new(None::<String>));
    let errors = failure.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(1),
        move || {
            let Some(ui) = window.upgrade() else { return };
            if began.elapsed() > Duration::from_secs(90) {
                *errors.borrow_mut() = Some(format!(
                    "stage {} timed out: {}",
                    next_stage.get(),
                    ui.get_status()
                ));
                slint::quit_event_loop().unwrap();
                return;
            }
            let ready = !ui.get_searching() && ui.get_status().starts_with("Ready");
            match next_stage.get() {
                0 if ready
                    && ui.get_folders().row_count() > 0
                    && c.tree.lock().unwrap().stats.is_some() =>
                {
                    startup.set(Some(began.elapsed().as_secs_f64() * 1000.0));
                    *before_query.borrow_mut() = ui.get_folders();
                    *before_page_query.borrow_mut() = ui.get_rows();
                    ui.set_query("metal".into());
                    ui.set_sample_filter(sample_filter);
                    ui.set_bit_filter(bit_filter);
                    since.set(Instant::now());
                    ui.invoke_search_requested(true);
                    next_stage.set(1);
                }
                1 => {
                    let rows_published =
                        !ui.get_searching() && ui.get_rows() != *before_page_query.borrow();
                    if rows_published && page_result.get().is_none() {
                        page_result.set(Some(since.get().elapsed().as_secs_f64() * 1000.0));
                    }
                    let model_published = ui.get_folders() != *before_query.borrow();
                    if !ready || !rows_published || !model_published {
                        return;
                    }
                    *published.borrow_mut() = Some(FirstPublication {
                        row_ids: ui.get_rows().iter().map(|row| row.id.to_string()).collect(),
                        summary: ui.get_result_summary().to_string(),
                        facets: c
                            .tree
                            .lock()
                            .unwrap()
                            .matches
                            .iter()
                            .map(|folder| (folder.id, folder.file_count))
                            .collect(),
                        root_rows: ui
                            .get_folders()
                            .iter()
                            .map(|folder| {
                                (
                                    folder.path.to_string(),
                                    folder.name.to_string(),
                                    folder.count.to_string(),
                                )
                            })
                            .collect(),
                    });
                    first_result.set(Some(since.get().elapsed().as_secs_f64() * 1000.0));
                    // Independently stress presentation after the real fixture's
                    // first page, directory facets and folder model have published.
                    let Some(root_id) = c
                        .tree
                        .lock()
                        .unwrap()
                        .visible
                        .first()
                        .map(|(folder, _, _)| folder.id)
                    else {
                        *errors.borrow_mut() = Some("metal published an empty folder model".into());
                        slint::quit_event_loop().unwrap();
                        return;
                    };
                    let mut folders = vec![Folder {
                        id: root_id,
                        parent_id: None,
                        name: "benchmark".into(),
                        path: "benchmark".into(),
                        file_count: 20_000,
                    }];
                    folders.extend((1..=20_000).map(|index| Folder {
                        id: root_id + index,
                        parent_id: Some(root_id),
                        name: format!("folder {index:05}"),
                        path: format!("benchmark/folder {index:05}"),
                        file_count: 1,
                    }));
                    c.tree.lock().unwrap().matches = Arc::new(folders);
                    *previous.borrow_mut() = ui.get_folders();
                    target.set(if c.tree.lock().unwrap().expanded.contains(&root_id) {
                        1
                    } else {
                        20_001
                    });
                    since.set(Instant::now());
                    ui.invoke_folder_toggle_requested(0);
                    next_stage.set(2);
                }
                step @ 2..=21
                    if ui.get_folders() != *previous.borrow()
                        && ui.get_folders().row_count() == target.get() =>
                {
                    results
                        .borrow_mut()
                        .push(since.get().elapsed().as_secs_f64() * 1000.0);
                    *previous.borrow_mut() = ui.get_folders();
                    since.set(Instant::now());
                    if step == 21 {
                        ui.set_query("metal".into());
                        ui.invoke_search_requested(true);
                        *old_cancel.borrow_mut() = Some(c.search_cancel.lock().unwrap().clone());
                    } else {
                        target.set(if target.get() == 1 { 20_001 } else { 1 });
                        ui.invoke_folder_toggle_requested(0);
                    }
                    next_stage.set(step + 1);
                }
                22 => {
                    ui.set_query("no-such-benchmark-sound".into());
                    since.set(Instant::now());
                    ui.invoke_search_requested(true);
                    if !old_cancel
                        .borrow()
                        .as_ref()
                        .unwrap()
                        .load(Ordering::Acquire)
                    {
                        *errors.borrow_mut() =
                            Some("Superseded broad request was not cancelled".into());
                        slint::quit_event_loop().unwrap();
                        return;
                    }
                    next_stage.set(23);
                }
                23 if ready
                    && ui.get_rows().row_count() == 0
                    && ui.get_folders().row_count() == 0
                    && c.tree.lock().unwrap().matches.is_empty() =>
                {
                    replacement.set(Some(since.get().elapsed().as_secs_f64() * 1000.0));
                    since.set(Instant::now());
                    next_stage.set(24);
                }
                24 if since.get().elapsed() >= Duration::from_millis(20) => {
                    if !ready
                        || ui.get_rows().row_count() != 0
                        || ui.get_folders().row_count() != 0
                        || !c.tree.lock().unwrap().matches.is_empty()
                    {
                        *errors.borrow_mut() =
                            Some("Superseded broad results replaced the empty query".into());
                    }
                    next_stage.set(25);
                    slint::quit_event_loop().unwrap();
                }
                _ => {}
            }
        },
    );
    ui.show().unwrap();
    slint::run_event_loop().unwrap();
    timer.stop();
    controller.shutdown();
    ui.hide().unwrap();
    // All stopwatch measurements are complete before opening the independent oracle.
    let oracle = SqliteProvider::open_read_only(&database).unwrap();
    let stats = oracle.stats().unwrap();
    let records = stats.audio_files.checked_add(stats.missing_files).unwrap();
    let request = SearchRequest {
        query: "metal".into(),
        limit: PAGE_SIZE,
        filters: first_filters,
        ..Default::default()
    };
    let expected_page = oracle.search(&request).unwrap();
    let expected = oracle.search_folders(&request).unwrap();
    let expected_root_rows = expected
        .iter()
        .filter(|folder| folder.parent_id.is_none())
        .map(|folder| {
            (
                folder.path.clone(),
                folder.name.clone(),
                folder.file_count.to_string(),
            )
        })
        .collect::<Vec<_>>();
    let mut samples = timings.borrow().clone();
    samples.sort_by(f64::total_cmp);
    let report = serde_json::json!({
        "sample": sample,
        "first_filter": first_filter,
        "records": records,
        "present_records": stats.audio_files,
        "missing_records": stats.missing_files,
        "backend": "Slint testing backend; model publication, no physical rendering",
        "oracle": "after_all_timing: independent provider stats/search/facets",
        "os_cache": "not flushed; normal startup runs before the first keyword",
        "timer_interval_ms": 1,
        "startup_publication_ms": startup_ms.get(),
        "first_result_page_ms": page_ms.get(),
        "first_query_and_folders_ms": first_ms.get(),
        "replacement_after_cancel_ms": replacement_ms.get(),
        "synthetic_directories": 20_001,
        "folder_toggle_samples_ms": samples,
    });
    println!("GUI_BENCHMARK_RESULT {report}");
    assert!(
        records >= 1_000_000,
        "Use an isolated million-record fixture"
    );
    assert!(
        !expected_root_rows.is_empty(),
        "metal must match the fixture"
    );
    let actual = first_publication.borrow();
    let actual = actual.as_ref().expect("first page and folder publication");
    assert_eq!(
        actual.row_ids,
        expected_page
            .results
            .iter()
            .map(|file| file.id.to_string())
            .collect::<Vec<_>>(),
        "Published page IDs must match the independent oracle"
    );
    let shown = if expected_page.total_count == 0 {
        "0".into()
    } else {
        format!("1–{}", expected_page.returned_count)
    };
    assert_eq!(
        actual.summary.split_once(" · ").unwrap().0,
        format!("{shown} of {} sounds", expected_page.total_count),
        "Published summary must report the exact page range and total count"
    );
    assert_eq!(
        actual.facets,
        expected
            .iter()
            .map(|folder| (folder.id, folder.file_count))
            .collect::<Vec<_>>(),
        "Published directory facets must match the independent oracle"
    );
    assert_eq!(
        actual.root_rows, expected_root_rows,
        "Published folder model"
    );
    assert!(failure.borrow().is_none(), "{:?}", failure.borrow());
    assert_eq!(stage.get(), 25, "benchmark timed out");
    assert_eq!(
        samples.len(),
        20,
        "Every expand/collapse must publish its model"
    );
    assert!(
        first_ms.get().unwrap() < 500.0,
        "First keyword rows and folder model must publish within 500ms"
    );
}
