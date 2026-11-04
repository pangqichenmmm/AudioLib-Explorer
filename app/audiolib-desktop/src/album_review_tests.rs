// app/audiolib-desktop/src/album_review_tests.rs
//! Exercise album filters, current-query facets and stale publications through real workers.
use super::*;

#[path = "artwork_management_tests.rs"]
mod management;

#[test]
fn albums_filter_results_follow_current_query_and_restore_with_tasks() {
    isolated("album_review::child_album_navigation");
}

fn wait_albums(fixture: &Fixture, count: usize) {
    wait_for(&fixture.ui, move |ui| {
        ui.get_albums().row_count() == count && !ui.get_searching()
    });
}

#[test]
#[ignore = "isolated album navigation regression"]
fn child_album_navigation() {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    // These statements modify only the isolated fixture. Album values are derived
    // from the same owned Metadata representation used by scanning and v7 backfill.
    let connection = rusqlite::Connection::open(fixture.provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&connection).unwrap();
    connection.execute(
        "UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.wav_metadata.fields.library',CASE WHEN filename LIKE 'gore%' THEN 'Gore album' ELSE 'Wind album' END)",
        [],
    ).unwrap();
    drop(connection);
    search(&fixture, "", 4);
    wait_albums(&fixture, 2);
    let gore = ui
        .get_albums()
        .iter()
        .find(|row| row.name == "Gore album")
        .unwrap()
        .id;
    let wind = ui
        .get_albums()
        .iter()
        .find(|row| row.name == "Wind album")
        .unwrap()
        .id;
    ui.invoke_album_selected(gore.clone());
    wait_for(ui, |ui| {
        !ui.get_searching() && ui.get_rows().row_count() == 3
    });
    assert_eq!(
        controller.request.lock().unwrap().filters.album_id,
        Some(gore.parse::<i64>().unwrap())
    );
    // The current query is wind while the selected album is gore: results are empty,
    // but cross-filter facets expose the matching wind album immediately.
    search(&fixture, "wind", 0);
    wait_albums(&fixture, 1);
    assert_eq!(
        ui.get_albums().row_data(0).unwrap().name.as_str(),
        "Wind album"
    );
    ui.invoke_reset_filters();
    wait_for(ui, |ui| {
        !ui.get_searching() && ui.get_rows().row_count() == 1
    });
    assert!(
        controller
            .request
            .lock()
            .unwrap()
            .filters
            .album_id
            .is_none()
    );
    assert!(ui.get_album_filter().is_empty());
    ui.invoke_album_selected(wind.clone());
    wait_for(ui, |ui| !ui.get_searching());
    assert_eq!(
        controller.request.lock().unwrap().filters.album_id,
        Some(wind.parse::<i64>().unwrap())
    );
    ui.invoke_album_selected(wind);
    wait_for(ui, |ui| !ui.get_searching());
    assert!(
        controller
            .request
            .lock()
            .unwrap()
            .filters
            .album_id
            .is_none()
    );

    search(&fixture, "", 4);
    ui.invoke_album_selected(gore.clone());
    wait_for(ui, |ui| {
        !ui.get_searching() && ui.get_rows().row_count() == 3
    });
    let first = active_id(ui);
    ui.invoke_search_task_created();
    wait_for(ui, |ui| {
        ui.get_search_tasks().row_count() == 2 && !ui.get_task_restoring() && !ui.get_searching()
    });
    assert!(ui.get_album_filter().is_empty());
    ui.invoke_album_selected(
        ui.get_albums()
            .iter()
            .find(|row| row.name == "Wind album")
            .unwrap()
            .id,
    );
    wait_for(ui, |ui| {
        !ui.get_searching() && ui.get_rows().row_count() == 1
    });
    ui.invoke_search_task_selected(first.clone().into());
    wait_for(ui, move |ui| {
        active_id(ui) == first
            && !ui.get_task_restoring()
            && !ui.get_searching()
            && ui.get_rows().row_count() == 3
    });
    wait_albums(&fixture, 2);
    assert_eq!(ui.get_album_filter().as_str(), "Gore album");

    let file = controller.records.lock().unwrap()[0].clone();
    present_file(ui, &file);
    ui.set_album_cover_ready(true);
    present_file(ui, &file);
    assert!(
        ui.get_album_cover_ready(),
        "saving annotations must retain the current cover"
    );

    // Queue an old local publication, then switch library context before delivery.
    let stale = SearchJob {
        request: controller.request.lock().unwrap().clone(),
        generation: controller.search_generation.load(Ordering::Acquire),
        not_before: Instant::now(),
        cancel: controller.search_cancel.lock().unwrap().clone(),
        facet_revision: controller.facet_revision.load(Ordering::Acquire),
        provider: fixture.provider.clone(),
    };
    controller.publish_albums(&stale);
    controller.invalidate_library_reads();
    controller.reset_library_view(ui, true, false);
    controller.remote_mode.store(true, Ordering::Release);
    ui.set_remote_mode(true);
    ui.invoke_album_selected(gore);
    wait_for(ui, |_| true);
    assert_eq!(ui.get_albums().row_count(), 0);
    assert!(ui.get_album_filter().is_empty());
    assert!(
        controller
            .request
            .lock()
            .unwrap()
            .filters
            .album_id
            .is_none()
    );
    controller.remote_mode.store(false, Ordering::Release);
    ui.set_remote_mode(false);
    finish(&fixture);
}

#[test]
fn all_sounds_restores_keyword_directory_facets_after_an_album_selection() {
    isolated("album_review::child_all_sounds_cross_index_local");
}

#[test]
fn all_sounds_restores_keyword_directory_facets_in_a_read_only_server_snapshot() {
    isolated("album_review::child_all_sounds_cross_index_server");
}

#[test]
#[ignore = "isolated local cross-index navigation regression"]
fn child_all_sounds_cross_index_local() {
    cross_index_navigation(false);
}

#[test]
#[ignore = "isolated read-only snapshot cross-index navigation regression"]
fn child_all_sounds_cross_index_server() {
    cross_index_navigation(true);
}

fn cross_index_fixture(server_snapshot: bool) -> Fixture {
    let mut fixture = fixture();
    let connection = rusqlite::Connection::open(fixture.provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&connection).unwrap();
    connection.execute(
        "UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.wav_metadata.fields.library',CASE WHEN filename LIKE 'gore%' THEN ?1 ELSE ?2 END)",
        ["Gore album", "Wind album"],
    ).unwrap();
    drop(connection);
    for file in fixture
        .provider
        .search(&SearchRequest::default())
        .unwrap()
        .results
    {
        fixture
            .provider
            .update_metadata(
                file.id,
                &MetadataPatch {
                    tags: Some("shared".into()),
                    favorite: Some(matches!(
                        file.filename.as_str(),
                        "gore one.wav" | "wind.wav"
                    )),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    if server_snapshot {
        // Keep a testing window registered while replacing the controller. The
        // server path uses an actual owned read-only SQLite snapshot, not a flag
        // on the writable local provider. No HTTP server or working library is used.
        fixture.controller.shutdown();
        let snapshot = fixture.root.join("server-snapshot.db");
        fixture.provider.backup_library(&snapshot).unwrap();
        fixture.provider = Arc::new(SqliteProvider::open_read_only(&snapshot).unwrap());
        let mut config = AppConfig::load_in(fixture.root.join("preferences")).unwrap();
        config.data_source.mode = audiolib_core::config::DataSourceMode::Server;
        config.data_source.server_url = "http://127.0.0.1:9".into();
        config.data_source.poll_interval_secs = 0;
        let playback = PlaybackConfig {
            volume: 0.0,
            ..Default::default()
        };
        fixture.controller = Controller::new(
            &fixture.ui,
            fixture.provider.clone(),
            fixture.engine.handle(),
            config,
            playback,
        );
        fixture
            .controller
            .remote_mode
            .store(true, Ordering::Release);
        fixture.ui.set_remote_mode(true);
        fixture.controller.remote.lock().unwrap().state = Some(audiolib_remote::CacheState {
            library_id: "cross-index-snapshot".into(),
            revision: 1,
            schema_version: 8,
            database_file: "server-snapshot.db".into(),
            server_url: "http://127.0.0.1:9".into(),
            ..Default::default()
        });
        // Supersede the initial local workspace load with this snapshot's server
        // scope before admitting any Album selection.
        fixture.controller.init_search_workspace();
        let controller = fixture.controller.clone();
        wait_for(&fixture.ui, move |ui| {
            !ui.get_search_workspace_loading()
                && !ui.get_task_restoring()
                && !ui.get_searching()
                && fully_published(&controller)
        });
        assert!(fixture.controller.remote_read_only());
    }
    fixture
}

fn wait_navigation(fixture: &Fixture, rows: usize) {
    let controller = fixture.controller.clone();
    wait_for(&fixture.ui, move |ui| {
        !ui.get_searching() && ui.get_rows().row_count() == rows && fully_published(&controller)
    });
}

fn directory_count(fixture: &Fixture, name: &str) -> Option<u64> {
    fixture
        .controller
        .tree
        .lock()
        .unwrap()
        .matches
        .iter()
        .find(|folder| folder.name == name)
        .map(|folder| folder.file_count)
}

fn assert_all_scope(fixture: &Fixture, favorites_only: bool) {
    let ui = &fixture.ui;
    let request = fixture.controller.request.lock().unwrap().clone();
    assert_eq!(
        request.query, "shared",
        "scope navigation must keep the keyword"
    );
    assert!(request.folder_id.is_none());
    assert!(
        request.filters.album_id.is_none() && request.filters.album.is_none(),
        "All sounds must release the Album scope as well as the folder scope"
    );
    assert!(ui.get_album_filter().is_empty());
    assert!(ui.get_album_filter_id().is_empty());
    assert!(!ui.get_album_filter_unavailable());
    assert_eq!(request.filters.favorites_only, favorites_only);
    assert_eq!(ui.get_favorites_only(), favorites_only);
    assert!(ui.get_selected_folder_path().is_empty());
    let state = fixture.controller.task_workspace.lock().unwrap();
    assert!(state.unavailable_album.is_none());
    let active = state.workspace.session.active_task_id.as_ref().unwrap();
    let task = state
        .workspace
        .session
        .tasks
        .iter()
        .find(|task| &task.id == active)
        .unwrap();
    assert!(task.definition.filters.album_id.is_none());
    assert!(task.definition.album_binding.is_none());
}

fn cross_index_navigation(server_snapshot: bool) {
    let fixture = cross_index_fixture(server_snapshot);
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    search(&fixture, "shared", 4);
    wait_albums(&fixture, 2);
    let gore = ui
        .get_albums()
        .iter()
        .find(|album| album.name == "Gore album")
        .unwrap()
        .id;
    let expected_ids = controller
        .records
        .lock()
        .unwrap()
        .iter()
        .map(|file| file.id)
        .collect::<HashSet<_>>();
    let root = ui
        .get_folders()
        .iter()
        .position(|folder| folder.name == "sounds")
        .unwrap();
    ui.invoke_folder_toggle_requested(root as i32);
    wait_for(ui, |ui| {
        ui.get_folders().iter().any(|folder| folder.name == "wind")
    });
    assert_eq!(directory_count(&fixture, "gore"), Some(3));
    assert_eq!(directory_count(&fixture, "wind"), Some(1));

    // Reproduce the reported click sequence with nonempty keyword results in
    // both albums and different directories.
    ui.invoke_album_selected(gore.clone());
    wait_navigation(&fixture, 3);
    assert_eq!(directory_count(&fixture, "wind"), None);
    assert!(!ui.get_folders().iter().any(|folder| folder.name == "wind"));
    let stale_generation = controller.search_generation.load(Ordering::Acquire);
    let stale_folders = controller.tree.lock().unwrap().matches.clone();
    controller.reload_tree();
    i_slint_backend_testing::ElementHandle::find_by_accessible_label(ui, "All sounds")
        .next()
        .expect("the global navigation button remains reachable with an Album filter")
        .invoke_accessible_default_action();
    assert_all_scope(&fixture, false);
    wait_navigation(&fixture, 4);
    assert_eq!(directory_count(&fixture, "gore"), Some(3));
    assert_eq!(directory_count(&fixture, "wind"), Some(1));
    assert!(ui.get_folders().iter().any(|folder| folder.name == "wind"));
    assert_eq!(
        controller
            .records
            .lock()
            .unwrap()
            .iter()
            .map(|file| file.id)
            .collect::<HashSet<_>>(),
        expected_ids
    );
    // A queued old tree rebuild and late success/error completions cannot put
    // the album-scoped folders back after the global navigation has completed.
    let current_folders = controller.tree.lock().unwrap().matches.clone();
    controller.search_folders_computed(ui, stale_generation, Ok(stale_folders));
    controller.search_folders_computed(ui, stale_generation, Err("obsolete Album facets".into()));
    wait_for(ui, |_| true);
    assert!(Arc::ptr_eq(
        &current_folders,
        &controller.tree.lock().unwrap().matches
    ));
    assert!(!ui.get_status().contains("obsolete Album facets"));

    // Folder -> Album is also an intersection; All sounds releases both scopes
    // rather than leaving an invisible Album constraint behind.
    choose_gore(&fixture);
    wait_navigation(&fixture, 3);
    wait_albums(&fixture, 1);
    ui.invoke_album_selected(gore.clone());
    wait_navigation(&fixture, 3);
    assert!(controller.request.lock().unwrap().folder_id.is_some());
    ui.invoke_all_library_requested();
    assert_all_scope(&fixture, false);
    wait_navigation(&fixture, 4);
    assert_eq!(directory_count(&fixture, "wind"), Some(1));

    // The Favorites menu reuses this callback after setting its own flag. Clear
    // an unavailable Album binding without resetting that unrelated filter.
    wait_albums(&fixture, 2);
    ui.invoke_album_selected(gore.clone());
    wait_navigation(&fixture, 3);
    ui.set_album_filter_unavailable(true);
    {
        let mut state = controller.task_workspace.lock().unwrap();
        state.unavailable_album = Some(gore.parse().unwrap());
        let active = state.workspace.session.active_task_id.clone().unwrap();
        state
            .workspace
            .session
            .tasks
            .iter_mut()
            .find(|task| task.id == active)
            .unwrap()
            .definition
            .album_binding
            .as_mut()
            .unwrap()
            .unavailable = true;
    }
    ui.set_favorites_only(true);
    ui.invoke_all_library_requested();
    assert_all_scope(&fixture, true);
    wait_navigation(&fixture, 2);
    assert_eq!(directory_count(&fixture, "gore"), Some(1));
    assert_eq!(directory_count(&fixture, "wind"), Some(1));
    finish(&fixture);
}

#[test]
fn all_sounds_drops_album_result_cache_when_an_invalid_filter_defers_search() {
    isolated("album_review::child_all_sounds_invalid_filter_cache_local");
}

#[test]
fn all_sounds_drops_album_result_cache_for_invalid_filter_in_read_only_snapshot() {
    isolated("album_review::child_all_sounds_invalid_filter_cache_server");
}

#[test]
#[ignore = "isolated local invalid-filter scope cache regression"]
fn child_all_sounds_invalid_filter_cache_local() {
    all_sounds_invalid_filter_cache(false);
}

#[test]
#[ignore = "isolated read-only snapshot invalid-filter scope cache regression"]
fn child_all_sounds_invalid_filter_cache_server() {
    all_sounds_invalid_filter_cache(true);
}

fn all_sounds_invalid_filter_cache(server_snapshot: bool) {
    let fixture = cross_index_fixture(server_snapshot);
    let ui = &fixture.ui;
    search(&fixture, "shared", 4);
    wait_albums(&fixture, 2);
    let gore = ui
        .get_albums()
        .iter()
        .find(|album| album.name == "Gore album")
        .unwrap()
        .id;
    ui.invoke_album_selected(gore);
    wait_navigation(&fixture, 3);
    let first = active_id(ui);

    // The last successful page belongs to Gore. An invalid duration prevents
    // both Apply and All sounds from dispatching a replacement search.
    ui.set_duration_min("unfinished".into());
    ui.invoke_apply_filters();
    assert!(ui.get_status().contains("Duration"));
    ui.invoke_all_library_requested();
    assert_all_scope(&fixture, false);
    assert_eq!(ui.get_rows().row_count(), 3);
    assert!(!ui.get_searching());

    // Repair the draft without submitting it, then leave and restore the task.
    // Its new all-library definition must never reuse Gore's old result cache.
    ui.set_duration_min("".into());
    ui.invoke_search_task_created();
    wait_for(ui, |ui| {
        !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
            && ui.get_search_tasks().row_count() == 2
    });
    search(&fixture, "wind", 1);
    ui.invoke_search_task_selected(first.clone().into());
    wait_for(ui, move |ui| {
        active_id(ui) == first
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
    });
    assert_all_scope(&fixture, false);
    assert_eq!(
        ui.get_rows().row_count(),
        4,
        "the restored all-library task must not reuse the previous Album's three-row cache"
    );
    wait_navigation(&fixture, 4);
    assert_eq!(directory_count(&fixture, "gore"), Some(3));
    assert_eq!(directory_count(&fixture, "wind"), Some(1));
    finish(&fixture);
}

#[test]
fn album_navigation_drops_result_cache_when_an_invalid_filter_defers_search() {
    isolated("album_review::child_album_navigation_invalid_filter_cache");
}

#[test]
fn folder_navigation_drops_result_cache_when_an_invalid_filter_defers_search() {
    isolated("album_review::child_folder_navigation_invalid_filter_cache");
}

#[test]
#[ignore = "isolated Album navigation invalid-filter cache regression"]
fn child_album_navigation_invalid_filter_cache() {
    scoped_navigation_invalid_filter_cache(true);
}

#[test]
#[ignore = "isolated folder navigation invalid-filter cache regression"]
fn child_folder_navigation_invalid_filter_cache() {
    scoped_navigation_invalid_filter_cache(false);
}

fn scoped_navigation_invalid_filter_cache(album_scope: bool) {
    let fixture = cross_index_fixture(false);
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    search(&fixture, "shared", 4);
    wait_albums(&fixture, 2);
    let wind = ui
        .get_albums()
        .iter()
        .find(|album| album.name == "Wind album")
        .unwrap()
        .id;
    let root = ui
        .get_folders()
        .iter()
        .position(|folder| folder.name == "sounds")
        .unwrap();
    ui.invoke_folder_toggle_requested(root as i32);
    wait_for(ui, |ui| {
        ui.get_folders().iter().any(|folder| folder.name == "gore")
    });
    let gore_index = ui
        .get_folders()
        .iter()
        .position(|folder| folder.name == "gore")
        .unwrap();
    let first = active_id(ui);

    // Both explicit scopes change the executable definition before validation.
    // Rejected input must not let them retain the previous global page's cache.
    ui.set_duration_min("unfinished".into());
    ui.invoke_apply_filters();
    assert!(ui.get_status().contains("Duration"));
    if album_scope {
        ui.invoke_album_selected(wind.clone());
        assert_eq!(
            controller.request.lock().unwrap().filters.album_id,
            Some(wind.parse::<i64>().unwrap())
        );
    } else {
        ui.invoke_folder_requested(gore_index as i32);
        assert!(controller.request.lock().unwrap().folder_id.is_some());
    }
    assert_eq!(ui.get_rows().row_count(), 4);
    assert!(!ui.get_searching());
    ui.set_duration_min("".into());
    ui.invoke_search_task_created();
    wait_for(ui, |ui| {
        !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
            && ui.get_search_tasks().row_count() == 2
    });
    search(&fixture, "wind", 1);
    ui.invoke_search_task_selected(first.clone().into());
    wait_for(ui, move |ui| {
        active_id(ui) == first
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
    });
    assert_eq!(controller.request.lock().unwrap().query, "shared");
    if album_scope {
        assert_eq!(ui.get_album_filter_id(), wind);
        assert!(controller.request.lock().unwrap().folder_id.is_none());
    } else {
        assert!(controller.request.lock().unwrap().folder_id.is_some());
        assert!(ui.get_album_filter_id().is_empty());
    }
    let expected = if album_scope { 1 } else { 3 };
    assert_eq!(
        ui.get_rows().row_count(),
        expected,
        "the restored navigation scope must not reuse the previous global four-row cache"
    );
    wait_navigation(&fixture, expected);
    finish(&fixture);
}
