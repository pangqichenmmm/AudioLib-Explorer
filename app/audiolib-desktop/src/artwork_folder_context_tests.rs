// app/audiolib-desktop/src/artwork_folder_context_tests.rs
//! Folder cover contexts share albums without sharing pixels or explicit choices.
use super::*;

const FIRST_COLOR: [u8; 4] = [20, 80, 220, 255];
const SECOND_COLOR: [u8; 4] = [220, 40, 60, 255];
const CHOSEN_COLOR: [u8; 4] = [240, 200, 20, 255];
const INHERITED_COLOR: [u8; 4] = [40, 160, 80, 255];

struct FolderFiles {
    first: AudioFile,
    first_plain: AudioFile,
    second: AudioFile,
    second_image: AudioFile,
}

fn folder_files(fixture: &Fixture) -> FolderFiles {
    let sounds = fixture.root.join("sounds");
    let blank = std::fs::read(sounds.join("wind/wind.wav")).unwrap();
    let mut connection = rusqlite::Connection::open(fixture.provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&connection).unwrap();
    let transaction = connection.transaction().unwrap();
    let (parent, depth): (i64, i64) = transaction
        .query_row(
            "SELECT id,depth FROM folders ORDER BY depth,id LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let metadata = serde_json::json!({
        "wav_metadata": {"fields": {"library": "One shared album, separate cover folders"}}
    })
    .to_string();
    for (name, first_id, count) in [("cover-first", 100_i64, 70_i64), ("cover-later", 200, 2)] {
        let directory = sounds.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        let directory_path = audiolib_db::display_path(&directory.canonicalize().unwrap());
        transaction
            .execute(
                "INSERT INTO folders(folder_path,folder_name,parent_id,depth) VALUES(?1,?2,?3,?4)",
                rusqlite::params![directory_path, name, parent, depth + 1],
            )
            .unwrap();
        let folder_id = transaction.last_insert_rowid();
        transaction
            .execute(
                "INSERT INTO folder_closure(ancestor_id,descendant_id,depth) VALUES(?1,?1,0)",
                [folder_id],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO folder_closure(ancestor_id,descendant_id,depth)
                 SELECT ancestor_id,?1,depth+1 FROM folder_closure WHERE descendant_id=?2",
                rusqlite::params![folder_id, parent],
            )
            .unwrap();
        for offset in 0..count {
            let filename = format!("{name}-{offset:03}.wav");
            let path = directory.join(&filename);
            std::fs::write(&path, &blank).unwrap();
            if first_id + offset == 100 {
                embedded_cover_color(&path, FIRST_COLOR);
            } else if first_id + offset == 201 {
                embedded_cover_color(&path, SECOND_COLOR);
            }
            let path = audiolib_db::display_path(&path.canonicalize().unwrap());
            transaction
                .execute(
                    "INSERT INTO audio_files(id,file_path,path_key,filename,folder_id,metadata_json)
                     VALUES(?1,?2,?2,?3,?4,?5)",
                    rusqlite::params![first_id + offset, path, filename, folder_id, metadata],
                )
                .unwrap();
        }
    }
    transaction.commit().unwrap();
    drop(connection);
    fixture.controller.config.lock().unwrap().allowed_scan_roots = vec![sounds];
    fixture.controller.config.lock().unwrap().save().unwrap();
    let files = FolderFiles {
        first: fixture.provider.get_audio(100).unwrap(),
        first_plain: fixture.provider.get_audio(101).unwrap(),
        second: fixture.provider.get_audio(200).unwrap(),
        second_image: fixture.provider.get_audio(201).unwrap(),
    };
    assert_eq!(files.first.album_id, files.second.album_id);
    assert_eq!(files.second.album_id, files.second_image.album_id);
    assert_ne!(files.first.folder_id, files.second.folder_id);
    files
}

fn choose_pixels(path: &Path, color: [u8; 4], width: u32) {
    crate::artwork::save_thumbnail(
        path,
        &image::RgbaImage::from_pixel(width, 3, image::Rgba(color)),
    )
    .unwrap();
}

fn selected_pixels(controller: &Controller, file: &AudioFile) -> Option<image::RgbaImage> {
    *controller.selected.lock().unwrap() = Some(file.clone());
    controller.load_artwork(&artwork_job(controller, file))
}

fn assert_pixels(controller: &Controller, file: &AudioFile, expected: [u8; 4]) {
    assert_eq!(
        selected_pixels(controller, file)
            .expect("folder has a cover")
            .get_pixel(0, 0)
            .0,
        expected
    );
}

fn ui_has_color(ui: &AppWindow, color: [u8; 4]) -> bool {
    ui.get_album_cover_ready()
        && ui
            .get_album_cover()
            .to_rgba8()
            .and_then(|pixels| pixels.as_slice().first().copied())
            .is_some_and(|pixel| [pixel.r, pixel.g, pixel.b, pixel.a] == color)
}

fn old_album_paths(controller: &Controller, file: &AudioFile) -> (PathBuf, PathBuf) {
    // This is the persisted pre-folder format, deliberately independent of the
    // new file-context key. Reading it must not turn it into a folder edit.
    let scope = format!("local:{}", controller.provider().album_namespace());
    let group = format!("album-id:{}", file.album_id.unwrap());
    let key = crate::artwork::key(&[&scope, &group]);
    let directory = controller
        .config
        .lock()
        .unwrap()
        .paths
        .settings
        .parent()
        .unwrap()
        .join("artwork-overrides");
    (
        directory.join(format!("{key}.png")),
        directory.join(format!("{key}.hidden")),
    )
}

#[test]
fn folder_candidates_are_selected_before_the_shared_album_limit() {
    isolated("album_review::management::folder_context::child_folder_candidates");
}

#[test]
#[ignore = "isolated folder artwork regression"]
fn child_folder_candidates() {
    let fixture = fixture();
    let files = folder_files(&fixture);
    let controller = &fixture.controller;
    let earlier = fixture
        .provider
        .album_members(
            files.first.album_id.unwrap(),
            64,
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
    assert_eq!(earlier.len(), 64);
    assert!(earlier.iter().all(|file| file.id < files.second.id));
    assert_ne!(
        controller.cached_artwork_path(&files.first),
        controller.cached_artwork_path(&files.second)
    );
    assert_eq!(
        controller.cached_artwork_path(&files.second),
        controller.cached_artwork_path(&files.second_image)
    );
    for file in [&files.first_plain, &files.first] {
        assert_pixels(controller, file, FIRST_COLOR);
    }
    for file in [&files.second, &files.second_image, &files.second] {
        assert_pixels(controller, file, SECOND_COLOR);
    }
    let old = artwork_job(controller, &files.second);
    *controller.selected.lock().unwrap() = Some(files.first.clone());
    assert!(!controller.artwork_current(&old));
    assert!(controller.load_artwork(&old).is_none());
    assert_pixels(controller, &files.first_plain, FIRST_COLOR);
    finish(&fixture);
}

#[test]
fn a_folder_switch_rejects_inflight_pixels_on_the_real_worker() {
    isolated("album_review::management::folder_context::child_inflight_folder_switch");
}

#[test]
#[ignore = "isolated folder artwork worker regression"]
fn child_inflight_folder_switch() {
    let fixture = fixture();
    let files = folder_files(&fixture);
    let controller = &fixture.controller;
    // Hold the real worker before it can consume/write a thumbnail. Switching
    // to another directory of the same album must cancel this admitted request.
    let held = controller.artwork_cache_lock.lock().unwrap();
    *controller.selected.lock().unwrap() = Some(files.first.clone());
    present_file(&fixture.ui, &files.first);
    controller.queue_artwork(&files.first);
    let old_cancel = controller.artwork_cancel.lock().unwrap().clone();
    let old = artwork_job(controller, &files.first);
    let deadline = Instant::now() + Duration::from_secs(5);
    while controller.artwork_job.lock().unwrap().is_some() {
        assert!(
            Instant::now() < deadline,
            "Artwork worker did not take its admitted job"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    *controller.selected.lock().unwrap() = Some(files.second.clone());
    present_file(&fixture.ui, &files.second);
    controller.queue_artwork(&files.second);
    assert!(old_cancel.load(Ordering::Acquire));
    assert!(!controller.artwork_current(&old));
    drop(held);
    wait_for(&fixture.ui, |ui| ui_has_color(ui, SECOND_COLOR));
    // An additional event-loop turn must not publish the earlier directory.
    wait_for(&fixture.ui, |ui| ui_has_color(ui, SECOND_COLOR));
    controller.select_id(files.first_plain.id, false);
    wait_for(&fixture.ui, |ui| ui_has_color(ui, FIRST_COLOR));
    controller.select_id(files.second.id, false);
    wait_for(&fixture.ui, |ui| ui_has_color(ui, SECOND_COLOR));
    finish(&fixture);
}

#[test]
fn folder_edits_outrank_but_never_delete_inherited_album_images() {
    isolated("album_review::management::folder_context::child_folder_manual_inheritance");
}

#[test]
#[ignore = "isolated folder manual artwork regression"]
fn child_folder_manual_inheritance() {
    let fixture = fixture();
    let files = folder_files(&fixture);
    let controller = &fixture.controller;
    let (old_image, _) = old_album_paths(controller, &files.first);
    choose_pixels(&old_image, INHERITED_COLOR, 3);
    let inherited_bytes = std::fs::read(&old_image).unwrap();
    assert_pixels(controller, &files.first_plain, INHERITED_COLOR);
    assert_pixels(controller, &files.second, INHERITED_COLOR);
    let (first_image, first_hidden) = override_paths(controller, &files.first);
    let (second_image, second_hidden) = override_paths(controller, &files.second);
    assert_ne!(first_image, second_image);
    assert!(!first_image.exists() && !second_image.exists());
    let chosen = fixture.root.join("folder-chosen.png");
    choose_pixels(&chosen, CHOSEN_COLOR, 5);
    let admitted_first = controller.artwork_action_context(Some(&files.first));
    // Admission captures the directory even if selection changes before save.
    *controller.selected.lock().unwrap() = Some(files.second.clone());
    assert_eq!(
        controller.perform_artwork_action(admitted_first, 2, Some(chosen), false),
        Ok(true)
    );
    assert!(first_image.is_file() && !second_image.exists());
    assert_pixels(controller, &files.first_plain, CHOSEN_COLOR);
    assert_pixels(controller, &files.second, INHERITED_COLOR);
    let hide = controller.artwork_action_context(Some(&files.first_plain));
    assert_eq!(
        controller.perform_artwork_action(hide, 3, None, false),
        Ok(true)
    );
    assert!(first_hidden.is_file() && !second_hidden.exists());
    assert!(selected_pixels(controller, &files.first).is_none());
    assert_pixels(controller, &files.second, INHERITED_COLOR);
    let restore = controller.artwork_action_context(Some(&files.first));
    assert_eq!(
        controller.perform_artwork_action(restore, 4, None, false),
        Ok(true)
    );
    assert!(!first_image.exists() && !first_hidden.exists());
    assert!(first_image.with_extension("migrated").is_file());
    assert_pixels(controller, &files.first_plain, FIRST_COLOR);
    assert_pixels(controller, &files.second, INHERITED_COLOR);
    let clear = controller.artwork_action_context(Some(&files.first));
    assert_eq!(
        controller.perform_artwork_action(clear, 1, None, false),
        Ok(true)
    );
    assert_pixels(controller, &files.first_plain, FIRST_COLOR);
    assert_pixels(controller, &files.second, INHERITED_COLOR);
    assert_eq!(std::fs::read(&old_image).unwrap(), inherited_bytes);
    finish(&fixture);
}

#[test]
fn folder_restore_ignores_inherited_hidden_without_unhiding_other_directories() {
    isolated("album_review::management::folder_context::child_folder_hidden_inheritance");
}

#[test]
#[ignore = "isolated folder inherited-hidden regression"]
fn child_folder_hidden_inheritance() {
    let fixture = fixture();
    let files = folder_files(&fixture);
    let controller = &fixture.controller;
    let (_, old_hidden) = old_album_paths(controller, &files.first);
    std::fs::create_dir_all(old_hidden.parent().unwrap()).unwrap();
    std::fs::write(&old_hidden, []).unwrap();
    assert!(selected_pixels(controller, &files.first).is_none());
    assert!(selected_pixels(controller, &files.second).is_none());
    let restore = controller.artwork_action_context(Some(&files.first));
    assert_eq!(
        controller.perform_artwork_action(restore, 4, None, false),
        Ok(true)
    );
    assert_pixels(controller, &files.first_plain, FIRST_COLOR);
    assert!(selected_pixels(controller, &files.second).is_none());
    assert_eq!(std::fs::read(&old_hidden).unwrap(), Vec::<u8>::new());
    finish(&fixture);
}

#[test]
fn folder_choices_survive_reopen_and_are_isolated_between_sources() {
    isolated("album_review::management::folder_context::child_folder_reopen_and_sources");
}

#[test]
#[ignore = "isolated folder artwork persistence regression"]
fn child_folder_reopen_and_sources() {
    let fixture = fixture();
    let files = folder_files(&fixture);
    let controller = &fixture.controller;
    assert_pixels(controller, &files.first_plain, FIRST_COLOR);
    assert_pixels(controller, &files.second, SECOND_COLOR);
    let chosen = fixture.root.join("persisted-folder.png");
    choose_pixels(&chosen, CHOSEN_COLOR, 5);
    let context = controller.artwork_action_context(Some(&files.first));
    assert_eq!(
        controller.perform_artwork_action(context, 2, Some(chosen), false),
        Ok(true)
    );
    let context = controller.artwork_action_context(Some(&files.second));
    assert_eq!(
        controller.perform_artwork_action(context, 3, None, false),
        Ok(true)
    );
    assert_pixels(controller, &files.first_plain, CHOSEN_COLOR);
    assert!(selected_pixels(controller, &files.second).is_none());
    let local = override_paths(controller, &files.first);
    *controller.selected.lock().unwrap() = Some(files.first.clone());
    let old_source = artwork_job(controller, &files.first);
    assert!(controller.artwork_current(&old_source));
    controller.source_generation.fetch_add(1, Ordering::AcqRel);
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(audiolib_remote::CacheState {
        library_id: "folder-context-server-a".into(),
        schema_version: 8,
        ..Default::default()
    });
    let server_a = override_paths(controller, &files.first);
    let server_a_other_folder = override_paths(controller, &files.second);
    assert_ne!(local, server_a);
    assert_ne!(server_a, server_a_other_folder);
    assert!(!controller.artwork_current(&old_source));
    assert!(controller.load_artwork(&old_source).is_none());
    controller
        .remote
        .lock()
        .unwrap()
        .state
        .as_mut()
        .unwrap()
        .library_id = "folder-context-server-b".into();
    let server_b = override_paths(controller, &files.first);
    assert_ne!(server_a, server_b);
    assert!(!server_a.0.exists() && !server_b.0.exists());
    controller.remote_mode.store(false, Ordering::Release);
    controller.remote.lock().unwrap().state = None;
    assert_eq!(override_paths(controller, &files.first), local);
    assert_pixels(controller, &files.first_plain, CHOSEN_COLOR);
    controller.capture_search_task(&fixture.ui);
    controller.shutdown();
    // Keep a window registered while replacing the controller. Hiding the last
    // testing window would queue Quit before the reopened provider can publish.

    let preferences = fixture.root.join("preferences");
    let config = AppConfig::load_in(preferences).unwrap();
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    let provider =
        Arc::new(SqliteProvider::open_existing(fixture.provider.database_path()).unwrap());
    let ui = AppWindow::new().unwrap();
    let reopened = Controller::new(
        &ui,
        provider.clone(),
        fixture.engine.handle(),
        config,
        playback,
    );
    ui.show().unwrap();
    fixture.ui.hide().unwrap();
    let ready = reopened.clone();
    wait_for(&ui, move |ui| {
        !ui.get_search_workspace_loading() && !ui.get_searching() && fully_published(&ready)
    });
    let first = provider.get_audio(files.first_plain.id).unwrap();
    let second = provider.get_audio(files.second.id).unwrap();
    assert_eq!(override_paths(&reopened, &first), local);
    assert_pixels(&reopened, &first, CHOSEN_COLOR);
    assert!(selected_pixels(&reopened, &second).is_none());
    let restore = reopened.artwork_action_context(Some(&second));
    assert_eq!(
        reopened.perform_artwork_action(restore, 4, None, false),
        Ok(true)
    );
    assert_pixels(&reopened, &second, SECOND_COLOR);
    assert_pixels(&reopened, &first, CHOSEN_COLOR);
    reopened.shutdown();
    ui.hide().unwrap();
}
