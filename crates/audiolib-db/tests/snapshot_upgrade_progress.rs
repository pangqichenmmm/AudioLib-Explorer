// crates/audiolib-db/tests/snapshot_upgrade_progress.rs
//! Owned atomic upgrades and resumable live upgrades preserve records across cancellation.
mod support;
use audiolib_core::{DataProvider, Error, SearchRequest};
use audiolib_db::{
    SnapshotUpgradeProgress, SqliteProvider,
    migration::{self, MigrationPhase},
};
use rusqlite::{Connection, params};
use std::{
    fs,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

#[test]
fn cancelled_snapshot_does_not_publish_or_overwrite_its_destination() {
    let dir = tempfile::tempdir().unwrap();
    let provider = SqliteProvider::open(&dir.path().join("library.db")).unwrap();
    let destination = dir.path().join("backup.db");
    fs::write(&destination, b"retain existing backup").unwrap();
    let result = provider.backup_library_cancellable(&destination, &AtomicBool::new(true));
    assert!(matches!(result, Err(Error::Cancelled)));
    assert_eq!(fs::read(&destination).unwrap(), b"retain existing backup");
    assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("audiolib-backup-")
    }));
}

fn legacy_fixture(path: &Path, records: usize) {
    let mut conn = support::connection(path).unwrap();
    conn.execute_batch(include_str!("fixtures/schema-v5.sql"))
        .unwrap();
    let table: String = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='audio_search'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let triggers: Vec<String> = conn
        .prepare(
            "SELECT sql FROM sqlite_schema WHERE type='trigger'
             AND name IN ('audio_files_ai','audio_files_au','audio_files_ad') ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let tx = conn.transaction().unwrap();
    tx.execute_batch(
        "DROP TRIGGER audio_files_ai; DROP TRIGGER audio_files_au;
         DROP TRIGGER audio_files_ad; DROP TABLE audio_search;",
    )
    .unwrap();
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO audio_files(id,file_path,filename,filename_tokenized,
                 user_tags,user_tags_tokenized,user_notes,user_notes_tokenized,metadata_json)
                 VALUES(?1,?2,?3,?3,'retain tags','retain tags','retain notes','retain notes',?4)",
            )
            .unwrap();
        for index in 0..records {
            let id = if index == 0 {
                i64::MIN
            } else if index + 1 == records {
                i64::MAX
            } else {
                index as i64 - (records / 2) as i64
            };
            let filename = format!("explosion_{index}.wav");
            insert
                .execute(params![
                    id,
                    format!("N:/synthetic/{filename}"),
                    filename,
                    r#"{"private":"keep","wav_metadata":{"fields":{"keywords":"Outdoor"}}}"#,
                ])
                .unwrap();
        }
    }
    if records > 0 {
        tx.execute(
            "INSERT INTO play_history(audio_file_id,played_at,filename,file_path)
             VALUES(?1,'2026-10-01T00:00:00Z','retained history','retained path')",
            [i64::MIN],
        )
        .unwrap();
    }
    tx.execute_batch(&table.replace(
        "tokenize='unicode61 remove_diacritics 2'",
        "tokenize='porter unicode61 remove_diacritics 2'",
    ))
    .unwrap();
    for trigger in triggers {
        tx.execute_batch(&trigger).unwrap();
    }
    tx.execute_batch(
        "INSERT INTO audio_search(audio_search) VALUES('rebuild');
         UPDATE audiolib_schema_features SET version=1 WHERE feature='tokenization';",
    )
    .unwrap();
    tx.commit().unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
}

fn assert_no_sidecars(path: &Path) {
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        assert!(!Path::new(&sidecar).exists(), "{sidecar:?}");
    }
}

fn phase(name: &str) -> MigrationPhase {
    match name {
        "copying" => MigrationPhase::Copying,
        "indexing" => MigrationPhase::Indexing,
        "validating" => MigrationPhase::Validating,
        "checkpoint" => MigrationPhase::Checkpoint,
        _ => panic!("unknown test phase"),
    }
}

fn assert_original_rows(path: &Path, records: i64) {
    let conn = support::connection(path).unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM audio_files", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, records);
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM audio_files WHERE user_notes!='retain notes' OR user_tags!='retain tags' OR json_extract(audiolib_metadata_json(metadata_json),'$.private')!='keep'", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM play_history WHERE filename='retained history' AND file_path='retained path'", [], |r| r.get::<_, i64>(0)).unwrap(), i64::from(records > 0));
    assert_eq!(
        conn.query_row(
            "SELECT version FROM audiolib_schema_features WHERE feature='tokenization'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn progress_counts_every_signed_id_and_reuses_the_completed_upgrade() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("old.db");
    legacy_fixture(&database, 10_003);
    assert!(SqliteProvider::check_owned_file(&database).unwrap().is_ok());
    let cancel = AtomicBool::new(false);
    let mut events = Vec::new();
    assert!(
        SqliteProvider::upgrade_owned_snapshot_with_progress_cancellable(
            &database,
            &cancel,
            |event| events.push(event),
        )
        .unwrap()
    );
    assert_eq!(events.first(), Some(&SnapshotUpgradeProgress::BackingUp));
    let mut counts: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SnapshotUpgradeProgress::Copying { completed, total } => Some((*completed, *total)),
            _ => None,
        })
        .collect();
    counts.dedup();
    assert_eq!(
        counts,
        [(5_000, 10_003), (10_000, 10_003), (10_003, 10_003)]
    );
    let index_counts: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SnapshotUpgradeProgress::Indexing { completed, total } => Some((*completed, *total)),
            _ => None,
        })
        .collect();
    assert_eq!(
        index_counts,
        [(5000, 10003), (10000, 10003), (10003, 10003)]
    );
    assert!(events.contains(&SnapshotUpgradeProgress::Publishing));
    assert!(events.contains(&SnapshotUpgradeProgress::Checkpoint));
    assert!(SqliteProvider::has_current_search_state(&database).unwrap());
    assert!(SqliteProvider::check_owned_file(&database).unwrap().is_ok());
    assert_no_sidecars(&database);
    let provider = SqliteProvider::open_read_only(&database).unwrap();
    assert_eq!(
        provider
            .search(&SearchRequest {
                query: "plos".into(),
                ..Default::default()
            })
            .unwrap()
            .total_count,
        10_003
    );
    for id in [i64::MIN, -1, 0, i64::MAX] {
        let audio = provider.get_audio(id).unwrap();
        assert_eq!(audio.tags, "retain tags");
        assert_eq!(audio.notes, "retain notes");
        assert_eq!(audio.attributes["private"], "keep");
    }
    drop(provider);
    let original = fs::read(&database).unwrap();
    events.clear();
    assert!(
        !SqliteProvider::upgrade_owned_snapshot_with_progress_cancellable(
            &database,
            &cancel,
            |event| events.push(event),
        )
        .unwrap()
    );
    assert!(events.is_empty());
    assert_eq!(fs::read(&database).unwrap(), original);
}

#[test]
fn cancelling_after_committed_copy_or_index_preserves_original_records_and_allows_rollback() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("old.db");
    legacy_fixture(&source, 5_003);
    let original = fs::read(&source).unwrap();
    for stage in ["batch", "fts"] {
        let database = temp.path().join(format!("cancel-{stage}.db"));
        fs::copy(&source, &database).unwrap();
        let cancel = AtomicBool::new(false);
        let mut reached = false;
        let result = SqliteProvider::upgrade_owned_snapshot_with_progress_cancellable(
            &database,
            &cancel,
            |event| {
                let stop = match event {
                    SnapshotUpgradeProgress::Copying { completed, .. } => {
                        stage == "batch" && completed == 5_000
                    }
                    SnapshotUpgradeProgress::Indexing {
                        completed: 5000, ..
                    } => stage == "fts",
                    _ => false,
                };
                if stop {
                    reached = true;
                    cancel.store(true, Ordering::Release);
                }
            },
        );
        assert!(reached, "{stage}");
        assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
        let job = migration::discover(&database).unwrap().unwrap();
        assert_eq!(
            job.phase,
            if stage == "batch" {
                MigrationPhase::Copying
            } else {
                MigrationPhase::Indexing
            }
        );
        assert_original_rows(&database, 5003);
        assert_original_rows(&job.directory.join("backup.db"), 5003);
        cancel.store(false, Ordering::Release);
        let restored = migration::rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
        assert_eq!(restored.phase, MigrationPhase::RolledBack);
        assert_original_rows(&database, 5003);
        assert!(SqliteProvider::check_owned_file(&database).unwrap().is_ok());
        assert_no_sidecars(&database);
    }
    assert_eq!(fs::read(&source).unwrap(), original);
}

#[test]
fn empty_snapshot_reports_zero_rows_and_finishes_the_fts_upgrade() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("empty.db");
    legacy_fixture(&database, 0);
    let mut events = Vec::new();
    assert!(
        SqliteProvider::upgrade_owned_snapshot_with_progress_cancellable(
            &database,
            &AtomicBool::new(false),
            |event| events.push(event),
        )
        .unwrap()
    );
    assert!(events.contains(&SnapshotUpgradeProgress::Copying {
        completed: 0,
        total: 0
    }));
    assert!(!events.iter().any(
        |e| matches!(e, SnapshotUpgradeProgress::Indexing { completed, .. } if *completed > 0)
    ));
    assert!(events.contains(&SnapshotUpgradeProgress::Validating));
    assert!(events.contains(&SnapshotUpgradeProgress::Publishing));
    assert_no_sidecars(&database);
    assert!(SqliteProvider::has_current_search_state(&database).unwrap());
}

#[test]
fn live_upgrade_resumes_each_committed_phase_without_repeating_rows() {
    for stop in ["copying", "indexing", "validating", "checkpoint"] {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("live.db");
        legacy_fixture(&database, 5_003);
        let cancel = AtomicBool::new(false);
        let result =
            SqliteProvider::open_existing_with_progress_cancellable(&database, &cancel, |event| {
                let reached = match event {
                    SnapshotUpgradeProgress::Copying {
                        completed: 5000, ..
                    } => stop == "copying",
                    SnapshotUpgradeProgress::Indexing {
                        completed: 5000, ..
                    } => stop == "indexing",
                    SnapshotUpgradeProgress::Validating => stop == "validating",
                    SnapshotUpgradeProgress::Checkpoint => stop == "checkpoint",
                    _ => false,
                };
                if reached {
                    cancel.store(true, Ordering::Release);
                }
            });
        assert!(matches!(result, Err(Error::Cancelled)), "{stop}");
        if stop == "checkpoint" {
            // Publication is already atomic and complete; only WAL truncation remains.
            let reader = SqliteProvider::open_read_only(&database).unwrap();
            assert_eq!(reader.stats().unwrap().audio_files, 5003);
            assert_eq!(
                reader.library_info().unwrap().schema_version,
                audiolib_db::SCHEMA_VERSION
            );
        } else {
            assert!(
                matches!(
                    SqliteProvider::open_read_only(&database),
                    Err(Error::Busy(_))
                ),
                "{stop}"
            );
        }
        let job = migration::discover(&database).unwrap().unwrap();
        assert_eq!(job.phase, phase(stop));
        let backup = job.directory.join("backup.db");
        assert_original_rows(&backup, 5003);
        if stop == "copying" || stop == "indexing" {
            let candidate = support::connection(job.directory.join("candidate.db")).unwrap();
            let key = if stop == "copying" { "audio_files" } else { "" };
            let completed: i64 = candidate
                .query_row(
                    "SELECT completed FROM audiolib_migration_copy WHERE name=?1",
                    [key],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(completed, 5000);
        }
        let mut events = Vec::new();
        let provider =
            SqliteProvider::open_existing_with_progress(&database, |event| events.push(event))
                .unwrap();
        assert!(!events.contains(&SnapshotUpgradeProgress::BackingUp));
        if stop == "copying" {
            assert_eq!(
                events.iter().find_map(|e| match e {
                    SnapshotUpgradeProgress::Copying { completed, .. } => Some(*completed),
                    _ => None,
                }),
                Some(5003)
            );
        }
        if stop == "indexing" {
            assert!(
                !events
                    .iter()
                    .any(|e| matches!(e, SnapshotUpgradeProgress::Copying { .. }))
            );
            assert_eq!(
                events.iter().find_map(|e| match e {
                    SnapshotUpgradeProgress::Indexing { completed, .. } => Some(*completed),
                    _ => None,
                }),
                Some(5003)
            );
        }
        assert_eq!(
            provider
                .search(&SearchRequest {
                    query: "plos".into(),
                    ..Default::default()
                })
                .unwrap()
                .total_count,
            5003
        );
        assert_eq!(provider.get_audio(i64::MIN).unwrap().notes, "retain notes");
        assert_eq!(
            provider.get_audio(i64::MAX).unwrap().attributes["private"],
            "keep"
        );
        let audit =
            Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        audiolib_db::register_library_functions(&audit).unwrap();
        assert_eq!(audit.query_row(
            "SELECT COUNT(*) FROM audio_files WHERE user_notes!='retain notes' OR user_tags!='retain tags' OR json_extract(audiolib_metadata_json(metadata_json),'$.private')!='keep'",
            [], |r| r.get::<_, i64>(0),
        ).unwrap(), 0);
        assert!(!audit.table_exists(None, "play_history").unwrap());
        assert!(
            !audit
                .column_exists(None, "audio_files", "play_count")
                .unwrap()
        );
        drop(audit);
        assert_eq!(
            fs::metadata(format!("{}-wal", database.display()))
                .map(|m| m.len())
                .unwrap_or(0),
            0
        );
        assert!(SqliteProvider::check_owned_file(&database).unwrap().is_ok());
        assert_eq!(
            migration::discover(&database).unwrap().unwrap().phase,
            MigrationPhase::Complete
        );
        migration::verify(&job.directory, &AtomicBool::new(false)).unwrap();
        assert_original_rows(&backup, 5003);
    }
}

#[test]
fn incomplete_upgrade_blocks_preexisting_provider_and_rejects_unknown_protocol() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("live.db");
    legacy_fixture(&database, 3);
    // Complete once, retain a live handle, then simulate a library restored to older derived state.
    let older = SqliteProvider::open_existing(&database).unwrap();
    let conn = support::connection(&database).unwrap();
    conn.execute(
        "UPDATE audiolib_schema_features SET version=1 WHERE feature='tokenization'",
        [],
    )
    .unwrap();
    drop(conn);
    let cancel = AtomicBool::new(false);
    let result =
        SqliteProvider::open_existing_with_progress_cancellable(&database, &cancel, |event| {
            if matches!(event, SnapshotUpgradeProgress::Copying { completed: 3, .. }) {
                assert!(matches!(
                    older.search(&SearchRequest::default()),
                    Err(Error::Busy(_))
                ));
                cancel.store(true, Ordering::Release);
            }
        });
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(matches!(older.get_audio(i64::MIN), Err(Error::Busy(_))));
    let conn = support::connection(&database).unwrap();
    assert!(
        conn.execute("UPDATE audio_files SET user_notes='must be blocked'", [])
            .is_err()
    );
    // Simulate external marker corruption, explicitly bypassing its write guard.
    let guards: Vec<String> = conn.prepare("SELECT name FROM sqlite_schema WHERE type='trigger' AND tbl_name='audiolib_storage_migration'").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    for name in guards {
        conn.execute_batch(&format!("DROP TRIGGER \"{}\"", name.replace('"', "\"\"")))
            .unwrap();
    }
    conn.execute("UPDATE audiolib_storage_migration SET protocol=99", [])
        .unwrap();
    drop(conn);
    assert!(matches!(
        SqliteProvider::open_existing(&database),
        Err(Error::Validation(_))
    ));
}

#[test]
fn checkpoint_reports_pinned_reader_and_truncates_after_it_releases() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("live.db");
    legacy_fixture(&database, 3);
    let provider = SqliteProvider::open_existing(&database).unwrap();
    let reader = support::connection(&database).unwrap();
    reader
        .execute_batch("BEGIN; SELECT COUNT(*) FROM audio_files;")
        .unwrap();
    let writer = support::connection(&database).unwrap();
    writer
        .execute("UPDATE audio_files SET favorite=1", [])
        .unwrap();
    assert!(matches!(provider.checkpoint_wal(), Err(Error::Busy(_))));
    assert!(
        fs::metadata(format!("{}-wal", database.display()))
            .unwrap()
            .len()
            > 0
    );
    reader.execute_batch("ROLLBACK").unwrap();
    provider.checkpoint_wal().unwrap();
    assert_eq!(
        fs::metadata(format!("{}-wal", database.display()))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn uncommitted_migration_crash_child() {
    let Some(directory) = std::env::var_os("AUDIOLIB_TEST_UNCOMMITTED_JOB") else {
        return;
    };
    let directory = Path::new(&directory);
    let stop = std::env::var("AUDIOLIB_TEST_UNCOMMITTED_PHASE").unwrap();
    let candidate = directory.join("candidate.db");
    let conn = support::connection(&candidate).unwrap();
    conn.execute_batch(
        "PRAGMA cache_size=5; PRAGMA cache_spill=ON; PRAGMA synchronous=FULL;
                        BEGIN IMMEDIATE;",
    )
    .unwrap();
    if stop == "copying" {
        conn.execute(
            "ATTACH DATABASE ?1 AS input",
            [directory.join("normalized.db").to_str().unwrap()],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO audio_files(id,file_path,filename,user_tags,user_notes,metadata_json)
             SELECT id,file_path,filename,user_tags,user_notes,metadata_json FROM input.audio_files
             WHERE id>(SELECT last_id FROM audiolib_migration_copy WHERE name='audio_files');
             UPDATE audiolib_migration_copy SET completed=5003,last_id=9223372036854775807
             WHERE name='audio_files';",
        )
        .unwrap();
    } else {
        assert_eq!(stop, "indexing");
        conn.execute_batch(
            "INSERT INTO audio_search(rowid,filename,filename_tokenized)
             SELECT id,filename,audiolib_tokenize(filename) FROM audio_files
             WHERE id>(SELECT last_id FROM audiolib_migration_copy WHERE name='');
             UPDATE audiolib_migration_copy SET completed=5003,last_id=9223372036854775807
             WHERE name='';",
        )
        .unwrap();
    }
    // Force dirty pages to disk, including the cursor and table/index modifications.
    // This fixture injects an uncommitted SQLite transaction, not a physical power loss.
    conn.execute_batch(
        "CREATE TABLE uncommitted_crash_probe(payload BLOB);
         INSERT INTO uncommitted_crash_probe VALUES(zeroblob(2097152));",
    )
    .unwrap();
    assert!(!conn.is_autocommit());
    let journal = fs::read(format!("{}-journal", candidate.display())).unwrap();
    assert_eq!(
        &journal[..8],
        &[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]
    );
    // Deliberately skip Connection/Transaction destructors: the parent must recover.
    std::process::exit(73);
}

#[test]
fn uncommitted_copy_and_index_crashes_recover_before_resuming() {
    for stop in ["copying", "indexing"] {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("uncommitted.db");
        legacy_fixture(&database, 5_003);
        let cancel = AtomicBool::new(false);
        let result =
            SqliteProvider::open_existing_with_progress_cancellable(&database, &cancel, |event| {
                let reached = match event {
                    SnapshotUpgradeProgress::Copying {
                        completed: 5000, ..
                    } => stop == "copying",
                    SnapshotUpgradeProgress::Indexing {
                        completed: 5000, ..
                    } => stop == "indexing",
                    _ => false,
                };
                if reached {
                    cancel.store(true, Ordering::Release);
                }
            });
        assert!(matches!(result, Err(Error::Cancelled)));
        let job = migration::discover(&database).unwrap().unwrap();
        let candidate = job.directory.join("candidate.db");
        let committed = fs::read(&candidate).unwrap();
        let original = migration::inspect(
            &database,
            audiolib_core::compatibility::LibraryAccess::Read,
            true,
            &AtomicBool::new(false),
        )
        .unwrap()
        .business;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "uncommitted_migration_crash_child"])
            .env("AUDIOLIB_TEST_UNCOMMITTED_JOB", &job.directory)
            .env("AUDIOLIB_TEST_UNCOMMITTED_PHASE", stop)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(73), "{stop}: {output:?}");
        assert_ne!(
            fs::read(&candidate).unwrap(),
            committed,
            "fixture must spill dirty pages"
        );
        assert_original_rows(&database, 5003);
        // Resume itself opens the hot-journal candidate and performs SQLite recovery.
        cancel.store(false, Ordering::Release);
        let resumed = migration::resume(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
        assert_eq!(resumed.phase, MigrationPhase::Complete);
        migration::verify(&job.directory, &cancel).unwrap();
        let audit = support::connection(&database).unwrap();
        assert!(!audit.table_exists(None, "uncommitted_crash_probe").unwrap());
        assert_eq!(audit.query_row("SELECT COUNT(*) FROM audio_files WHERE user_notes!='retain notes' OR user_tags!='retain tags' OR json_extract(audiolib_metadata_json(metadata_json),'$.private')!='keep'", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        drop(audit);
        let provider = SqliteProvider::open_read_only(&database).unwrap();
        assert_eq!(
            provider
                .search(&SearchRequest {
                    query: "plos".into(),
                    ..Default::default()
                })
                .unwrap()
                .total_count,
            5003
        );
        drop(provider);
        assert_original_rows(&job.directory.join("backup.db"), 5003);
        migration::rollback(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
        assert_eq!(
            migration::inspect(
                &database,
                audiolib_core::compatibility::LibraryAccess::Read,
                true,
                &cancel
            )
            .unwrap()
            .business,
            original
        );
        assert_no_sidecars(&database);
    }
}

#[test]
fn backup_transaction_crash_child() {
    let Some(source) = std::env::var_os("AUDIOLIB_TEST_BACKUP_CRASH_SOURCE") else {
        return;
    };
    let destination = std::env::var_os("AUDIOLIB_TEST_BACKUP_CRASH_DESTINATION").unwrap();
    let from =
        Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut to = Connection::open(&destination).unwrap();
    to.execute_batch("PRAGMA cache_size=5; PRAGMA cache_spill=ON; PRAGMA synchronous=FULL;")
        .unwrap();
    let backup = rusqlite::backup::Backup::new(&from, &mut to).unwrap();
    // Match the shared publication/rollback executor's page batch, but exit before commit.
    assert!(matches!(
        backup.step(4096).unwrap(),
        rusqlite::backup::StepResult::More
    ));
    assert!(backup.progress().remaining > 0);
    let journal = fs::read(format!("{}-journal", Path::new(&destination).display())).unwrap();
    assert_eq!(
        &journal[..8],
        &[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]
    );
    std::process::exit(73);
}

#[test]
fn partial_publication_and_rollback_backup_transactions_recover_after_process_exit() {
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("publication.db");
    legacy_fixture(&database, 3);
    let conn = support::connection(&database).unwrap();
    let page_size: i64 = conn
        .pragma_query_value(None, "page_size", |row| row.get(0))
        .unwrap();
    conn.execute_batch("CREATE TABLE extension_crash_payload(id INTEGER PRIMARY KEY,value BLOB);")
        .unwrap();
    // More than one real executor batch; the opaque business payload must also round-trip.
    conn.execute(
        "INSERT INTO extension_crash_payload VALUES(7,zeroblob(?1))",
        [page_size * 5000],
    )
    .unwrap();
    drop(conn);
    let cancel = AtomicBool::new(false);
    let reviewed = migration::plan(&database, &cancel).unwrap();
    assert!(matches!(
        migration::apply(&reviewed, &cancel, &mut |event| {
            if event.phase == MigrationPhase::Publishing {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        }),
        Err(Error::Cancelled)
    ));
    let job = migration::discover(&database).unwrap().unwrap();
    for (artifact, phase) in [
        ("candidate.db", MigrationPhase::Publishing),
        ("backup.db", MigrationPhase::RollingBack),
    ] {
        if phase == MigrationPhase::RollingBack {
            assert!(matches!(
                migration::rollback(&job.directory, &cancel, &mut |event| {
                    if event.phase == MigrationPhase::RollingBack {
                        Err(Error::Cancelled)
                    } else {
                        Ok(())
                    }
                }),
                Err(Error::Cancelled)
            ));
        }
        assert_eq!(
            migration::discover(&database).unwrap().unwrap().phase,
            phase
        );
        let committed = fs::read(&database).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "backup_transaction_crash_child"])
            .env(
                "AUDIOLIB_TEST_BACKUP_CRASH_SOURCE",
                job.directory.join(artifact),
            )
            .env("AUDIOLIB_TEST_BACKUP_CRASH_DESTINATION", &database)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(73), "{phase:?}: {output:?}");
        assert_ne!(
            fs::read(&database).unwrap(),
            committed,
            "fixture must spill partial backup pages"
        );
        let resumed = migration::resume(&job.directory, &cancel, &mut |_| Ok(())).unwrap();
        assert_eq!(
            resumed.phase,
            if phase == MigrationPhase::Publishing {
                MigrationPhase::Complete
            } else {
                MigrationPhase::RolledBack
            }
        );
        migration::verify(&job.directory, &cancel).unwrap();
        assert_no_sidecars(&database);
    }
    assert_eq!(
        migration::inspect(
            &database,
            audiolib_core::compatibility::LibraryAccess::Read,
            true,
            &cancel
        )
        .unwrap()
        .business,
        reviewed.source.business
    );
    assert_original_rows(&database, 3);
}

#[test]
fn upgrade_crash_child() {
    let Some(path) = std::env::var_os("AUDIOLIB_TEST_UPGRADE_CRASH_DB") else {
        return;
    };
    let stop = std::env::var("AUDIOLIB_TEST_UPGRADE_CRASH_PHASE").unwrap();
    let _ = SqliteProvider::open_existing_with_progress(Path::new(&path), |event| {
        let reached = match event {
            SnapshotUpgradeProgress::Copying {
                completed: 5000, ..
            } => stop == "copying",
            SnapshotUpgradeProgress::Indexing {
                completed: 5000, ..
            } => stop == "indexing",
            SnapshotUpgradeProgress::Validating => stop == "validating",
            SnapshotUpgradeProgress::Checkpoint => stop == "checkpoint",
            _ => false,
        };
        if reached {
            std::process::exit(73);
        }
    });
    panic!("Crash phase was never reached");
}

#[test]
fn abrupt_process_exit_preserves_resume_cursor_and_all_user_data() {
    for phase in ["copying", "indexing", "validating", "checkpoint"] {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("crash.db");
        legacy_fixture(&database, 5_003);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "upgrade_crash_child"])
            .env("AUDIOLIB_TEST_UPGRADE_CRASH_DB", &database)
            .env("AUDIOLIB_TEST_UPGRADE_CRASH_PHASE", phase)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(73), "{phase}: {output:?}");
        let job = migration::discover(&database).unwrap().unwrap();
        assert_eq!(job.phase, crate::phase(phase));
        assert_original_rows(&job.directory.join("backup.db"), 5003);
        let provider = SqliteProvider::open_existing(&database).unwrap();
        assert_eq!(
            provider
                .search(&SearchRequest {
                    query: "plos".into(),
                    ..Default::default()
                })
                .unwrap()
                .total_count,
            5003
        );
        assert_eq!(provider.get_audio(i64::MIN).unwrap().notes, "retain notes");
        assert_eq!(
            provider.get_audio(i64::MAX).unwrap().attributes["private"],
            "keep"
        );
        assert!(SqliteProvider::check_owned_file(&database).unwrap().is_ok());
        drop(provider);
        let mut events = Vec::new();
        drop(
            SqliteProvider::open_existing_with_progress(&database, |event| events.push(event))
                .unwrap(),
        );
        assert!(!events.iter().any(|e| matches!(
            e,
            SnapshotUpgradeProgress::BackingUp
                | SnapshotUpgradeProgress::Indexing { .. }
                | SnapshotUpgradeProgress::Copying { .. }
        )));
    }
}
