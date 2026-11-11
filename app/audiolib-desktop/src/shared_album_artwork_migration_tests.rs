// app/audiolib-desktop/src/shared_album_artwork_migration_tests.rs
//! Legacy server choices retain their semantic owner when native albums arrive.
use super::*;
use audiolib_core::compatibility::LibraryFormat;
use audiolib_remote::{CacheFormat, CacheState};

const LIBRARY: &str = "owned-album-choice-library";

#[test]
fn legacy_projection_choices_survive_native_server_albums() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "controller::shared_album_artwork::migration_tests::child_legacy_projection_choices",
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

fn snapshot(path: &Path, native: bool) -> Arc<SqliteProvider> {
    let provider = Arc::new(SqliteProvider::open(path).unwrap());
    let conn = rusqlite::Connection::open(path).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    if native {
        conn.execute("INSERT INTO audio_files(id,file_path,filename,metadata_json) VALUES(99,'dummy.wav','dummy.wav',?1)",[r#"{"wav_metadata":{"fields":{"library":"Earlier native album"}}}"#]).unwrap();
    }
    for (id, name) in [(1, "Custom"), (2, "Hidden"), (3, "Embedded")] {
        conn.execute(
            "INSERT INTO audio_files(id,file_path,filename,metadata_json) VALUES(?1,?2,?2,?3)",
            rusqlite::params![
                id,
                format!("{name}.wav"),
                serde_json::json!({"wav_metadata":{"fields":{"library":name}}}).to_string()
            ],
        )
        .unwrap();
    }
    provider
}

fn state(legacy: bool) -> CacheState {
    let local = LibraryFormat::target();
    let mut source = local.clone();
    if legacy {
        source.schema_version = 7;
        source.features.remove("album_entities");
        source.features.remove("metadata_storage");
    }
    CacheState {
        library_id: LIBRARY.into(),
        schema_version: 8,
        format: Some(Box::new(CacheFormat {
            source: Some(source),
            local,
            generation: u64::from(legacy),
        })),
        ..Default::default()
    }
}

#[test]
#[ignore = "isolated shared album preference migration regression"]
fn child_legacy_projection_choices() {
    i_slint_backend_testing::init_no_event_loop();
    let root = tempfile::tempdir().unwrap();
    let preferences = root.path().join("preferences");
    std::fs::create_dir_all(&preferences).unwrap();
    let legacy = snapshot(&root.path().join("legacy-owned.db"), false);
    let native = snapshot(&root.path().join("native.db"), true);
    assert_ne!(legacy.album_namespace(), native.album_namespace());
    let playback = PlaybackConfig {
        volume: 0.0,
        ..Default::default()
    };
    std::fs::write(preferences.join("settings.json"),serde_json::to_vec(&serde_json::json!({"active_database":legacy.database_path(),"language":"en","playback":playback})).unwrap()).unwrap();
    let config = AppConfig::load_in(preferences.clone()).unwrap();
    let engine = audiolib_audio::AudioEngine::new(playback.clone()).unwrap();
    let ui = AppWindow::new().unwrap();
    let controller = Controller::new(&ui, legacy.clone(), engine.handle(), config, playback);
    controller.remote_mode.store(true, Ordering::Release);
    controller.remote.lock().unwrap().state = Some(state(true));
    let directory = preferences.join("artwork-overrides");
    let chosen = root.path().join("chosen.png");
    crate::artwork::save_thumbnail(
        &chosen,
        &image::RgbaImage::from_pixel(3, 3, image::Rgba([20, 40, 60, 255])),
    )
    .unwrap();
    let pixels = std::fs::read(&chosen).unwrap();
    let mut old_keys = Vec::new();
    for (id, action, name) in [(1, 2, "Custom"), (2, 3, "Hidden"), (3, 4, "Embedded")] {
        let file = legacy.get_audio(id).unwrap();
        // Seed the older whole-album preference format, before directory-scoped edits.
        let (actual_directory, key) = controller
            .album_artwork_identity(file.album_entity.as_ref().unwrap())
            .unwrap();
        assert_eq!(actual_directory, directory);
        assert_eq!(
            key,
            crate::artwork::key(&[
                &format!("server:{LIBRARY}"),
                &format!("shared-metadata:{name}")
            ])
        );
        // An explicit embedded choice must outrank a retained pre-entity image.
        let original =
            crate::artwork::key(&[&format!("server:{LIBRARY}"), &format!("album:{name}")]);
        crate::artwork::save_thumbnail(
            &directory.join(format!("{original}.png")),
            &image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 0, 0, 255])),
        )
        .unwrap();
        match action {
            2 => {
                std::fs::copy(&chosen, directory.join(format!("{key}.png"))).unwrap();
            }
            3 => {
                std::fs::write(directory.join(format!("{key}.hidden")), []).unwrap();
            }
            4 => {}
            _ => unreachable!(),
        }
        crate::album_override_migration::mark_completed(&directory, &key).unwrap();
        assert!(directory.join(format!("{key}.migrated")).is_file());
        old_keys.push((id, action, key, original));
    }
    let unassigned = Album {
        id: 700,
        name: "Unassigned library".into(),
        kind: AlbumKind::Folder,
        metadata_key: None,
        folder_id: None,
        folder_path: None,
        representative_audio_id: None,
    };
    let (_, unassigned_key) = controller
        .album_artwork_identity(&unassigned)
        .expect("legacy unassigned entity has a stable choice identity");
    assert_eq!(
        unassigned_key,
        crate::artwork::key(&[&format!("server:{LIBRARY}"), "shared-unassigned-folder"])
    );
    let folder = Album {
        folder_path: Some("shared-unassigned-folder".into()),
        ..unassigned
    };
    assert_ne!(
        controller.album_artwork_identity(&folder).unwrap().1,
        unassigned_key
    );

    controller.cancel_artwork();
    controller.source_generation.fetch_add(1, Ordering::AcqRel);
    *controller.sqlite.write().unwrap() = native.clone();
    controller.remote.lock().unwrap().state = Some(state(false));
    for (id, action, old_key, original) in old_keys {
        let file = native.get_audio(id).unwrap();
        assert_ne!(file.album_id, legacy.get_audio(id).unwrap().album_id);
        *controller.selected.lock().unwrap() = Some(file.clone());
        let folder_context = controller.artwork_folder_context(&file);
        let album = file.album_entity.clone().unwrap();
        let (_, key) = controller.album_artwork_identity(&album).unwrap();
        assert_ne!(key, old_key);
        let job = ArtworkJob {
            album: album.clone(),
            folder: folder_context,
            source_generation: controller.source_generation.load(Ordering::Acquire),
            epoch: controller.artwork_epoch.load(Ordering::Acquire),
            cancel: Arc::new(AtomicBool::new(false)),
            notice: Mutex::new(None),
        };
        controller.migrate_album_choices(&job, &native, &album, &directory, &key);
        assert!(job.notice.lock().unwrap().is_none());
        assert!(directory.join(format!("{key}.migrated")).is_file());
        assert_eq!(
            directory.join(format!("{key}.hidden")).is_file(),
            action == 3
        );
        assert_eq!(directory.join(format!("{key}.png")).is_file(), action == 2);
        if action == 2 {
            assert_eq!(
                std::fs::read(directory.join(format!("{key}.png"))).unwrap(),
                pixels
            );
        }
        assert!(directory.join(format!("{old_key}.migrated")).is_file());
        assert!(directory.join(format!("{original}.png")).is_file());
        controller.migrate_album_choices(&job, &native, &album, &directory, &key);
        assert_eq!(directory.join(format!("{key}.png")).is_file(), action == 2);
    }
    controller.shutdown();
    drop(engine);
}
