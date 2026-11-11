// app/audiolib-desktop/src/panic_diagnostics.rs
//! Best-effort synchronous panic evidence, independent of the tracing worker.
use std::{
    backtrace::Backtrace,
    fs::{File, OpenOptions},
    io::{self, Write},
    panic::PanicHookInfo,
    path::Path,
    sync::{Mutex, TryLockError},
    thread::Thread,
    time::{SystemTime, UNIX_EPOCH},
};

const LOG_FILE: &str = "AudioLibExplorer-panic.log";

/// Pre-opened sink used only after a panic, never by ordinary audio callbacks.
pub(super) struct PanicLog {
    file: Mutex<File>,
}

impl PanicLog {
    /// Failure only disables this extra sink; the existing panic hook still runs.
    pub(super) fn open(directory: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join(LOG_FILE))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// Persist the caller's one captured backtrace without formatting the payload.
    /// A concurrent/reentrant panic never waits on this diagnostic mutex. File
    /// errors are ignored so diagnostics cannot turn an I/O failure into a panic.
    pub(super) fn record(&self, info: &PanicHookInfo<'_>, thread: &Thread, trace: &Backtrace) {
        let mut file = match self.file.try_lock() {
            Ok(file) => file,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis());
        let _ = writeln!(
            file,
            "=== panic unix_ms={timestamp} pid={} thread_id={:?} thread_name={:?} ===",
            std::process::id(),
            thread.id(),
            thread.name().unwrap_or("unnamed")
        );
        if let Some(location) = info.location() {
            let _ = writeln!(
                file,
                "location={}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            );
        } else {
            let _ = writeln!(file, "location=unavailable");
        }
        let _ = writeln!(
            file,
            "payload=omitted; application configuration and command-line arguments are not recorded"
        );
        let _ = writeln!(file, "backtrace:\n{trace}\n=== end panic ===");
        let _ = file.flush();
        let _ = file.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    const TEST_DIRECTORY: &str = "AUDIOLIB_PANIC_TEST_LOG_DIR";
    const SECRET: &str = "SECRET_PANIC_PAYLOAD_MUST_NOT_BE_RECORDED";

    #[test]
    fn synchronous_record_survives_abort_without_panic_payload() {
        let root = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "panic_diagnostics::tests::child_abort_fixture",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env(TEST_DIRECTORY, root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Resolving a large debug backtrace can take longer on slower disks.
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < Duration::from_secs(120) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("The owned abort fixture exceeded its 120-second deadline");
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("Could not wait for the owned abort fixture: {error}");
                }
            }
        };
        assert!(
            !status.success(),
            "The fixture must abort before normal teardown"
        );
        let record = std::fs::read_to_string(root.path().join(LOG_FILE)).unwrap();
        assert!(record.contains("panic-fixture-thread"));
        assert!(record.contains("location="));
        assert!(record.contains("panic_diagnostics.rs"));
        assert!(record.contains("payload=omitted"));
        assert!(record.contains("backtrace:"));
        assert!(record.contains("=== end panic ==="));
        assert!(!record.contains(SECRET));
    }

    #[test]
    #[ignore = "private subprocess abort fixture with an isolated owned log directory"]
    fn child_abort_fixture() {
        let Some(directory) = std::env::var_os(TEST_DIRECTORY) else {
            return;
        };
        let log = PanicLog::open(Path::new(&directory)).unwrap();
        std::panic::set_hook(Box::new(move |info| {
            let trace = Backtrace::force_capture();
            log.record(info, &std::thread::current(), &trace);
            std::process::abort();
        }));
        let _ = std::thread::Builder::new()
            .name("panic-fixture-thread".into())
            .spawn(|| panic!("{SECRET}"))
            .unwrap()
            .join();
    }
}
