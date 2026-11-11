// app/audiolib-desktop/src/search_task_scope_paging_tests.rs
//! Actual controller restoration and provider-backed continuation regressions.
use super::*;
use audiolib_core::{SearchScope, search_workspace::SearchTaskDefinition};

fn wait_for_window(ui: &AppWindow, ready: impl Fn(&AppWindow) -> bool + 'static) {
    let measured = std::cell::RefCell::new(None::<(slint::ModelRc<crate::AudioRow>, usize)>);
    wait_for(ui, move |ui| {
        let rows = ui.get_rows();
        let count = rows.row_count();
        if measured
            .borrow()
            .as_ref()
            .is_none_or(|(model, size)| model != &rows || *size != count)
        {
            // System-time Slint tests have no renderer. Measure each new model
            // once so viewport restoration observes the real repeater geometry.
            let _ = i_slint_backend_testing::ElementHandle::find_by_element_id(
                ui,
                "AppWindow::result-list",
            )
            .next()
            .map(|list| list.size());
            measured.replace(Some((rows, count)));
        }
        ready(ui)
    });
}

fn select_task(fixture: &Fixture, id: &str, count: usize) {
    fixture.ui.invoke_search_task_selected(id.into());
    let id = id.to_owned();
    let controller = fixture.controller.clone();
    wait_for_window(&fixture.ui, move |ui| {
        active_id(ui) == id
            && !ui.get_task_restoring()
            && !ui.get_searching()
            && ui.get_rows().row_count() == count
            && controller.task_workspace.lock().unwrap().page_complete
    });
}

#[test]
fn scopes_survive_task_switches_favorites_and_workspace_storage() {
    isolated("scope_paging::child_scopes_survive_task_switches");
}

#[test]
#[ignore = "isolated real controller task-scope restoration"]
fn child_scopes_survive_task_switches() {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    ui.set_search_scope_index(2);
    ui.invoke_search_scope_changed();
    search(&fixture, "wind", 1);
    let path_task = active_id(ui);
    ui.invoke_search_task_favorite_toggled(path_task.clone().into());
    ui.invoke_search_task_created();
    wait_for(ui, |ui| {
        ui.get_search_tasks().row_count() == 2 && !ui.get_task_restoring() && !ui.get_searching()
    });
    ui.set_search_scope_index(3);
    ui.invoke_search_scope_changed();
    search(&fixture, "gore", 3);
    let filename_task = active_id(ui);
    controller.capture_search_task(ui);
    controller.task_workspace.lock().unwrap().caches.clear();
    select_task(&fixture, &path_task, 1);
    assert_eq!(ui.get_search_scope_index(), 2);
    assert_eq!(controller.request.lock().unwrap().scope, SearchScope::Path);
    assert_eq!(ui.get_query(), "wind");
    select_task(&fixture, &filename_task, 3);
    assert_eq!(ui.get_search_scope_index(), 3);
    assert_eq!(
        controller.request.lock().unwrap().scope,
        SearchScope::Filename
    );
    select_task(&fixture, &path_task, 1);
    assert!(
        !ui.get_search_tasks()
            .iter()
            .find(|task| task.active)
            .unwrap()
            .modified
    );

    controller.persist_search_task(ui);
    controller.wait_workspace_writes().unwrap();
    let target = controller
        .task_workspace
        .lock()
        .unwrap()
        .target
        .clone()
        .unwrap();
    let restored = fixture.provider.load_search_workspace(&target).unwrap();
    assert_eq!(
        restored
            .session
            .tasks
            .iter()
            .find(|task| task.id == path_task)
            .unwrap()
            .definition
            .scope,
        SearchScope::Path
    );
    assert_eq!(
        restored
            .session
            .tasks
            .iter()
            .find(|task| task.id == filename_task)
            .unwrap()
            .definition
            .scope,
        SearchScope::Filename
    );
    assert_eq!(restored.favorites[0].definition.scope, SearchScope::Path);

    // A workspace from before scope persistence always means legacy main fields,
    // regardless of the most recently selected application-level preference.
    select_task(&fixture, &filename_task, 3);
    {
        let mut state = controller.task_workspace.lock().unwrap();
        let task = state
            .workspace
            .session
            .tasks
            .iter_mut()
            .find(|task| task.id == path_task)
            .unwrap();
        task.definition =
            serde_json::from_str::<SearchTaskDefinition>(r#"{"query":"wind"}"#).unwrap();
        state.caches.clear();
    }
    select_task(&fixture, &path_task, 1);
    assert_eq!(ui.get_search_scope_index(), 1);
    assert_eq!(controller.request.lock().unwrap().scope, SearchScope::Main);
    finish(&fixture);
}

fn seed_path_pages(fixture: &Fixture) {
    let conn = rusqlite::Connection::open(fixture.provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    let wind = fixture
        .provider
        .find_folder_by_path(&fixture.root.join("sounds/wind"))
        .unwrap()
        .unwrap();
    let gore = fixture
        .provider
        .find_folder_by_path(&fixture.root.join("sounds/gore"))
        .unwrap()
        .unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    for (folder, first, count) in [(&wind, 1_000, 250), (&gore, 2_000, 254)] {
        for id in first..first + count {
            let filename = format!("unrecorded_{id}.wav");
            let path = format!("{}/{}", folder.path, filename);
            tx.execute(
                "INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES(?1,?2,?3,?4,1)",
                rusqlite::params![id, path, filename, folder.id],
            ).unwrap();
        }
    }
    tx.commit().unwrap();
    fixture.provider.rebuild_directory_search().unwrap();
}

fn seed_extra_path_pages(fixture: &Fixture, count: i64) {
    seed_folder_pages(fixture, "wind", 3_000, count);
}

fn seed_folder_pages(fixture: &Fixture, folder_name: &str, first: i64, count: i64) {
    let conn = rusqlite::Connection::open(fixture.provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    let folder = fixture
        .provider
        .find_folder_by_path(&fixture.root.join("sounds").join(folder_name))
        .unwrap()
        .unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    for id in first..first + count {
        let filename = format!("unrecorded_{id}.wav");
        let path = format!("{}/{}", folder.path, filename);
        tx.execute(
            "INSERT INTO audio_files(id,file_path,filename,folder_id,missing) VALUES(?1,?2,?3,?4,1)",
            rusqlite::params![id, path, filename, folder.id],
        ).unwrap();
    }
    tx.commit().unwrap();
    drop(conn);
    fixture.provider.rebuild_directory_search().unwrap();
}

fn reference_pages(provider: &SqliteProvider, mut request: SearchRequest) -> Vec<AudioFile> {
    let mut output = Vec::new();
    loop {
        let page = provider.search(&request).unwrap();
        let end = request.offset + page.returned_count;
        let more = page.next_cursor.is_some()
            || (page.total_count_exact && (end as u64) < page.total_count);
        request.offset = end;
        request.cursor = page.next_cursor;
        let returned = page.returned_count;
        output.extend(page.results);
        if !more || returned == 0 {
            return output;
        }
    }
}

#[test]
fn hidden_server_pages_do_not_stop_continuous_browsing() {
    isolated("scope_paging::child_hidden_server_pages");
}

#[test]
#[ignore = "isolated actual SQLite pages and server removal overlays"]
fn child_hidden_server_pages() {
    let fixture = fixture();
    seed_path_pages(&fixture);
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    // Only the in-memory removal overlay needs server mode. The provider and
    // settings remain owned local fixtures; this test performs no HTTP calls.
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(audiolib_remote::CacheState {
        library_id: "hidden-page-fixture".into(),
        revision: 1,
        ..Default::default()
    });
    ui.set_visibility_index(2);
    for (index, query, scope, hidden_count) in [
        (2, "wind", SearchScope::Path, PAGE_SIZE),
        (1, "", SearchScope::Main, PAGE_SIZE * 2),
    ] {
        let mut reference = SearchRequest {
            query: query.into(),
            scope,
            limit: 1_000,
            ..Default::default()
        };
        reference.filters.visibility = FileVisibility::All;
        let all = fixture.provider.search(&reference).unwrap().results;
        let hidden = all[..hidden_count]
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>();
        controller.hide_removed(&hidden, &HashSet::new(), hidden_count as u64);
        ui.set_search_scope_index(index);
        ui.set_query(query.into());
        controller.request_navigation_search(ui, true, true);
        let ready = controller.clone();
        wait_for(ui, move |ui| !ui.get_searching() && fully_published(&ready));
        assert_eq!(
            controller
                .records
                .lock()
                .unwrap()
                .iter()
                .map(|file| file.id)
                .collect::<Vec<_>>(),
            all[hidden_count..]
                .iter()
                .map(|file| file.id)
                .collect::<Vec<_>>(),
            "hidden SQL spans must not publish a false empty result"
        );
        assert_eq!(controller.request.lock().unwrap().offset, hidden_count);
        assert!(
            !ui.get_can_previous(),
            "the first visible page has no predecessor"
        );
        assert!(!ui.get_can_next());
        controller
            .remote
            .lock()
            .unwrap()
            .state
            .as_mut()
            .unwrap()
            .revision += 1;
        assert!(controller.settle_hidden());
    }

    let mut reference = SearchRequest {
        limit: 1_000,
        ..Default::default()
    };
    reference.filters.visibility = FileVisibility::All;
    let all = fixture.provider.search(&reference).unwrap().results;
    let ids = || {
        controller
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>()
    };
    let first = all[..PAGE_SIZE]
        .iter()
        .map(|file| file.id)
        .collect::<Vec<_>>();
    let last = all[PAGE_SIZE * 2..]
        .iter()
        .map(|file| file.id)
        .collect::<Vec<_>>();
    let hidden = all[PAGE_SIZE..PAGE_SIZE * 2]
        .iter()
        .map(|file| file.id)
        .collect::<Vec<_>>();
    controller.hide_removed(&hidden, &HashSet::new(), hidden.len() as u64);
    search(&fixture, "", PAGE_SIZE);
    assert_eq!(ids(), first);
    controller.request_page(ui, 1);
    let ready = controller.clone();
    wait_for(ui, move |ui| !ui.get_searching() && fully_published(&ready));
    assert_eq!(ids(), last);
    controller.request_page(ui, -1);
    let ready = controller.clone();
    wait_for(ui, move |ui| !ui.get_searching() && fully_published(&ready));
    assert_eq!(ids(), first, "Previous returns to a displayed range");

    // Growing a continuous window must preserve its visible prefix even while
    // the next entire SQL page is hidden, without adding placeholder rows.
    assert!(controller.extend_result_window(ui, 1));
    let ready = controller.clone();
    wait_for(ui, move |ui| !ui.get_searching() && fully_published(&ready));
    let expected = first.into_iter().chain(last).collect::<Vec<_>>();
    assert_eq!(ids(), expected);
    assert!(!ui.get_can_next());
    controller
        .remote
        .lock()
        .unwrap()
        .state
        .as_mut()
        .unwrap()
        .revision += 1;
    assert!(controller.settle_hidden());

    let hidden = all.iter().map(|file| file.id).collect::<Vec<_>>();
    controller.hide_removed(&hidden, &HashSet::new(), hidden.len() as u64);
    search(&fixture, "", 0);
    assert_eq!(controller.request.lock().unwrap().offset, PAGE_SIZE * 2);
    assert!(!ui.get_can_previous() && !ui.get_can_next());
    assert!(controller.search_job.lock().unwrap().is_none());
    controller.remote_mode.store(false, Ordering::Release);
    finish(&fixture);
}

#[test]
fn continuous_windows_cross_long_hidden_spans_in_both_directions() {
    isolated("scope_paging::child_long_hidden_window_spans");
}

#[test]
#[ignore = "isolated bounded window navigation across long removal overlays"]
fn child_long_hidden_window_spans() {
    let fixture = fixture();
    seed_path_pages(&fixture);
    seed_extra_path_pages(&fixture, 496);
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    let mut reference = SearchRequest {
        limit: PAGE_SIZE,
        ..Default::default()
    };
    reference.filters.visibility = FileVisibility::All;
    let all = reference_pages(&fixture.provider, reference);
    assert_eq!(all.len(), 1_004);
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(audiolib_remote::CacheState {
        library_id: "long-hidden-window-fixture".into(),
        revision: 1,
        ..Default::default()
    });
    let hidden = all[PAGE_SIZE..PAGE_SIZE * 4]
        .iter()
        .map(|file| file.id)
        .collect::<Vec<_>>();
    controller.hide_removed(&hidden, &HashSet::new(), hidden.len() as u64);
    ui.set_visibility_index(2);
    ui.set_search_scope_index(1);
    let ids = || {
        controller
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>()
    };
    let expected = all[..PAGE_SIZE]
        .iter()
        .chain(&all[PAGE_SIZE * 4..])
        .map(|file| file.id)
        .collect::<Vec<_>>();
    let wait_ready = |phase: &str| {
        eprintln!("long gap: {phase}");
        let c = controller.clone();
        wait_for_window(ui, move |ui| {
            !ui.get_searching() && !ui.get_task_restoring() && fully_published(&c)
        });
    };
    search(&fixture, "", PAGE_SIZE);
    wait_ready("initial geometry");
    for _ in 0..4 {
        if !ui.get_can_next() {
            break;
        }
        assert!(controller.extend_result_window(ui, 1));
        wait_ready("forward append");
    }
    assert_eq!(
        ids(),
        expected,
        "a long hidden gap must not strand the prefix"
    );
    assert!(!ui.get_can_next());
    let top = ((-ui.get_task_result_scroll_y()).max(0.0) / ui.get_task_result_row_height().max(1.0))
        .floor() as usize;
    assert_eq!(ids()[top], all[0].id);

    // A saved nonzero raw position is not proof that earlier matches are absent.
    {
        let mut request = controller.request.lock().unwrap();
        request.offset = PAGE_SIZE * 4;
        request.limit = PAGE_SIZE * 2;
        request.cursor = None;
    }
    controller.request_navigation_search(ui, true, false);
    wait_ready("cold suffix");
    assert_eq!(ids(), expected[PAGE_SIZE..]);
    controller.request_page(ui, -1);
    wait_ready("cold Previous");
    assert_eq!(
        ids(),
        expected[..PAGE_SIZE],
        "cold Previous crosses the hidden gap"
    );
    {
        let mut request = controller.request.lock().unwrap();
        request.offset = PAGE_SIZE * 4;
        request.limit = PAGE_SIZE * 2;
        request.cursor = None;
    }
    controller.request_navigation_search(ui, true, false);
    wait_ready("suffix before upward append");
    for _ in 0..4 {
        if !ui.get_can_previous() {
            break;
        }
        assert!(controller.extend_result_window(ui, -1));
        wait_ready("upward append");
    }
    assert_eq!(ids(), expected, "upward browsing must probe earlier spans");
    assert!(!ui.get_can_previous());
    let top = ((-ui.get_task_result_scroll_y()).max(0.0) / ui.get_task_result_row_height().max(1.0))
        .floor() as usize;
    assert_eq!(ids()[top], all[PAGE_SIZE * 4].id);
    assert!(ids().len() <= PAGE_SIZE * 3);
    controller.remote_mode.store(false, Ordering::Release);
    finish(&fixture);
}

#[test]
fn path_windows_keep_raw_coverage_through_trims_and_task_restores() {
    isolated("scope_paging::child_path_window_coverage");
}

#[test]
#[ignore = "isolated Path cursors, bounded window trims and task cache restoration"]
fn child_path_window_coverage() {
    let fixture = fixture();
    seed_path_pages(&fixture);
    seed_extra_path_pages(&fixture, 1_149);
    // Keep the target directory below the existing half-library broadness
    // boundary. These control rows must not match the Path query.
    seed_folder_pages(&fixture, "gore", 5_000, 1_149);
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    ui.set_visibility_index(2);
    ui.set_search_scope_index(2);
    assert!(fixture.provider.path_search_ready().unwrap());
    let mut reference = SearchRequest {
        query: "wind".into(),
        scope: SearchScope::Path,
        limit: PAGE_SIZE,
        ..Default::default()
    };
    reference.filters.visibility = FileVisibility::All;
    let baseline = fixture.provider.search(&reference).unwrap();
    assert_eq!(
        baseline.results.len(),
        PAGE_SIZE,
        "the owned Path fixture must have a full first page"
    );
    let all = reference_pages(&fixture.provider, reference)
        .into_iter()
        .map(|file| file.id)
        .collect::<Vec<_>>();
    assert_eq!(all.len(), 1_400);
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(audiolib_remote::CacheState {
        library_id: "path-window-fixture".into(),
        revision: 1,
        ..Default::default()
    });
    // Follow the runtime source handoff for the task/viewport owner too. Merely
    // toggling the overlay flag would leave a local workspace in server mode.
    // Settings remain local to this owned fixture, so no HTTP poller is enabled.
    ui.set_remote_mode(true);
    controller.init_search_workspace();
    let ready = controller.clone();
    wait_for_window(ui, move |ui| {
        !ui.get_search_workspace_loading()
            && !ui.get_searching()
            && !ui.get_task_restoring()
            && fully_published(&ready)
    });
    assert!(controller.result_scroll_belongs_to_workspace());
    controller.hide_removed(
        &all[PAGE_SIZE..PAGE_SIZE * 3],
        &HashSet::new(),
        (PAGE_SIZE * 2) as u64,
    );
    ui.set_visibility_index(2);
    ui.set_search_scope_index(2);
    search(&fixture, "wind", PAGE_SIZE);
    let task = active_id(ui);
    let ids = || {
        controller
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>()
    };
    let wait_path = |stage: &str, expected_rows: Option<usize>, expected_task: Option<String>| {
        eprintln!("Path window stage: {stage}");
        let stage = stage.to_owned();
        let started = Instant::now();
        let reported = std::cell::Cell::new(false);
        let c = controller.clone();
        let target = task.clone();
        wait_for_window(ui, move |ui| {
            if started.elapsed() > Duration::from_secs(10) && !reported.replace(true) {
                let request = c.request.lock().unwrap().clone();
                let publication = c.search_publication.lock().unwrap().as_ref().map(|p| {
                    (
                        match p.page {
                            SearchPagePhase::Pending => "pending",
                            SearchPagePhase::Published => "published",
                            SearchPagePhase::Failed => "failed",
                        },
                        p.model_published,
                        p.expected_folders.is_some(),
                        p.pending_folders.is_some(),
                    )
                });
                let state = c.task_workspace.lock().unwrap();
                let saved = state
                    .workspace
                    .session
                    .tasks
                    .iter()
                    .find(|t| t.id == target)
                    .map(|t| (t.view.offset, t.view.list_top_row, t.view.list_row_offset));
                let cache = state
                    .caches
                    .get(&target)
                    .map(|cache| (cache.records.len(), cache.limit));
                eprintln!(
                    "Path stage {stage}: rows={} expected={expected_rows:?} offset={} limit={} cursor={} searching={} restoring={} loading={} publication={publication:?} complete={} page_rev={:?} folder_rev={:?} saved={saved:?} cache={cache:?} y={} row_height={} visible_height={}",
                    ui.get_rows().row_count(),
                    request.offset,
                    request.limit,
                    request.cursor.is_some(),
                    ui.get_searching(),
                    ui.get_task_restoring(),
                    state.loading,
                    state.page_complete,
                    state.page_revision,
                    state.folders_revision,
                    ui.get_task_result_scroll_y(),
                    ui.get_task_result_row_height(),
                    ui.get_task_result_visible_height()
                );
            }
            !ui.get_searching()
                && !ui.get_task_restoring()
                && fully_published(&c)
                && expected_rows.is_none_or(|count| ui.get_rows().row_count() == count)
                && expected_task
                    .as_ref()
                    .is_none_or(|target| active_id(ui) == *target)
        });
    };
    let extend = |direction, stage: &str| {
        assert!(controller.extend_result_window(ui, direction));
        wait_path(stage, None, None);
    };
    let grow = all[..PAGE_SIZE]
        .iter()
        .chain(&all[PAGE_SIZE * 3..PAGE_SIZE * 4])
        .copied()
        .collect::<Vec<_>>();
    extend(1, "first disjoint append");
    assert_eq!(ids(), grow);

    // A warm task cache must carry the disjoint raw coverage with its records.
    controller.create_search_task();
    wait_path("new task before warm restore", None, None);
    ui.invoke_search_task_selected(task.clone().into());
    wait_path("warm restore", Some(PAGE_SIZE * 2), Some(task.clone()));
    extend(1, "append after warm restore");
    let expected = all[..PAGE_SIZE]
        .iter()
        .chain(&all[PAGE_SIZE * 3..PAGE_SIZE * 5])
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(ids(), expected);

    // Move past the prefix before a bounded forward trim may evict it.
    let row_height = ui.get_task_result_row_height().max(1.0);
    ui.invoke_restore_result_window_viewport(-(PAGE_SIZE as f32 * row_height + 7.0));
    wait_for_window(ui, |ui| !ui.get_task_restoring());
    extend(1, "first forward trim");
    assert_eq!(ids(), all[PAGE_SIZE * 3..PAGE_SIZE * 6]);
    ui.invoke_restore_result_window_viewport(-(PAGE_SIZE as f32 * row_height + 7.0));
    wait_for_window(ui, |ui| !ui.get_task_restoring());
    extend(1, "second forward trim");
    assert_eq!(ids(), all[PAGE_SIZE * 4..]);
    assert!(!ui.get_can_next());

    // Reverse trims the tail and then crosses the hidden spans in the opposite
    // direction. Its discarded tail cursor must not skip those rows later.
    extend(-1, "reverse tail trim");
    assert_eq!(ids(), all[PAGE_SIZE * 3..PAGE_SIZE * 6]);
    extend(-1, "reverse across gap");
    let disjoint = all[..PAGE_SIZE]
        .iter()
        .chain(&all[PAGE_SIZE * 3..PAGE_SIZE * 5])
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(ids(), disjoint);
    assert!(!ui.get_can_previous());

    controller.create_search_task();
    wait_path("new task before cold restore", None, None);
    controller
        .task_workspace
        .lock()
        .unwrap()
        .caches
        .remove(&task);
    ui.invoke_search_task_selected(task.clone().into());
    wait_path("cold restore", Some(PAGE_SIZE * 3), Some(task.clone()));
    assert_eq!(
        ids(),
        disjoint,
        "cold restoration rebuilds visible rows across gaps"
    );
    assert!(controller.request.lock().unwrap().cursor.is_none());
    extend(1, "forward after cold restore");
    assert_eq!(ids(), all[PAGE_SIZE * 3..PAGE_SIZE * 6]);
    assert!(ids().len() <= PAGE_SIZE * 3);
    finish(&fixture);
}

#[test]
fn navigation_retains_visible_rows_after_empty_previous_and_cancelled_reads() {
    isolated("scope_paging::child_navigation_recovery_and_revision");
}

#[test]
#[ignore = "isolated navigation failures, empty prefixes and writer revisions"]
fn child_navigation_recovery_and_revision() {
    let fixture = fixture();
    seed_path_pages(&fixture);
    seed_extra_path_pages(&fixture, 496);
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    ui.set_visibility_index(2);
    ui.set_search_scope_index(1);
    let mut reference = SearchRequest {
        limit: PAGE_SIZE,
        ..Default::default()
    };
    reference.filters.visibility = FileVisibility::All;
    let all = reference_pages(&fixture.provider, reference)
        .into_iter()
        .map(|file| file.id)
        .collect::<Vec<_>>();
    let ids = || {
        controller
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>()
    };
    let ready = || {
        let c = controller.clone();
        wait_for_window(ui, move |ui| {
            !ui.get_searching() && !ui.get_task_restoring() && fully_published(&c)
        });
    };
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(audiolib_remote::CacheState {
        library_id: "navigation-recovery-fixture".into(),
        revision: 1,
        ..Default::default()
    });
    controller.hide_removed(
        &all[..PAGE_SIZE * 4],
        &HashSet::new(),
        (PAGE_SIZE * 4) as u64,
    );
    controller.request.lock().unwrap().offset = PAGE_SIZE * 4;
    controller.request_navigation_search(ui, true, false);
    ready();
    let tail = ids();
    assert_eq!(tail, all[PAGE_SIZE * 4..PAGE_SIZE * 5]);
    controller.request_page(ui, -1);
    ready();
    assert_eq!(
        ids(),
        tail,
        "an empty earlier region does not replace the visible page"
    );
    assert!(!ui.get_can_previous());
    assert!(controller.extend_result_window(ui, 1));
    ready();
    assert_eq!(ids(), all[PAGE_SIZE * 4..]);
    controller
        .remote
        .lock()
        .unwrap()
        .state
        .as_mut()
        .unwrap()
        .revision += 1;
    assert!(controller.settle_hidden());
    controller.remote_mode.store(false, Ordering::Release);

    search(&fixture, "", PAGE_SIZE);
    let before_failure = ids();
    controller.request_page(ui, 1);
    // Supersede the first worker/callback deterministically, then submit that
    // actual pending navigation to the real worker with cancellation admitted.
    // This avoids timing races and does not add a production test hook.
    let generation = controller.search_generation.fetch_add(1, Ordering::AcqRel) + 1;
    controller.tree_generation.fetch_add(1, Ordering::AcqRel);
    controller
        .search_cancel
        .lock()
        .unwrap()
        .store(true, Ordering::Release);
    let cancel = Arc::new(AtomicBool::new(true));
    *controller.search_cancel.lock().unwrap() = cancel.clone();
    *controller.facet_job.lock().unwrap() = None;
    if let Some(state) = controller.search_publication.lock().unwrap().as_mut() {
        state.generation = generation;
        state.page = SearchPagePhase::Pending;
        state.model_published = false;
        state.pending_folders = None;
        state.expected_folders = None;
    }
    *controller.search_job.lock().unwrap() = Some(SearchJob {
        request: controller.request.lock().unwrap().clone(),
        generation,
        not_before: Instant::now(),
        cancel,
        facet_revision: controller.facet_revision.load(Ordering::Acquire),
        provider: fixture.provider.clone(),
        path_probe: None,
        window: None,
        backward_before: None,
    });
    controller.search_wake.notify_one();
    ready();
    assert_eq!(ids(), before_failure);
    assert!(
        controller.extend_result_window(ui, 1),
        "cancelled navigation retains read admission"
    );
    ready();
    assert_eq!(ids(), all[..PAGE_SIZE * 2]);

    // An active window must rebuild against a changed provider revision, even
    // for a one-probe delta. First retain a surviving top ID, then delete that
    // entire displayed range to verify the rebuild needs no old anchor.
    let height = ui.get_task_result_row_height().max(1.0);
    ui.invoke_restore_result_window_viewport(-(100.0 * height + 7.0));
    ready();
    let anchor = ids()[100];
    for (remove_count, keep_anchor) in [(50, true), (PAGE_SIZE * 2, false)] {
        let previous = ids();
        let revision = controller
            .window_coverage
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .revision;
        fixture
            .provider
            .remove_audio(&previous[..remove_count])
            .unwrap();
        assert_ne!(fixture.provider.search_revision().unwrap(), revision);
        let request = controller.request.lock().unwrap().clone();
        let expected = fixture
            .provider
            .search(&request)
            .unwrap()
            .results
            .into_iter()
            .map(|file| file.id)
            .collect::<Vec<_>>();
        assert!(controller.extend_result_window(ui, 1));
        ready();
        assert_eq!(
            ids(),
            expected,
            "a changed revision replaces rather than joins the old window"
        );
        if keep_anchor {
            let top = ((-ui.get_task_result_scroll_y()).max(0.0) / height).floor() as usize;
            assert_eq!(ids()[top], anchor);
        }
    }
    finish(&fixture);
}

#[test]
fn lower_bound_path_pages_use_continuations_and_observed_counts() {
    isolated("scope_paging::child_lower_bound_path_pages");
}

#[test]
#[ignore = "isolated actual SQLite path pages and native UI navigation"]
fn child_lower_bound_path_pages() {
    let fixture = fixture();
    seed_path_pages(&fixture);
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    ui.set_visibility_index(2);
    ui.set_search_scope_index(2);
    search(&fixture, "wind", PAGE_SIZE);
    assert!(ui.get_result_summary().contains("≥201"));
    assert_eq!(ui.get_page_label(), "1/?");
    assert!(ui.get_can_next());
    let first = controller
        .records
        .lock()
        .unwrap()
        .iter()
        .map(|file| file.id)
        .collect::<HashSet<_>>();
    controller.request_page(ui, 1);
    let ready = controller.clone();
    wait_for(ui, move |ui| {
        !ui.get_searching() && ui.get_rows().row_count() == 51 && fully_published(&ready)
    });
    assert_eq!(controller.request.lock().unwrap().offset, PAGE_SIZE);
    assert!(controller.request.lock().unwrap().cursor.is_some());
    // A cursor page reports its observed suffix, not an exact global total.
    assert_eq!(ui.get_page_label(), "2/?");
    assert!(ui.get_result_summary().contains("≥51 sounds"));
    assert!(!ui.get_can_next());
    let second = controller
        .records
        .lock()
        .unwrap()
        .iter()
        .map(|file| file.id)
        .collect::<HashSet<_>>();
    assert!(first.is_disjoint(&second));
    assert_eq!(first.len() + second.len(), 251);
    controller.request_page(ui, -1);
    let ready = controller.clone();
    wait_for(ui, move |ui| {
        !ui.get_searching() && ui.get_rows().row_count() == PAGE_SIZE && fully_published(&ready)
    });
    assert_eq!(controller.request.lock().unwrap().offset, 0);
    assert!(controller.request.lock().unwrap().cursor.is_none());
    assert_eq!(
        controller
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|file| file.id)
            .collect::<HashSet<_>>(),
        first
    );

    // An exhausted nonzero offset has no exact global count. Do not invent a
    // total from the offset or clamp it to a last page that was not observed.
    controller.request.lock().unwrap().offset = 1_000;
    controller.request_navigation_search(ui, true, false);
    let ready = controller.clone();
    wait_for(ui, move |ui| {
        !ui.get_searching() && ui.get_rows().row_count() == 0 && fully_published(&ready)
    });
    assert_eq!(controller.request.lock().unwrap().offset, 1_000);
    assert!(controller.request.lock().unwrap().cursor.is_none());
    assert_eq!(ui.get_page_label(), "6/?");
    assert!(ui.get_result_summary().contains("≥0 sounds"));
    assert!(ui.get_can_previous());
    assert!(!ui.get_can_next());

    // A populated terminal page without a cursor has traversed the full prefix
    // and can still publish its exact global total.
    controller.request.lock().unwrap().offset = PAGE_SIZE;
    controller.request_navigation_search(ui, true, false);
    let ready = controller.clone();
    wait_for(ui, move |ui| {
        !ui.get_searching() && ui.get_rows().row_count() == 51 && fully_published(&ready)
    });
    assert_eq!(controller.request.lock().unwrap().offset, PAGE_SIZE);
    assert!(controller.request.lock().unwrap().cursor.is_none());
    assert_eq!(ui.get_page_label(), "2/2");
    assert!(!ui.get_result_summary().contains('≥'));
    assert!(ui.get_result_summary().contains("251 sounds"));
    assert!(ui.get_can_previous());
    assert!(!ui.get_can_next());
    finish(&fixture);
}
