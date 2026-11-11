// crates/audiolib-db/tests/smv2/driver.rs
//! Migration entrypoint for the MG cases: `audiolib-migrate` (audiolib-db binary,
//! built with feature `test-hooks`).
//!
//! Resolution: `AUDIOLIB_SMV2_MIGRATE_CMD` if set, else an `audiolib-migrate`
//! executable next to the test binary's target profile directory (present after
//! `cargo build -p audiolib-db --features test-hooks --bin audiolib-migrate`). Without
//! either, tests that need it are `#[ignore]`d with [`PENDING`]; run them with
//! `cargo test -p audiolib-db --features test-hooks --test smv2_mg -- --ignored`.
//!
//! Protocol (one process per call):
//!   `audiolib-migrate <action> --db <path> [--sql-trace <file>]`
//!   action ∈ startup | start | pause | resume | cancel | status | rollback
//!   `audiolib-migrate <main_search|path_search> --db <path> --q <q> [--limit N] [--cursor C]`
//! The last stdout line is JSON validated against
//! `doc/schemas/migration-report.schema.json` (search actions: `ids`, `next_cursor`).
//! Exit 0 = ok; nonzero → read `error_code`. A fault abort leaves no JSON.
//!
//! Test hooks (env):
//!   AUDIOLIB_MIGRATION_FAULT  abort:batch_boundary:<n> | abort:mid_batch:<n> |
//!                             abort:optimize | abort:switch_tx | sqlite_full:batch:<n> |
//!                             backup_fail   (abort:* = process abort, like kill -9)
//!   AUDIOLIB_MIGRATION_FAKE_FREE_BYTES      override the free-space probe
//!   AUDIOLIB_MIGRATION_PAUSE_AFTER_BATCHES  pause after n committed batches
//!   AUDIOLIB_MIGRATION_BATCH_ROWS           initial batch size
//!   AUDIOLIB_MIGRATION_SLOW_BATCH           `<n>:<ms>` sleep inside batch n
//!   AUDIOLIB_MIGRATION_HOLD_SWITCH_MS       (requested) hold `switching` for N ms
//!   AUDIOLIB_DEFER_SCHEMA9=1                `startup` leaves s8, state=deferred
//!   (`--sql-trace <file>` also exists under test-hooks)
//!
//! Cross-process control: `pause`/`cancel` write `v9_control(cmd, requested_at)`;
//! the runner checks between batches and stops within 2 s. `<db>.upgrade.lock`
//! holds the owner pid; a stale pid is reclaimed. Second `start` while running:
//! exit 2, error_code `already_running`, state = current state.
//!
//! Admin HTTP mirrors the CLI: POST /admin/api/migration/{start,pause,resume,cancel,
//! rollback}, GET /admin/api/migration/status, same JSON (see smv2/http.rs).
use super::schema;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
};

pub const ENV_CMD: &str = "AUDIOLIB_SMV2_MIGRATE_CMD";
pub const PENDING: &str =
    "pending server migration branch (audiolib-migrate not found; set AUDIOLIB_SMV2_MIGRATE_CMD)";
pub const FAKE_FREE_BYTES: &str = "AUDIOLIB_MIGRATION_FAKE_FREE_BYTES";
pub const PAUSE_AFTER_BATCHES: &str = "AUDIOLIB_MIGRATION_PAUSE_AFTER_BATCHES";
pub const BATCH_ROWS: &str = "AUDIOLIB_MIGRATION_BATCH_ROWS";
pub const SLOW_BATCH: &str = "AUDIOLIB_MIGRATION_SLOW_BATCH";
pub const FAULT: &str = "AUDIOLIB_MIGRATION_FAULT";
/// Requested hook: hold the `switching` state for N ms (used when set).
pub const HOLD_SWITCH_MS: &str = "AUDIOLIB_MIGRATION_HOLD_SWITCH_MS";

pub struct Driver {
    program: PathBuf,
}

#[derive(Debug)]
pub struct Outcome {
    pub code: Option<i32>,
    pub report: Value,
    pub stdout: String,
    pub stderr: String,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
    /// Killed by an abort fault: nonzero exit without a JSON report.
    pub fn aborted(&self) -> bool {
        !self.ok() && self.report.is_null()
    }
    pub fn state(&self) -> &str {
        self.report["state"].as_str().unwrap_or("")
    }
    pub fn error_code(&self) -> Option<&str> {
        self.report["error_code"].as_str()
    }
    pub fn progress(&self) -> (u64, u64) {
        let p = &self.report["progress"];
        (
            p["done"].as_u64().unwrap_or(0),
            p["total"].as_u64().unwrap_or(0),
        )
    }
    pub fn phase_ms(&self, name: &str) -> Option<f64> {
        self.report["phases"][name].as_f64()
    }
    /// `batches[{rows,ms}]` (guaranteed by the agreed report).
    pub fn batches(&self) -> Vec<(u64, f64)> {
        self.report["batches"]
            .as_array()
            .unwrap_or_else(|| panic!("report lacks batches: {self:?}"))
            .iter()
            .map(|x| {
                (
                    x["rows"].as_u64().unwrap_or(0),
                    x["ms"].as_f64().unwrap_or(0.0),
                )
            })
            .collect()
    }
    pub fn eta_s(&self) -> &Value {
        &self.report["eta_s"]
    }
}

impl Driver {
    pub fn from_env() -> Option<Driver> {
        if let Some(p) = std::env::var_os(ENV_CMD) {
            return Some(Driver { program: p.into() });
        }
        // target/<profile>/deps/<test>.exe → target/<profile>/audiolib-migrate
        let exe = std::env::current_exe().ok()?;
        let candidate = exe
            .parent()?
            .parent()?
            .join(format!("audiolib-migrate{}", std::env::consts::EXE_SUFFIX));
        candidate.is_file().then_some(Driver { program: candidate })
    }

    /// The driver, or a panic carrying [`PENDING`] (only reached under `--ignored`).
    pub fn require() -> Driver {
        Driver::from_env().unwrap_or_else(|| panic!("{PENDING}"))
    }

    fn exec(&self, args: &[&std::ffi::OsStr], env: &[(&str, String)]) -> Outcome {
        let mut cmd = Command::new(&self.program);
        cmd.args(args);
        // Only the switching-observation case explicitly forwards this hook.
        // Inheriting it in other cases would invalidate their write-lock timings.
        cmd.env_remove(HOLD_SWITCH_MS);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("spawn audiolib-migrate");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let report = stdout
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .and_then(|l| serde_json::from_str::<Value>(l.trim()).ok())
            .unwrap_or(Value::Null);
        Outcome {
            code: out.status.code(),
            report,
            stdout,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    pub fn run(&self, action: &str, db: &Path, env: &[(&str, String)]) -> Outcome {
        self.run_traced(action, db, env, None)
    }

    pub fn run_traced(
        &self,
        action: &str,
        db: &Path,
        env: &[(&str, String)],
        sql_trace: Option<&Path>,
    ) -> Outcome {
        let mut args: Vec<&std::ffi::OsStr> =
            vec![action.as_ref(), "--db".as_ref(), db.as_os_str()];
        if let Some(t) = sql_trace {
            args.extend([std::ffi::OsStr::new("--sql-trace"), t.as_os_str()]);
        }
        let out = self.exec(&args, env);
        if !out.aborted() {
            let errors = schema::validate(schema::report_schema(), &out.report);
            assert!(
                errors.is_empty(),
                "{action}: report violates schema: {errors:?}\n{out:?}"
            );
            if !out.ok() {
                assert!(
                    out.error_code().is_some(),
                    "{action}: nonzero exit without error_code: {out:?}"
                );
            }
        }
        out
    }

    /// All ids for `q` from `main_search` / `path_search`, following `next_cursor`.
    pub fn search(&self, action: &str, db: &Path, q: &str) -> BTreeSet<i64> {
        let mut ids = BTreeSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10_000 {
            let mut args: Vec<&std::ffi::OsStr> = vec![
                action.as_ref(),
                "--db".as_ref(),
                db.as_os_str(),
                "--q".as_ref(),
                q.as_ref(),
                "--limit".as_ref(),
                "500".as_ref(),
            ];
            if let Some(c) = &cursor {
                args.extend([std::ffi::OsStr::new("--cursor"), c.as_ref()]);
            }
            let out = self.exec(&args, &[]);
            assert!(out.ok(), "{action} {q}: {out:?}");
            let errors = schema::validate_search(&out.report);
            assert!(
                errors.is_empty(),
                "{action}: output violates schema: {errors:?}"
            );
            let page: Vec<i64> = serde_json::from_value(out.report["ids"].clone()).unwrap();
            let before = ids.len();
            ids.extend(page.iter().copied());
            assert!(
                ids.len() - before == page.len(),
                "{action} {q}: duplicate ids across pages"
            );
            match out.report["next_cursor"].as_str() {
                Some(c) if !page.is_empty() => cursor = Some(c.to_string()),
                _ => return ids,
            }
        }
        panic!("{action} {q}: cursor did not terminate");
    }

    /// Run `start`, then `resume` until done (bounded), returning the last outcome.
    pub fn migrate_to_done(&self, db: &Path) -> Outcome {
        let mut last = self.run("start", db, &[]);
        for _ in 0..10 {
            if last.state() == "done" {
                break;
            }
            last = self.run("resume", db, &[]);
        }
        assert_eq!(last.state(), "done", "migration did not finish: {last:?}");
        last
    }
}
