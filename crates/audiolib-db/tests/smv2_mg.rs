// crates/audiolib-db/tests/smv2_mg.rs
//! MG-01..MG-18 (doc/TEST-SEARCH-METADATA-V2.md §6): schema 8→9 migration.
//!
//! The migration is not in this branch yet. Tests needing it are ignored with
//! `pending server migration branch`; run them with the entrypoint described in
//! `smv2/driver.rs`:
//!   cargo build -p audiolib-db --features test-hooks --bin audiolib-migrate
//!   cargo test -p audiolib-db --features test-hooks --test smv2_mg -- --ignored
//! (or point AUDIOLIB_SMV2_MIGRATE_CMD at another executable)
//! MG-09 (old client vs schema-9 file) runs now against the current client code.
mod smv2;
mod support;

use audiolib_core::{DataProvider, MetadataPatch};
use audiolib_db::{ScanCancellation, ScanOptions, ScanService, SqliteProvider};
use rusqlite::Connection;
use serde_json::json;
use smv2::{driver::*, *};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(Fixture::default_size)
}

fn conn(db: &Path) -> Connection {
    support::connection(db).unwrap()
}

fn env(k: &'static str, v: impl ToString) -> (&'static str, String) {
    (k, v.to_string())
}

/// Observable end state of a migrated library, compared across interrupted and
/// uninterrupted runs.
#[derive(Debug, PartialEq)]
struct EndState {
    user_version: i64,
    dir_rows: i64,
    folder_rows: i64,
    path_rows: i64,
    file_rows: i64,
    path_runs: Vec<(i64, i64, i64, i64)>,
    path_tokens: String,
    main_shadow: String,
}

fn end_state(db: &Path) -> EndState {
    let c = conn(db);
    let count = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    EndState {
        user_version: user_version(db),
        dir_rows: count("SELECT count(*) FROM audio_dir_search"),
        folder_rows: count("SELECT count(*) FROM folders"),
        path_rows: count("SELECT coalesce(sum(n),0) FROM audio_dir_runs"),
        file_rows: count("SELECT count(*) FROM audio_files"),
        path_runs: c
            .prepare("SELECT dir_id, lo, hi, n FROM audio_dir_runs ORDER BY dir_id, lo")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap(),
        path_tokens: fts_instance_hash(&c, "audio_dir_search"),
        main_shadow: main_fts_shadow_hash(&c),
    }
}

fn reference() -> &'static EndState {
    static R: OnceLock<EndState> = OnceLock::new();
    R.get_or_init(|| {
        let (dir, db) = fixture().copy_db();
        Driver::require().migrate_to_done(&db);
        let s = end_state(&db);
        drop(dir);
        s
    })
}

fn assert_still_usable_s8(db: &Path) {
    assert_eq!(user_version(db), 8);
    let p = SqliteProvider::open_read_only(db).expect("pre-switch library must stay a usable s8");
    let f = fixture();
    for (token, _, _) in PLANTED {
        assert_eq!(
            hits(&p, f, token)
                .iter()
                .filter(|h| h.1.ends_with(".wav"))
                .count(),
            expected_planted(token, f.files),
            "{token}"
        );
    }
}

fn db_size(db: &Path) -> u64 {
    fs::metadata(db).unwrap().len()
}

// ------------------------------------------------------------------------ MG-01
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_01_kill_and_resume_at_every_fault_point() {
    let d = Driver::require();
    for point in [
        "abort:batch_boundary:1",
        "abort:mid_batch:1",
        "abort:mid_batch:2",
        "abort:optimize",
        "abort:switch_tx",
    ] {
        let (_dir, db) = fixture().copy_db();
        let first = d.run("start", &db, &[env(FAULT, point)]);
        assert!(first.aborted(), "{point}: fault did not abort: {first:?}");
        // Every fault here precedes the switch commit: still a usable schema 8.
        assert_still_usable_s8(&db);
        let c = conn(&db);
        if point.starts_with("abort:mid_batch") || point.starts_with("abort:batch_boundary") {
            assert!(
                table_exists(&c, "v9_checkpoint"),
                "{point}: checkpoint missing"
            );
        }
        drop(c);
        let resumed = d.run("resume", &db, &[]);
        assert_eq!(resumed.state(), "done", "{point}: {resumed:?}");
        assert_eq!(
            &end_state(&db),
            reference(),
            "{point}: differs from one-shot run"
        );
    }
}

// ------------------------------------------------------------------------ MG-02
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_02_space_precheck_1_29x_refuses_1_30x_proceeds() {
    let d = Driver::require();
    let (_dir, db) = fixture().copy_db();
    let size = db_size(&db);
    let before = file_sha(&db);
    let low = (size as f64 * 1.29).floor() as u64;
    let r = d.run("start", &db, &[env(FAKE_FREE_BYTES, low)]);
    assert_eq!(r.error_code(), Some("insufficient_space"), "{r:?}");
    assert_ne!(r.state(), "running");
    assert_eq!(file_sha(&db), before, "refused migration modified the DB");
    let ok = (size as f64 * 1.30).ceil() as u64;
    let r = d.run("start", &db, &[env(FAKE_FREE_BYTES, ok)]);
    assert_ne!(r.error_code(), Some("insufficient_space"), "{r:?}");
}

// ------------------------------------------------------------------------ MG-03
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_03_backup_failure_aborts_and_backup_is_verified_and_kept() {
    let d = Driver::require();
    let (dir, db) = fixture().copy_db();
    let before = file_sha(&db);
    let r = d.run("start", &db, &[env(FAULT, "backup_fail")]);
    assert_eq!(r.state(), "failed", "{r:?}");
    assert_eq!(r.error_code(), Some("backup_failed"), "{r:?}");
    assert_eq!(
        file_sha(&db),
        before,
        "backup failure modified the original"
    );
    let r = d.migrate_to_done(&db);
    let backups: Vec<PathBuf> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy();
            n.starts_with("audiolib-rollback-") && n.ends_with(".db")
        })
        .collect();
    assert_eq!(
        backups.len(),
        1,
        "expected one audiolib-rollback-*.db next to the DB: {r:?}"
    );
    let b = Connection::open(&backups[0]).unwrap();
    let ic: String = b
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ic, "ok");
    assert!(
        !b.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .exists([])
            .unwrap()
    );
    assert_eq!(user_version(&backups[0]), 8);
    // A second (no-op) run must not delete it.
    d.run("start", &db, &[]);
    assert!(backups[0].exists(), "backup was deleted");
}

// ------------------------------------------------------------------------ MG-04
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_04_read_only_db_is_not_migrated() {
    let d = Driver::require();
    let (_dir, db) = fixture().copy_db();
    let before = file_sha(&db);
    let mut perm = fs::metadata(&db).unwrap().permissions();
    perm.set_readonly(true);
    fs::set_permissions(&db, perm.clone()).unwrap();
    let r = d.run("start", &db, &[]);
    assert_eq!(r.error_code(), Some("read_only"), "{r:?}");
    assert_eq!(r.state(), "failed", "{r:?}");
    let backups = fs::read_dir(db.parent().unwrap())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("audiolib-rollback-")
        })
        .count();
    assert_eq!(backups, 0, "read_only preflight must not take a backup");
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(false);
    fs::set_permissions(&db, perm).unwrap();
    assert_eq!(file_sha(&db), before);
    assert_eq!(user_version(&db), 8);
}

// ------------------------------------------------------------------------ MG-05
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_05_upgrade_lock_is_exclusive() {
    let d = Arc::new(Driver::require());
    let (_dir, db) = fixture().copy_db();
    let slow = [env(SLOW_BATCH, "1:1500")];
    let (d1, db1) = (d.clone(), db.clone());
    let first = std::thread::spawn(move || d1.run("start", &db1, &slow));
    std::thread::sleep(Duration::from_millis(500));
    let second = d.run("start", &db, &[]);
    assert_eq!(
        second.error_code(),
        Some("locked"),
        "second migrator did not see upgrade.lock: {second:?}"
    );
    let first = first.join().unwrap();
    assert!(matches!(first.state(), "done" | "paused"), "{first:?}");
    let ic: String = conn(&db)
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ic, "ok");
    let mut lock = db.as_os_str().to_owned();
    lock.push(".upgrade.lock");
    assert!(
        !Path::new(&lock).exists(),
        "upgrade.lock left behind after the owner finished"
    );
}

/// `<db>.upgrade.lock` holding a dead pid is reclaimed automatically.
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_05_stale_upgrade_lock_is_reclaimed() {
    let d = Driver::require();
    let (_dir, db) = fixture().copy_db();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--list")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();
    let mut lock = db.as_os_str().to_owned();
    lock.push(".upgrade.lock");
    fs::write(&lock, dead_pid.to_string()).unwrap();
    let r = d.migrate_to_done(&db);
    assert_eq!(r.error_code(), None, "{r:?}");
    assert_eq!(user_version(&db), 9);
}

// ------------------------------------------------------------------------ MG-06
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks; optional AUDIOLIB_SMV2_LARGE_DB)"]
fn mg_06_record_timings() {
    let d = Driver::require();
    let (_dir, db) = match std::env::var_os("AUDIOLIB_SMV2_LARGE_DB") {
        Some(src) => {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("library.db");
            fs::copy(src, &db).unwrap();
            (dir, db)
        }
        None => fixture().copy_db(),
    };
    let started = Instant::now();
    let r = d.migrate_to_done(&db);
    let total = started.elapsed();
    let mut ms: Vec<f64> = r.batches().into_iter().map(|(_, ms)| ms).collect();
    assert!(!ms.is_empty(), "no batches recorded: {r:?}");
    ms.sort_by(f64::total_cmp);
    let pct = |p: f64| {
        ms.get(((ms.len() as f64 - 1.0) * p).round() as usize)
            .copied()
            .unwrap_or(0.0)
    };
    for k in ["backup", "path_build", "optimize", "switch"] {
        assert!(r.phase_ms(k).is_some(), "phases lacks {k}: {r:?}");
    }
    let phases = r.report["phases"].to_string().to_lowercase();
    assert!(
        !phases.contains("rebuild") || !phases.contains("audio_search\""),
        "main table rebuild phase present: {phases}"
    );
    println!(
        "SMV2-TIMING {}",
        json!({
            "case": "MG-06", "total_s": total.as_secs_f64(), "db_bytes": db_size(&db),
            "batches": ms.len(), "batch_ms_p50": pct(0.5), "batch_ms_p95": pct(0.95),
            "batch_ms_max": ms.last(), "phases": r.report["phases"],
        })
    );
}

// ------------------------------------------------------------------------ MG-07
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_07_cancel_before_switch_and_rollback_after() {
    let d = Driver::require();
    let f = fixture();
    let (_dir, db) = f.copy_db();
    let main_before = main_fts_shadow_hash(&conn(&db));
    let r = d.run("start", &db, &[env(PAUSE_AFTER_BATCHES, 1)]);
    assert_eq!(r.state(), "paused", "{r:?}");
    let r = d.run("cancel", &db, &[]);
    assert_eq!(r.state(), "cancelled", "{r:?}");
    let c = conn(&db);
    assert!(!table_exists(&c, "audio_path_search"));
    assert!(!table_exists(&c, "v9_checkpoint"));
    assert_eq!(main_fts_shadow_hash(&c), main_before);
    drop(c);
    assert_still_usable_s8(&db);
    // After the switch: restore from audiolib-rollback-*.db.
    d.migrate_to_done(&db);
    let r = d.run("rollback", &db, &[]);
    assert_eq!(r.state(), "rolled_back", "{r:?}");
    assert_still_usable_s8(&db);
    assert_eq!(main_fts_shadow_hash(&conn(&db)), main_before);
}

// ------------------------------------------------------------------------ MG-08
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_08_second_run_is_a_noop() {
    let d = Driver::require();
    let (_dir, db) = fixture().copy_db();
    d.migrate_to_done(&db);
    checkpoint(&db);
    let before = file_sha(&db);
    let r = d.run("start", &db, &[]);
    assert_eq!(r.state(), "done", "{r:?}");
    assert!(r.batches().is_empty(), "second run did work: {r:?}");
    checkpoint(&db);
    assert_eq!(file_sha(&db), before, "second run modified the DB");
}

// ------------------------------------------------------------------------ MG-09
/// Runs now: the current (schema-8) client facing a schema-9 file. A full s9 file
/// does not exist yet, so the fixture is stamped `user_version=9`; rerun with a real
/// migrated file via the ignored variant below.
#[test]
fn mg_09_old_client_refuses_or_reads_schema9_without_writing() {
    let (_dir, db) = fixture().copy_db();
    conn(&db).pragma_update(None, "user_version", 9).unwrap();
    old_client_contract(&db);
}

#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_09_old_client_on_real_migrated_file() {
    let (_dir, db) = fixture().copy_db();
    Driver::require().migrate_to_done(&db);
    old_client_contract(&db);
}

fn old_client_contract(db: &Path) {
    checkpoint(db);
    let before = file_sha(db);
    match SqliteProvider::open(db) {
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            assert!(
                msg.contains("schema") || msg.contains("newer") || msg.contains("version"),
                "unclear refusal: {e}"
            );
        }
        Ok(p) => {
            let any = p
                .search(&Default::default())
                .unwrap()
                .results
                .into_iter()
                .next()
                .unwrap();
            assert!(
                p.update_metadata(
                    any.id,
                    &MetadataPatch {
                        notes: Some("x".into()),
                        ..Default::default()
                    }
                )
                .is_err(),
                "old client wrote into a schema-9 library"
            );
        }
    }
    match SqliteProvider::open_read_only(db) {
        Err(e) => eprintln!("MG-09 read-only open refused: {e}"),
        Ok(_) => eprintln!("MG-09 read-only open allowed"),
    }
    checkpoint(db);
    assert_eq!(
        file_sha(db),
        before,
        "old client modified the schema-9 file"
    );
    assert_eq!(user_version(db), 9);
}

// ------------------------------------------------------------------------ MG-10
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_10_integrity_counts_and_main_fts_unchanged() {
    let d = Driver::require();
    let (_dir, db) = fixture().copy_db();
    let c = conn(&db);
    let main_before = main_fts_shadow_hash(&c);
    let main_rows: i64 = c
        .query_row("SELECT count(*) FROM audio_search", [], |r| r.get(0))
        .unwrap();
    drop(c);
    d.migrate_to_done(&db);
    let c = conn(&db);
    for t in ["audio_search", "audio_dir_search"] {
        c.execute(
            &format!("INSERT INTO {t}({t}) VALUES('integrity-check')"),
            [],
        )
        .unwrap();
    }
    let ic: String = c
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ic, "ok");
    let s = end_state(&db);
    assert_eq!(
        s.dir_rows, s.folder_rows,
        "audio_dir_search vs folders rows"
    );
    assert_eq!(
        s.path_rows, s.file_rows,
        "audio_dir_runs sum(n) vs audio_files rows"
    );
    let ids_match: i64 = c
        .query_row(
            "SELECT count(*) FROM audio_files f WHERE NOT EXISTS (
                SELECT 1 FROM audio_dir_runs r JOIN audio_dir_search d ON d.rowid=r.dir_id
                WHERE r.dir_id=f.folder_id AND f.id BETWEEN r.lo AND r.hi)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ids_match, 0);
    let orphan_runs: i64 = c
        .query_row(
            "SELECT count(*) FROM audio_dir_runs WHERE dir_id NOT IN (SELECT id FROM folders)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphan_runs, 0);
    assert_eq!(
        c.query_row("SELECT count(*) FROM audio_search", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        main_rows
    );
    assert_eq!(s.main_shadow, main_before, "main FTS shadow tables changed");
}

// ------------------------------------------------------------------------ MG-11
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_11_adaptive_batches_and_bounded_write_lock() {
    let d = Driver::require();
    let f = fixture();
    // Slow batch 2 → batch 3 must shrink.
    let (_dir, db) = f.copy_db();
    let r = d.run(
        "start",
        &db,
        &[env(BATCH_ROWS, 50), env(SLOW_BATCH, "2:1800")],
    );
    let rows: Vec<u64> = r.batches().iter().map(|(rows, _)| *rows).collect();
    assert!(
        rows.len() >= 3 && rows[2] < rows[1],
        "batch after a slow one did not shrink: {rows:?}"
    );
    // Concurrent search + writer load: per-batch ≤2 s and writer wait ≤2 s.
    let (_dir2, db2) = f.copy_db();
    let stop = Arc::new(AtomicBool::new(false));
    let (s2, p2) = (stop.clone(), db2.clone());
    let load = std::thread::spawn(move || {
        let c = support::connection(&p2).unwrap();
        c.busy_timeout(Duration::from_secs(30)).unwrap();
        let mut worst = Duration::ZERO;
        while !s2.load(Ordering::Relaxed) {
            let t = Instant::now();
            let _ = c.execute("UPDATE audio_files SET user_notes='mg11' WHERE id=(SELECT min(id) FROM audio_files)", []);
            worst = worst.max(t.elapsed());
            let _: i64 = c
                .query_row(
                    "SELECT count(*) FROM audio_search WHERE audio_search MATCH 'zildjian'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
        }
        worst
    });
    let r = d.migrate_to_done(&db2);
    stop.store(true, Ordering::Relaxed);
    let worst_write = load.join().unwrap();
    let mut ms: Vec<f64> = r.batches().into_iter().map(|(_, ms)| ms).collect();
    ms.sort_by(f64::total_cmp);
    println!(
        "SMV2-TIMING {}",
        json!({"case":"MG-11","batch_ms_p95": ms.get(ms.len().saturating_sub(1) * 95 / 100), "batch_ms_max": ms.last(), "writer_wait_max_ms": worst_write.as_millis()})
    );
    assert!(
        ms.last().copied().unwrap_or(0.0) <= 2000.0,
        "batch over 2 s"
    );
    assert!(
        worst_write <= Duration::from_secs(2),
        "writer blocked {worst_write:?}"
    );
}

// ------------------------------------------------------------------------ MG-12
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_12_no_vacuum_on_migration_path() {
    let d = Driver::require();
    let (dir, db) = fixture().copy_db();
    let trace = dir.path().join("sql-trace.log");
    let before = db_size(&db);
    let r = d.run_traced("start", &db, &[], Some(&trace));
    assert_eq!(r.state(), "done", "{r:?}");
    let sql = fs::read_to_string(&trace).expect("entrypoint must write --sql-trace");
    assert!(!sql.is_empty(), "empty SQL trace");
    assert!(
        !sql.to_uppercase().contains("VACUUM"),
        "VACUUM executed during migration"
    );
    println!(
        "SMV2-TIMING {}",
        json!({"case":"MG-12","db_bytes_before": before, "db_bytes_after": db_size(&db)})
    );
}

// ------------------------------------------------------------------------ MG-13
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_13_sqlite_full_pauses_and_resumes() {
    let d = Driver::require();
    let (_dir, db) = fixture().copy_db();
    let r = d.run("start", &db, &[env(FAULT, "sqlite_full:batch:2")]);
    assert_eq!(r.state(), "paused", "{r:?}");
    assert_eq!(r.error_code(), Some("disk_full"), "{r:?}");
    assert!(
        table_exists(&conn(&db), "v9_checkpoint"),
        "checkpoint dropped on SQLITE_FULL"
    );
    assert_still_usable_s8(&db);
    let r = d.run("resume", &db, &[]);
    assert_eq!(r.state(), "done", "{r:?}");
    assert_eq!(&end_state(&db), reference());
}

// ------------------------------------------------------------------------ MG-14
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_14_state_machine_start_pause_resume_cancel() {
    // HTTP permission checks (no manage → 403) belong in the server crate; this
    // checks the state machine and progress fields through the entrypoint.
    let d = Driver::require();
    let (_dir, db) = fixture().copy_db();
    assert_eq!(d.run("status", &db, &[]).state(), "idle");
    let r = d.run("start", &db, &[env(PAUSE_AFTER_BATCHES, 1)]);
    assert_eq!(r.state(), "paused", "{r:?}");
    let (done, total) = r.progress();
    assert!(done > 0 && done <= total, "progress after one batch: {r:?}");
    assert!(r.eta_s().is_number(), "eta_s while paused mid-run: {r:?}");
    assert!(!r.batches().is_empty(), "{r:?}");
    let r = d.run("resume", &db, &[]);
    assert_eq!(r.state(), "done", "{r:?}");
    let (done, total) = r.progress();
    assert_eq!(done, total, "{r:?}");
}

// ------------------------------------------------------------------------ MG-15
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_15_s8_reads_and_writes_continue_during_backfill() {
    // 503 + Retry-After during the switch and /health.upgrade are HTTP-level and
    // belong in the server crate; this checks library availability between batches.
    let d = Driver::require();
    let f = fixture();
    let (_dir, db) = f.copy_db();
    let r = d.run("start", &db, &[env(PAUSE_AFTER_BATCHES, 1)]);
    assert_eq!(r.state(), "paused", "{r:?}");
    assert_still_usable_s8(&db);
    let p = SqliteProvider::open(&db).expect("s8 writer during backfill");
    let any = p.search(&Default::default()).unwrap().results.remove(0);
    p.update_metadata(
        any.id,
        &MetadataPatch {
            tags: Some("mg15".into()),
            ..Default::default()
        },
    )
    .unwrap();
    drop(p);
    assert_eq!(d.run("resume", &db, &[]).state(), "done");
}

// ------------------------------------------------------------------------ MG-16
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_16_defer_schema9_env() {
    let d = Driver::require();
    let (_dir, db) = fixture().copy_db();
    let r = d.run("startup", &db, &[env("AUDIOLIB_DEFER_SCHEMA9", 1)]);
    assert_eq!(r.state(), "deferred", "{r:?}");
    assert_eq!(user_version(&db), 8);
    assert_still_usable_s8(&db);
    assert!(!table_exists(&conn(&db), "audio_path_search"));
    // Without the env, startup migrates an s8 library automatically.
    let (_dir2, db2) = fixture().copy_db();
    let r = d.run("startup", &db2, &[]);
    assert_eq!(r.state(), "done", "{r:?}");
    assert_eq!(user_version(&db2), 9);
}

// ------------------------------------------------------------------------ MG-17
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_17_scan_and_rename_during_backfill_reach_path_table() {
    let d = Driver::require();
    // This case renames source directories, so it owns a separate WAV fixture.
    let f = Fixture::default_size();
    let (_dir, db) = f.copy_db();
    let r = d.run("start", &db, &[env(PAUSE_AFTER_BATCHES, 1)]);
    assert_eq!(r.state(), "paused", "{r:?}");
    let mut c = conn(&db);
    // t3 indexes directory segments. Select a file in the directory being renamed
    // explicitly: max(audio_files.id) can be a non-WAV clone in another directory.
    let id: i64 = c
        .query_row(
            "SELECT max(af.id) FROM audio_files af JOIN folders d ON d.id=af.folder_id WHERE d.folder_name='Urban'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let source = f.root.join("Ambience/Urban");
    let renamed = f.root.join("Ambience/Mg17renamed");
    fs::rename(&source, &renamed).unwrap();
    // Model a committed child-directory rename, retaining folder/file identities
    // and closure. The public relink operation only accepts top-level roots,
    // whose own names are deliberately excluded from t3 path terms.
    let tx = c.transaction().unwrap();
    assert_eq!(
        tx.execute(
            "UPDATE folders SET folder_name='Mg17renamed',folder_path=replace(folder_path,'Urban','Mg17renamed') WHERE id=(SELECT folder_id FROM audio_files WHERE id=?1)",
            [id],
        )
        .unwrap(),
        1
    );
    assert!(
        tx.execute(
            "UPDATE audio_files SET file_path=replace(file_path,'Urban','Mg17renamed'),path_key=replace(path_key,'urban','mg17renamed') WHERE folder_id=(SELECT folder_id FROM audio_files WHERE id=?1)",
            [id],
        )
        .unwrap() > 0
    );
    tx.commit().unwrap();
    drop(c);
    let provider = Arc::new(SqliteProvider::open(&db).unwrap());
    let scanned = f.root.join("Mg17scanned/new.wav");
    fs::create_dir_all(scanned.parent().unwrap()).unwrap();
    fs::write(&scanned, riff(&[])).unwrap();
    let progress = ScanService::new(provider.clone())
        .scan(
            &f.root,
            &ScanOptions::default(),
            &ScanCancellation::default(),
            |_| {},
        )
        .unwrap();
    assert_eq!(progress.errors, 0);
    assert_eq!(progress.added, 1);
    let scanned_id = provider.find_audio_by_path(&scanned).unwrap().unwrap().id;
    drop(provider);
    assert_eq!(d.run("resume", &db, &[]).state(), "done");
    let s = end_state(&db);
    assert_eq!(s.dir_rows, s.folder_rows);
    assert_eq!(s.path_rows, s.file_rows);
    let hit = d.search("path_search", &db, "Mg17renamed");
    assert!(hit.contains(&id), "renamed row not in path table: {hit:?}");
    assert!(!d.search("path_search", &db, "Urban").contains(&id));
    assert!(
        d.search("path_search", &db, "Mg17scanned")
            .contains(&scanned_id)
    );
}

// ------------------------------------------------------------------------ MG-18
#[test]
#[ignore = "pending server migration branch (audiolib-migrate, feature test-hooks)"]
fn mg_18_switch_transaction_is_atomic() {
    let d = Driver::require();
    let (_dir, db) = fixture().copy_db();
    let st = d.run("status", &db, &[]);
    assert_eq!(st.report["switch"]["source_search_rev"], 2, "{st:?}");
    let r = d.migrate_to_done(&db);
    assert_eq!(user_version(&db), 9);
    let sw = &r.report["switch"];
    assert_eq!(sw["source_search_rev"], 3, "{r:?}");
    for feature in ["search_path", "vorbis_tags", "vendor_metadata"] {
        assert!(
            sw["features"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == feature)),
            "missing {feature}: {r:?}"
        );
    }
    // Abort inside the switch: none of the switch writes may be visible.
    let (_dir2, db2) = fixture().copy_db();
    assert!(
        d.run("start", &db2, &[env(FAULT, "abort:switch_tx")])
            .aborted()
    );
    assert_eq!(user_version(&db2), 8);
    let st = d.run("status", &db2, &[]);
    assert!(
        st.report["switch"]["source_search_rev"] == 2,
        "partial switch: {st:?}"
    );
}

// ---------------------------------------------------------- report schema checks
/// Runs now: the shared schema accepts the agreed report shape and rejects drift.
#[test]
fn report_schema_examples() {
    use smv2::schema::{report_schema, validate};
    let good = json!({
        "state": "paused", "error_code": null,
        "progress": {"done": 500, "total": 1200},
        "phases": {"backup": 12.5, "path_build": 340, "optimize": 0, "switch": 0},
        "switch": {"source_search_rev": 2, "features": []},
        "batches": [{"rows": 500, "ms": 210.0}], "eta_s": 1.4
    });
    assert_eq!(validate(report_schema(), &good), Vec::<String>::new());
    let minimal = json!({"state": "failed", "error_code": "backup_failed",
        "progress": {"done": 0, "total": 0},
        "phases": {"backup": 1, "path_build": 0, "optimize": 0, "switch": 0},
        "switch": null, "batches": [], "eta_s": null});
    assert_eq!(validate(report_schema(), &minimal), Vec::<String>::new());
    let mut bad = Vec::new();
    for (k, v) in [
        ("state", json!("weird")),
        ("error_code", json!("SQLITE_FULL")),
        ("error_code", json!("INSUFFICIENT_SPACE")),
        ("progress", json!({"done": -1, "total": 0})),
        ("phases", json!({"optimize": 0})),
        ("switch", json!({"source_search_rev": 4, "features": []})),
        ("batches", json!([{"rows": 1}])),
    ] {
        let mut b = minimal.clone();
        b[k] = v;
        bad.push(b);
    }
    for k in ["batches", "eta_s", "progress"] {
        let mut b = minimal.clone();
        b.as_object_mut().unwrap().remove(k);
        bad.push(b);
    }
    for bad in bad {
        assert!(
            !validate(report_schema(), &bad).is_empty(),
            "accepted {bad}"
        );
    }
}

// ------------------------------------------------------- HTTP (pending server)
// One disposable server instance serving AUDIOLIB_SMV2_ADMIN_DB (a fresh schema-8
// copy, e.g. of the fixture). Each test needs that file back at schema 8, so run them
// one at a time against a fresh instance:
//   cargo test -p audiolib-db --features test-hooks --test smv2_mg mg_1 -- --ignored --test-threads=1
// The migration is driven on that file with `audiolib-migrate` and held in progress
// with AUDIOLIB_MIGRATION_PAUSE_AFTER_BATCHES / SLOW_BATCH.
use smv2::http::{self, Server};

fn server_lock() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

fn fresh_s8_server() -> Server {
    let srv = Server::from_env();
    assert_eq!(
        user_version(&srv.db),
        8,
        "AUDIOLIB_SMV2_ADMIN_DB must be a fresh schema-8 copy for this test"
    );
    srv
}

fn any_id(db: &Path) -> i64 {
    let c = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    c.query_row("SELECT min(id) FROM audio_files", [], |r| r.get(0))
        .unwrap()
}

fn assert_report_body(resp: &http::Response) -> serde_json::Value {
    use smv2::schema::{report_schema, validate};
    let body = resp.json();
    let errors = validate(report_schema(), &body);
    assert!(
        errors.is_empty(),
        "admin body violates schema: {errors:?}\n{resp:?}"
    );
    body
}

fn assert_http_error(resp: &http::Response, status: u16, code: &str, retry_after: &str) {
    use smv2::schema::{http_schema, validate};
    assert_eq!(resp.status, status, "{resp:?}");
    let body = resp.json();
    let errors = validate(&http_schema()["$defs"]["error_body"], &body);
    assert!(
        errors.is_empty(),
        "error body violates schema: {errors:?}\n{resp:?}"
    );
    assert_eq!(body["error_code"], code, "{resp:?}");
    assert_eq!(resp.header("retry-after"), Some(retry_after), "{resp:?}");
}

/// Admin GET /admin/api/migration/status validates against the shared schema and
/// agrees with `audiolib-migrate status` on the same file (idle, then paused).
#[test]
#[ignore = "pending server migration HTTP (set AUDIOLIB_SMV2_ADMIN_URL, AUDIOLIB_SMV2_ADMIN_DB)"]
fn mg_14_admin_status_matches_cli_and_schema() {
    let _g = server_lock();
    let srv = fresh_s8_server();
    let d = Driver::require();
    let compare = |label: &str| {
        let http = assert_report_body(&srv.status());
        let cli = d.run("status", &srv.db, &[]);
        for k in ["state", "error_code", "progress", "switch"] {
            assert_eq!(
                http[k], cli.report[k],
                "{label}: admin vs CLI {k}: {http} vs {:?}",
                cli.report
            );
        }
        let keys = |v: &serde_json::Value| -> Vec<String> {
            v.as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default()
        };
        assert_eq!(
            keys(&http["phases"]),
            keys(&cli.report["phases"]),
            "{label}: phase names"
        );
    };
    compare("idle");
    let r = d.run("start", &srv.db, &[env(PAUSE_AFTER_BATCHES, 1)]);
    assert_eq!(r.state(), "paused", "{r:?}");
    compare("paused");
    assert_eq!(d.run("resume", &srv.db, &[]).state(), "done");
    compare("done");
}

/// CLI: second start while running → exit 2 / already_running / current state;
/// admin POST start → 409 with the same body; pause from another process lands
/// through v9_control within one batch (≤2 s) plus the injected slow batch.
#[test]
#[ignore = "pending server migration HTTP (set AUDIOLIB_SMV2_ADMIN_URL, AUDIOLIB_SMV2_ADMIN_DB)"]
fn mg_14_http_already_running_and_cross_process_pause() {
    let _g = server_lock();
    let srv = fresh_s8_server();
    let d = Arc::new(Driver::require());
    let (d1, db1) = (d.clone(), srv.db.clone());
    let runner = std::thread::spawn(move || {
        d1.run(
            "start",
            &db1,
            &[env(BATCH_ROWS, 50), env(SLOW_BATCH, "2:1500")],
        )
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    while d.run("status", &srv.db, &[]).state() != "running" {
        assert!(Instant::now() < deadline, "migration never reached running");
        std::thread::sleep(Duration::from_millis(50));
    }
    let again = d.run("start", &srv.db, &[]);
    assert_eq!(again.code, Some(2), "{again:?}");
    assert_eq!(again.error_code(), Some("already_running"), "{again:?}");
    assert_eq!(again.state(), "running", "{again:?}");
    let resp = srv.admin("start");
    assert_eq!(resp.status, 409, "{resp:?}");
    let body = assert_report_body(&resp);
    assert_eq!(body["error_code"], "already_running");
    assert_eq!(body["state"], "running");
    let asked = Instant::now();
    let p = d.run("pause", &srv.db, &[]);
    assert!(p.ok(), "{p:?}");
    assert!(table_exists(&conn(&srv.db), "v9_control"));
    let first = runner.join().unwrap();
    let took = asked.elapsed();
    assert_eq!(first.state(), "paused", "{first:?}");
    assert!(took <= Duration::from_millis(3500), "pause took {took:?}");
    assert_eq!(d.run("resume", &srv.db, &[]).state(), "done");
}

/// During the backfill: reads 200 with x-audiolib-migration; path-only scope 503
/// migration_in_progress (Retry-After 30); scope=all 200 main-only with
/// x-audiolib-path-search: unavailable.
#[test]
#[ignore = "pending server migration HTTP (set AUDIOLIB_SMV2_ADMIN_URL, AUDIOLIB_SMV2_ADMIN_DB)"]
fn mg_15_http_reads_and_scopes_during_backfill() {
    let _g = server_lock();
    let srv = fresh_s8_server();
    let d = Driver::require();
    let id = any_id(&srv.db);
    let r = d.run("start", &srv.db, &[env(PAUSE_AFTER_BATCHES, 1)]);
    assert_eq!(r.state(), "paused", "{r:?}");
    let state = assert_report_body(&srv.status())["state"].clone();
    assert_eq!(state, "paused");
    for name in ["LIST", "MAIN_SEARCH", "PLAYBACK", "ARTWORK"] {
        let resp = srv.call(name, id);
        assert_eq!(resp.status, 200, "{name}: {resp:?}");
        assert_eq!(
            resp.header("x-audiolib-migration"),
            state.as_str(),
            "{name}: {resp:?}"
        );
    }
    let path = srv.call("PATH_SEARCH", id);
    assert_http_error(&path, 503, "migration_in_progress", "30");
    assert_eq!(
        path.header("x-audiolib-migration"),
        state.as_str(),
        "{path:?}"
    );
    let all = srv.call("ALL_SEARCH", id);
    assert_eq!(all.status, 200, "{all:?}");
    assert_eq!(
        all.header("x-audiolib-path-search"),
        Some("unavailable"),
        "{all:?}"
    );
    assert_eq!(
        all.header("x-audiolib-migration"),
        state.as_str(),
        "{all:?}"
    );
    let main = srv.call("MAIN_SEARCH", id);
    assert_eq!(
        all.json(),
        main.json(),
        "scope=all must return main-table results only"
    );
    assert_eq!(d.run("resume", &srv.db, &[]).state(), "done");
    let after = srv.call("LIST", id);
    assert_eq!(after.header("x-audiolib-migration"), None, "{after:?}");
    assert_eq!(srv.call("PATH_SEARCH", id).status, 200);
}

/// During `switching`: writes 503 migration_switching (Retry-After 30), reads 200.
/// The switch window is short; status is polled while a slowed run finishes.
#[test]
#[ignore = "pending server migration HTTP (set AUDIOLIB_SMV2_ADMIN_URL, AUDIOLIB_SMV2_ADMIN_DB)"]
fn mg_15_http_writes_rejected_during_switch() {
    let _g = server_lock();
    let srv = fresh_s8_server();
    let d = Arc::new(Driver::require());
    let id = any_id(&srv.db);
    let (d1, db1) = (d.clone(), srv.db.clone());
    // AUDIOLIB_MIGRATION_HOLD_SWITCH_MS (requested hook) keeps `switching` observable.
    let hold = std::env::var(HOLD_SWITCH_MS).ok();
    let runner = std::thread::spawn(move || {
        let env: Vec<_> = hold.into_iter().map(|v| (HOLD_SWITCH_MS, v)).collect();
        let mut last = d1.run("start", &db1, &env);
        for _ in 0..10 {
            if last.state() == "done" {
                break;
            }
            last = d1.run("resume", &db1, &env);
        }
        assert_eq!(last.state(), "done", "{last:?}");
    });
    let mut seen = false;
    while !runner.is_finished() {
        if srv.status().json()["state"] == "switching" {
            for name in ["VENDOR_IMPORT", "ANNOTATIONS"] {
                let resp = srv.call(name, id);
                if resp.header("x-audiolib-migration") != Some("switching") {
                    continue; // the switch ended between the two requests
                }
                assert_http_error(&resp, 503, "migration_switching", "30");
                seen = true;
            }
            let read = srv.call("LIST", id);
            assert_eq!(read.status, 200, "read during switch: {read:?}");
        }
    }
    runner.join().unwrap();
    assert!(
        seen,
        "never observed a write inside the switch window (set AUDIOLIB_MIGRATION_HOLD_SWITCH_MS)"
    );
}

/// After a failure and after a rollback: status shows state + error_code, and the
/// library endpoints behave as schema 8.
#[test]
#[ignore = "pending server migration HTTP (set AUDIOLIB_SMV2_ADMIN_URL, AUDIOLIB_SMV2_ADMIN_DB)"]
fn mg_14_http_failure_and_rollback_fall_back_to_s8() {
    let _g = server_lock();
    let srv = fresh_s8_server();
    let d = Driver::require();
    let id = any_id(&srv.db);
    let rb = srv.admin("rollback");
    assert!(rb.status >= 400, "{rb:?}");
    assert_eq!(assert_report_body(&rb)["error_code"], "not_migrated");
    let r = d.run("start", &srv.db, &[env(FAULT, "backup_fail")]);
    assert_eq!(r.state(), "failed", "{r:?}");
    let st = assert_report_body(&srv.status());
    assert_eq!(
        (st["state"].as_str(), st["error_code"].as_str()),
        (Some("failed"), Some("backup_failed"))
    );
    let s8_reads = |label: &str| {
        for name in ["LIST", "MAIN_SEARCH", "PLAYBACK", "ARTWORK", "ANNOTATIONS"] {
            let resp = srv.call(name, id);
            assert!(resp.status < 300, "{label} {name}: {resp:?}");
        }
    };
    s8_reads("after failure");
    assert_eq!(user_version(&srv.db), 8);
    d.migrate_to_done(&srv.db);
    let rb = srv.admin("rollback");
    assert_eq!(rb.status, 200, "{rb:?}");
    assert_eq!(assert_report_body(&rb)["state"], "rolled_back");
    let st = assert_report_body(&srv.status());
    assert_eq!(st["state"], "rolled_back");
    assert_eq!(user_version(&srv.db), 8);
    s8_reads("after rollback");
}
