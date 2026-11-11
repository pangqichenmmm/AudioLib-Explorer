// crates/audiolib-db/tests/scan_unchanged_rows.rs
//! Incremental scans record unchanged files in a TEMP table instead of rewriting their
//! rows (5347a49): unchanged rows stay byte-identical, vanished files are still marked
//! missing, a returning file is un-missed, new and modified files are handled normally.
use audiolib_core::DataProvider;
use audiolib_core::{FileVisibility, MetadataPatch, ScanState, SearchFilters, SearchRequest};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider, import_legacy};
use rusqlite::Connection;
use std::{fs, path::Path, sync::Arc};

fn write_wav(path: &Path, frames: u32) {
    let bytes = frames * 2;
    let mut data = Vec::new();
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(36 + bytes).to_le_bytes());
    data.extend_from_slice(b"WAVEfmt ");
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&48000u32.to_le_bytes());
    data.extend_from_slice(&96000u32.to_le_bytes());
    data.extend_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&16u16.to_le_bytes());
    data.extend_from_slice(b"data");
    data.extend_from_slice(&bytes.to_le_bytes());
    data.resize(data.len() + bytes as usize, 0);
    fs::write(path, data).unwrap();
}

/// `(filename, missing, seen_scan_id, file_size)` of every row, by filename.
fn rows(path: &Path) -> Vec<(String, i64, Option<i64>, i64)> {
    let conn = Connection::open(path).unwrap();
    let mut statement = conn
        .prepare(
            "SELECT filename,missing,seen_scan_id,file_size FROM audio_files ORDER BY filename",
        )
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn unchanged_files_are_not_rewritten_but_missing_new_and_modified_still_work() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir_all(root.join("sub")).unwrap();
    for (name, frames) in [("a.wav", 480), ("b.wav", 960), ("c.wav", 1440)] {
        write_wav(&root.join(name), frames);
    }
    write_wav(&root.join("sub").join("d.wav"), 240);
    let provider = Arc::new(SqliteProvider::open(&temp.path().join("lib.db")).unwrap());
    let db = provider.database_path().to_path_buf();
    let scanner = ScanService::new(provider.clone());
    let scan = |incremental: bool| {
        scanner
            .scan(
                &root,
                &ScanOptions {
                    incremental,
                    ..Default::default()
                },
                &ScanCancellation::default(),
                |_| {},
            )
            .unwrap()
    };

    let first = scan(true);
    assert_eq!((first.added, first.unchanged, first.removed), (4, 0, 0));
    let before = rows(&db);
    assert_eq!(before.len(), 4);

    // 1. Nothing changed: all unchanged, rows untouched (no new seen_scan_id), none missing.
    let same = scan(true);
    assert_eq!(
        (same.added, same.updated, same.unchanged, same.removed),
        (0, 0, 4, 0)
    );
    assert_eq!(rows(&db), before, "unchanged rows must not be rewritten");

    // 2. A file disappears. Only a full scan reconciles missing files; an incremental one
    // leaves everything alone (and still rewrites nothing).
    fs::remove_file(root.join("b.wav")).unwrap();
    let incremental = scan(true);
    assert_eq!(
        (incremental.unchanged, incremental.removed),
        (3, 0),
        "incremental scans never mark files missing"
    );
    assert_eq!(rows(&db), before);
    let gone = scan(false);
    assert_eq!(gone.state, ScanState::Completed);
    assert_eq!((gone.added, gone.removed), (0, 1));
    let after = rows(&db);
    let b = after.iter().find(|row| row.0 == "b.wav").unwrap();
    assert_eq!(b.1, 1, "vanished file must be marked missing");
    assert!(
        after
            .iter()
            .filter(|row| row.0 != "b.wav")
            .all(|row| row.1 == 0)
    );
    // It is not counted as newly removed again, stays missing, and is hidden from search.
    let again = scan(false);
    assert_eq!(again.removed, 0);
    let incremental = scan(true);
    assert_eq!((incremental.unchanged, incremental.removed), (3, 0));
    assert_eq!(rows(&db).iter().find(|row| row.0 == "b.wav").unwrap().1, 1);
    assert_eq!(
        provider
            .search(&SearchRequest::default())
            .unwrap()
            .total_count,
        3,
        "missing files are hidden from search"
    );

    // 3. The file returns: visible again, without counting as new or changed.
    write_wav(&root.join("b.wav"), 960);
    let back = scan(true);
    assert_eq!(back.removed, 0);
    assert_eq!(back.added + back.updated + back.unchanged, 4);
    assert!(rows(&db).iter().all(|row| row.1 == 0));
    assert_eq!(
        provider
            .search(&SearchRequest::default())
            .unwrap()
            .total_count,
        4
    );

    // 4. New and modified files in one scan; a following full scan agrees.
    write_wav(&root.join("sub").join("e.wav"), 100);
    write_wav(&root.join("a.wav"), 4800);
    let mixed = scan(true);
    assert_eq!((mixed.added, mixed.updated, mixed.removed), (1, 1, 0));
    assert_eq!(mixed.unchanged, 3);
    let full = scan(false);
    assert_eq!(full.removed, 0);
    assert_eq!(rows(&db).len(), 5);
    assert!(rows(&db).iter().all(|row| row.1 == 0));

    // 5. Vanishing in a subfolder works on a later full scan too (the temp table of the
    // older sessions was cleared, nothing stale protects the file).
    fs::remove_file(root.join("sub").join("d.wav")).unwrap();
    let sub = scan(false);
    assert_eq!(sub.removed, 1);
    let missing: Vec<_> = rows(&db).into_iter().filter(|row| row.1 == 1).collect();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].0, "d.wav");
    provider.check_integrity().unwrap();
}

#[test]
fn unchanged_imported_non_present_flags_reappear_without_updating_present_rows() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("audio");
    fs::create_dir(&root).unwrap();
    for name in ["present.wav", "null.wav", "positive.wav", "negative.wav"] {
        write_wav(&root.join(name), 480);
    }
    // Legacy columns remain nullable after a lossless import.
    let legacy = temp.path().join("legacy.db");
    let conn = Connection::open(&legacy).unwrap();
    conn.execute_batch(
        "CREATE TABLE audio_files(id INTEGER PRIMARY KEY,file_path TEXT NOT NULL UNIQUE,
         filename TEXT NOT NULL,missing INTEGER);
         CREATE TABLE folders(id INTEGER PRIMARY KEY,folder_path TEXT NOT NULL UNIQUE,
         folder_name TEXT NOT NULL,parent_id INTEGER);",
    )
    .unwrap();
    drop(conn);
    let database = temp.path().join("library.db");
    import_legacy(&legacy, &database).unwrap();
    let provider = Arc::new(SqliteProvider::open_existing(&database).unwrap());
    let scanner = ScanService::new(provider.clone());
    let scan = || {
        scanner
            .scan(
                &root,
                &ScanOptions::default(),
                &ScanCancellation::default(),
                |_| {},
            )
            .unwrap()
    };
    assert_eq!(scan().added, 4);
    let all = SearchRequest {
        filters: SearchFilters {
            visibility: FileVisibility::All,
            ..Default::default()
        },
        ..Default::default()
    };
    for file in provider.search(&all).unwrap().results {
        provider
            .update_metadata(
                file.id,
                &MetadataPatch {
                    tags: Some(format!("retain-{}", file.id)),
                    notes: Some("private notes".into()),
                    favorite: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let conn = Connection::open(&database).unwrap();
    conn.execute_batch(
        "UPDATE audio_files SET missing=CASE filename WHEN 'null.wav' THEN NULL
         WHEN 'positive.wav' THEN 2 WHEN 'negative.wav' THEN -1 ELSE 0 END;
         CREATE TABLE test_updated_ids(id INTEGER NOT NULL);
         CREATE TRIGGER test_unchanged_updates AFTER UPDATE ON audio_files BEGIN
          INSERT INTO test_updated_ids VALUES(new.id);
         END;",
    )
    .unwrap();
    let before = provider.search(&all).unwrap().results;
    let present_id = before
        .iter()
        .find(|file| file.filename == "present.wav")
        .unwrap()
        .id;
    let progress = scan();
    assert_eq!(
        (
            progress.unchanged,
            progress.updated,
            progress.added,
            progress.errors
        ),
        (4, 0, 0, 0)
    );
    let after = provider.search(&SearchRequest::default()).unwrap().results;
    assert_eq!(after.len(), 4);
    let representative = after.iter().map(|file| file.id).min().unwrap();
    for mut expected in before {
        expected.missing = false;
        // Shared representatives are derived from all present members. Returning
        // lower-ID members changes this DTO field without rewriting present rows.
        let album = expected.album_entity.as_mut().unwrap();
        assert_eq!(album.representative_audio_id, Some(present_id));
        album.representative_audio_id = Some(representative);
        let actual = after.iter().find(|file| file.id == expected.id).unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
    let changed: Vec<i64> = conn
        .prepare("SELECT id FROM test_updated_ids ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(changed.len(), 3);
    assert!(
        !changed.contains(&present_id),
        "present unchanged row must not receive any UPDATE"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM audio_files WHERE missing=0",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        4
    );
}
