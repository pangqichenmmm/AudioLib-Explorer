// crates/audiolib-db/src/bin/audiolib-migrate.rs
//! Explicit schema-9 migration tool. Never creates a missing library.
use audiolib_db::migration9::{self, Action, Failure, Options, Report, State};
use std::{path::PathBuf, sync::atomic::AtomicBool};

fn main() {
    let report = run().unwrap_or_else(|e| {
        eprintln!("{e}");
        Report {
            state: State::Failed,
            error_code: Some(Failure::IoError),
            ..Default::default()
        }
    });
    println!(
        "{}",
        serde_json::to_string(&report).expect("serializable migration report")
    );
    std::process::exit(report.exit_code());
}
fn run() -> Result<Report, Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let action = args
        .next()
        .ok_or("usage: audiolib-migrate <action> --db <path>")?;
    let action = action.to_str().ok_or("invalid action")?;
    let mut db: Option<PathBuf> = None;
    let mut query = String::new();
    let mut limit = 100;
    let mut cursor = None;
    #[allow(unused_mut)]
    let mut options = Options::default();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or("option requires a value")?;
        match flag.to_str().ok_or("invalid option")? {
            "--db" => db = Some(value.into()),
            "--q" => query = value.into_string().map_err(|_| "invalid query")?,
            "--limit" => limit = value.to_str().ok_or("invalid limit")?.parse()?,
            "--cursor" => cursor = Some(value.into_string().map_err(|_| "invalid cursor")?),
            #[cfg(feature = "test-hooks")]
            "--sql-trace" => options.sql_trace = Some(value.into()),
            _ => return Err("unknown option (fault injection requires test-hooks)".into()),
        }
    }
    let db = db.ok_or("--db is required")?;
    if matches!(action, "main_search" | "path_search") {
        return Ok(migration9::search(
            &db,
            query,
            if action == "main_search" {
                audiolib_core::SearchScope::Main
            } else {
                audiolib_core::SearchScope::Path
            },
            limit,
            cursor,
        )?);
    }
    Ok(migration9::execute(
        &db,
        action.parse::<Action>()?,
        &options,
        &AtomicBool::new(false),
    ))
}
