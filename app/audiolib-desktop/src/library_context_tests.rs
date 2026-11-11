// app/audiolib-desktop/src/library_context_tests.rs
//! Regression coverage for same-library context, annotation refresh and removal disclosure.
use super::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[test]
fn metadata_refresh_includes_full_text_and_annotation_dependencies() {
    assert!(!metadata_affects_search(&SearchRequest::default()));
    for request in [
        SearchRequest {
            query: "door".into(),
            ..Default::default()
        },
        SearchRequest {
            order_by: SortOrder::Tags,
            ..Default::default()
        },
        SearchRequest {
            order_by: SortOrder::Modified,
            ..Default::default()
        },
        SearchRequest {
            filters: SearchFilters {
                favorites_only: true,
                ..Default::default()
            },
            ..Default::default()
        },
        SearchRequest {
            filters: SearchFilters {
                tagged_only: true,
                ..Default::default()
            },
            ..Default::default()
        },
        SearchRequest {
            filters: SearchFilters {
                tag_contains: Some("metal".into()),
                ..Default::default()
            },
            ..Default::default()
        },
        SearchRequest {
            filters: SearchFilters {
                color: Some("#ffffff".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    ] {
        assert!(metadata_affects_search(&request));
    }
}

#[test]
fn playback_stops_only_for_the_affected_record_or_library() {
    let file = AudioFile::from(audiolib_core::AudioFileData {
        id: 7,
        folder_id: Some(3),
        ..Default::default()
    });
    let folders = HashSet::from([3]);
    assert!(
        !LibraryOperation::Batch(BatchMetadataRequest {
            ids: vec![7],
            patch: MetadataPatch::default(),
            tag_mode: TagEditMode::Replace,
        })
        .affects_playback(Some(&file), &folders)
    );
    assert!(
        !LibraryOperation::Import("annotations.csv".into()).affects_playback(Some(&file), &folders)
    );
    assert!(!LibraryOperation::RemoveAudio(vec![8]).affects_playback(Some(&file), &folders));
    assert!(LibraryOperation::RemoveAudio(vec![7]).affects_playback(Some(&file), &folders));
    assert!(LibraryOperation::Relink(7, "new.wav".into()).affects_playback(Some(&file), &folders));
    assert!(!LibraryOperation::RemoveFolder(1).affects_playback(Some(&file), &HashSet::from([1])));
    assert!(LibraryOperation::RemoveFolder(1).affects_playback(Some(&file), &folders));
    assert!(LibraryOperation::Restore("snapshot.db".into()).affects_playback(None, &folders));
}

#[test]
fn removal_confirmation_discloses_names_and_hidden_records() {
    let files = vec![AudioFile::from(audiolib_core::AudioFileData {
        id: 2,
        filename: "visible.wav".into(),
        ..Default::default()
    })];
    for chinese in [false, true] {
        let text = removal_selection_preview(&[2, 7], &files, chinese);
        assert!(text.contains("visible.wav") && text.contains("#7"));
        assert!(text.contains(if chinese {
            "1 条不在当前列表中"
        } else {
            "1 selected record is outside"
        }));
    }
    assert!(
        LibraryOperation::RemoveAudio(vec![2])
            .confirmation(false)
            .contains("1 record and")
    );
}

#[test]
fn removal_folder_scope_includes_deep_descendants_and_reports_absent_roots() {
    let root = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&root.path().join("library.db")).unwrap();
    let sounds = root.path().join("sounds");
    let mut deepest = sounds.clone();
    for _ in 0..80 {
        deepest.push("a");
    }
    provider.ensure_scan_folder(&deepest, &sounds).unwrap();
    let top = provider.folders(None).unwrap()[0].id;
    let subtree = required_folder_subtree(&provider, top).unwrap();
    assert_eq!(subtree.len(), 81);
    assert!(required_folder_subtree(&provider, -99).is_err());
}

#[test]
fn same_library_mutations_and_saves_preserve_browsing_context() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "controller::library_context_tests::child_library_context",
            "--ignored",
            "--test-threads=1",
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
#[ignore = "Slint event loop requires an isolated process"]
fn child_library_context() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let owned = tempfile::tempdir().unwrap();
    let sounds = owned.path().join("sounds");
    std::fs::create_dir(&sounds).unwrap();
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
    for index in 0..205 {
        std::fs::write(sounds.join(format!("sound{index:03}.wav")), &wav).unwrap();
    }
    let provider = Arc::new(SqliteProvider::open(&owned.path().join("library.db")).unwrap());
    ScanService::new(provider.clone())
        .scan(
            &sounds,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let selected = provider
        .search(&SearchRequest {
            order_by: SortOrder::Filename,
            offset: PAGE_SIZE,
            ..Default::default()
        })
        .unwrap()
        .results[0]
        .clone();
    let other = provider
        .search(&SearchRequest {
            order_by: SortOrder::Filename,
            ..Default::default()
        })
        .unwrap()
        .results[0]
        .id;
    let folder = selected.folder_id.unwrap();
    let engine = audiolib_audio::AudioEngine::new(PlaybackConfig::default()).unwrap();
    engine
        .handle()
        .send_and_wait(
            AudioCommand::Load(selected.file_path.clone().into()),
            Duration::from_secs(4),
        )
        .unwrap();
    let audio_revision = engine.handle().status().transport_revision;
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(
        &ui,
        provider.clone(),
        engine.handle(),
        AppConfig::load_in(owned.path().join("preferences")).unwrap(),
        PlaybackConfig::default(),
    );
    ui.set_sort_index(1);
    *controller.request.lock().unwrap() = SearchRequest {
        folder_id: Some(folder),
        offset: PAGE_SIZE,
        order_by: SortOrder::Filename,
        ..Default::default()
    };
    {
        let mut tree = controller.tree.lock().unwrap();
        tree.selected = Some(folder);
        tree.expanded.insert(folder);
    }
    *controller.selected.lock().unwrap() = Some(selected.clone());
    present_file(&ui, &selected);
    controller
        .multi_selection
        .lock()
        .unwrap()
        .extend([selected.id, other]);
    controller.run_library_operation(LibraryOperation::Batch(BatchMetadataRequest {
        ids: vec![selected.id],
        patch: MetadataPatch {
            tags: Some("bulk".into()),
            ..Default::default()
        },
        tag_mode: TagEditMode::Replace,
    }));
    let stage = Rc::new(Cell::new(0));
    let observed = stage.clone();
    let failure = Rc::new(RefCell::new(None::<String>));
    let errors = failure.clone();
    let generations = Rc::new(Cell::new((0, 0)));
    let c = controller.clone();
    let p = provider.clone();
    let selected_id = selected.id;
    let backup_path = owned.path().join("backup.db");
    let window = ui.as_weak();
    let started = Instant::now();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(15), move || {
        let Some(ui) = window.upgrade() else { return };
        let result = (|| -> Result<(), String> {
            if started.elapsed() > Duration::from_secs(18) { return Err("Context flow timed out".into()); }
            if c.library_busy.load(Ordering::Acquire) || ui.get_searching() { return Ok(()); }
            if !matches!(observed.get(), 4) {
                let tree = c.tree.lock().unwrap();
                if tree.selected != Some(folder) || !tree.expanded.contains(&folder) || c.request.lock().unwrap().offset != PAGE_SIZE {
                    return Err("Same-library operation lost folder/expansion/page".into());
                }
            }
            match observed.get() {
                0 if ui.get_edit_tags() == "bulk" && ui.get_rows().row_count() > 0 => {
                    if ui.get_selected_id() != selected_id.to_string() || c.audio.status().transport_revision != audio_revision {
                        return Err("Batch lost selection or stopped the loaded audition".into());
                    }
                    generations.set((c.search_generation.load(Ordering::Acquire), c.facet_revision.load(Ordering::Acquire)));
                    ui.set_edit_notes("in-place save".into()); ui.set_metadata_dirty(true); c.save_metadata(&ui); observed.set(1);
                }
                1 if !ui.get_metadata_dirty() && ui.get_edit_notes() == "in-place save" => {
                    if generations.get() != (c.search_generation.load(Ordering::Acquire), c.facet_revision.load(Ordering::Acquire)) {
                        return Err("Unfiltered Metadata save repeated the search/facets".into());
                    }
                    if p.get_audio(selected_id).unwrap().notes != "in-place save" { return Err("Metadata write was not durable".into()); }
                    c.metadata.lock().unwrap().failures.insert(-99, "historical failure".into());
                    c.run_library_operation(LibraryOperation::Backup(backup_path.clone())); observed.set(2);
                }
                2 if backup_path.exists() && !ui.get_searching() => {
                    if !ui.get_library_feedback().contains("unsaved annotations") && !ui.get_library_feedback().contains("部分标注") {
                        return Err("Read-only operation hid its unsaved annotation warning".into());
                    }
                    c.metadata.lock().unwrap().failures.clear();
                    c.run_library_operation(LibraryOperation::RemoveAudio(vec![other])); observed.set(3);
                }
                3 => {
                    if ui.get_selected_id() != selected_id.to_string() || c.multi_selection.lock().unwrap().contains(&other)
                        || !c.multi_selection.lock().unwrap().contains(&selected_id) || c.audio.status().transport_revision != audio_revision {
                        return Err("Unrelated removal disturbed selection/playback or retained removed IDs".into());
                    }
                    c.run_library_operation(LibraryOperation::RemoveAudio(vec![selected_id])); observed.set(4);
                }
                4 => {
                    if ui.get_has_selection() || !ui.get_selected_id().is_empty() || !c.multi_selection.lock().unwrap().is_empty() {
                        return Err("Removed inspector record remained selected".into());
                    }
                    if ui.get_audio_ready() || c.audio.status().generation != c.removed_audio_generation.load(Ordering::Acquire) {
                        return Err("Removed loaded audio can still be replayed through the UI".into());
                    }
                    let before = c.audio.status().transport_revision;
                    ui.invoke_play_requested();
                    if c.audio.status().transport_revision != before {
                        return Err("Play replayed the removed loaded generation".into());
                    }
                    observed.set(5); slint::quit_event_loop().unwrap();
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = result { *errors.borrow_mut() = Some(error); slint::quit_event_loop().ok(); }
    });
    ui.show().unwrap();
    slint::run_event_loop().unwrap();
    timer.stop();
    controller.shutdown();
    ui.hide().unwrap();
    assert!(failure.borrow().is_none(), "{:?}", failure.borrow());
    assert_eq!(stage.get(), 5);
    drop(controller);
    drop(engine);
    drop(provider);
}
