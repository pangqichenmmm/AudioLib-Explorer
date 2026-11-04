// app/audiolib-desktop/src/search_task_album_binding_tests.rs
//! Owned namespace collisions never retarget open or saved album searches.
use super::*;

fn provider(path: &Path, names: &[&str]) -> Arc<SqliteProvider> {
    let provider = Arc::new(SqliteProvider::open(path).unwrap());
    let conn = rusqlite::Connection::open(path).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    for (index, name) in names.iter().enumerate() {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename,metadata_json) VALUES(?1,?2,?2,?3)",
            rusqlite::params![
                index as i64 + 1,
                format!("{name}.wav"),
                serde_json::json!({"wav_metadata":{"fields":{"library":name}}}).to_string()
            ],
        )
        .unwrap();
    }
    provider
}

fn task(provider: &SqliteProvider, id: i64) -> SearchTask {
    let album = provider.get_audio(id).unwrap().album_entity.unwrap();
    SearchTask {
        id: format!("task-{id}"),
        title: album.name.clone(),
        definition: SearchTaskDefinition {
            filters: SearchFilters {
                album_id: Some(album.id),
                ..Default::default()
            },
            album_binding: Some(binding(&provider.album_namespace(), &album)),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn open_and_saved_album_filters_rebind_after_native_id_collisions_and_restart() {
    let root = tempfile::tempdir().unwrap();
    // This is the identity transition produced by a legacy projection which
    // preserved A=1/B=2 before the server later introduced earlier C records.
    let legacy = provider(&root.path().join("legacy-owned.db"), &["A", "B", "C"]);
    let native = provider(&root.path().join("native.db"), &["C", "A", "B"]);
    let mut workspace = SearchWorkspace::for_scope("server-same-library");
    workspace.session.tasks = vec![task(&legacy, 1), task(&legacy, 2)];
    workspace.session.active_task_id = Some("task-1".into());
    workspace.favorites = workspace
        .session
        .tasks
        .iter()
        .map(|task| SavedSearchTask {
            id: format!("saved-{}", task.id),
            title: task.title.clone(),
            definition: task.definition.clone(),
            ..Default::default()
        })
        .collect();
    let target = SearchWorkspaceTarget::new(root.path().join("personal"), "server-same-library");
    legacy.save_search_workspace(&target, &workspace).unwrap();
    let mut restored = native.load_search_workspace(&target).unwrap();
    reconcile_workspace_albums(&native, &mut restored).unwrap();
    for (index, task) in restored.session.tasks.iter().enumerate() {
        assert_eq!(task.definition.filters.album_id, Some(index as i64 + 2));
        assert_eq!(
            task.definition.album_binding.as_ref().unwrap().namespace,
            native.album_namespace()
        );
        assert!(!album_filter_unavailable(&task.definition));
        let results = native.search(&task.definition.to_request(20, 0)).unwrap();
        assert_eq!(results.results.len(), 1);
        assert_eq!(results.results[0].album, task.title);
        assert_eq!(task.definition, restored.favorites[index].definition);
    }
    native.save_search_workspace(&target, &restored).unwrap();
    let mut restart = native.load_search_workspace(&target).unwrap();
    reconcile_workspace_albums(&native, &mut restart).unwrap();
    assert_eq!(restart, restored);
}

#[test]
fn missing_semantics_and_unbound_ids_remain_unavailable_instead_of_accepting_reused_ids() {
    let root = tempfile::tempdir().unwrap();
    let old = provider(&root.path().join("old.db"), &["A"]);
    let next = provider(&root.path().join("next.db"), &["C"]);
    let mut definition = task(&old, 1).definition;
    assert!(resolve_album_filter(&next, &mut definition).unwrap());
    assert_eq!(definition.filters.album_id, Some(1));
    assert!(album_filter_unavailable(&definition));
    assert!(binding_label(definition.album_binding.as_ref().unwrap(), false).contains('A'));
    definition.album_binding.as_mut().unwrap().namespace = next.album_namespace();
    definition.album_binding.as_mut().unwrap().unavailable = false;
    assert!(
        resolve_album_filter(&next, &mut definition).unwrap(),
        "same namespace still verifies the semantic owner"
    );
    definition.album_binding = None;
    assert!(
        resolve_album_filter(&next, &mut definition).unwrap(),
        "unbound numeric IDs are never adopted"
    );
    assert_eq!(definition.filters.album_id, Some(1));
    definition.filters.album_id = None;
    definition.filters.album = Some("A".into());
    assert!(!resolve_album_filter(&next, &mut definition).unwrap());
    assert_eq!(definition.filters.album.as_deref(), Some("A"));
}

#[test]
fn unassigned_and_same_named_folder_bindings_retain_their_distinct_paths() {
    let root = tempfile::tempdir().unwrap();
    let old = provider(&root.path().join("folders-old.db"), &[]);
    let next = provider(&root.path().join("folders-next.db"), &[]);
    for (provider, reverse) in [(&old, false), (&next, true)] {
        let conn = rusqlite::Connection::open(provider.database_path()).unwrap();
        audiolib_db::register_library_functions(&conn).unwrap();
        for (id, path) in if reverse {
            [(1, "/second/Wind"), (2, "/first/Wind")]
        } else {
            [(1, "/first/Wind"), (2, "/second/Wind")]
        } {
            conn.execute(
                "INSERT INTO folders(id,folder_path,folder_name) VALUES(?1,?2,'Wind')",
                rusqlite::params![id, path],
            )
            .unwrap();
            conn.execute("INSERT INTO folder_closure VALUES(?1,?1,0)", [id])
                .unwrap();
            conn.execute("INSERT INTO audio_files(id,file_path,filename,folder_id) VALUES(?1,?2,'one.wav',?1)",rusqlite::params![id,format!("{path}/one.wav")]).unwrap();
        }
        conn.execute("INSERT INTO audio_files(id,file_path,filename) VALUES(3,'unassigned.wav','unassigned.wav')",[]).unwrap();
    }
    let mut folder = task(&old, 1).definition;
    assert!(!resolve_album_filter(&next, &mut folder).unwrap());
    assert_eq!(folder.filters.album_id, next.get_audio(2).unwrap().album_id);
    assert_eq!(
        folder
            .album_binding
            .as_ref()
            .unwrap()
            .folder_path
            .as_deref(),
        Some("/first/Wind")
    );
    let mut unassigned = task(&old, 3).definition;
    assert!(!resolve_album_filter(&next, &mut unassigned).unwrap());
    assert_eq!(
        unassigned.filters.album_id,
        next.get_audio(3).unwrap().album_id
    );
    assert!(
        unassigned
            .album_binding
            .as_ref()
            .unwrap()
            .folder_path
            .is_none()
    );
}

#[test]
fn relocated_roots_keep_open_and_saved_album_filters_on_the_same_live_entity() {
    let root = tempfile::tempdir().unwrap();
    let old = root.path().join("old-root");
    let source = old.join("Pack/nested/one.wav");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let mut wav = b"RIFF".to_vec();
    wav.extend(44u32.to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(48_000u32.to_le_bytes());
    wav.extend(96_000u32.to_le_bytes());
    wav.extend(2u16.to_le_bytes());
    wav.extend(16u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend(8u32.to_le_bytes());
    wav.extend([0u8; 8]);
    std::fs::write(&source, &wav).unwrap();
    let provider = provider(&root.path().join("relink.db"), &[]);
    audiolib_db::ScanService::new(provider.clone())
        .scan(
            &old,
            &audiolib_db::ScanOptions::default(),
            &audiolib_db::ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let file = provider
        .search(&SearchRequest::default())
        .unwrap()
        .results
        .remove(0);
    let original = task(&provider, file.id);
    let mut workspace = SearchWorkspace::for_scope("owned-relink");
    workspace.session.tasks.push(original.clone());
    workspace.favorites.push(SavedSearchTask {
        id: "saved".into(),
        title: "Pack".into(),
        definition: original.definition.clone(),
        ..Default::default()
    });
    let namespace = provider.album_namespace();
    let moved = root.path().join("moved-root");
    std::fs::rename(&old, &moved).unwrap();
    assert_eq!(provider.relink_folder(&old, &moved).unwrap(), 1);
    reconcile_workspace_albums(&provider, &mut workspace).unwrap();
    let current = provider.get_audio(file.id).unwrap();
    assert_eq!(provider.album_namespace(), namespace);
    assert_eq!(current.album_id, file.album_id);
    for definition in [
        &workspace.session.tasks[0].definition,
        &workspace.favorites[0].definition,
    ] {
        assert!(!album_filter_unavailable(definition));
        assert_eq!(definition.filters.album_id, current.album_id);
        assert_eq!(
            definition.album_binding.as_ref().unwrap().folder_path,
            current.album_entity.as_ref().unwrap().folder_path
        );
        assert_ne!(
            definition.album_binding.as_ref().unwrap().folder_path,
            original
                .definition
                .album_binding
                .as_ref()
                .unwrap()
                .folder_path
        );
        assert_eq!(
            provider
                .search(&definition.to_request(20, 0))
                .unwrap()
                .results[0]
                .id,
            file.id
        );
    }
    assert_eq!(
        std::fs::read(moved.join("Pack/nested/one.wav")).unwrap(),
        wav
    );
    // Retirement remains unavailable even though live root relocation is admitted.
    provider
        .remove_folder(current.album_entity.unwrap().folder_id.unwrap())
        .unwrap();
    assert!(resolve_album_filter(&provider, &mut workspace.session.tasks[0].definition).unwrap());
}

#[test]
fn unresolved_album_and_restore_gates_never_dispatch_an_unfiltered_or_colliding_search() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "controller::search_tasks::album_bindings::tests::child_unavailable_album_gate",
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn isolated_fresh_album(child: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("controller::search_tasks::album_bindings::tests::{child}"),
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn fresh_workspace_resolves_known_album_binding_before_resuming_search() {
    isolated_fresh_album("child_fresh_known_album");
}

#[test]
fn fresh_workspace_blocks_unbound_album_id_when_first_query_is_deferred() {
    isolated_fresh_album("child_fresh_unbound_album");
}

#[test]
fn fresh_workspace_checks_album_identity_after_a_short_query_cancels_deferred_search() {
    isolated_fresh_album("child_fresh_unbound_album_short_query");
}

#[test]
#[ignore = "isolated fresh workspace deferred album dispatch regression"]
fn child_fresh_known_album() {
    fresh_album_dispatch(true, false);
}

#[test]
#[ignore = "isolated fresh workspace deferred album dispatch regression"]
fn child_fresh_unbound_album() {
    fresh_album_dispatch(false, false);
}

#[test]
#[ignore = "isolated fresh workspace cancelled deferred search identity regression"]
fn child_fresh_unbound_album_short_query() {
    fresh_album_dispatch(false, true);
}

fn fresh_album_dispatch(known: bool, cancel_with_short_query: bool) {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let root = tempfile::tempdir().unwrap();
    let next = provider(&root.path().join("next.db"), &["C"]);
    let preferences = root.path().join("preferences");
    std::fs::create_dir_all(&preferences).unwrap();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    std::fs::write(preferences.join("settings.json"),serde_json::to_vec(&serde_json::json!({"active_database":next.database_path(),"language":"en","playback":playback})).unwrap()).unwrap();
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(
        &ui,
        next.clone(),
        engine.handle(),
        AppConfig::load_in(preferences).unwrap(),
        playback,
    );
    let album = next.get_audio(1).unwrap().album_entity.unwrap();
    controller.remember_task_album_bindings(
        if known {
            next.album_namespace()
        } else {
            "old-namespace".into()
        },
        [album],
    );
    controller.request.lock().unwrap().filters.album_id = Some(1);
    ui.set_album_filter_id("1".into());
    ui.set_album_filter("Pending album".into());
    if cancel_with_short_query {
        ui.set_query("valid".into());
    }
    controller.request_search(&ui, true, false);
    assert!(
        controller.search_job.lock().unwrap().is_none(),
        "loading must not dispatch an unchecked album ID"
    );
    if cancel_with_short_query {
        assert!(
            controller
                .task_workspace
                .lock()
                .unwrap()
                .deferred_search
                .is_some()
        );
        ui.set_query("x".into());
        controller.request_search(&ui, true, false);
        assert!(
            controller
                .task_workspace
                .lock()
                .unwrap()
                .deferred_search
                .is_none()
        );
        assert!(ui.get_status().contains("at least 2"));
        assert!(controller.search_job.lock().unwrap().is_none());
    }
    let observed = controller.clone();
    let weak = ui.as_weak();
    let began = Instant::now();
    let completed = std::rc::Rc::new(std::cell::Cell::new(false));
    let finished = completed.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(10),
        move || {
            // The testing backend may tick again before consuming Quit. The
            // completed branch deliberately changes the query, so run it once.
            if finished.get() {
                return;
            }
            assert!(
                began.elapsed() < Duration::from_secs(12),
                "fresh album request was not resumed"
            );
            let Some(ui) = weak.upgrade() else { return };
            let ready = !ui.get_search_workspace_loading()
                && !ui.get_task_restoring()
                && !ui.get_searching();
            if ready && cancel_with_short_query {
                assert!(ui.get_album_filter_unavailable());
                assert_eq!(ui.get_query().as_str(), "x");
                assert!(
                    observed
                        .task_workspace
                        .lock()
                        .unwrap()
                        .unavailable_album
                        .is_some()
                );
                finished.set(true);
                ui.set_query("valid".into());
                observed.request_search(&ui, true, false);
                assert_eq!(observed.request.lock().unwrap().filters.album_id, Some(1));
                assert!(ui.get_album_filter_unavailable());
                assert!(observed.search_job.lock().unwrap().is_none());
                assert!(observed.search_publication.lock().unwrap().is_none());
                assert_eq!(ui.get_rows().row_count(), 0);
                slint::quit_event_loop().unwrap();
                return;
            }
            if ready
                && if known {
                    ui.get_rows().row_count() == 1
                } else {
                    ui.get_album_filter_unavailable()
                }
            {
                assert_eq!(observed.request.lock().unwrap().filters.album_id, Some(1));
                finished.set(true);
                slint::quit_event_loop().unwrap();
            }
        },
    );
    ui.show().unwrap();
    slint::run_event_loop().unwrap();
    timer.stop();
    assert!(completed.get(), "fresh album dispatch did not complete");
    assert_eq!(ui.get_rows().row_count(), usize::from(known));
    assert_eq!(ui.get_album_filter_unavailable(), !known);
    if known {
        assert_eq!(controller.records.lock().unwrap()[0].album, "C");
    } else {
        assert!(controller.search_publication.lock().unwrap().is_none());
    }
    controller.shutdown();
    ui.hide().unwrap();
    drop(engine);
}

#[test]
#[ignore = "isolated unavailable album search gate regression"]
fn child_unavailable_album_gate() {
    i_slint_backend_testing::init_no_event_loop();
    let root = tempfile::tempdir().unwrap();
    let old = provider(&root.path().join("old.db"), &["A"]);
    let next = provider(&root.path().join("next.db"), &["C"]);
    let preferences = root.path().join("preferences");
    std::fs::create_dir_all(&preferences).unwrap();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    std::fs::write(preferences.join("settings.json"),serde_json::to_vec(&serde_json::json!({"active_database":next.database_path(),"language":"en","playback":playback})).unwrap()).unwrap();
    let config = AppConfig::load_in(preferences).unwrap();
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, next.clone(), engine.handle(), config, playback);
    let original = task(&old, 1);
    {
        let mut state = controller.task_workspace.lock().unwrap();
        state.loading = false;
        state.workspace = SearchWorkspace::for_scope("owned-test");
        state.target = Some(SearchWorkspaceTarget::new(
            root.path().join("personal"),
            "owned-test",
        ));
        state.workspace.session.tasks.push(original.clone());
        state.workspace.session.active_task_id = Some(original.id.clone());
    }
    let (_, _, epoch) = controller
        .prepare_search_task_restore(&ui, &original.id)
        .unwrap();
    controller.request_navigation_search(&ui, true, false);
    assert!(
        controller.search_job.lock().unwrap().is_none(),
        "restoring must not dispatch the old numeric ID"
    );
    let resolved = resolve_task(&next, original.clone(), None).unwrap();
    controller.finish_search_task_restore(&ui, &original.id, epoch, Ok(resolved));
    controller.request_navigation_search(&ui, true, false);
    assert!(controller.search_job.lock().unwrap().is_none());
    assert_eq!(controller.request.lock().unwrap().filters.album_id, Some(1));
    assert!(ui.get_status().contains("unavailable"));
    assert!(ui.get_album_filter().contains('A'));
    assert!(ui.get_album_filter_unavailable());
    assert_eq!(ui.get_rows().row_count(), 0);
    let retained = controller
        .task_workspace
        .lock()
        .unwrap()
        .workspace
        .session
        .tasks[0]
        .definition
        .clone();
    assert!(album_filter_unavailable(&retained));
    assert_eq!(
        retained.album_binding.unwrap().metadata_key.as_deref(),
        Some("A")
    );
    // Facet publication owns C's identity. Its reused numeric ID must neither
    // highlight it as A nor make the first explicit click toggle to no filter.
    let available = next.get_audio(1).unwrap().album_entity.unwrap();
    controller.remember_task_album_bindings(next.album_namespace(), [available.clone()]);
    ui.set_albums(ModelRc::new(VecModel::from(vec![crate::AlbumRow {
        id: available.id.to_string().into(),
        name: available.name.into(),
        count: "1".into(),
        ..Default::default()
    }])));
    ui.invoke_album_selected("1".into());
    assert!(!ui.get_album_filter_unavailable());
    assert_eq!(ui.get_album_filter().as_str(), "C");
    let request = controller.request.lock().unwrap().clone();
    assert_eq!(request.filters.album_id, Some(1));
    assert_eq!(next.search(&request).unwrap().results[0].album, "C");
    controller.persist_search_task(&ui);
    controller.wait_workspace_writes().unwrap();
    let target = controller
        .task_workspace
        .lock()
        .unwrap()
        .target
        .clone()
        .unwrap();
    let mut restarted = next.load_search_workspace(&target).unwrap();
    reconcile_workspace_albums(&next, &mut restarted).unwrap();
    let saved = &restarted.session.tasks[0].definition;
    assert!(!album_filter_unavailable(saved));
    assert_eq!(saved.filters.album_id, Some(1));
    assert_eq!(
        saved
            .album_binding
            .as_ref()
            .unwrap()
            .metadata_key
            .as_deref(),
        Some("C")
    );
    assert_eq!(
        next.search(&saved.to_request(20, 0)).unwrap().results[0].album,
        "C"
    );
    controller.shutdown();
    drop(engine);
}
