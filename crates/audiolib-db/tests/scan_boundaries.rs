// crates/audiolib-db/tests/scan_boundaries.rs
use audiolib_core::{DataProvider, ScanState, SearchRequest};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use rusqlite::{Connection, params};
use std::{fs, path::Path, sync::Arc};

fn wav(path: &Path) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&38u32.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&48000u32.to_le_bytes());
    bytes.extend_from_slice(&96000u32.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&0i16.to_le_bytes());
    fs::write(path, bytes).unwrap();
}
fn count(provider: &SqliteProvider, query: &str) -> u64 {
    provider
        .search(&SearchRequest {
            query: query.into(),
            ..Default::default()
        })
        .unwrap()
        .total_count
}

#[test]
fn nonrecursive_reconciliation_keeps_descendants_and_unsupported_records() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir_all(root.join("child")).unwrap();
    wav(&root.join("direct.wav"));
    wav(&root.join("child").join("nested.wav"));
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("db.sqlite")).unwrap());
    let service = ScanService::new(provider.clone());
    service
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    let conn = Connection::open(provider.database_path()).unwrap();
    audiolib_db::register_library_functions(&conn).unwrap();
    conn.execute(
        "INSERT INTO audio_files(file_path,filename) VALUES(?1,'legacy.opus')",
        params![root.join("legacy.opus").to_string_lossy()],
    )
    .unwrap();
    fs::remove_file(root.join("direct.wav")).unwrap();
    fs::remove_file(root.join("child").join("nested.wav")).unwrap();
    let result = service
        .scan(
            &root,
            &ScanOptions {
                incremental: false,
                recursive: false,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(result.state, ScanState::Completed);
    assert_eq!(result.errors, 0);
    assert_eq!(count(&provider, "direct"), 0);
    assert_eq!(count(&provider, "nested"), 1);
    assert_eq!(count(&provider, "legacy"), 1);
    service
        .scan(
            &root,
            &ScanOptions {
                incremental: false,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(count(&provider, "nested"), 0);
    assert_eq!(count(&provider, "legacy"), 1);
}

#[test]
fn pre_cancelled_full_scan_does_not_reconcile_and_allowed_roots_are_checked() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    let allowed = temp.path().join("allowed");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&allowed).unwrap();
    wav(&root.join("keep.wav"));
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("db.sqlite")).unwrap());
    let service = ScanService::new(provider.clone());
    service
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    fs::remove_file(root.join("keep.wav")).unwrap();
    let cancellation = ScanCancellation::default();
    cancellation.cancel();
    let progress = service
        .scan(
            &root,
            &ScanOptions {
                incremental: false,
                ..Default::default()
            },
            &cancellation,
            |_| {},
        )
        .unwrap();
    assert_eq!(progress.state, ScanState::Cancelled);
    assert_eq!(progress.processed, 0);
    assert_eq!(count(&provider, "keep"), 1);
    assert!(
        service
            .scan(
                &root,
                &ScanOptions {
                    allowed_roots: vec![allowed],
                    ..Default::default()
                },
                &ScanCancellation::default(),
                |_| {}
            )
            .is_err()
    );
    assert_eq!(count(&provider, "keep"), 1);
}

#[test]
fn skipped_symlink_directory_disables_missing_reconciliation() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    let outside = temp.path().join("outside");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&outside).unwrap();
    wav(&root.join("keep.wav"));
    wav(&outside.join("outside.wav"));
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("db.sqlite")).unwrap());
    let service = ScanService::new(provider.clone());
    service
        .scan(
            &root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    fs::remove_file(root.join("keep.wav")).unwrap();
    #[cfg(windows)]
    let link = std::os::windows::fs::symlink_dir(&outside, root.join("linked"));
    #[cfg(unix)]
    let link = std::os::unix::fs::symlink(&outside, root.join("linked"));
    if let Err(error) = link {
        eprintln!("Symlink fixture unavailable on this host: {error}");
        return;
    }
    let progress = service
        .scan(
            &root,
            &ScanOptions {
                incremental: false,
                ..Default::default()
            },
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert!(progress.errors > 0);
    assert_eq!(count(&provider, "keep"), 1);
    assert_eq!(count(&provider, "outside"), 0);
}

#[test]
fn full_scan_of_an_empty_root_directory_never_marks_files_missing() {
    // An unmounted network volume leaves an empty mount-point directory behind.
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("mount");
    fs::create_dir_all(root.join("Category")).unwrap();
    wav(&root.join("Category").join("boom.wav"));
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("db.sqlite")).unwrap());
    let service = ScanService::new(provider.clone());
    let full = ScanOptions {
        incremental: false,
        ..Default::default()
    };
    service
        .scan(&root, &full, &ScanCancellation::default(), |_| {})
        .unwrap();
    fs::remove_dir_all(root.join("Category")).unwrap();
    // Finder leaves hidden files in an unmounted mount-point directory too.
    fs::write(root.join(".DS_Store"), b"x").unwrap();
    fs::write(root.join("._Category"), b"x").unwrap();
    // So do NAS and OS housekeeping folders of a share.
    fs::create_dir_all(root.join("@eaDir")).unwrap();
    fs::create_dir_all(root.join("#recycle")).unwrap();
    fs::write(root.join("Thumbs.db"), b"x").unwrap();
    let result = service
        .scan(&root, &full, &ScanCancellation::default(), |_| {})
        .unwrap();
    assert_eq!(result.state, ScanState::Completed);
    assert_eq!(result.removed, 0);
    assert_eq!(count(&provider, "boom"), 1, "still visible, not missing");
    // A missing root fails the scan instead of reconciling anything.
    fs::remove_dir_all(&root).unwrap();
    assert!(
        service
            .scan(&root, &full, &ScanCancellation::default(), |_| {})
            .is_err()
    );
    assert_eq!(count(&provider, "boom"), 1);
}
