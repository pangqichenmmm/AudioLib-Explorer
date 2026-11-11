// app/audiolib-desktop/src/search_task_favorite_review_tests.rs
//! Reviewed favorite association, semantic folder identity and explicit-name regressions.
use super::super::{resolve_task, same_search_definition};
use super::*;
use audiolib_core::search_workspace::{
    SavedSearchTask, SearchAudioRef, SearchFolderRef, SearchTask, SearchTaskDefinition,
};

fn wait_task(fixture: &Fixture, id: &str) {
    let wanted = id.to_owned();
    wait_for(&fixture.ui, move |ui| {
        active_id(ui) == wanted
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
    });
}

fn new_task(fixture: &Fixture) -> String {
    let count = fixture.ui.get_search_tasks().row_count() + 1;
    fixture.ui.invoke_search_task_created();
    wait_for(&fixture.ui, move |ui| {
        ui.get_search_tasks().row_count() == count
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
    });
    active_id(&fixture.ui)
}

fn reused_task_favorite(background: bool) {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    search(&fixture, "wind", 1);
    let first = active_id(ui);
    ui.invoke_search_task_favorite_toggled(first.clone().into());
    let saved = {
        let state = controller.task_workspace.lock().unwrap();
        state.workspace.favorites[0].clone()
    };
    search(&fixture, "gore", 3);
    let reused = new_task(&fixture);
    search(&fixture, "wind", 1);
    ui.invoke_search_task_renamed(reused.clone().into(), "My wind task".into());
    if background {
        ui.invoke_search_task_selected(first.clone().into());
        wait_task(&fixture, &first);
    }

    ui.invoke_saved_search_opened(saved.id.clone().into());
    wait_task(&fixture, &reused);
    assert_eq!(ui.get_search_tasks().row_count(), 2);
    let row = ui
        .get_search_tasks()
        .iter()
        .find(|task| task.active)
        .unwrap();
    assert!(row.favorite);
    assert!(!row.modified);
    assert_eq!(row.name, "My wind task");
    {
        let state = controller.task_workspace.lock().unwrap();
        let task = state
            .workspace
            .session
            .tasks
            .iter()
            .find(|task| task.id == reused)
            .unwrap();
        assert_eq!(task.saved_search_id.as_deref(), Some(saved.id.as_str()));
        assert_eq!(state.workspace.favorites, vec![saved]);
    }
    controller.wait_workspace_writes().unwrap();
    ui.invoke_search_task_favorite_toggled(reused.into());
    assert_eq!(ui.get_saved_search_tasks().row_count(), 0);
    assert!(ui.get_search_tasks().iter().all(|task| !task.favorite));
    let state = controller.task_workspace.lock().unwrap();
    assert!(state.workspace.favorites.is_empty());
    assert!(
        state
            .workspace
            .session
            .tasks
            .iter()
            .all(|task| task.saved_search_id.is_none())
    );
    drop(state);
    finish(&fixture);
}

#[test]
fn opening_a_favorite_binds_an_already_active_matching_task_without_creating_duplicates() {
    isolated("favorite_review::child_reused_active_favorite");
}

#[test]
#[ignore = "isolated already-active matching favorite association"]
fn child_reused_active_favorite() {
    reused_task_favorite(false);
}

#[test]
fn opening_a_favorite_binds_a_background_matching_task_without_creating_duplicates() {
    isolated("favorite_review::child_reused_background_favorite");
}

#[test]
#[ignore = "isolated background matching favorite association"]
fn child_reused_background_favorite() {
    reused_task_favorite(true);
}

#[test]
fn full_capacity_reuses_a_favorite_after_folder_identity_drift_without_marking_it_modified() {
    isolated("favorite_review::child_folder_identity_drift");
}

#[test]
#[ignore = "isolated favorite scope identity after snapshot folder-ID drift"]
fn child_folder_identity_drift() {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    search(&fixture, "gore", 3);
    choose_gore(&fixture);
    let first = active_id(ui);
    ui.invoke_search_task_favorite_toggled(first.clone().into());
    let saved = {
        let mut state = controller.task_workspace.lock().unwrap();
        let saved = &mut state.workspace.favorites[0];
        // A snapshot may assign a new numeric ID while retaining this indexed
        // path. Only the favorite's old ID is stale; the active task is valid.
        saved.definition.folder.as_mut().unwrap().id += 1_000_000;
        saved.clone()
    };
    controller.publish_search_tasks(ui);
    assert!(
        !ui.get_search_tasks()
            .iter()
            .find(|task| task.active)
            .unwrap()
            .modified
    );
    let capacity = audiolib_core::search_workspace::MAX_SEARCH_TASKS;
    for _ in 1..capacity {
        new_task(&fixture);
    }
    assert_eq!(ui.get_search_tasks().row_count(), capacity);
    ui.invoke_saved_search_opened(saved.id.clone().into());
    wait_task(&fixture, &first);
    assert_eq!(ui.get_search_tasks().row_count(), capacity);
    let row = ui
        .get_search_tasks()
        .iter()
        .find(|task| task.active)
        .unwrap();
    assert!(row.favorite && !row.modified);
    assert_eq!(ui.get_rows().row_count(), 3);
    let state = controller.task_workspace.lock().unwrap();
    assert_eq!(state.workspace.favorites, vec![saved]);
    let task = state
        .workspace
        .session
        .tasks
        .iter()
        .find(|task| task.id == first)
        .unwrap();
    assert_ne!(
        task.definition.folder.as_ref().unwrap().id,
        state.workspace.favorites[0]
            .definition
            .folder
            .as_ref()
            .unwrap()
            .id
    );
    drop(state);
    finish(&fixture);
}

#[test]
fn definition_identity_allows_id_drift_without_merging_distinct_indexed_aliases() {
    let original = SearchTaskDefinition {
        query: "wind".into(),
        folder: Some(SearchFolderRef {
            id: 1,
            path: "X:/Sounds/Wind".into(),
        }),
        ..Default::default()
    };
    let mut compared = original.clone();
    compared.folder = Some(SearchFolderRef {
        id: 91,
        path: "X:/Sounds/Wind".into(),
    });
    assert!(same_search_definition(&original, &compared));
    compared.folder.as_mut().unwrap().path = "x:\\Sounds\\Wind".into();
    assert!(!same_search_definition(&original, &compared));
    compared.folder.as_mut().unwrap().id = 1;
    assert!(same_search_definition(&original, &compared));
    compared.folder.as_mut().unwrap().path = "X:/Sounds/wind".into();
    assert!(!same_search_definition(&original, &compared));
    compared.folder.as_mut().unwrap().path = "X:/Sounds/Rain".into();
    assert!(!same_search_definition(&original, &compared));
    compared.folder = None;
    assert!(!same_search_definition(&original, &compared));
    let changes: [fn(&mut SearchTaskDefinition); 6] = [
        |definition: &mut SearchTaskDefinition| definition.query.push_str(" rain"),
        |definition: &mut SearchTaskDefinition| definition.scope = audiolib_core::SearchScope::Path,
        |definition: &mut SearchTaskDefinition| definition.include_subfolders = false,
        |definition: &mut SearchTaskDefinition| definition.filters.favorites_only = true,
        |definition: &mut SearchTaskDefinition| definition.order_by = SortOrder::Filename,
        |definition: &mut SearchTaskDefinition| definition.descending = true,
    ];
    for change in changes {
        let mut compared = original.clone();
        change(&mut compared);
        assert!(!same_search_definition(&original, &compared));
    }

    let original = SearchTaskDefinition {
        folder: Some(SearchFolderRef {
            id: 1,
            path: "/server/Audio".into(),
        }),
        ..Default::default()
    };
    let mut compared = original.clone();
    compared.folder.as_mut().unwrap().path = "/server/audio".into();
    assert!(!same_search_definition(&original, &compared));
    compared.folder.as_mut().unwrap().path = "/server/Audio\\Wind".into();
    let mut literal_backslash = compared.clone();
    literal_backslash.folder.as_mut().unwrap().path = "/server/Audio/Wind".into();
    assert!(!same_search_definition(&compared, &literal_backslash));
}

#[test]
fn full_capacity_favorite_open_does_not_rebind_a_case_distinct_posix_directory() {
    isolated("favorite_review::child_case_distinct_full_capacity");
}

#[test]
#[ignore = "isolated full-capacity case-distinct POSIX folder favorite"]
fn child_case_distinct_full_capacity() {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    let prefix = format!(
        "/__audiolib_task_case/{}",
        fixture.root.file_name().unwrap().to_string_lossy()
    );
    let uppercase = format!("{prefix}/Audio");
    let lowercase = format!("{prefix}/audio");
    for path in [&uppercase, &lowercase] {
        fixture
            .provider
            .ensure_scan_folder(Path::new(path), Path::new(path))
            .unwrap();
    }
    let upper = fixture
        .provider
        .find_folder_by_path(Path::new(&uppercase))
        .unwrap()
        .unwrap();
    let lower = fixture
        .provider
        .find_folder_by_path(Path::new(&lowercase))
        .unwrap()
        .unwrap();
    assert_ne!(upper.id, lower.id);
    let capacity = audiolib_core::search_workspace::MAX_SEARCH_TASKS;
    for _ in 1..capacity {
        new_task(&fixture);
    }
    let before = active_id(ui);
    let saved = SavedSearchTask {
        id: "case-distinct-favorite".into(),
        title: "Lowercase directory".into(),
        definition: SearchTaskDefinition {
            // A reused numeric ID cannot override the saved source path.
            folder: Some(SearchFolderRef {
                id: upper.id,
                path: lower.path,
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    {
        let mut state = controller.task_workspace.lock().unwrap();
        let task = &mut state.workspace.session.tasks[0];
        task.definition.folder = Some(SearchFolderRef {
            id: upper.id,
            path: upper.path,
        });
        task.view.raw_inputs.clear();
        state.workspace.favorites.push(saved.clone());
    }
    controller.publish_search_tasks(ui);
    ui.invoke_saved_search_opened(saved.id.clone().into());
    assert_eq!(active_id(ui), before);
    assert_eq!(ui.get_search_tasks().row_count(), capacity);
    assert!(ui.get_status().contains("limit"));
    let state = controller.task_workspace.lock().unwrap();
    assert_eq!(state.workspace.favorites, vec![saved]);
    assert!(
        state
            .workspace
            .session
            .tasks
            .iter()
            .all(|task| task.saved_search_id.is_none())
    );
    drop(state);
    finish(&fixture);
}

#[test]
fn reused_ids_restore_exact_posix_folder_selected_checked_and_anchor_paths() {
    isolated("favorite_review::child_case_distinct_id_reuse");
}

#[test]
#[ignore = "isolated snapshot-style ID reuse for POSIX task references"]
fn child_case_distinct_id_reuse() {
    let fixture = fixture();
    let prefix = format!(
        "/__audiolib_task_case/{}",
        fixture.root.file_name().unwrap().to_string_lossy()
    );
    let uppercase = format!("{prefix}/Audio");
    let lowercase = format!("{prefix}/audio");
    for path in [&uppercase, &lowercase] {
        fixture
            .provider
            .ensure_scan_folder(Path::new(path), Path::new(path))
            .unwrap();
    }
    let upper = fixture
        .provider
        .find_folder_by_path(Path::new(&uppercase))
        .unwrap()
        .unwrap();
    let lower = fixture
        .provider
        .find_folder_by_path(Path::new(&lowercase))
        .unwrap()
        .unwrap();
    let upper_file = format!("{uppercase}/same.wav");
    let lower_file = format!("{lowercase}/same.wav");
    {
        // Only this child's owned temporary library is initialized. Register
        // the official SQLite functions so normal triggers/revisions stay live.
        let connection = rusqlite::Connection::open(fixture.provider.database_path()).unwrap();
        audiolib_db::register_library_functions(&connection).unwrap();
        for (id, path, folder_id) in [
            (900_001, &upper_file, upper.id),
            (900_002, &lower_file, lower.id),
        ] {
            connection.execute(
                "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,missing) VALUES(?1,?2,?3,'same.wav',?4,0)",
                rusqlite::params![id, path, path.to_lowercase(), folder_id],
            ).unwrap();
        }
    }
    let folder = SearchFolderRef {
        id: lower.id,
        path: upper.path.clone(),
    };
    let audio = SearchAudioRef {
        id: 900_002,
        file_path: upper_file.clone(),
    };
    let mut task = SearchTask::default();
    task.definition.folder = Some(folder.clone());
    task.view.expanded_folders.push(folder);
    task.view.selected_audio = Some(audio.clone());
    task.view.checked_audio.push(audio.clone());
    task.view.selection_anchor = Some(audio);
    let (resolved, _, selected, missing, _) = resolve_task(&fixture.provider, task, None).unwrap();
    assert!(!missing);
    assert_eq!(resolved.definition.folder.as_ref().unwrap().id, upper.id);
    assert_eq!(
        resolved.definition.folder.as_ref().unwrap().path,
        upper.path
    );
    assert_eq!(resolved.view.expanded_folders[0].id, upper.id);
    assert_eq!(selected.as_ref().unwrap().id, 900_001);
    assert_eq!(selected.as_ref().unwrap().file_path, upper_file);
    assert_eq!(resolved.view.selected_audio.as_ref().unwrap().id, 900_001);
    assert_eq!(resolved.view.checked_audio[0].id, 900_001);
    assert_eq!(resolved.view.selection_anchor.as_ref().unwrap().id, 900_001);

    // A third spelling does not name either indexed entry. Losing those refs
    // is safer than selecting the record which reused their numeric ID.
    let absent_folder = format!("{prefix}/AUDIO");
    let absent_file = format!("{absent_folder}/same.wav");
    let mut absent = resolved;
    absent.definition.folder = Some(SearchFolderRef {
        id: lower.id,
        path: absent_folder.clone(),
    });
    absent.view.expanded_folders = vec![SearchFolderRef {
        id: lower.id,
        path: absent_folder,
    }];
    let reference = SearchAudioRef {
        id: 900_002,
        file_path: absent_file,
    };
    absent.view.selected_audio = Some(reference.clone());
    absent.view.checked_audio = vec![reference.clone()];
    absent.view.selection_anchor = Some(reference);
    let (resolved, _, selected, missing, _) =
        resolve_task(&fixture.provider, absent, None).unwrap();
    assert!(missing);
    assert!(selected.is_none());
    assert!(resolved.view.selected_audio.is_none());
    assert!(resolved.view.checked_audio.is_empty());
    assert!(resolved.view.selection_anchor.is_none());
    assert!(resolved.view.expanded_folders.is_empty());
    finish(&fixture);
}

fn explicit_name_survives_transitions(name: &str) {
    let fixture = fixture();
    let ui = &fixture.ui;
    let controller = &fixture.controller;
    search(&fixture, "gore", 3);
    let first = active_id(ui);
    ui.invoke_search_task_renamed(first.clone().into(), name.into());
    assert_eq!(
        ui.get_search_tasks()
            .iter()
            .find(|task| task.active)
            .unwrap()
            .name,
        name
    );
    ui.invoke_search_task_favorite_toggled(first.clone().into());
    let saved_id = ui.get_saved_search_tasks().row_data(0).unwrap().id;
    let second = new_task(&fixture);
    search(&fixture, "wind", 1);
    ui.invoke_search_task_selected(first.clone().into());
    wait_task(&fixture, &first);
    assert_eq!(
        ui.get_search_tasks()
            .iter()
            .find(|task| task.active)
            .unwrap()
            .name,
        name
    );
    ui.invoke_search_task_selected(second.clone().into());
    wait_task(&fixture, &second);
    ui.invoke_search_task_closed(first.into());
    assert_eq!(ui.get_search_tasks().row_count(), 1);
    ui.invoke_saved_search_opened(saved_id.clone());
    wait_for(ui, |ui| {
        ui.get_search_tasks().row_count() == 2
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
    });
    assert_eq!(
        ui.get_search_tasks()
            .iter()
            .find(|task| task.active)
            .unwrap()
            .name,
        name
    );

    // A user-chosen `default` must remain literal when automatic labels would
    // translate into 默认搜索; this also persists the real runtime language.
    ui.set_chinese(true);
    ui.invoke_language_requested(true);
    let language = controller.clone();
    wait_for(ui, move |ui| {
        !ui.get_searching() && language.config.lock().unwrap().language == "cn"
    });
    assert_eq!(
        ui.get_search_tasks()
            .iter()
            .find(|task| task.active)
            .unwrap()
            .name,
        name
    );
    controller.capture_search_task(ui);
    controller.shutdown();

    let config = AppConfig::load_in(fixture.root.join("preferences")).unwrap();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let restored_ui = AppWindow::new().unwrap();
    let restored = Controller::new(
        &restored_ui,
        fixture.provider.clone(),
        engine.handle(),
        config,
        playback,
    );
    restored_ui.show().unwrap();
    ui.hide().unwrap();
    wait_for(&restored_ui, |ui| {
        ui.get_search_tasks().row_count() == 2
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
    });
    assert_eq!(
        restored_ui
            .get_search_tasks()
            .iter()
            .find(|task| task.active)
            .unwrap()
            .name,
        name
    );
    {
        let state = restored.task_workspace.lock().unwrap();
        let task = state
            .workspace
            .session
            .tasks
            .iter()
            .find(|task| Some(&task.id) == state.workspace.session.active_task_id.as_ref())
            .unwrap();
        assert!(task.title_is_custom);
        assert_eq!(task.title, name);
        assert!(state.workspace.favorites[0].title_is_custom);
        assert_eq!(state.workspace.favorites[0].title, name);
    }
    // Renaming the favorite independently also keeps explicit-name provenance
    // when it is opened into a fresh task.
    restored_ui.invoke_saved_search_renamed(saved_id.clone(), "Search 2".into());
    let current = active_id(&restored_ui);
    restored_ui.invoke_search_task_closed(current.into());
    wait_for(&restored_ui, |ui| {
        ui.get_search_tasks().row_count() == 1
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
    });
    restored_ui.invoke_saved_search_opened(saved_id);
    wait_for(&restored_ui, |ui| {
        ui.get_search_tasks().row_count() == 2
            && !ui.get_search_workspace_loading()
            && !ui.get_task_restoring()
            && !ui.get_searching()
    });
    assert_eq!(
        restored_ui
            .get_search_tasks()
            .iter()
            .find(|task| task.active)
            .unwrap()
            .name,
        "Search 2"
    );
    restored.capture_search_task(&restored_ui);
    restored.shutdown();
    restored_ui.hide().unwrap();
}

#[test]
fn explicit_legacy_shaped_name_survives_switch_restart_and_favorite_reopen() {
    isolated("favorite_review::child_explicit_legacy_name");
}

#[test]
#[ignore = "isolated explicit legacy-shaped name through task transitions"]
fn child_explicit_legacy_name() {
    explicit_name_survives_transitions("Search 2");
}

#[test]
fn explicit_default_name_survives_switch_restart_and_favorite_reopen() {
    isolated("favorite_review::child_explicit_default_name");
}

#[test]
#[ignore = "isolated explicit default name through task transitions"]
fn child_explicit_default_name() {
    explicit_name_survives_transitions("default");
}
