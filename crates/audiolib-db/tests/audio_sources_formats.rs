// crates/audiolib-db/tests/audio_sources_formats.rs
//! Extended sources publish exact formats atomically and preserve library identities.
#![cfg(feature = "scanner")]

use audiolib_core::{
    DataProvider, MetadataPatch, ScanProgress, SearchRequest,
    compatibility::{
        AUDIO_SOURCES_FEATURE, LibraryAccess, PLAN_B_AUDIO_FORMAT, PLAN_B_FORMAT,
        PLAN_B_V9_AUDIO_FORMAT, PLAN_B_V9_FORMAT,
    },
};
use audiolib_db::{
    ScanCancellation, ScanOptions, ScanService, SqliteProvider, migration,
    migration9::{self, Action, Options, State},
};
use std::{
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};
mod support;

fn wav() -> Vec<u8> {
    [
        b"RIFF".as_slice(),
        &44_u32.to_le_bytes(),
        b"WAVEfmt ",
        &16_u32.to_le_bytes(),
        &1_u16.to_le_bytes(),
        &1_u16.to_le_bytes(),
        &48_000_u32.to_le_bytes(),
        &96_000_u32.to_le_bytes(),
        &2_u16.to_le_bytes(),
        &16_u16.to_le_bytes(),
        b"data",
        &8_u32.to_le_bytes(),
        &[0; 8],
    ]
    .concat()
}

fn aiff() -> Vec<u8> {
    fn chunk(bytes: &mut Vec<u8>, name: &[u8; 4], payload: &[u8]) {
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(payload);
        if !payload.len().is_multiple_of(2) {
            bytes.push(0);
        }
    }
    let mut bytes = b"FORM\0\0\0\0AIFF".to_vec();
    let common = [
        1_u16.to_be_bytes().as_slice(),
        &4_u32.to_be_bytes(),
        &16_u16.to_be_bytes(),
        &[0x40, 0x0e, 0xbb, 0x80, 0, 0, 0, 0, 0, 0],
    ]
    .concat();
    chunk(&mut bytes, b"COMM", &common);
    chunk(&mut bytes, b"ANNO", b"aiffsourceboundary");
    chunk(&mut bytes, b"SSND", &[0; 16]);
    let size = bytes.len() as u32 - 8;
    bytes[4..8].copy_from_slice(&size.to_be_bytes());
    bytes
}

fn copy_mp3(path: &Path) {
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../audiolib-audio/src/fixtures/mp3/cbr-info.mp3"),
        path,
    )
    .unwrap();
}

fn bw64() -> Vec<u8> {
    let pcm = wav();
    let mut bytes = b"BW64".to_vec();
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    bytes.extend_from_slice(b"WAVEds64");
    bytes.extend_from_slice(&28_u32.to_le_bytes());
    bytes.extend_from_slice(&80_u64.to_le_bytes());
    bytes.extend_from_slice(&8_u64.to_le_bytes());
    bytes.extend_from_slice(&4_u64.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&pcm[12..40]);
    bytes.extend_from_slice(&u32::MAX.to_le_bytes());
    bytes.extend_from_slice(&[0; 8]);
    bytes
}

fn scan(provider: &Arc<SqliteProvider>, root: &Path, incremental: bool) -> ScanProgress {
    ScanService::new(provider.clone())
        .scan(
            root,
            &ScanOptions {
                incremental,
                workers: 2,
                batch_size: 2,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap()
}

fn assert_format(path: &Path, expected: &str, extended: bool) {
    let inspected =
        migration::inspect(path, LibraryAccess::Read, false, &AtomicBool::new(false)).unwrap();
    assert_eq!(
        inspected.format.snapshot_format_id().as_deref(),
        Some(expected)
    );
    assert_eq!(
        inspected.format.features.get(AUDIO_SOURCES_FEATURE),
        extended.then_some(&1)
    );
    assert!(inspected.decision.direct_use);
    if extended {
        let conn = support::connection(path).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT value FROM library_meta WHERE key='snapshot_format'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            expected
        );
    }
}

#[test]
fn failed_probe_and_failed_write_do_not_activate_extended_source_format() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let database = temp.path().join("library.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    assert_format(&database, PLAN_B_FORMAT, false);
    fs::write(root.join("invalid.mp3"), b"not an MPEG audio payload").unwrap();
    let failed_probe = scan(&provider, &root, true);
    assert_eq!((failed_probe.added, failed_probe.errors), (0, 1));
    assert_format(&database, PLAN_B_FORMAT, false);
    fs::remove_file(root.join("invalid.mp3")).unwrap();
    fs::write(root.join("blocked.bwf"), wav()).unwrap();
    let conn = support::connection(&database).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_owned_fixture BEFORE INSERT ON audio_files WHEN new.filename='blocked.bwf' BEGIN SELECT RAISE(ABORT,'owned fixture write failure'); END;").unwrap();
    assert!(
        ScanService::new(provider.clone())
            .scan(
                &root,
                &ScanOptions::default(),
                &ScanCancellation::default(),
                |_| {}
            )
            .is_err()
    );
    assert_format(&database, PLAN_B_FORMAT, false);
    assert_eq!(provider.stats().unwrap().audio_files, 0);
    conn.execute_batch("DROP TRIGGER reject_owned_fixture;")
        .unwrap();
    let success = scan(&provider, &root, true);
    assert_eq!((success.added, success.errors), (1, 0));
    provider.rebuild_directory_search().unwrap();
    assert_format(&database, PLAN_B_AUDIO_FORMAT, true);
    drop(conn);
    drop(provider);
    assert!(SqliteProvider::open_existing(&database).is_ok());
}

#[test]
fn renamed_containers_use_actual_extractors_and_refresh_retains_annotations() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let mp3 = root.join("mpeg-renamed.wav");
    copy_mp3(&mp3);
    fs::write(root.join("aiff-renamed.wav"), aiff()).unwrap();
    let database = temp.path().join("library.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    let first = scan(&provider, &root, true);
    assert_eq!((first.added, first.errors), (2, 0));
    assert_format(&database, PLAN_B_AUDIO_FORMAT, true);
    let page = provider.search(&SearchRequest::default()).unwrap();
    let mpeg = page
        .results
        .iter()
        .find(|file| file.filename == "mpeg-renamed.wav")
        .unwrap();
    let aiff = page
        .results
        .iter()
        .find(|file| file.filename == "aiff-renamed.wav")
        .unwrap();
    let mpeg_id = mpeg.id;
    let aiff_id = aiff.id;
    assert_eq!(mpeg.attributes["container"], "MP3");
    assert_eq!(aiff.attributes["container"], "AIFF");
    assert!(mpeg.attributes["source_audio_layout"].is_object());
    for id in [mpeg_id, aiff_id] {
        provider
            .update_metadata(
                id,
                &MetadataPatch {
                    tags: Some("preserved-source-tag".into()),
                    notes: Some("preserved source notes".into()),
                    favorite: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let repeated = scan(&provider, &root, true);
    assert_eq!(
        (
            repeated.added,
            repeated.updated,
            repeated.unchanged,
            repeated.errors
        ),
        (0, 0, 2, 0)
    );
    let conn = support::connection(&database).unwrap();
    conn.execute("UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.wav_metadata.mp3_version',0,'$.unknown_extension',json('{\"retained\":true}')) WHERE id=?1", [mpeg_id]).unwrap();
    conn.execute("UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.wav_metadata.aiff_version',0,'$.unknown_extension',json('{\"retained\":true}')) WHERE id=?1", [aiff_id]).unwrap();
    drop(conn);
    let refreshed = scan(&provider, &root, true);
    assert_eq!(
        (refreshed.updated, refreshed.unchanged, refreshed.errors),
        (2, 0, 0)
    );
    for id in [mpeg_id, aiff_id] {
        let file = provider.get_audio(id).unwrap();
        assert_eq!(file.tags, "preserved-source-tag");
        assert_eq!(file.notes, "preserved source notes");
        assert!(file.favorite);
        assert_eq!(file.attributes["unknown_extension"]["retained"], true);
    }
    let conn = support::connection(&database).unwrap();
    for malformed_marker in ["\"1\"", "1.0", "true"] {
        conn.execute("UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.wav_metadata.mp3_version',json(?1)) WHERE id=?2", rusqlite::params![malformed_marker, mpeg_id]).unwrap();
        conn.execute("UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.wav_metadata.aiff_version',json(?1)) WHERE id=?2", rusqlite::params![malformed_marker, aiff_id]).unwrap();
        let refreshed = scan(&provider, &root, true);
        assert_eq!(
            (refreshed.updated, refreshed.unchanged, refreshed.errors),
            (2, 0, 0),
            "{malformed_marker}"
        );
    }
    drop(conn);
    // A source format change must clear the previous extractor's owned layout.
    fs::write(&mp3, wav()).unwrap();
    let replaced = scan(&provider, &root, false);
    assert_eq!(replaced.errors, 0);
    let replaced = provider.get_audio(mpeg_id).unwrap();
    assert_eq!(replaced.attributes["container"], "RIFF");
    assert!(replaced.attributes["source_audio_layout"].is_null());
    assert!(replaced.attributes["wav_metadata"]["mp3_version"].is_null());
    assert_eq!(replaced.notes, "preserved source notes");
    assert_eq!(replaced.attributes["unknown_extension"]["retained"], true);
    assert_eq!(provider.stats().unwrap().audio_files, 2);
}

#[test]
fn new_extensions_reconcile_missing_without_losing_identity_or_annotations() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    for name in ["source.aif", "source.aiff"] {
        fs::write(root.join(name), aiff()).unwrap();
    }
    fs::write(root.join("source.bwf"), wav()).unwrap();
    copy_mp3(&root.join("source.mp3"));
    let database = temp.path().join("library.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    assert_eq!(scan(&provider, &root, true).added, 4);
    let original = provider.search(&SearchRequest::default()).unwrap().results;
    for file in &original {
        provider
            .update_metadata(
                file.id,
                &MetadataPatch {
                    notes: Some("missing-source annotation".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        fs::remove_file(root.join(&file.filename)).unwrap();
    }
    fs::write(root.join("control.wav"), wav()).unwrap();
    let missing = scan(&provider, &root, false);
    assert_eq!((missing.added, missing.removed, missing.errors), (1, 4, 0));
    for file in &original {
        assert!(provider.get_audio(file.id).unwrap().missing);
        let target = root.join(&file.filename);
        if file.filename.ends_with(".mp3") {
            copy_mp3(&target);
        } else if file.filename.ends_with(".bwf") {
            fs::write(target, wav()).unwrap();
        } else {
            fs::write(target, aiff()).unwrap();
        }
    }
    let returned = scan(&provider, &root, true);
    assert_eq!((returned.added, returned.errors), (0, 0));
    for file in &original {
        let restored = provider.get_audio(file.id).unwrap();
        assert!(!restored.missing);
        assert_eq!(restored.notes, "missing-source annotation");
    }
    assert_eq!(provider.stats().unwrap().audio_files, 5);
}

#[test]
fn schema9_activation_rollback_and_legacy_projection_preserve_exact_requirements() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("retained.wav"), wav()).unwrap();
    let database = temp.path().join("server.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    assert_eq!(scan(&provider, &root, true).added, 1);
    let id = provider.search(&SearchRequest::default()).unwrap().results[0].id;
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                notes: Some("retained legacy annotation".into()),
                favorite: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    let conn = support::connection(&database).unwrap();
    conn.execute("UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.unknown_extension',json('{\"retained\":true}')) WHERE id=?1", [id]).unwrap();
    drop(conn);
    drop(provider);
    let cancel = AtomicBool::new(false);
    let migrated = migration9::execute(&database, Action::Start, &Options::default(), &cancel);
    assert_eq!(migrated.state, State::Done, "{migrated:?}");
    assert_format(&database, PLAN_B_V9_FORMAT, false);
    let provider =
        Arc::new(SqliteProvider::open_server_existing(&database, &cancel, |_| {}).unwrap());
    fs::write(root.join("new.bwf"), wav()).unwrap();
    copy_mp3(&root.join("renamed.wav"));
    let added = scan(&provider, &root, true);
    assert_eq!((added.added, added.errors), (2, 0));
    provider.rebuild_directory_search().unwrap();
    assert_format(&database, PLAN_B_V9_AUDIO_FORMAT, true);
    let snapshot = temp.path().join("owned-legacy.db");
    provider.backup_library(&snapshot).unwrap();
    let info = SqliteProvider::legacy_snapshot_info_cancellable(&database, &cancel).unwrap();
    assert_eq!(info.stats.audio_files, 1);
    let projected = SqliteProvider::derive_legacy_snapshot_cancellable(&snapshot, &cancel).unwrap();
    assert_eq!(projected.stats.audio_files, 1);
    assert_format(&snapshot, PLAN_B_FORMAT, false);
    let legacy = SqliteProvider::open_read_only(&snapshot).unwrap();
    let kept = legacy.get_audio(id).unwrap();
    assert_eq!(kept.notes, "retained legacy annotation");
    assert!(kept.favorite);
    assert_eq!(kept.attributes["unknown_extension"]["retained"], true);
    assert_eq!(
        legacy
            .search(&SearchRequest::default())
            .unwrap()
            .total_count,
        1
    );
    drop(legacy);
    drop(provider);
    let rolled_back =
        migration9::execute(&database, Action::Rollback, &Options::default(), &cancel);
    assert_eq!(rolled_back.state, State::RolledBack, "{rolled_back:?}");
    assert_format(&database, PLAN_B_AUDIO_FORMAT, true);
    let restored = SqliteProvider::open_existing(&database).unwrap();
    assert_eq!(restored.stats().unwrap().audio_files, 3);
    assert_eq!(
        restored.get_audio(id).unwrap().notes,
        "retained legacy annotation"
    );
    drop(restored);
    let repeated = migration9::execute(&database, Action::Start, &Options::default(), &cancel);
    assert_eq!(repeated.state, State::Done, "{repeated:?}");
    assert_format(&database, PLAN_B_V9_AUDIO_FORMAT, true);
    assert!(SqliteProvider::open_existing(&database).is_err());
}

#[test]
fn higher_source_feature_revision_is_not_repaired_or_opened_for_writes() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("library.db");
    drop(SqliteProvider::open(&database).unwrap());
    support::connection(&database)
        .unwrap()
        .execute(
            "INSERT INTO audiolib_schema_features(feature,version) VALUES('audio_sources',2)",
            [],
        )
        .unwrap();
    assert!(SqliteProvider::open_existing(&database).is_err());
    assert!(SqliteProvider::open_read_only(&database).is_err());
    let conn = support::connection(&database).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT version FROM audiolib_schema_features WHERE feature='audio_sources'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
}

#[test]
fn old_rf64_identity_refreshes_once_without_invalidating_ordinary_riff() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("ordinary.wav"), wav()).unwrap();
    fs::write(root.join("broadcast.wav"), bw64()).unwrap();
    let database = temp.path().join("library.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    let first = scan(&provider, &root, true);
    assert_eq!((first.added, first.errors), (2, 0));
    let broadcast = provider
        .search(&SearchRequest {
            query: "broadcast".into(),
            ..Default::default()
        })
        .unwrap()
        .results[0]
        .id;
    let conn = support::connection(&database).unwrap();
    conn.execute("UPDATE audio_files SET metadata_json=json_remove(json_set(audiolib_metadata_json(metadata_json),'$.container','RF64'),'$.wav_metadata.container_version') WHERE id=?1", [broadcast]).unwrap();
    let repaired = scan(&provider, &root, true);
    assert_eq!(
        (repaired.updated, repaired.unchanged, repaired.errors),
        (1, 1, 0)
    );
    let current = provider.get_audio(broadcast).unwrap();
    assert_eq!(current.attributes["container"], "BW64");
    assert_eq!(current.attributes["wav_metadata"]["container_version"], 1);
    let repeated = scan(&provider, &root, true);
    assert_eq!(
        (repeated.updated, repeated.unchanged, repeated.errors),
        (0, 2, 0)
    );
    // A numeric-looking string must not pass the version contract.
    conn.execute("UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.wav_metadata.container_version','1') WHERE id=?1", [broadcast]).unwrap();
    let malformed = scan(&provider, &root, true);
    assert_eq!(
        (malformed.updated, malformed.unchanged, malformed.errors),
        (1, 1, 0)
    );
    assert_format(&database, PLAN_B_FORMAT, false);
}

#[test]
fn legacy_projection_retains_wav_rows_with_non_integer_extension_markers() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let cases = [
        ("string.wav", serde_json::json!("future")),
        ("bool.wav", serde_json::json!(true)),
        ("float.wav", serde_json::json!(1.0)),
    ];
    for (name, _) in &cases {
        fs::write(root.join(name), wav()).unwrap();
    }
    let database = temp.path().join("server.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    assert_eq!(scan(&provider, &root, true).added, 3);
    let conn = support::connection(&database).unwrap();
    for (name, marker) in &cases {
        conn.execute("UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.wav_metadata.mp3_version',json(?1),'$.wav_metadata.aiff_version',json(?1),'$.unknown_extension',json('{\"retained\":true}')) WHERE filename=?2", rusqlite::params![marker.to_string(), name]).unwrap();
    }
    drop(conn);
    drop(provider);
    let cancel = AtomicBool::new(false);
    let migrated = migration9::execute(&database, Action::Start, &Options::default(), &cancel);
    assert_eq!(migrated.state, State::Done, "{migrated:?}");
    assert_format(&database, PLAN_B_V9_FORMAT, false);
    let provider = SqliteProvider::open_server_existing(&database, &cancel, |_| {}).unwrap();
    let snapshot = temp.path().join("owned-legacy.db");
    provider.backup_library(&snapshot).unwrap();
    assert_eq!(
        SqliteProvider::legacy_snapshot_info_cancellable(&database, &cancel)
            .unwrap()
            .stats
            .audio_files,
        3
    );
    assert_eq!(
        SqliteProvider::derive_legacy_snapshot_cancellable(&snapshot, &cancel)
            .unwrap()
            .stats
            .audio_files,
        3
    );
    assert_format(&snapshot, PLAN_B_FORMAT, false);
    let legacy = SqliteProvider::open_read_only(&snapshot).unwrap();
    let rows = legacy.search(&SearchRequest::default()).unwrap().results;
    assert_eq!(rows.len(), 3);
    for (name, marker) in &cases {
        let file = rows.iter().find(|file| &file.filename == name).unwrap();
        assert_eq!(&file.attributes["wav_metadata"]["mp3_version"], marker);
        assert_eq!(&file.attributes["wav_metadata"]["aiff_version"], marker);
        assert_eq!(file.attributes["unknown_extension"]["retained"], true);
    }
}

#[test]
fn af1_backup_restore_and_search_maintenance_preserve_sources_and_annotations() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let source = root.join("source.aiff");
    let control = root.join("control.wav");
    let source_bytes = aiff();
    let control_bytes = wav();
    fs::write(&source, &source_bytes).unwrap();
    fs::write(&control, &control_bytes).unwrap();
    let database = temp.path().join("library.db");
    let provider = Arc::new(SqliteProvider::open(&database).unwrap());
    let initial = scan(&provider, &root, true);
    assert_eq!((initial.added, initial.errors), (2, 0));
    assert_format(&database, PLAN_B_AUDIO_FORMAT, true);
    let id = provider.find_audio_by_path(&source).unwrap().unwrap().id;
    let control_id = provider.find_audio_by_path(&control).unwrap().unwrap().id;
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                tags: Some("restoredaf1annotation".into()),
                notes: Some("retained AF1 backup notes".into()),
                favorite: Some(true),
                color: Some("#123456".into()),
            },
        )
        .unwrap();
    let expected = provider.get_audio(id).unwrap();
    let query = SearchRequest {
        query: "restoredaf1annotation".into(),
        ..Default::default()
    };
    assert_eq!(provider.search(&query).unwrap().total_count, 1);
    let backup = temp.path().join("owned-af1-backup.db");
    let backed_up = provider.backup_library(&backup).unwrap();
    assert_eq!(backed_up.total_records, 2);
    assert!(backed_up.bytes > 0);
    assert_format(&backup, PLAN_B_AUDIO_FORMAT, true);

    // Public removal changes only the library; the source remains available.
    assert_eq!(provider.remove_audio(&[id]).unwrap(), 1);
    provider
        .update_metadata(
            control_id,
            &MetadataPatch {
                notes: Some("after AF1 backup".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(provider.get_audio(id).is_err());
    assert_eq!(provider.search(&query).unwrap().total_count, 0);
    assert_eq!(fs::read(&source).unwrap(), source_bytes);

    let restored = provider.restore_library(&backup).unwrap();
    assert!(restored.rollback_backup.is_file());
    assert_eq!(restored.library.stats.audio_files, 2);
    let rollback = SqliteProvider::open_read_only(&restored.rollback_backup).unwrap();
    assert!(rollback.get_audio(id).is_err());
    assert_eq!(
        rollback.get_audio(control_id).unwrap().notes,
        "after AF1 backup"
    );
    drop(rollback);
    assert_format(&database, PLAN_B_AUDIO_FORMAT, true);
    let current = provider.get_audio(id).unwrap();
    assert_eq!(current.id, expected.id);
    assert_eq!(current.file_path, expected.file_path);
    assert_eq!(current.sample_rate, expected.sample_rate);
    assert_eq!(current.bit_depth, expected.bit_depth);
    assert_eq!(current.channels, expected.channels);
    assert_eq!(current.duration, expected.duration);
    assert_eq!(current.attributes["container"], "AIFF");
    assert_eq!(
        current.attributes["wav_metadata"],
        expected.attributes["wav_metadata"]
    );
    assert_eq!(current.tags, expected.tags);
    assert_eq!(current.notes, expected.notes);
    assert_eq!(current.color, expected.color);
    assert!(current.favorite);
    assert_eq!(provider.search(&query).unwrap().total_count, 1);
    assert_eq!(provider.get_audio(control_id).unwrap().notes, "");

    let maintained = provider
        .maintain_library(audiolib_core::MaintenanceAction::RebuildSearch)
        .unwrap();
    assert!(maintained.rollback_backup.is_file());
    assert!(maintained.integrity.is_ok());
    assert_format(&database, PLAN_B_AUDIO_FORMAT, true);
    let maintained = provider.get_audio(id).unwrap();
    assert_eq!(
        maintained.attributes["wav_metadata"],
        expected.attributes["wav_metadata"]
    );
    assert_eq!(maintained.tags, expected.tags);
    assert_eq!(maintained.notes, expected.notes);
    assert_eq!(maintained.color, expected.color);
    assert!(maintained.favorite);
    assert_eq!(provider.search(&query).unwrap().total_count, 1);
    assert!(provider.check_library().unwrap().is_ok());
    drop(provider);
    let reopened = SqliteProvider::open_existing(&database).unwrap();
    assert_eq!(
        reopened.get_audio(id).unwrap().notes,
        "retained AF1 backup notes"
    );
    assert_eq!(fs::read(source).unwrap(), source_bytes);
    assert_eq!(fs::read(control).unwrap(), control_bytes);
}

#[test]
fn legacy_import_publishes_af1_for_existing_mp3_and_aif_records_without_changing_source() {
    for rowid_meta in [false, true] {
        legacy_import_af1_with_meta_layout(rowid_meta);
    }
}

fn legacy_import_af1_with_meta_layout(rowid_meta: bool) {
    let temp = tempfile::tempdir().unwrap();
    let audio = temp.path().join("owned-audio");
    fs::create_dir(&audio).unwrap();
    let mp3 = audio.join("legacy.mp3");
    let aif = audio.join("legacy.aif");
    copy_mp3(&mp3);
    fs::write(&aif, aiff()).unwrap();
    let mp3_bytes = fs::read(&mp3).unwrap();
    let aif_bytes = fs::read(&aif).unwrap();
    let source = temp.path().join("owned-legacy.db");
    let destination = temp.path().join("imported.db");
    let conn = support::connection(&source).unwrap();
    conn.execute_batch(include_str!("fixtures/schema-v5.sql"))
        .unwrap();
    if rowid_meta {
        conn.execute_batch("ALTER TABLE library_meta RENAME TO owned_source_meta;
            CREATE TABLE library_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            INSERT INTO library_meta(key,value) SELECT key,value FROM owned_source_meta ORDER BY key;
            DROP TABLE owned_source_meta;").unwrap();
    }
    let cases = [(42_i64, &mp3, "legacy.mp3"), (77_i64, &aif, "legacy.aif")];
    for (id, path, filename) in &cases {
        let metadata = serde_json::json!({
            "wav_metadata":{"fields":{"description":"legacyembeddedmetadata"}},
            "unknown_extension":{"retained":filename},
        });
        conn.execute(
            "INSERT INTO audio_files(id,file_path,path_key,filename,filename_tokenized,file_size,sample_rate,bit_depth,channels,duration,
             bwf_description,bwf_description_tokenized,
             user_tags,user_tags_tokenized,user_notes,user_notes_tokenized,user_color,favorite,metadata_json,created_at,modified_at)
             VALUES(?1,?2,?2,?3,audiolib_tokenize(?3),?4,48000,16,1,0.125,
             'legacyembeddedmetadata',audiolib_tokenize('legacyembeddedmetadata'),'legacyaf1tag',audiolib_tokenize('legacyaf1tag'),
             'retained legacy import notes',audiolib_tokenize('retained legacy import notes'),'#123456',1,?5,
             '2024-01-01 01:02:03','2024-01-02 02:03:04')",
            rusqlite::params![id, path.to_string_lossy().as_ref(), filename, fs::metadata(path).unwrap().len() as i64, metadata.to_string()],
        ).unwrap();
    }
    drop(conn);
    let source_bytes = fs::read(&source).unwrap();

    // The public importer must carry the owned staging copy through migration,
    // candidate verification and final publication with its actual af1 format.
    let imported = audiolib_db::import_legacy(&source, &destination).unwrap();
    assert!(imported.source_preserved);
    assert_eq!(imported.audio_files, 2);
    assert_eq!(imported.schema_version, 8);
    assert_format(&destination, PLAN_B_AUDIO_FORMAT, true);
    let provider = SqliteProvider::open_existing(&destination).unwrap();
    assert_eq!(provider.stats().unwrap().audio_files, 2);
    for (id, path, filename) in &cases {
        let file = provider.get_audio(*id).unwrap();
        assert_eq!(file.id, *id);
        assert_eq!(file.filename, *filename);
        assert_eq!(file.file_path, path.to_string_lossy().as_ref());
        assert_eq!(file.tags, "legacyaf1tag");
        assert_eq!(file.notes, "retained legacy import notes");
        assert_eq!(file.color.as_deref(), Some("#123456"));
        assert!(file.favorite);
        assert_eq!(
            file.attributes["wav_metadata"]["fields"]["description"],
            "legacyembeddedmetadata"
        );
        assert_eq!(file.attributes["unknown_extension"]["retained"], *filename);
    }
    assert_eq!(
        provider
            .search(&SearchRequest {
                query: "legacyaf1tag".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        2
    );
    assert!(provider.check_library().unwrap().is_ok());
    assert_eq!(fs::read(source).unwrap(), source_bytes);
    assert_eq!(fs::read(mp3).unwrap(), mp3_bytes);
    assert_eq!(fs::read(aif).unwrap(), aif_bytes);
}
