// crates/audiolib-db/tests/scan_management.rs
#![cfg(feature = "scanner")]

use audiolib_core::{DataProvider, MetadataPatch, ScanProgress, ScanState, SearchRequest};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use rusqlite::Connection;
use std::{collections::BTreeSet, fs, path::Path, sync::Arc};

/// Write a tiny silent WAV in the test's temporary directory; never open an output device.
fn wav(path: &Path, extensible: bool) {
    let bits = if extensible { 32_u16 } else { 16 };
    let block_align = bits / 8;
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&(if extensible { 0xfffe_u16 } else { 1 }).to_le_bytes());
    fmt.extend_from_slice(&1_u16.to_le_bytes());
    fmt.extend_from_slice(&48_000_u32.to_le_bytes());
    fmt.extend_from_slice(&(48_000_u32 * u32::from(block_align)).to_le_bytes());
    fmt.extend_from_slice(&block_align.to_le_bytes());
    fmt.extend_from_slice(&bits.to_le_bytes());
    if extensible {
        fmt.extend_from_slice(&22_u16.to_le_bytes());
        fmt.extend_from_slice(&24_u16.to_le_bytes());
        fmt.extend_from_slice(&4_u32.to_le_bytes());
        fmt.extend_from_slice(&[
            1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
        ]);
    }
    let data = vec![0; usize::from(block_align) * 4];
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&(20 + fmt.len() as u32 + data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&fmt);
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&data);
    fs::write(path, bytes).unwrap();
}

fn full_scan(service: &ScanService, root: &Path, recursive: bool) -> ScanProgress {
    service
        .scan(
            root,
            &ScanOptions {
                incremental: false,
                recursive,
                batch_size: 2,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap()
}

fn persisted_issues(conn: &Connection, progress: &ScanProgress) {
    let (errors, json, truncated): (i64, String, i64) = conn
        .query_row(
            "SELECT errors,error_details,errors_truncated FROM scan_sessions WHERE id=?1",
            [progress.session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(u64::try_from(errors).unwrap(), progress.errors);
    assert_eq!(u64::try_from(truncated).unwrap(), progress.errors_truncated);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json).unwrap(),
        serde_json::to_value(&progress.error_details).unwrap()
    );
}

#[test]
fn unicode_nonrecursive_cleanup_counts_only_newly_missing_files() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("ÉCHO");
    fs::create_dir_all(root.join("child")).unwrap();
    let direct = root.join("direct.wav");
    wav(&direct, false);
    wav(&root.join("child").join("nested.wav"), false);
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("db.sqlite")).unwrap());
    let service = ScanService::new(provider.clone());
    assert_eq!(full_scan(&service, &root, true).added, 2);
    let conn = Connection::open(provider.database_path()).unwrap();
    let id: i64 = conn
        .query_row(
            "SELECT id FROM audio_files WHERE filename='direct.wav'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    provider
        .update_metadata(
            id,
            &MetadataPatch {
                tags: Some("keep-tag".into()),
                ..Default::default()
            },
        )
        .unwrap();
    fs::remove_file(&direct).unwrap();
    fs::remove_file(root.join("child").join("nested.wav")).unwrap();
    let removed = full_scan(&service, &root, false);
    assert_eq!(removed.state, ScanState::Completed);
    assert_eq!((removed.errors, removed.removed), (0, 1));
    assert_eq!(
        conn.query_row(
            "SELECT files_removed FROM scan_sessions WHERE id=?1",
            [removed.session_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        1
    );
    assert_eq!(
        provider
            .search(&SearchRequest::default())
            .unwrap()
            .total_count,
        1
    );
    assert!(
        conn.query_row("SELECT missing FROM audio_files WHERE id=?1", [id], |row| {
            row.get::<_, bool>(0)
        })
        .unwrap()
    );
    let repeated = full_scan(&service, &root, false);
    assert_eq!(repeated.removed, 0);
    assert_eq!(
        conn.query_row(
            "SELECT files_removed FROM scan_sessions WHERE id=?1",
            [repeated.session_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap(),
        0
    );
    wav(&direct, false);
    let restored = full_scan(&service, &root, false);
    assert_eq!(restored.removed, 0);
    assert_eq!(provider.get_audio(id).unwrap().tags, "keep-tag");
    assert_eq!(
        provider
            .search(&SearchRequest::default())
            .unwrap()
            .total_count,
        2
    );
}

#[test]
fn two_damaged_wavs_retain_distinct_paths_and_failure_reasons() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("wrong-header.wav"), b"not a WAV payload").unwrap();
    let mut truncated = b"RIFF".to_vec();
    truncated.extend_from_slice(&36_u32.to_le_bytes());
    truncated.extend_from_slice(b"WAVEfmt ");
    truncated.extend_from_slice(&16_u32.to_le_bytes());
    fs::write(root.join("truncated-chunk.wav"), truncated).unwrap();
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("db.sqlite")).unwrap());
    let progress = full_scan(&ScanService::new(provider.clone()), &root, true);
    assert_eq!(progress.state, ScanState::Completed);
    assert_eq!(
        (progress.discovered, progress.processed, progress.errors),
        (2, 2, 2)
    );
    assert_eq!(progress.errors_truncated, 0);
    assert_eq!(progress.error_details.len(), 2);
    let filenames: BTreeSet<_> = progress
        .error_details
        .iter()
        .map(|issue| {
            Path::new(&issue.path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        filenames,
        BTreeSet::from(["wrong-header.wav".into(), "truncated-chunk.wav".into()])
    );
    let messages: BTreeSet<_> = progress
        .error_details
        .iter()
        .map(|issue| &issue.message)
        .collect();
    assert_eq!(messages.len(), 2);
    assert!(messages.iter().all(|message| !message.is_empty()));
    assert!(
        progress
            .error_details
            .iter()
            .all(|issue| Path::new(&issue.path).is_absolute())
    );
    assert_eq!(
        progress.message,
        progress.error_details.last().unwrap().message
    );
    persisted_issues(
        &Connection::open(provider.database_path()).unwrap(),
        &progress,
    );
}

#[test]
fn scan_error_details_are_bounded_without_losing_counts_or_reconciling_missing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let keep = root.join("keep.wav");
    wav(&keep, false);
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("db.sqlite")).unwrap());
    let service = ScanService::new(provider.clone());
    full_scan(&service, &root, true);
    fs::remove_file(&keep).unwrap();
    for index in 0..107 {
        fs::write(
            root.join(format!("broken-{index:03}.wav")),
            b"not a WAV payload",
        )
        .unwrap();
    }
    let progress = full_scan(&service, &root, true);
    assert_eq!(progress.state, ScanState::Completed);
    assert_eq!(
        (progress.discovered, progress.processed, progress.errors),
        (107, 107, 107)
    );
    assert_eq!(
        (progress.error_details.len(), progress.errors_truncated),
        (100, 7)
    );
    assert_eq!(progress.removed, 0);
    let paths: BTreeSet<_> = progress
        .error_details
        .iter()
        .map(|issue| &issue.path)
        .collect();
    assert_eq!(paths.len(), 100);
    assert!(
        progress
            .error_details
            .iter()
            .all(|issue| !issue.message.is_empty())
    );
    // A partially successful full scan must leave previously indexed records visible.
    assert_eq!(
        provider
            .search(&SearchRequest::default())
            .unwrap()
            .total_count,
        1
    );
    persisted_issues(
        &Connection::open(provider.database_path()).unwrap(),
        &progress,
    );
}

#[test]
fn extensible_valid_bits_are_removed_after_pcm_replacement_preserving_user_data() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    let file = root.join("one.wav");
    wav(&file, true);
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("db.sqlite")).unwrap());
    let service = ScanService::new(provider.clone());
    full_scan(&service, &root, true);
    let audio = provider
        .search(&SearchRequest::default())
        .unwrap()
        .results
        .remove(0);
    assert_eq!(audio.bit_depth, 32);
    assert_eq!(audio.attributes["valid_bits_per_sample"], 24);
    provider
        .update_metadata(
            audio.id,
            &MetadataPatch {
                tags: Some("user tag".into()),
                notes: Some("private notes".into()),
                color: Some("#123456".into()),
                favorite: Some(true),
            },
        )
        .unwrap();

    let conn = Connection::open(provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute(
        "UPDATE audio_files SET metadata_json=json_set(audiolib_metadata_json(metadata_json),'$.plugin_private','keep') WHERE id=?1",
        [audio.id],
    )
    .unwrap();
    wav(&file, false);
    let refreshed = full_scan(&service, &root, true);
    assert_eq!(
        (refreshed.errors, refreshed.added, refreshed.updated),
        (0, 0, 1)
    );
    let current = provider.get_audio(audio.id).unwrap();
    assert_eq!(current.bit_depth, 16);
    assert!(current.attributes.get("valid_bits_per_sample").is_none());
    assert!(current.attributes.get("channel_mask").is_none());
    assert_eq!(current.attributes["plugin_private"], "keep");
    assert_eq!(
        (
            current.tags.as_str(),
            current.notes.as_str(),
            current.color.as_deref(),
            current.favorite
        ),
        ("user tag", "private notes", Some("#123456"), true)
    );
}
