// app/audiolib-desktop/src/artwork_management_tests.rs
//! Cover preferences and manual management use the actual controller and owned files.
use super::*;

#[path = "artwork_busy_tests.rs"]
mod busy;
#[path = "artwork_contract_review_tests.rs"]
mod contracts;
#[path = "artwork_folder_context_tests.rs"]
mod folder_context;
#[path = "artwork_worker_review_tests.rs"]
mod worker_review;

fn selected_artwork_file(fixture: &Fixture) -> AudioFile {
    let file = fixture
        .provider
        .search(&SearchRequest::default())
        .unwrap()
        .results
        .remove(0);
    let connection = rusqlite::Connection::open(fixture.provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&connection).unwrap();
    connection
        .execute(
            "UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.wav_metadata.fields.library',?2) WHERE id=?1",
            rusqlite::params![file.id, "Artwork album"],
        )
        .unwrap();
    drop(connection);
    let file = fixture.provider.get_audio(file.id).unwrap();
    fixture.controller.config.lock().unwrap().allowed_scan_roots =
        vec![fixture.root.join("sounds")];
    *fixture.controller.selected.lock().unwrap() = Some(file.clone());
    present_file(&fixture.ui, &file);
    file
}

fn artwork_job(controller: &Controller, file: &AudioFile) -> ArtworkJob {
    ArtworkJob {
        album: file.album_entity.clone().unwrap(),
        folder: controller.artwork_folder_context(file),
        source_generation: controller.source_generation.load(Ordering::Acquire),
        epoch: controller.artwork_epoch.load(Ordering::Acquire),
        cancel: Arc::new(AtomicBool::new(false)),
        notice: Mutex::new(None),
    }
}

fn override_paths(controller: &Controller, file: &AudioFile) -> (PathBuf, PathBuf) {
    let (directory, key) = controller.artwork_identity(file).unwrap();
    (
        directory.join(format!("{key}.png")),
        directory.join(format!("{key}.hidden")),
    )
}

fn manual_cover(path: &Path) {
    let pixels = image::RgbaImage::from_pixel(3, 3, image::Rgba([0, 0, 255, 255]));
    crate::artwork::save_thumbnail(path, &pixels).unwrap();
}

fn embedded_cover(path: &Path) {
    embedded_cover_color(path, [255, 0, 0, 255]);
}

fn embedded_cover_color(path: &Path, color: [u8; 4]) {
    let pixels = image::RgbaImage::from_pixel(4, 2, image::Rgba(color));
    let mut png = std::io::Cursor::new(Vec::new());
    pixels.write_to(&mut png, image::ImageFormat::Png).unwrap();
    let mut picture = b"\0image/png\0\x03\0".to_vec();
    picture.extend(png.into_inner());
    let mut frame = b"APIC".to_vec();
    frame.extend((picture.len() as u32).to_be_bytes());
    frame.extend([0, 0]);
    frame.extend(picture);
    let length = frame.len() as u32;
    let mut tag = b"ID3\x03\0\0".to_vec();
    tag.extend([
        ((length >> 21) & 0x7f) as u8,
        ((length >> 14) & 0x7f) as u8,
        ((length >> 7) & 0x7f) as u8,
        (length & 0x7f) as u8,
    ]);
    tag.extend(frame);
    let mut wav = std::fs::read(path).unwrap();
    wav.extend(b"id3 ");
    wav.extend((tag.len() as u32).to_le_bytes());
    wav.extend(&tag);
    if !tag.len().is_multiple_of(2) {
        wav.push(0);
    }
    let riff_size = (wav.len() - 8) as u32;
    wav[4..8].copy_from_slice(&riff_size.to_le_bytes());
    std::fs::write(path, wav).unwrap();
}

#[test]
fn folder_album_default_is_independent_of_selected_wav_and_manual_choices_are_shared() {
    isolated("album_review::management::child_shared_folder_cover");
}

#[test]
#[ignore = "isolated shared folder artwork regression"]
fn child_shared_folder_cover() {
    let fixture = fixture();
    let controller = &fixture.controller;
    controller.config.lock().unwrap().allowed_scan_roots = vec![fixture.root.join("sounds")];
    let mut files = fixture
        .provider
        .search(&SearchRequest::default())
        .unwrap()
        .results;
    let directory = audiolib_db::normalize_source_path_identity(&audiolib_db::display_path(
        &fixture.root.join("sounds/gore").canonicalize().unwrap(),
    ));
    files.retain(|file| audiolib_db::source_parent_path_identity(&file.file_path) == directory);
    files.sort_by_key(|file| file.id);
    assert!(files.len() >= 2);
    let first = &files[0];
    let second = &files[1];
    assert_eq!(first.album_id, second.album_id);
    assert_eq!(first.folder_id, second.folder_id);
    assert_eq!(first.album_entity.as_ref().unwrap().kind, AlbumKind::Folder);
    let blue = [0, 0, 255, 255];
    embedded_cover_color(Path::new(&first.file_path), blue);
    embedded_cover_color(Path::new(&second.file_path), [255, 0, 0, 255]);
    for file in [second, first, second] {
        *controller.selected.lock().unwrap() = Some(file.clone());
        let pixels = controller
            .load_artwork(&artwork_job(controller, file))
            .unwrap();
        assert_eq!(
            pixels.get_pixel(0, 0).0,
            blue,
            "the first member in the same directory supplies the shared cover"
        );
    }
    assert_eq!(
        override_paths(controller, first),
        override_paths(controller, second)
    );
    let context = controller.artwork_action_context(Some(second));
    assert_eq!(
        controller.perform_artwork_action(context, 3, None, false),
        Ok(true)
    );
    *controller.selected.lock().unwrap() = Some(first.clone());
    assert!(
        controller
            .load_artwork(&artwork_job(controller, first))
            .is_none()
    );
    let context = controller.artwork_action_context(Some(first));
    assert_eq!(
        controller.perform_artwork_action(context, 4, None, false),
        Ok(true)
    );
    assert_eq!(
        controller
            .load_artwork(&artwork_job(controller, first))
            .unwrap()
            .get_pixel(0, 0)
            .0,
        blue
    );
    finish(&fixture);
}

#[test]
fn manual_cover_hide_restore_and_cache_clear_preserve_separate_lifetimes() {
    isolated("album_review::management::child_manual_cover_management");
}

#[test]
#[ignore = "isolated artwork management regression"]
fn child_manual_cover_management() {
    let fixture = fixture();
    let file = selected_artwork_file(&fixture);
    let controller = &fixture.controller;
    let ui = &fixture.ui;
    embedded_cover(Path::new(&file.file_path));
    let (manual, hidden) = override_paths(controller, &file);
    manual_cover(&manual);
    controller.queue_artwork(&file);
    wait_for(ui, |ui| ui.get_album_cover_ready());
    assert_eq!(ui.get_album_cover().size().width, 3);
    // Presentation uses the provider's shared entity even when source Metadata
    // text is refreshed independently of an annotation-only save.
    let custom_cover = ui.get_album_cover();
    let mut ungrouped = file.clone();
    ungrouped.attributes["wav_metadata"]["fields"]["library"] = "".into();
    ungrouped.album.clear();
    present_file(ui, &ungrouped);
    ui.set_album_cover(custom_cover);
    ui.set_album_cover_ready(true);
    let mut annotated = ungrouped.clone();
    annotated.notes = "Saved annotation".into();
    present_file(ui, &annotated);
    assert!(
        ui.get_album_cover_ready(),
        "saving an ungrouped record retains its cover"
    );
    assert_eq!(ui.get_album_cover().size().width, 3);
    present_file(ui, &file);
    controller.queue_artwork(&file);
    wait_for(ui, |ui| ui.get_album_cover_ready());

    ui.invoke_artwork_action_requested(3);
    wait_for(ui, |ui| {
        !ui.get_artwork_busy() && !ui.get_album_cover_ready()
    });
    assert!(hidden.is_file());
    assert!(!manual.exists());
    assert!(
        controller
            .load_artwork(&artwork_job(controller, &file))
            .is_none()
    );
    // Re-reading the persisted marker must continue to suppress the embedded cover.
    controller.queue_artwork(&file);
    assert!(
        controller
            .load_artwork(&artwork_job(controller, &file))
            .is_none()
    );

    ui.invoke_artwork_action_requested(4);
    wait_for(ui, |ui| {
        !ui.get_artwork_busy() && ui.get_album_cover_ready()
    });
    assert!(!hidden.exists());
    assert_eq!(ui.get_album_cover().size().width, 4);

    manual_cover(&manual);
    let cache = controller.config.lock().unwrap().paths.cache.clone();
    let thumbnails = cache.join("artwork");
    std::fs::create_dir_all(&thumbnails).unwrap();
    let automatic = thumbnails.join("0123456789abcdef.png");
    let etag = automatic.with_extension("etag");
    manual_cover(&automatic);
    std::fs::write(&etag, b"\"server-cover\"").unwrap();
    let unrelated = thumbnails.join("keep.txt");
    std::fs::write(&unrelated, b"not an application thumbnail").unwrap();
    ui.invoke_artwork_action_requested(1);
    wait_for(ui, |ui| {
        !ui.get_artwork_busy() && ui.get_album_cover_ready()
    });
    assert!(!automatic.exists());
    assert!(!etag.exists());
    assert!(unrelated.is_file());
    assert!(
        manual.is_file(),
        "clearing generated cache retains manual cover"
    );
    assert_eq!(ui.get_album_cover().size().width, 3);
    finish(&fixture);
}

#[test]
fn disabling_artwork_persists_and_prevents_decode_or_request_queueing() {
    isolated("album_review::management::child_artwork_preference");
}

#[test]
#[ignore = "isolated artwork preference regression"]
fn child_artwork_preference() {
    let fixture = fixture();
    let file = selected_artwork_file(&fixture);
    let controller = &fixture.controller;
    let ui = &fixture.ui;
    let (manual, _) = override_paths(controller, &file);
    manual_cover(&manual);
    controller.queue_artwork(&file);
    wait_for(ui, |ui| ui.get_album_cover_ready());

    ui.invoke_artwork_enabled_requested(false);
    let saved = controller.clone();
    wait_for(ui, move |ui| {
        !ui.get_artwork_enabled() && !saved.config.lock().unwrap().show_album_artwork
    });
    assert!(!ui.get_album_cover_ready());
    let preferences = controller.config.lock().unwrap().data_root().to_path_buf();
    assert!(
        !AppConfig::load_in(preferences.clone())
            .unwrap()
            .show_album_artwork
    );
    assert!(
        controller
            .load_artwork(&artwork_job(controller, &file))
            .is_none()
    );

    // The same disabled preference prevents remote selection from queuing any
    // HTTP-capable job, even when a snapshot identity exists.
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(audiolib_remote::CacheState {
        library_id: "artwork-server".into(),
        ..Default::default()
    });
    controller.queue_artwork(&file);
    assert!(controller.artwork_job.lock().unwrap().is_none());
    assert!(
        controller
            .load_artwork(&artwork_job(controller, &file))
            .is_none()
    );
    controller.remote_mode.store(false, Ordering::Release);
    controller.remote.lock().unwrap().state = None;

    ui.invoke_artwork_enabled_requested(true);
    let saved = controller.clone();
    wait_for(ui, move |ui| {
        ui.get_artwork_enabled()
            && ui.get_album_cover_ready()
            && saved.config.lock().unwrap().show_album_artwork
    });
    assert!(AppConfig::load_in(preferences).unwrap().show_album_artwork);
    assert!(manual.is_file());
    finish(&fixture);
}

#[test]
fn cover_identity_and_stale_jobs_remain_bound_to_their_library_and_selection() {
    isolated("album_review::management::child_artwork_identity_and_cancellation");
}

#[test]
#[ignore = "isolated artwork identity and cancellation regression"]
fn child_artwork_identity_and_cancellation() {
    let fixture = fixture();
    let file = selected_artwork_file(&fixture);
    let controller = &fixture.controller;
    let (local, _) = override_paths(controller, &file);
    manual_cover(&local);
    let original = artwork_job(controller, &file);
    assert!(controller.load_artwork(&original).is_some());

    let other = Arc::new(SqliteProvider::open(&fixture.root.join("other.db")).unwrap());
    *controller.sqlite.write().unwrap() = other;
    let (other_local, _) = override_paths(controller, &file);
    assert_ne!(local, other_local);
    assert!(!other_local.exists());
    *controller.sqlite.write().unwrap() = fixture.provider.clone();

    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(audiolib_remote::CacheState {
        library_id: "server-library-a".into(),
        ..Default::default()
    });
    let (server_a, _) = override_paths(controller, &file);
    manual_cover(&server_a);
    controller
        .remote
        .lock()
        .unwrap()
        .state
        .as_mut()
        .unwrap()
        .library_id = "server-library-b".into();
    let (server_b, _) = override_paths(controller, &file);
    assert_ne!(local, server_a);
    assert_ne!(server_a, server_b);
    assert!(!server_b.exists());
    controller.remote_mode.store(false, Ordering::Release);
    controller.remote.lock().unwrap().state = None;

    // These are the same gates consulted before decoding and again before the
    // worker publishes pixels onto the Slint event loop.
    controller
        .selection_generation
        .fetch_add(1, Ordering::AcqRel);
    assert!(
        controller.artwork_current(&original),
        "selection changes within the same shared album retain its cover request"
    );
    assert!(controller.load_artwork(&original).is_some());
    let old_source = artwork_job(controller, &file);
    controller.source_generation.fetch_add(1, Ordering::AcqRel);
    assert!(!controller.artwork_current(&old_source));
    assert!(controller.load_artwork(&old_source).is_none());
    let cancelled = artwork_job(controller, &file);
    cancelled.cancel.store(true, Ordering::Release);
    assert!(!controller.artwork_current(&cancelled));
    assert!(controller.load_artwork(&cancelled).is_none());
    let cleared = artwork_job(controller, &file);
    controller.cancel_artwork();
    assert!(!controller.artwork_current(&cleared));
    assert!(controller.load_artwork(&cleared).is_none());
    assert!(local.is_file());
    assert!(server_a.is_file());
    finish(&fixture);
}
