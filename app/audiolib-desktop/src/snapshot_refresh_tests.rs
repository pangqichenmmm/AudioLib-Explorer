// app/audiolib-desktop/src/snapshot_refresh_tests.rs
//! Isolated regressions for automatic server refresh during audition and search retention.
use super::super::*;
use super::source_transition_tests::{
    configuration, isolated_module, request, respond, root, run_until,
};
use super::*;
use audiolib_core::config::{ActiveSource, ServerProfile};
use std::io::Write;
use std::sync::atomic::AtomicUsize;

fn fixture(
    count: usize,
) -> (
    Arc<Controller>,
    AppWindow,
    audiolib_audio::AudioEngine,
    DataSourceSwitch,
    PathBuf,
) {
    let root = root();
    let sounds = root.join("sounds");
    std::fs::create_dir_all(&sounds).unwrap();
    let mut bytes = Vec::new();
    bytes.extend(b"RIFF");
    bytes.extend(996u32.to_le_bytes());
    bytes.extend(b"WAVEfmt ");
    bytes.extend(16u32.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(1u16.to_le_bytes());
    bytes.extend(48_000u32.to_le_bytes());
    bytes.extend(96_000u32.to_le_bytes());
    bytes.extend(2u16.to_le_bytes());
    bytes.extend(16u16.to_le_bytes());
    bytes.extend(b"data");
    bytes.extend(960u32.to_le_bytes());
    bytes.resize(1004, 0);
    for index in 0..count {
        std::fs::write(sounds.join(format!("metal-{index:04}.wav")), &bytes).unwrap();
    }
    std::fs::write(sounds.join("wind.wav"), &bytes).unwrap();
    let source = Arc::new(SqliteProvider::open(&root.join("source.db")).unwrap());
    ScanService::new(source.clone())
        .scan(
            &sounds,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let ids = source
        .search(&SearchRequest {
            limit: 500,
            ..Default::default()
        })
        .unwrap()
        .results
        .into_iter()
        .map(|file| file.id)
        .collect();
    source
        .update_metadata_batch(&BatchMetadataRequest {
            ids,
            patch: MetadataPatch {
                tags: Some("reviewed".into()),
                favorite: Some(true),
                ..Default::default()
            },
            tag_mode: TagEditMode::Replace,
        })
        .unwrap();
    let mut config = configuration(&root.join("preferences"));
    config.data_source.mode = audiolib_core::config::DataSourceMode::Server;
    config.data_source.server_url = "http://127.0.0.1:9".into();
    config.data_source.poll_interval_secs = 0;
    let cache = RemoteCache::new(&config.data_root());
    let old = CacheState {
        library_id: "audition-library".into(),
        server_url: config.data_source.server_url.clone(),
        revision: 1,
        database_file: "snapshot-r1.db".into(),
        ..Default::default()
    };
    let next = CacheState {
        revision: 2,
        database_file: "snapshot-r2.db".into(),
        ..old.clone()
    };
    let database = cache.database_path(&old);
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    source.backup_library(&database).unwrap();
    source.backup_library(&cache.database_path(&next)).unwrap();
    cache.save_state(&old).unwrap();
    let provider = Arc::new(SqliteProvider::open_read_only(&database).unwrap());
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, provider, engine.handle(), config, playback);
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(old);
    (
        controller,
        ui,
        engine,
        DataSourceSwitch::Server {
            database: cache.database_path(&next),
            state: Some(Box::new(next)),
        },
        sounds,
    )
}

fn await_results(controller: &Arc<Controller>, ui: &AppWindow, rows: usize) {
    let c = controller.clone();
    let window = ui.as_weak();
    run_until(ui, move || {
        window.upgrade().is_some_and(|ui| {
            !c.library_busy.load(Ordering::Acquire)
                && !ui.get_searching()
                && ui.get_rows().row_count() == rows
                && c.search_publication
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|publication| {
                        publication.generation == c.search_generation.load(Ordering::Acquire)
                            && publication.model_published
                    })
        })
    });
}

#[test]
fn a_same_library_refresh_retains_the_filtered_search_folder_and_page() {
    isolated_module("snapshot_refresh_tests", "child_search_retention");
}

#[test]
fn same_library_refresh_keeps_an_armed_result_scroll() {
    isolated_module(
        "snapshot_refresh_tests",
        "child_server_library_switch_resets_result_scroll",
    );
}

#[test]
#[ignore = "isolated Slint result-scroll retention across server libraries"]
fn child_server_library_switch_resets_result_scroll() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let (controller, ui, _engine, same, _) = fixture(1);
    ui.show().unwrap();
    ui.set_remote_mode(true);
    ui.invoke_restore_search_task_viewport(-640.0, -12.0, 0.0, 0.0, 0.0);
    assert!(
        ui.get_task_result_scroll_y() < -600.0,
        "armed offset {}",
        ui.get_task_result_scroll_y()
    );
    assert!(controller.is_same_library_snapshot_refresh(&same));
    controller.reset_library_view(&ui, false, true);
    assert!(
        ui.get_task_restoring(),
        "same-library refresh discarded the armed result restore"
    );
    assert!(
        ui.get_task_result_scroll_y() < -600.0,
        "same-library refresh reset the armed result scroll to {}",
        ui.get_task_result_scroll_y()
    );
    assert!(
        ui.get_task_result_scroll_x() < -10.0,
        "same-library refresh reset the armed horizontal scroll to {}",
        ui.get_task_result_scroll_x()
    );

    let DataSourceSwitch::Server { database, state } = &same else {
        unreachable!("fixture opens a server snapshot")
    };
    let mut other = state.clone().expect("snapshot state");
    other.library_id = "library-b".into();
    let other_library = DataSourceSwitch::Server {
        database: database.clone(),
        state: Some(other),
    };
    assert!(!controller.is_same_library_snapshot_refresh(&other_library));
    controller.reset_library_view(&ui, true, false);
    assert!(!ui.get_task_restoring());
    let cleared = ui.get_task_result_scroll_y();
    let cleared_x = ui.get_task_result_scroll_x();
    assert!(
        cleared.abs() < 1.0 && cleared_x.abs() < 1.0,
        "server library A→B must clear the result list, y={cleared} x={cleared_x}"
    );
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
#[ignore = "isolated Slint snapshot search retention regression"]
fn child_search_retention() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let (controller, ui, _engine, target, _) = fixture(405);
    let folder = controller.provider().folders(None).unwrap().remove(0);
    ui.set_query("metal".into());
    ui.set_sample_filter(2);
    ui.set_bit_filter(1);
    ui.set_channel_filter(1);
    ui.set_favorites_only(true);
    ui.set_tagged_only(true);
    ui.set_tag_filter("reviewed".into());
    ui.set_duration_min("0".into());
    ui.set_duration_max("1".into());
    ui.set_sort_index(1);
    ui.set_sort_descending(true);
    {
        let mut tree = controller.tree.lock().unwrap();
        tree.selected = Some(folder.id);
        tree.expanded.insert(folder.id);
        tree.known = Arc::new(HashMap::from([(folder.id, folder.clone())]));
    }
    {
        let mut request = controller.request.lock().unwrap();
        request.folder_id = Some(folder.id);
        request.offset = PAGE_SIZE;
    }
    controller.request_search(&ui, true, false);
    await_results(&controller, &ui, PAGE_SIZE);
    let before = controller.request.lock().unwrap().clone();
    let old_provider = controller.provider();
    // An already queued search cannot republish old snapshot rows after the swap.
    controller.request_search(&ui, false, false);
    let old_cancel = controller.search_cancel.lock().unwrap().clone();
    controller.run_library_operation(LibraryOperation::DataSource(target.clone()));
    await_results(&controller, &ui, PAGE_SIZE);
    assert!(old_cancel.load(Ordering::Acquire));
    assert!(!Arc::ptr_eq(&old_provider, &controller.provider()));
    assert_eq!(ui.get_query(), "metal");
    assert_eq!(ui.get_sort_index(), 1);
    assert!(ui.get_sort_descending());
    assert_eq!(ui.get_sample_filter(), 2);
    assert!(ui.get_favorites_only() && ui.get_tagged_only());
    assert_eq!(ui.get_tag_filter(), "reviewed");
    assert_eq!(ui.get_page_label(), "2/3");
    let after = controller.request.lock().unwrap().clone();
    assert_eq!(after.query, before.query);
    assert_eq!(after.filters, before.filters);
    assert_eq!(after.folder_id, before.folder_id);
    assert_eq!(after.offset, before.offset);
    assert_eq!(after.order_by, before.order_by);
    assert_eq!(after.descending, before.descending);
    assert_eq!(controller.tree.lock().unwrap().selected, Some(folder.id));
    assert!(
        controller
            .tree
            .lock()
            .unwrap()
            .expanded
            .contains(&folder.id)
    );
    assert_eq!(ui.get_selected_folder_path(), folder.path);
    assert!(
        (0..ui.get_rows().row_count())
            .all(|row| { ui.get_rows().row_data(row).unwrap().name.contains("metal") })
    );
    let DataSourceSwitch::Server { database, state } = target else {
        unreachable!()
    };
    let mut other_endpoint = state.clone().unwrap();
    other_endpoint.server_url = "http://127.0.0.1:10".into();
    assert!(
        !controller.is_same_library_snapshot_refresh(&DataSourceSwitch::Server {
            database: database.clone(),
            state: Some(other_endpoint),
        })
    );
    // A genuine library replacement still invalidates the old folder IDs and query.
    let mut other_library = state.unwrap();
    other_library.library_id = "replacement-library".into();
    let cache = RemoteCache::new(&controller.data_root());
    let replacement_database = cache.database_path(&other_library);
    std::fs::create_dir_all(replacement_database.parent().unwrap()).unwrap();
    controller
        .provider()
        .backup_library(&replacement_database)
        .unwrap();
    controller.run_library_operation(LibraryOperation::DataSource(DataSourceSwitch::Server {
        database: replacement_database,
        state: Some(other_library),
    }));
    await_results(&controller, &ui, PAGE_SIZE);
    assert_eq!(ui.get_query(), "");
    assert_eq!(controller.request.lock().unwrap().folder_id, None);
    assert_eq!(controller.request.lock().unwrap().offset, 0);
    // Sorting is now a global table preference, independent of library query and IDs.
    assert_eq!(ui.get_sort_index(), 1);
    assert!(ui.get_sort_descending());
    assert_eq!(
        controller.request.lock().unwrap().order_by,
        SortOrder::Filename
    );
    assert!(controller.request.lock().unwrap().descending);
    assert!(!ui.get_favorites_only());
    controller.shutdown();
    ui.hide().unwrap();
}

#[test]
fn automatic_resync_waits_for_loading_audio_and_coalesces_duplicate_requests() {
    isolated_module("snapshot_refresh_tests", "child_loading_refresh");
}

#[test]
#[ignore = "isolated Slint in-flight audition refresh regression"]
fn child_loading_refresh() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let (controller, ui, _engine, _, sounds) = fixture(1);
    ui.set_query("metal".into());
    controller.request_search(&ui, true, true);
    await_results(&controller, &ui, 1);
    let old_provider = controller.provider();
    let bytes = Arc::new(std::fs::read(sounds.join("metal-0000.wav")).unwrap());
    let virtual_path = sounds.join("loading.wav");
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let release_rx = Mutex::new(release_rx);
    let first = AtomicBool::new(true);
    let opener: audiolib_audio::SourceOpener = Arc::new(move || {
        if first.swap(false, Ordering::AcqRel) {
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(4))
                .unwrap();
        }
        let source: Box<dyn audiolib_audio::ByteSource> =
            Box::new(std::io::Cursor::new(bytes.as_ref().clone()));
        Ok((source, bytes.len() as u64))
    });
    audiolib_audio::register_stream_source(&virtual_path, opener);
    controller
        .audio
        .send(AudioCommand::Load(virtual_path.clone()))
        .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(4)).unwrap();
    assert_eq!(controller.audio.status().state, PlaybackState::Loading);
    controller.config.lock().unwrap().data_source.auto_sync = true;
    controller.apply_revision_check(
        &ui,
        Some(CachedIdentity {
            library_id: "audition-library".into(),
            revision: 1,
        }),
        RevisionCheck::NewRevision {
            cached: 1,
            server: 2,
        },
        None,
        true,
    );
    assert!(controller.remote.lock().unwrap().auto_deferred);
    assert!(!controller.remote_busy.load(Ordering::Acquire));
    controller.post_remote_resync("owned test revision mismatch");
    controller.post_remote_resync("owned duplicate revision mismatch");
    assert_eq!(controller.remote.lock().unwrap().resync_pending, Some(0));
    let c = controller.clone();
    let started = Instant::now();
    let release = Arc::new(AtomicBool::new(false));
    let released = release.clone();
    let window = ui.as_weak();
    run_until(&ui, move || {
        if started.elapsed() < Duration::from_millis(250) {
            assert!(Arc::ptr_eq(&old_provider, &c.provider()));
            assert!(!c.library_busy.load(Ordering::Acquire));
            assert_eq!(window.upgrade().unwrap().get_query(), "metal");
            assert_eq!(window.upgrade().unwrap().get_rows().row_count(), 1);
            return false;
        }
        if !released.swap(true, Ordering::AcqRel) {
            release_tx.send(()).unwrap();
        }
        c.remote_generation.load(Ordering::Acquire) == 1 && !c.remote_busy.load(Ordering::Acquire)
    });
    assert!(release.load(Ordering::Acquire));
    assert_eq!(ui.get_query(), "metal");
    assert_eq!(ui.get_rows().row_count(), 1);
    assert_eq!(controller.audio.status().state, PlaybackState::Ready);
    assert_eq!(controller.remote_generation.load(Ordering::Acquire), 1);
    assert!(controller.remote.lock().unwrap().resync_pending.is_none());
    controller.shutdown();
    audiolib_audio::unregister_stream_source(&virtual_path);
    ui.hide().unwrap();
}

#[test]
fn reordered_write_callbacks_keep_the_current_snapshot_revision() {
    isolated_module("snapshot_refresh_tests", "child_write_callback_order");
}

#[test]
#[ignore = "isolated Slint write callback ordering regression"]
fn child_write_callback_order() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let (controller, ui, _engine, _, _) = fixture(1);
    let mut state = controller.remote.lock().unwrap().state.clone().unwrap();
    // Both writes have already published their cache updates under WRITE_LOCK.
    // Their provider listeners run after that lock is released and can reorder.
    state.revision = 45;
    let cache = RemoteCache::new(&controller.data_root());
    cache.save_state(&state).unwrap();
    controller.remote.lock().unwrap().check = Some(RevisionCheck::NewRevision {
        cached: 1,
        server: 45,
    });
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let delayed = controller.clone();
    let library_id = state.library_id.clone();
    let database_file = state.database_file.clone();
    let first = std::thread::spawn(move || {
        entered_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(4)).unwrap();
        delayed.remote_write_event(
            &library_id,
            &database_file,
            WriteEvent::Applied { revision: 44 },
        );
    });
    entered_rx.recv_timeout(Duration::from_secs(4)).unwrap();
    controller.remote_write_event(
        &state.library_id,
        &state.database_file,
        WriteEvent::Applied { revision: 45 },
    );
    release_tx.send(()).unwrap();
    first.join().unwrap();
    {
        let remote = controller.remote.lock().unwrap();
        assert_eq!(remote.state.as_ref().unwrap().revision, 45);
        assert!(matches!(
            remote.check,
            Some(RevisionCheck::UpToDate { revision: 45 })
        ));
    }
    assert_eq!(cache.state(&state.library_id).unwrap().revision, 45);
    // A revision check made against the latest published cache remains current,
    // rather than being discarded because a delayed callback regressed runtime.
    controller.apply_revision_check(
        &ui,
        Some(CachedIdentity {
            library_id: state.library_id,
            revision: 45,
        }),
        RevisionCheck::UpToDate { revision: 45 },
        None,
        false,
    );
    assert_eq!(ui.get_remote_notice_kind(), NOTICE_NONE);
    assert_eq!(controller.remote_generation.load(Ordering::Acquire), 0);
    assert!(!controller.remote_busy.load(Ordering::Acquire));
    controller.shutdown();
    ui.hide().unwrap();
}

#[allow(dead_code)]
struct SnapshotServer {
    url: String,
    requests: Arc<AtomicUsize>,
    snapshots: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: std::thread::JoinHandle<()>,
}

#[allow(dead_code)]
impl SnapshotServer {
    fn start(library_id: &str, revision: u64, bytes: Vec<u8>) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let snapshots = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let seen = requests.clone();
        let downloaded = snapshots.clone();
        let stopping = stop.clone();
        let library_id = library_id.to_owned();
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let mut socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept snapshot mock: {error}"),
                };
                socket.set_nonblocking(false).unwrap();
                let request = request(&mut socket);
                seen.fetch_add(1, Ordering::AcqRel);
                let path = request.split_whitespace().nth(1).unwrap();
                match path {
                    "/api/v1/health" => {
                        respond(&mut socket, "200 OK", &serde_json::json!({"status":"ok","version":env!("CARGO_PKG_VERSION")}).to_string())
                    }
                    "/api/v1/capabilities" => respond(&mut socket, "200 OK", &serde_json::json!({"compatibility":audiolib_core::compatibility::compatibility_manifest()}).to_string()),
                    "/api/v1/library" => respond(
                        &mut socket,
                        "200 OK",
                        &serde_json::json!({
                            "library_id": library_id,
                            "identity_bound_writes": true,
                            "revision": revision,
                            "schema_version": audiolib_db::SCHEMA_VERSION,
                            "snapshot_format": {"id":audiolib_core::compatibility::PLAN_B_FORMAT,"format":audiolib_core::compatibility::LibraryFormat::target()},
                        })
                        .to_string(),
                    ),
                    "/api/v1/roots" => respond(&mut socket, "200 OK", "[]"),
                    "/api/v1/library/snapshot" => {
                        downloaded.fetch_add(1, Ordering::AcqRel);
                        let format = serde_json::to_string(&audiolib_core::compatibility::LibraryFormat::target()).unwrap();
                        let manifest = serde_json::to_string(&audiolib_core::compatibility::compatibility_manifest()).unwrap();
                        let format_id = audiolib_core::compatibility::PLAN_B_FORMAT;
                        let schema_version = audiolib_db::SCHEMA_VERSION;
                        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nX-Library-Id: {library_id}\r\nX-Library-Revision: {revision}\r\nX-Schema-Version: {schema_version}\r\nX-Audiolib-Snapshot-Format: {format_id}\r\nX-Audiolib-Library-Format: {format}\r\nX-Audiolib-Server-Compatibility: {manifest}\r\nX-Search-Storage-Revision: 1\r\nX-Tokenization-Revision: 3\r\nX-Source-Search-Revision: 2\r\nX-Source-Tags-Revision: 2\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
                        socket.write_all(&bytes).unwrap();
                    }
                    _ => respond(
                        &mut socket,
                        "404 Not Found",
                        r#"{"error_code":"NOT_FOUND","message":"unsupported test endpoint"}"#,
                    ),
                }
            }
        });
        Self {
            url,
            requests,
            snapshots,
            stop,
            worker,
        }
    }

    fn finish(self) {
        self.stop.store(true, Ordering::Release);
        self.worker.join().unwrap();
    }
}

#[test]
fn a_profile_switch_keeps_play_noops_off_the_network() {
    isolated_module("snapshot_refresh_tests", "child_play_profile_switch");
}

#[test]
#[ignore = "isolated Slint play/profile drain regression"]
fn child_play_profile_switch() {
    play_source_switch(PlayTransition::ProfileSwitch);
}

#[test]
fn editing_the_active_profile_endpoint_keeps_play_noops_off_the_network() {
    isolated_module("snapshot_refresh_tests", "child_play_profile_edit");
}

#[test]
#[ignore = "isolated Slint play/active endpoint edit regression"]
fn child_play_profile_edit() {
    play_source_switch(PlayTransition::ProfileEdit);
}

#[test]
fn closing_finishes_play_noops_without_cache_changes() {
    isolated_module("snapshot_refresh_tests", "child_play_close");
}

#[test]
#[ignore = "isolated Slint play/close regression"]
fn child_play_close() {
    play_source_switch(PlayTransition::Close);
}

#[test]
fn a_provider_swap_preserves_metadata_after_play_noops() {
    isolated_module("snapshot_refresh_tests", "child_play_provider_swap");
}

#[test]
#[ignore = "isolated Slint play/provider swap regression"]
fn child_play_provider_swap() {
    play_source_switch(PlayTransition::SnapshotSwap);
}

enum PlayTransition {
    ProfileSwitch,
    ProfileEdit,
    SnapshotSwap,
    Close,
}

fn play_source_switch(transition: PlayTransition) {
    let edit_active = matches!(transition, PlayTransition::ProfileEdit);
    let close = matches!(transition, PlayTransition::Close);
    let snapshot_swap = matches!(transition, PlayTransition::SnapshotSwap);
    i_slint_backend_testing::init_integration_test_with_system_time();
    let (controller, ui, _engine, mut target, _) = fixture(1);
    let file = controller
        .provider()
        .search(&SearchRequest {
            query: "metal".into(),
            ..Default::default()
        })
        .unwrap()
        .results
        .remove(0);
    let id = file.id;
    let mut state = controller.remote.lock().unwrap().state.clone().unwrap();
    if snapshot_swap {
        let DataSourceSwitch::Server { database, .. } = &target else {
            unreachable!()
        };
        let mut snapshot_file = file.clone();
        snapshot_file.notes = "New server snapshot".into();
        SqliteProvider::apply_snapshot_records(database, &[snapshot_file]).unwrap();
    }
    // B has the same UUID/revision but independently edited Metadata.
    let clone_path = root().join("server-b.db");
    controller.provider().backup_library(&clone_path).unwrap();
    let clone = SqliteProvider::open(&clone_path).unwrap();
    clone
        .update_metadata(
            id,
            &MetadataPatch {
                notes: Some("B snapshot Metadata".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let b_snapshot = root().join("server-b-snapshot.db");
    clone.backup_library(&b_snapshot).unwrap();
    drop(clone);
    let b = SnapshotServer::start(
        &state.library_id,
        state.revision,
        std::fs::read(b_snapshot).unwrap(),
    );
    let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url_a = format!("http://{}", a.local_addr().unwrap());
    a.set_nonblocking(true).unwrap();
    state.server_url = url_a.clone();
    let cache = RemoteCache::new(&controller.data_root());
    cache.save_state(&state).unwrap();
    controller.remote.lock().unwrap().state = Some(state.clone());
    {
        let mut config = controller.config.lock().unwrap();
        config.data_source.server_url = url_a.clone();
        config.data_source.active = Some("a".into());
        config.data_source.auto_sync = false;
        config.data_source.profiles = vec![ServerProfile {
            id: "a".into(),
            name: "A".into(),
            server_url: url_a.clone(),
            ..Default::default()
        }];
        if !edit_active {
            config.data_source.profiles.push(ServerProfile {
                id: "b".into(),
                name: "B".into(),
                server_url: b.url.clone(),
                ..Default::default()
            });
        }
    }
    controller
        .record_local_history(Path::new(&file.file_path))
        .unwrap();
    // Dispatch twice across the transition; neither may send a play request.
    controller
        .record_local_history(Path::new(&file.file_path))
        .unwrap();
    let local_history = audiolib_db::local_play_history::LocalPlayHistory::open(
        &controller.data_root(),
        &format!("server-{}", state.library_id),
        audiolib_core::config::PLAY_HISTORY_LIMIT_PER_LIBRARY,
    )
    .unwrap();
    assert_eq!(local_history.page(10, 0).unwrap().len(), 2);
    let history_path = local_history.database_path().to_path_buf();

    if snapshot_swap {
        let DataSourceSwitch::Server { state, .. } = &mut target else {
            unreachable!()
        };
        state.as_mut().unwrap().server_url = url_a.clone();
        controller.run_library_operation(LibraryOperation::DataSource(target));
    } else if close {
        controller.request_close(&ui);
    } else if edit_active {
        ui.invoke_sm_open_requested(0);
        ui.set_ds_server_url(b.url.clone().into());
        ui.set_ds_dirty(true);
        ui.invoke_sm_save_requested();
    } else {
        controller.activate_data_source(ActiveSource::Profile("b".into()));
    }
    assert!(if snapshot_swap {
        controller.library_busy.load(Ordering::Acquire)
    } else if close {
        controller.closing.load(Ordering::Acquire)
    } else {
        controller.source_changing.load(Ordering::Acquire)
    });
    let c = controller.clone();
    let url_b = b.url.clone();
    run_until(&ui, move || {
        if close {
            // Completion must hide the window and quit its own event loop.
            return false;
        }
        if snapshot_swap {
            return !c.library_busy.load(Ordering::Acquire)
                && c.remote_plays.lock().unwrap().workers == 0
                && c.remote
                    .lock()
                    .unwrap()
                    .state
                    .as_ref()
                    .is_some_and(|state| {
                        state.revision == 2 && state.database_file == "snapshot-r2.db"
                    });
        }
        !c.source_changing.load(Ordering::Acquire)
            && !c.library_busy.load(Ordering::Acquire)
            && !c.remote_busy.load(Ordering::Acquire)
            && c.remote_plays.lock().unwrap().workers == 0
            && c.remote
                .lock()
                .unwrap()
                .state
                .as_ref()
                .is_some_and(|state| {
                    state.revision == 1 && cached_source_matches(&state.server_url, &url_b)
                })
    });
    assert!(matches!(a.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
    assert!(history_path.is_file());
    assert_eq!(local_history.page(10, 0).unwrap().len(), 2);
    if close {
        assert!(!ui.window().is_visible());
        assert_eq!(controller.remote_plays.lock().unwrap().workers, 0);
        assert_eq!(b.requests.load(Ordering::Acquire), 0);
        let active = cache.active().unwrap();
        assert!(cached_source_matches(&active.server_url, &state.server_url));
        assert_eq!(active.revision, 1);
        assert!(
            serde_json::to_value(controller.provider().get_audio(id).unwrap())
                .unwrap()
                .get("play_count")
                .is_none()
        );
        controller.shutdown();
        b.finish();
        return;
    }
    if snapshot_swap {
        assert_eq!(b.requests.load(Ordering::Acquire), 0);
        let current = controller.provider().get_audio(id).unwrap();
        assert_eq!(current.notes, "New server snapshot");
        assert!(
            serde_json::to_value(&current)
                .unwrap()
                .get("play_count")
                .is_none()
        );
        let active = cache.active().unwrap();
        assert!(cached_source_matches(&active.server_url, &state.server_url));
        // The provider swap is separate from sync's cache promotion. Playback
        // must leave the previous cached revision and Metadata untouched.
        assert_eq!(active.database_file, state.database_file);
        assert_eq!(active.revision, 1);
        assert_eq!(
            SqliteProvider::open_read_only(&cache.database_path(&active))
                .unwrap()
                .get_audio(id)
                .unwrap()
                .notes,
            file.notes
        );
        controller.shutdown();
        ui.hide().unwrap();
        b.finish();
        return;
    }
    assert_eq!(b.snapshots.load(Ordering::Acquire), 1);
    let current = controller.provider().get_audio(id).unwrap();
    assert_eq!(current.notes, "B snapshot Metadata");
    assert!(
        serde_json::to_value(&current)
            .unwrap()
            .get("play_count")
            .is_none()
    );
    let active = cache.active().unwrap();
    assert!(cached_source_matches(&active.server_url, &b.url));
    assert_eq!(active.revision, 1);
    assert_eq!(
        SqliteProvider::open_read_only(&cache.database_path(&active))
            .unwrap()
            .get_audio(id)
            .unwrap()
            .notes,
        "B snapshot Metadata"
    );
    controller.shutdown();
    ui.hide().unwrap();
    b.finish();
}
