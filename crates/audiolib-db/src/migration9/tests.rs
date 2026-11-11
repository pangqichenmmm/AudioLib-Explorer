// crates/audiolib-db/src/migration9/tests.rs
//! Control admission at the final backfill/publication boundary.
use super::*;
use std::sync::mpsc;

#[cfg(windows)]
#[test]
fn durable_report_publication_during_coordinated_readers_keeps_complete_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    drop(crate::SqliteProvider::open(&path).unwrap());
    let conn = open(&path, false).unwrap();
    let mut saved = load(&path, &conn).unwrap();
    save(&path, &saved).unwrap();
    let stop = AtomicBool::new(false);
    let outcome = std::thread::scope(|scope| {
        for _ in 0..4 {
            let stop = &stop;
            let path = &path;
            scope.spawn(move || {
                let reader = open(path, false).unwrap();
                while !stop.load(Ordering::Acquire) {
                    // The production load/status path participates in the same
                    // stable lease. Uncoordinated raw/AV readers are not claimed.
                    let current = load(path, &reader).unwrap();
                    assert_eq!(current.library_id, identity(&reader).unwrap());
                }
            });
        }
        let result = (|| {
            for sequence in 0..500 {
                saved.report.progress.done = sequence;
                save(&path, &saved)?;
            }
            Ok::<_, Error>(())
        })();
        stop.store(true, Ordering::Release);
        result
    });
    outcome.unwrap();
    assert_eq!(load(&path, &conn).unwrap().report.progress.done, 499);
}

#[cfg(windows)]
#[test]
fn durable_report_readonly_legacy_status_creates_no_coordination_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    drop(crate::SqliteProvider::open(&path).unwrap());
    let conn = open(&path, false).unwrap();
    let saved = load(&path, &conn).unwrap();
    let report = sidecar(&path, ".migration9.json");
    fs::write(&report, serde_json::to_vec(&saved).unwrap()).unwrap();
    let before = fs::read(&report).unwrap();
    let original_permissions = fs::metadata(&path).unwrap().permissions();
    let mut permissions = original_permissions.clone();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).unwrap();
    let result = status(&path);
    fs::set_permissions(&path, original_permissions).unwrap();
    assert_eq!(result.unwrap().state, State::Idle);
    assert_eq!(fs::read(&report).unwrap(), before);
    assert!(!sidecar(&path, ".migration9-report.lock").exists());
}

#[cfg(windows)]
#[test]
fn durable_report_real_readonly_refusal_preserves_existing_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    drop(crate::SqliteProvider::open(&path).unwrap());
    let conn = open(&path, false).unwrap();
    let mut saved = load(&path, &conn).unwrap();
    save(&path, &saved).unwrap();
    let report = sidecar(&path, ".migration9.json");
    let before = fs::read(&report).unwrap();
    let original_permissions = fs::metadata(&report).unwrap().permissions();
    let mut permissions = original_permissions.clone();
    permissions.set_readonly(true);
    fs::set_permissions(&report, permissions).unwrap();
    saved.report.progress.done = 123;
    let result = save(&path, &saved);
    fs::set_permissions(&report, original_permissions).unwrap();
    assert!(
        matches!(result,Err(Error::Io(ref error)) if error.kind()==std::io::ErrorKind::PermissionDenied)
    );
    assert_eq!(fs::read(&report).unwrap(), before);
}

#[test]
fn an_existing_sqlite_writer_is_locked_and_preserves_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    drop(crate::SqliteProvider::open(&path).unwrap());
    let blocker = open(&path, true).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    // Retain the competing transaction until the migration's complete admission
    // deadline expires. This reproduces contention without scheduler luck.
    let started = Instant::now();
    let report = execute(
        &path,
        Action::Start,
        &Options::default(),
        &AtomicBool::new(false),
    );
    let elapsed = started.elapsed();
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(report.error_code, Some(Failure::Locked), "{report:?}");
    assert_eq!(report.state, State::Paused);
    assert!(elapsed < Duration::from_secs(11), "{elapsed:?}");
    assert_eq!(version(&blocker).unwrap(), 8);
    assert!(!exists(&blocker, "v9_checkpoint").unwrap());
    assert!(crate::schema::current_format_ready(&blocker).unwrap());
}

#[test]
fn admission_survives_a_writer_held_longer_than_the_old_busy_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    drop(crate::SqliteProvider::open(&path).unwrap());
    let (holding, held) = mpsc::channel();
    std::thread::scope(|scope| {
        let path = &path;
        scope.spawn(move || {
            let blocker = open(path, true).unwrap();
            blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
            holding.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(2200));
            blocker.execute_batch("ROLLBACK").unwrap();
        });
        held.recv_timeout(Duration::from_secs(5)).unwrap();
        let report = execute(
            path,
            Action::Start,
            &Options::default(),
            &AtomicBool::new(false),
        );
        assert_eq!(report.state, State::Done, "{report:?}");
        assert_eq!(report.error_code, None);
        assert_eq!(report.switch.unwrap().user_version, 9);
    });
}

#[test]
fn admission_cancels_promptly_without_losing_a_checkpoint_and_can_resume() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    drop(crate::SqliteProvider::open(&path).unwrap());
    // Let the real migration create its verified backup/checkpoint, then pause
    // before publication. A blocked resume must leave that durable state intact.
    let cancel = AtomicBool::new(true);
    let mut conn = open(&path, true).unwrap();
    let mut saved = load(&path, &conn).unwrap();
    saved.report.state = State::Running;
    storage::backup(&path, &conn, &mut saved, &AtomicBool::new(false)).unwrap();
    storage::prepare(&mut conn, &AtomicBool::new(false)).unwrap();
    save(&path, &saved).unwrap();
    let before: (i64, i64) = conn
        .query_row("SELECT done,total FROM v9_checkpoint", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    cancel.store(false, Ordering::Release);
    let started = Instant::now();
    let report = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            cancel.store(true, Ordering::Release);
        });
        execute(&path, Action::Resume, &Options::default(), &cancel)
    });
    let elapsed = started.elapsed();
    conn.execute_batch("ROLLBACK").unwrap();
    assert_eq!(report.state, State::Paused, "{report:?}");
    assert_eq!(report.error_code, None);
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    let after: (i64, i64) = conn
        .query_row("SELECT done,total FROM v9_checkpoint", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(before, after);
    let resumed = execute(
        &path,
        Action::Resume,
        &Options::default(),
        &AtomicBool::new(false),
    );
    assert_eq!(resumed.state, State::Done, "{resumed:?}");
    assert_eq!(resumed.error_code, None);
}

#[test]
fn control_requests_remain_excluded_by_library_maintenance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    drop(crate::SqliteProvider::open(&path).unwrap());
    let mut conn = open(&path, true).unwrap();
    storage::prepare(&mut conn, &AtomicBool::new(false)).unwrap();
    let _administration = AdministrationLease::exclusive(&path).unwrap();
    assert!(matches!(
        storage::request_control(&path, Action::Pause),
        Err(Error::Busy(_))
    ));
    assert_eq!(
        conn.query_row("SELECT count(*) FROM v9_control", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn publication_drains_admitted_control_before_taking_exclusive_administration() {
    for action in [Action::Pause, Action::Cancel] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        drop(crate::SqliteProvider::open(&path).unwrap());
        let mut conn = open(&path, true).unwrap();
        storage::prepare(&mut conn, &AtomicBool::new(false)).unwrap();
        let mut saved = load(&path, &conn).unwrap();
        saved.report.state = State::Running;
        save(&path, &saved).unwrap();

        // Hold exactly the leases of a control request admitted while the last
        // batch owns SQLite's writer. Publication must let that request commit
        // and release its shared administration lease before attempting upgrade.
        let control = storage::ControlLease::acquire(&path).unwrap();
        let administration = AdministrationLease::shared(&path).unwrap();
        std::thread::scope(|scope| {
            let (started, starting) = mpsc::channel();
            let (finished, completion) = mpsc::channel();
            let path = &path;
            scope.spawn(move || {
                let mut publisher = open(path, true).unwrap();
                started.send(()).unwrap();
                let result =
                    storage::publish(path, &mut publisher, &mut saved, &AtomicBool::new(false));
                finished.send((result, saved.report)).unwrap();
            });
            starting.recv_timeout(Duration::from_secs(5)).unwrap();
            let pending = completion.recv_timeout(Duration::from_millis(100));
            assert!(
                matches!(pending, Err(mpsc::RecvTimeoutError::Timeout)),
                "publication overtook an admitted control request: {pending:?}"
            );
            conn.execute(
                "INSERT INTO v9_control VALUES(1,?1,'test')",
                [if action == Action::Cancel {
                    "cancel"
                } else {
                    "pause"
                }],
            )
            .unwrap();
            drop(administration);
            drop(control);
            let (result, report) = completion.recv_timeout(Duration::from_secs(5)).unwrap();
            result.unwrap();
            assert_eq!(
                report.state,
                if action == Action::Cancel {
                    State::Cancelled
                } else {
                    State::Paused
                }
            );
            assert_eq!(report.phases["switch"], 0.0);
            assert_eq!(version(&conn).unwrap(), 8);
        });
    }
}
