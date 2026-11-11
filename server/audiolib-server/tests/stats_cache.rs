// server/audiolib-server/tests/stats_cache.rs
//! `GET /library` statistics are cached per revision (the aggregates scan the whole
//! table: ~30 s cold on a 909k-row library): a slow cache of an older revision is served
//! as is while one background refresh recomputes it, a restart reads the sidecar, and a
//! stale cache is never recomputed once per request.
use audiolib_core::DataProvider;
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use audiolib_server::{AppState, ServerConfig};
use std::{
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

fn wav(path: &Path) {
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    for value in [1_u16, 1] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.extend_from_slice(&96_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&8_u32.to_le_bytes());
    bytes.extend_from_slice(&[0; 8]);
    let riff_size = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn state_with_files(count: usize) -> (tempfile::TempDir, ServerConfig, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let sounds = dir.path().join("sounds");
    for index in 0..count {
        wav(&sounds.join("Group").join(format!("f_{index}.wav")));
    }
    let db = dir.path().join("library").join("library.db");
    let progress = ScanService::new(Arc::new(SqliteProvider::open(&db).unwrap()))
        .scan(
            &sounds,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(progress.errors, 0, "fixture scan must succeed");
    assert_eq!(
        progress.added, count as u64,
        "fixture files must be indexed"
    );
    let config = ServerConfig::from_toml_str(
        &format!(
            "bind = \"127.0.0.1:0\"\nlibrary_db = 'library/library.db'\n\
             [[roots]]\nname = \"SFX\"\nserver_path = '{}'\n",
            sounds.display()
        ),
        dir.path(),
    )
    .unwrap();
    let state = AppState::open(&config, None).unwrap();
    (dir, config, state)
}

fn sidecar(config: &ServerConfig) -> std::path::PathBuf {
    let mut name = config.library_db.clone().into_os_string();
    name.push(".stats.json");
    name.into()
}

#[test]
fn stats_are_cached_persisted_and_a_slow_stale_cache_is_served_while_refreshing() {
    let (_dir, config, state) = state_with_files(3);
    let first = state.library_response().unwrap();
    assert_eq!(first.info.stats.audio_files, 3);
    let file = sidecar(&config);
    assert!(file.is_file(), "the first computation writes the sidecar");

    // A cache of another revision that took long to compute: served as is (not
    // recomputed for this request), then replaced by one background refresh.
    let mut cached: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    cached["revision"] = (first.revision + 1000).into();
    cached["millis"] = 60_000.into();
    cached["stats"]["audio_files"] = 12345.into();
    fs::write(&file, serde_json::to_vec(&cached).unwrap()).unwrap();
    let restarted = AppState::open(&config, None).unwrap();
    let stale = restarted.library_response().unwrap();
    assert_eq!(
        stale.info.stats.audio_files, 12345,
        "a slow stale cache must not be recomputed inside the request"
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let now = restarted.library_response().unwrap();
        if now.info.stats.audio_files == 3 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "background refresh never finished"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(saved["stats"]["audio_files"], 3);
    assert_eq!(saved["revision"], first.revision);

    // A cache that is fast to recompute is redone inline: the reply is always exact.
    let mut cached = saved;
    cached["revision"] = (first.revision + 1000).into();
    cached["millis"] = 1.into();
    cached["stats"]["audio_files"] = 777.into();
    fs::write(&file, serde_json::to_vec(&cached).unwrap()).unwrap();
    let again = AppState::open(&config, None).unwrap();
    assert_eq!(again.library_response().unwrap().info.stats.audio_files, 3);

    // A sidecar of another library is ignored.
    cached["library_id"] = "someone-else".into();
    cached["millis"] = 60_000.into();
    fs::write(&file, serde_json::to_vec(&cached).unwrap()).unwrap();
    let other = AppState::open(&config, None).unwrap();
    assert_eq!(other.library_response().unwrap().info.stats.audio_files, 3);
}

/// Plays keep the revision unchanged. Metadata edits carry aggregates forward to
/// the new revision, and the startup warm-up does not
/// recompute a sidecar that already matches the current revision.
#[test]
fn stats_neutral_writes_carry_the_cache_forward_and_warm_up_skips_a_current_sidecar() {
    let (_dir, config, state) = state_with_files(3);
    let first = state.library_response().unwrap();
    let file = sidecar(&config);

    // Mark the cached numbers so a recomputation would be visible.
    let mut cached: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    cached["stats"]["audio_files"] = 4242.into();
    fs::write(&file, serde_json::to_vec(&cached).unwrap()).unwrap();
    let state = AppState::open(&config, None).unwrap();
    state.refresh_stats_in_background();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        state.library_response().unwrap().info.stats.audio_files,
        4242,
        "warm-up must not recompute a sidecar of the current revision"
    );

    let id = state
        .provider()
        .search(&Default::default())
        .unwrap()
        .results[0]
        .id;
    let played = state.record_play(id).unwrap();
    assert_eq!(played.revision_before, first.revision);
    assert_eq!(played.revision, first.revision);
    let edited = state
        .update_metadata(
            id,
            &audiolib_core::MetadataPatch {
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(edited.revision, first.revision + 1);
    let after = state.library_response().unwrap();
    assert_eq!(after.revision, edited.revision);
    assert_eq!(
        after.info.stats.audio_files, 4242,
        "a play / metadata edit must reuse the cached aggregates"
    );
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(saved["revision"], edited.revision);
}
