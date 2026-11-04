// crates/audiolib-db/tests/search_workspace.rs
//! Personal search sessions survive independently of libraries and server snapshots.
use audiolib_core::{
    DataProvider, SavedSearchTask, SearchAudioRef, SearchFolderRef, SearchTask,
    SearchTaskDefinition, SearchWorkspace, SearchWorkspaceTarget,
};
use audiolib_db::{
    SqliteProvider,
    local_play_history::{local_library_id, safe_library_id},
};
use std::{fs, path::Path};

fn workspace_path(target: &SearchWorkspaceTarget) -> std::path::PathBuf {
    target
        .data_root
        .join("search-workspaces")
        .join(safe_library_id(&target.library_scope).unwrap())
        .join("workspace.json")
}

fn task(id: &str, query: &str) -> SearchTask {
    SearchTask {
        id: id.into(),
        title: format!("Search {id}"),
        definition: SearchTaskDefinition {
            query: query.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn provider(directory: &Path) -> SqliteProvider {
    SqliteProvider::open(&directory.join("library.db")).unwrap()
}

#[test]
fn read_only_provider_round_trips_unfavorited_tasks_without_changing_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    drop(provider(directory.path()));
    let library = directory.path().join("library.db");
    let before = fs::read(&library).unwrap();
    let provider = SqliteProvider::open_read_only(&library).unwrap();
    let target = SearchWorkspaceTarget::new(
        directory.path().join("userdata"),
        local_library_id(&library).unwrap(),
    );
    let mut workspace = provider.load_search_workspace(&target).unwrap();
    assert!(workspace.session.tasks.is_empty());
    assert!(
        !target.data_root.exists(),
        "reading absent state must not create a store"
    );
    workspace.session.tasks = vec![task("one", "wood"), task("two", "metal")];
    workspace.session.active_task_id = Some("two".into());
    workspace.session.tasks[0].definition.folder = Some(SearchFolderRef {
        id: 2,
        path: "/sounds/wood".into(),
    });
    workspace.session.tasks[0].definition.filters.include_tags = vec!["soft".into()];
    workspace.session.tasks[0].definition.filters.exclude_tags = vec!["impact".into()];
    workspace.session.tasks[0].view.selected_audio = Some(SearchAudioRef {
        id: 9,
        file_path: "/sounds/wood/soft.wav".into(),
    });
    workspace.session.tasks[0]
        .view
        .raw_inputs
        .insert("duration_min".into(), "invalid draft".into());
    workspace.session.tasks[0].view.list_scroll_y = -75.0;
    workspace.favorites.push(SavedSearchTask {
        id: "saved".into(),
        title: "Original wood".into(),
        title_is_custom: true,
        definition: workspace.session.tasks[0].definition.clone(),
    });
    provider.save_search_workspace(&target, &workspace).unwrap();
    workspace.session.tasks[0].definition.query = "edited wood".into();
    provider.save_search_workspace(&target, &workspace).unwrap();
    assert_eq!(provider.load_search_workspace(&target).unwrap(), workspace);
    assert_eq!(workspace.favorites[0].definition.query, "wood");
    assert_eq!(fs::read(&library).unwrap(), before);
    let mut paths = fs::read_dir(workspace_path(&target).parent().unwrap()).unwrap();
    assert!(paths.next().is_some());
    assert!(
        paths.next().is_none(),
        "successful replacement must leave no temporary file"
    );
}

#[test]
fn personal_library_scopes_are_isolated_and_snapshot_names_are_irrelevant() {
    let directory = tempfile::tempdir().unwrap();
    let provider = provider(directory.path());
    let target_a = SearchWorkspaceTarget::new(directory.path().join("userdata"), "server-A");
    let target_b = SearchWorkspaceTarget::new(&target_a.data_root, "server-a");
    let mut workspace = SearchWorkspace::for_scope(&target_a.library_scope);
    workspace.session.tasks.push(task("task", "wind"));
    provider
        .save_search_workspace(&target_a, &workspace)
        .unwrap();
    assert!(
        provider
            .load_search_workspace(&target_b)
            .unwrap()
            .session
            .tasks
            .is_empty()
    );
    assert!(
        provider
            .save_search_workspace(&target_b, &workspace)
            .is_err()
    );
    drop(provider);
    let other = SqliteProvider::open(&directory.path().join("replacement-snapshot.db")).unwrap();
    assert_eq!(other.load_search_workspace(&target_a).unwrap(), workspace);
}

#[test]
fn workspace_round_trips_signed_audio_and_folder_identities_from_an_existing_library() {
    let directory = tempfile::tempdir().unwrap();
    let provider = provider(directory.path());
    let connection = rusqlite::Connection::open(provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&connection).unwrap();
    for (id, path) in [(-9, "/sounds/negative"), (0, "/sounds/zero")] {
        connection
            .execute(
                "INSERT INTO folders(id,folder_path,folder_name) VALUES(?1,?2,'sounds')",
                rusqlite::params![id, path],
            )
            .unwrap();
    }
    for id in [i64::MIN, -7, 0] {
        connection
            .execute(
                "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,missing)
                 VALUES(?1,?2,?2,'wind.wav',-9,0)",
                rusqlite::params![id, format!("/sounds/negative/{id}.wav")],
            )
            .unwrap();
    }
    connection
        .execute_batch(
            "INSERT INTO folder_closure(ancestor_id,descendant_id,depth)
             VALUES(-9,-9,0),(0,0,0);
             UPDATE folders SET file_count=3,total_file_count=3 WHERE id=-9;",
        )
        .unwrap();
    let library = provider.database_path().to_owned();
    drop(connection);
    drop(provider);
    let provider = SqliteProvider::open_existing(&library).unwrap();

    let selected = provider.get_audio(-7).unwrap();
    let zero = provider.get_audio(0).unwrap();
    let minimum = provider.get_audio(i64::MIN).unwrap();
    let negative_folder = provider.get_folder(-9).unwrap();
    let zero_folder = provider.get_folder(0).unwrap();
    let target = SearchWorkspaceTarget::new(
        directory.path().join("userdata"),
        local_library_id(provider.database_path()).unwrap(),
    );
    let mut workspace = provider.load_search_workspace(&target).unwrap();
    let mut current = task("signed-identities", "wind");
    current.definition.folder = Some(SearchFolderRef {
        id: negative_folder.id,
        path: negative_folder.path,
    });
    current.view.expanded_folders = vec![SearchFolderRef {
        id: zero_folder.id,
        path: zero_folder.path,
    }];
    current.view.selected_audio = Some(SearchAudioRef {
        id: selected.id,
        file_path: selected.file_path,
    });
    current.view.checked_audio = [zero, minimum]
        .into_iter()
        .map(|file| SearchAudioRef {
            id: file.id,
            file_path: file.file_path,
        })
        .collect();
    current.view.selection_anchor = current.view.checked_audio.first().cloned();
    workspace.session.active_task_id = Some(current.id.clone());
    workspace.session.tasks.push(current);

    provider.save_search_workspace(&target, &workspace).unwrap();
    assert_eq!(provider.load_search_workspace(&target).unwrap(), workspace);
    assert_eq!(provider.get_audio(-7).unwrap().id, -7);
    assert_eq!(provider.get_folder(0).unwrap().id, 0);
}

#[test]
fn damaged_foreign_and_newer_files_are_preserved_on_load_and_save() {
    let directory = tempfile::tempdir().unwrap();
    let provider = provider(directory.path());
    let target = SearchWorkspaceTarget::new(directory.path().join("userdata"), "server-A");
    let workspace = SearchWorkspace::for_scope(&target.library_scope);
    provider.save_search_workspace(&target, &workspace).unwrap();
    let path = workspace_path(&target);
    for raw in [
        b"{broken".to_vec(),
        br#"{"unrelated":"json"}"#.to_vec(),
        br#"{"format_version":999,"library_scope":"server-A"}"#.to_vec(),
        br#"{"format_version":1,"library_scope":"server-B"}"#.to_vec(),
    ] {
        fs::write(&path, &raw).unwrap();
        assert!(provider.load_search_workspace(&target).is_err());
        assert!(provider.save_search_workspace(&target, &workspace).is_err());
        assert_eq!(fs::read(&path).unwrap(), raw);
    }
}

#[test]
fn version_one_workspaces_keep_all_tasks_and_favorites_until_an_ordered_v2_save() {
    let directory = tempfile::tempdir().unwrap();
    let provider = provider(directory.path());
    let target = SearchWorkspaceTarget::new(directory.path().join("userdata"), "server-legacy");
    let mut expected = SearchWorkspace::for_scope(&target.library_scope);
    expected.session.tasks = vec![task("one", "wood"), task("two", "metal")];
    expected.session.active_task_id = Some("two".into());
    expected.session.tasks[0].definition.filters.album = Some("Legacy Library".into());
    expected.session.tasks[0]
        .view
        .raw_inputs
        .insert("duration_min".into(), "unfinished draft".into());
    expected.session.tasks[0].view.list_scroll_y = -75.0;
    expected.favorites.push(SavedSearchTask {
        id: "saved".into(),
        title: "Original wood".into(),
        definition: expected.session.tasks[0].definition.clone(),
        ..Default::default()
    });
    provider.save_search_workspace(&target, &expected).unwrap();
    let path = workspace_path(&target);
    let mut legacy = serde_json::to_value(&expected).unwrap();
    legacy["format_version"] = 1.into();
    for task in legacy["session"]["tasks"].as_array_mut().unwrap() {
        task["definition"]
            .as_object_mut()
            .unwrap()
            .remove("album_binding");
    }
    for favorite in legacy["favorites"].as_array_mut().unwrap() {
        favorite["definition"]
            .as_object_mut()
            .unwrap()
            .remove("album_binding");
    }
    let original = serde_json::to_vec_pretty(&legacy).unwrap();
    fs::write(&path, &original).unwrap();
    let admitted = provider.load_search_workspace(&target).unwrap();
    assert_eq!(admitted, expected);
    assert_eq!(admitted.format_version, 2);
    assert_eq!(
        fs::read(&path).unwrap(),
        original,
        "loading v1 must not rewrite the user's file"
    );
    provider.save_search_workspace(&target, &admitted).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&path).unwrap()).unwrap()["format_version"],
        2
    );
    assert_eq!(provider.load_search_workspace(&target).unwrap(), expected);
}

#[test]
fn new_album_conditions_use_v2_and_the_captured_v1_reader_refuses_them() {
    // v0.8.0 deserialized unknown fields permissively, then validated this exact
    // independent version gate before exposing any stored search conditions.
    #[derive(serde::Deserialize)]
    struct PublishedV1Gate {
        format_version: u32,
    }
    impl PublishedV1Gate {
        fn accepts(&self) -> bool {
            self.format_version == 1
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let provider = provider(directory.path());
    let target = SearchWorkspaceTarget::new(directory.path().join("userdata"), "server-current");
    let mut current = SearchWorkspace::for_scope(&target.library_scope);
    let mut filtered = task("one", "");
    filtered.definition.filters.album_id = Some(17);
    filtered.definition.album_binding = Some(audiolib_core::search_workspace::AlbumFilterBinding {
        namespace: "owned-namespace".into(),
        kind: audiolib_core::AlbumKind::Metadata,
        metadata_key: Some("Only this library".into()),
        folder_path: None,
        unavailable: false,
    });
    current.session.tasks.push(filtered);
    provider.save_search_workspace(&target, &current).unwrap();
    let path = workspace_path(&target);
    let bytes = fs::read(&path).unwrap();
    let published: PublishedV1Gate = serde_json::from_slice(&bytes).unwrap();
    assert!(
        !published.accepts(),
        "the old reader must refuse before ignoring album_id"
    );
    assert_eq!(provider.load_search_workspace(&target).unwrap(), current);
    assert_eq!(fs::read(&path).unwrap(), bytes);
}

#[test]
fn unknown_workspace_conditions_cannot_be_lost_during_v1_upgrade_or_v2_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let provider = provider(directory.path());
    let target = SearchWorkspaceTarget::new(directory.path().join("userdata"), "server-unknown");
    let mut current = SearchWorkspace::for_scope(&target.library_scope);
    current.session.tasks.push(task("one", "wood"));
    provider.save_search_workspace(&target, &current).unwrap();
    let path = workspace_path(&target);
    for version in [1, 2] {
        let mut unknown = serde_json::to_value(&current).unwrap();
        unknown["format_version"] = version.into();
        unknown["session"]["tasks"][0]["definition"]["filters"]["future_album_condition"] =
            serde_json::json!({"restrict_to":"preserve this"});
        let bytes = serde_json::to_vec(&unknown).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(provider.load_search_workspace(&target).is_err());
        assert!(provider.save_search_workspace(&target, &current).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn thirty_open_tasks_reopen_with_order_and_last_active_task_intact() {
    let directory = tempfile::tempdir().unwrap();
    let first = provider(directory.path());
    let target = SearchWorkspaceTarget::new(directory.path().join("userdata"), "server-A");
    let mut workspace = SearchWorkspace::for_scope(&target.library_scope);
    workspace.session.tasks = (0..30)
        .map(|index| task(&index.to_string(), &format!("sound {index}")))
        .collect();
    workspace.session.active_task_id = Some("29".into());
    workspace.session.tasks[29].view.list_scroll_y = -150.0;
    workspace.session.tasks[29]
        .view
        .raw_inputs
        .insert("duration_min".into(), "unfinished".into());
    first.save_search_workspace(&target, &workspace).unwrap();
    drop(first);
    let reopened = provider(directory.path());
    assert_eq!(reopened.load_search_workspace(&target).unwrap(), workspace);
}

#[test]
fn too_many_open_tasks_do_not_replace_the_last_session() {
    let directory = tempfile::tempdir().unwrap();
    let provider = provider(directory.path());
    let target = SearchWorkspaceTarget::new(directory.path().join("userdata"), "server-A");
    let mut workspace = SearchWorkspace::for_scope(&target.library_scope);
    workspace.session.tasks.push(task("one", "wood"));
    provider.save_search_workspace(&target, &workspace).unwrap();
    let before = fs::read(workspace_path(&target)).unwrap();
    workspace.session.tasks = (0..=audiolib_core::search_workspace::MAX_SEARCH_TASKS)
        .map(|index| task(&index.to_string(), "wood"))
        .collect();
    assert!(provider.save_search_workspace(&target, &workspace).is_err());
    assert_eq!(fs::read(workspace_path(&target)).unwrap(), before);
}

#[cfg(windows)]
#[test]
fn junction_workspace_directories_cannot_redirect_personal_writes() {
    let directory = tempfile::tempdir().unwrap();
    let provider = provider(directory.path());
    let data_root = directory.path().join("userdata");
    let outside = directory.path().join("outside");
    fs::create_dir_all(&data_root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let workspaces = data_root.join("search-workspaces");
    let command = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&workspaces)
        .arg(&outside)
        .output()
        .unwrap();
    assert!(
        command.status.success(),
        "{}",
        String::from_utf8_lossy(&command.stderr)
    );
    let target = SearchWorkspaceTarget::new(&data_root, "server-A");
    let workspace = SearchWorkspace::for_scope(&target.library_scope);
    assert!(provider.load_search_workspace(&target).is_err());
    assert!(provider.save_search_workspace(&target, &workspace).is_err());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    fs::remove_dir(&workspaces).unwrap();
}
