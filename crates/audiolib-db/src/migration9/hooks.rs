// crates/audiolib-db/src/migration9/hooks.rs
//! Fault injection, compiled exclusively with the explicitly selected test-hooks feature.
use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    sync::{Mutex, OnceLock},
    time::Duration,
};

pub(super) fn number(key: &str) -> Option<u64> {
    std::env::var(key).ok()?.parse().ok()
}
pub(super) fn fault(point: &str) -> bool {
    std::env::var("AUDIOLIB_MIGRATION_FAULT").as_deref() == Ok(point)
}
pub(super) fn abort(point: &str) {
    if fault(point) {
        // Exit without unwinding: SQLite must recover the open transaction on restart.
        // Unlike abort(), this does not create a platform crash dump or dialog.
        std::process::exit(86);
    }
}
pub(super) fn slow(batch: usize) {
    if let Ok(value) = std::env::var("AUDIOLIB_MIGRATION_SLOW_BATCH")
        && let Some((n, ms)) = value.split_once(':')
        && n.parse::<usize>().ok() == Some(batch)
        && let Ok(ms) = ms.parse::<u64>()
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
}
static TRACE: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();
pub(super) fn trace(conn: &mut rusqlite::Connection, path: &Path) -> std::io::Result<()> {
    *TRACE.get_or_init(Default::default).lock().unwrap() =
        Some(OpenOptions::new().create(true).append(true).open(path)?);
    conn.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(|event| {
            if let rusqlite::trace::TraceEvent::Stmt(_, sql) = event
                && let Ok(mut file) = TRACE.get_or_init(Default::default).lock()
                && let Some(file) = file.as_mut()
            {
                let _ = writeln!(file, "{sql}");
            }
        }),
    );
    Ok(())
}
