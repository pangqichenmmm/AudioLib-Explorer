// app/audiolib-cli/src/migration.rs
//! Explicit local migration commands. No provider or default settings are opened.
use audiolib_core::compatibility::LibraryAccess;
use audiolib_db::migration::{self, MigrationPlan, MigrationProgress};
use clap::Subcommand;
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Commands operate only on the explicit existing `--database` path.
#[derive(Subcommand)]
pub(super) enum Command {
    /// Diagnose actual business readability, references and safe repairability without writes.
    Diagnose,
    /// Read the actual format and compatibility; never opens the library for writing.
    Inspect {
        /// Hash every business value as well as checking the format.
        #[arg(long)]
        deep: bool,
        /// Evaluate write compatibility instead of read compatibility.
        #[arg(long)]
        writable: bool,
    },
    /// Create a source-bound plan file. The source is read-only and output must be new.
    Plan {
        #[arg(long)]
        output: PathBuf,
        /// Import old local playback history after backup. Omit for server/snapshot jobs.
        #[arg(long)]
        local_history_root: Option<PathBuf>,
        /// Maximum local event rows; 0 disables recording and the legacy event import.
        #[arg(long, default_value_t = audiolib_core::config::PLAY_HISTORY_LIMIT_PER_LIBRARY)]
        history_limit: u32,
    },
    /// Apply a reviewed plan; rejects changed source data or a different tool recipe.
    Apply {
        #[arg(long)]
        plan: PathBuf,
    },
    /// Continue the active durable job discovered next to this library.
    Resume,
    /// Verify the active job's backup, business projection and actual format.
    Verify,
    /// Restore the active job's backup if later business/schema changes would not be lost.
    Rollback,
}

/// Install cancellation once per CLI process; committed boundaries remain resumable.
pub(super) fn run(database: Option<&Path>, command: &Command) -> anyhow::Result<Value> {
    let database = database.ok_or_else(|| {
        anyhow::anyhow!("Migration commands require --database <existing-library.db>")
    })?;
    let cancel = Arc::new(AtomicBool::new(false));
    let signal = cancel.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Release))?;
    execute(database, command, &cancel, &mut |event| {
        // Local diagnostics may include recovery paths. Stdout remains one JSON reply.
        eprintln!("{}", serde_json::to_string(&event).expect("progress JSON"));
        if cancel.load(Ordering::Acquire) {
            Err(audiolib_core::Error::Cancelled)
        } else {
            Ok(())
        }
    })
}

fn read_plan(path: &Path) -> anyhow::Result<MigrationPlan> {
    const LIMIT: u64 = 16 * 1024 * 1024;
    let mut bytes = Vec::new();
    File::open(path)?.take(LIMIT + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= LIMIT, "Migration plan exceeds 16 MiB");
    Ok(serde_json::from_slice(&bytes)?)
}

fn execute(
    database: &Path,
    command: &Command,
    cancel: &AtomicBool,
    observer: &mut dyn FnMut(MigrationProgress) -> audiolib_core::Result<()>,
) -> anyhow::Result<Value> {
    let database = database.canonicalize()?;
    match command {
        Command::Diagnose => Ok(json!(migration::diagnose(&database, cancel)?)),
        Command::Inspect { deep, writable } => Ok(json!(migration::inspect(
            &database,
            if *writable {
                LibraryAccess::Write
            } else {
                LibraryAccess::Read
            },
            *deep,
            cancel
        )?)),
        Command::Plan {
            output,
            local_history_root,
            history_limit,
        } => {
            // Check a durable local import owner before calling privileged planner APIs.
            // Read-only plans do not create an import sidecar when one is absent.
            let local = local_history_root
                .as_ref()
                .map(|root| -> anyhow::Result<_> {
                    Ok(migration::LocalHistoryMigration {
                        data_root: root.canonicalize()?,
                        library_id: audiolib_db::local_play_history::local_library_id(&database)?,
                        limit: *history_limit,
                    })
                })
                .transpose()?;
            let _owner = migration::admit_import_owner(&database, local, false)?;
            let plan = match local_history_root {
                Some(root) => migration::plan_local(&database, root, *history_limit, cancel)?,
                None => migration::plan(&database, cancel)?,
            };
            let bytes = serde_json::to_vec_pretty(&plan)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)?;
            file.write_all(&bytes)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            Ok(json!({"plan_file": output.canonicalize()?, "plan": plan}))
        }
        Command::Apply { plan } => {
            let plan = read_plan(plan)?;
            anyhow::ensure!(
                plan.source.source == database,
                "Plan belongs to a different library; --database must match its canonical source"
            );
            let _owner =
                migration::admit_import_owner(&database, plan.local_history.clone(), false)?;
            Ok(json!(migration::apply(&plan, cancel, observer)?))
        }
        Command::Resume | Command::Verify | Command::Rollback => {
            let job = migration::discover(&database)?
                .ok_or_else(|| anyhow::anyhow!("No active migration job for this library"))?;
            let recovery = matches!(command, Command::Verify | Command::Rollback)
                || job.phase == migration::MigrationPhase::RollingBack;
            let _owner =
                migration::admit_import_owner(&database, job.plan.local_history.clone(), recovery)?;
            let result = match command {
                Command::Resume => json!(migration::resume(&job.directory, cancel, observer)?),
                Command::Verify => json!({
                    "job_directory": job.directory,
                    "business": migration::verify(&job.directory, cancel)?,
                }),
                Command::Rollback => json!(migration::rollback(&job.directory, cancel, observer)?),
                _ => unreachable!(),
            };
            Ok(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiolib_db::SqliteProvider;

    #[test]
    fn explicit_plan_and_apply_cannot_bypass_a_pending_local_import_owner() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.db");
        let path = root.path().join("imported.db");
        let data = root.path().join("client-data");
        let conn = rusqlite::Connection::open(&source).unwrap();
        audiolib_db::register_library_functions(&conn).unwrap();
        conn.execute_batch(include_str!(
            "../../../crates/audiolib-db/tests/fixtures/schema-v5.sql"
        ))
        .unwrap();
        conn.execute_batch("INSERT INTO audio_files(id,file_path,filename) VALUES(1,'one.wav','one.wav');
          INSERT INTO play_history(audio_file_id,played_at,filename,file_path) VALUES(1,'invalid UTC','one.wav','one.wav');").unwrap();
        drop(conn);
        assert!(audiolib_db::import_legacy_local(&source, &path, &data, 5000).is_err());
        assert!(path.is_file());
        assert_eq!(
            migration::discover(&path).unwrap().unwrap().phase,
            migration::MigrationPhase::Backup
        );
        let original = std::fs::read(&path).unwrap();
        let cancel = AtomicBool::new(false);
        let output = root.path().join("server-plan.json");
        let command = Command::Plan {
            output: output.clone(),
            local_history_root: None,
            history_limit: 5000,
        };
        assert!(execute(&path, &command, &cancel, &mut |_| Ok(())).is_err());
        assert!(!output.exists());
        // The application boundary must also reject a privileged raw plan loaded
        // from disk, before it can create another job or bypass the local import.
        let raw = migration::plan(&path, &cancel).unwrap();
        std::fs::write(&output, serde_json::to_vec(&raw).unwrap()).unwrap();
        let job_before = migration::discover(&path).unwrap().unwrap().id;
        assert!(
            execute(
                &path,
                &Command::Apply { plan: output },
                &cancel,
                &mut |_| Ok(())
            )
            .is_err()
        );
        assert_eq!(migration::discover(&path).unwrap().unwrap().id, job_before);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn inspection_does_not_create_a_missing_library_or_upgrade_an_existing_one() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("library.db");
        let command = Command::Inspect {
            deep: true,
            writable: true,
        };
        let cancel = AtomicBool::new(false);
        assert!(execute(&path, &command, &cancel, &mut |_| Ok(())).is_err());
        assert!(!path.exists());
        drop(SqliteProvider::open(&path).unwrap());
        let before = std::fs::read(&path).unwrap();
        let result = execute(&path, &command, &cancel, &mut |_| Ok(())).unwrap();
        assert_eq!(
            result["format"]["schema_version"],
            audiolib_db::SCHEMA_VERSION
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(migration::discover(&path).unwrap().is_none());
    }

    #[test]
    fn diagnosis_requires_an_existing_path_and_emits_evidence_without_a_job() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("library.db");
        let cancel = AtomicBool::new(false);
        assert!(run(None, &Command::Diagnose).is_err());
        assert!(execute(&path, &Command::Diagnose, &cancel, &mut |_| Ok(())).is_err());
        assert!(!path.exists());
        drop(SqliteProvider::open(&path).unwrap());
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA journal_mode=DELETE;")
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let result = execute(&path, &Command::Diagnose, &cancel, &mut |_| {
            panic!("diagnosis cannot mutate")
        })
        .unwrap();
        assert_eq!(result["automatic_rebuild_allowed"], false);
        assert_eq!(
            result["inspection"]["format"]["schema_version"],
            audiolib_db::SCHEMA_VERSION
        );
        assert_eq!(result["findings"], json!([]));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(migration::discover(&path).unwrap().is_none());
    }

    #[test]
    fn apply_rejects_a_plan_for_another_library_before_creating_a_job() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.db");
        let second = root.path().join("second.db");
        for path in [&first, &second] {
            drop(SqliteProvider::open(path).unwrap());
            // An intentionally stale derived revision gives the registered repair route.
            rusqlite::Connection::open(path)
                .unwrap()
                .execute("DELETE FROM library_meta WHERE key='source_search_rev'", [])
                .unwrap();
        }
        let cancel = AtomicBool::new(false);
        let output = root.path().join("plan.json");
        let command = Command::Plan {
            output: output.clone(),
            local_history_root: None,
            history_limit: audiolib_core::config::PLAY_HISTORY_LIMIT_PER_LIBRARY,
        };
        execute(&first, &command, &cancel, &mut |_| Ok(())).unwrap();
        let bytes = std::fs::read(&output).unwrap();
        assert!(execute(&first, &command, &cancel, &mut |_| Ok(())).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), bytes);
        let error = execute(
            &second,
            &Command::Apply { plan: output },
            &cancel,
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("different library"));
        assert!(migration::discover(&first).unwrap().is_none());
        assert!(migration::discover(&second).unwrap().is_none());
    }

    #[test]
    fn commands_resume_a_committed_job_verify_and_hold_after_rollback() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("library.db");
        drop(SqliteProvider::open(&path).unwrap());
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute("DELETE FROM library_meta WHERE key='source_search_rev'", [])
            .unwrap();
        let cancel = AtomicBool::new(false);
        let plan_file = root.path().join("plan.json");
        execute(
            &path,
            &Command::Plan {
                output: plan_file.clone(),
                local_history_root: None,
                history_limit: audiolib_core::config::PLAY_HISTORY_LIMIT_PER_LIBRARY,
            },
            &cancel,
            &mut |_| Ok(()),
        )
        .unwrap();
        let interrupted = execute(
            &path,
            &Command::Apply { plan: plan_file },
            &cancel,
            &mut |event| {
                if event.phase == migration::MigrationPhase::Validating {
                    Err(audiolib_core::Error::Cancelled)
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(
            interrupted
                .unwrap_err()
                .downcast_ref::<audiolib_core::Error>(),
            Some(audiolib_core::Error::Cancelled)
        ));
        assert!(migration::discover(&path).unwrap().is_some());
        let result = execute(&path, &Command::Resume, &cancel, &mut |_| Ok(())).unwrap();
        assert_eq!(result["phase"], "Complete");
        execute(&path, &Command::Verify, &cancel, &mut |_| Ok(())).unwrap();
        let result = execute(&path, &Command::Rollback, &cancel, &mut |_| Ok(())).unwrap();
        assert_eq!(result["phase"], "RolledBack");
        execute(&path, &Command::Verify, &cancel, &mut |_| Ok(())).unwrap();
        assert!(matches!(
            SqliteProvider::open_existing(&path),
            Err(audiolib_core::Error::Busy(_))
        ));
    }
}
