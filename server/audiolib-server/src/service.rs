// server/audiolib-server/src/service.rs
//! Listen-first readiness: the HTTP listener is served immediately while the library opens
//! in the background, so a slow or blocked open (for example a macOS removable-volume
//! permission prompt) never leaves the port dead.
//!
//! Until the library is ready, `GET /health` reports `starting` and authenticated endpoints
//! answer 503 `UNAVAILABLE`. A failed open is logged, reported as `error`, and retried with
//! exponential backoff instead of exiting, so launchd does not crash-loop the process.
use crate::{
    ApiError, AppState, ServerConfig, auth::Security, config::BearerToken,
    state::spawn_revision_poller,
};
use std::{
    sync::{
        Arc, PoisonError, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Fixed client-facing text for a failed library open (`/health` and 503 replies).
pub const FAILED_MESSAGE: &str = "The library could not be opened; the server keeps retrying";

/// Library lifecycle as seen by request handlers.
#[derive(Clone)]
pub enum Readiness {
    /// The library is still being opened.
    Starting,
    /// The library is open and endpoints are served.
    Ready(AppState),
    /// The last open attempt failed with this error (logged, never sent to clients);
    /// a retry is pending.
    Failed(String),
}

struct ServiceInner {
    security: Arc<Security>,
    readiness: RwLock<Readiness>,
    progress: RwLock<Option<audiolib_db::SnapshotUpgradeProgress>>,
    updates: crate::update::UpdateManager,
    stopping: Arc<AtomicBool>,
}

/// Router state: authentication and admin settings ([`Security`], known before the
/// library opens) plus [`Readiness`].
#[derive(Clone)]
pub struct Service {
    inner: Arc<ServiceInner>,
}

impl Service {
    /// A service whose library is not open yet with fixed in-memory security: the token
    /// is required everywhere, `None` disables authentication ([`Security::fixed`]).
    pub fn starting(token: impl Into<Option<BearerToken>>) -> Self {
        Self::with(Security::fixed(token.into()), Readiness::Starting)
    }

    /// A service whose library is not open yet, using `security` (built from the
    /// configuration by [`Security::from_config`]).
    pub fn starting_with(security: Arc<Security>) -> Self {
        Self::with(security, Readiness::Starting)
    }

    fn with(security: Arc<Security>, readiness: Readiness) -> Self {
        Self {
            inner: Arc::new(ServiceInner {
                security,
                readiness: RwLock::new(readiness),
                progress: RwLock::new(None),
                updates: crate::update::UpdateManager::default(),
                stopping: Arc::new(AtomicBool::new(false)),
            }),
        }
    }

    /// Current readiness (a cheap clone).
    pub fn readiness(&self) -> Readiness {
        self.inner
            .readiness
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stop startup retries and request a durable-boundary cancellation of an upgrade.
    pub fn request_stop(&self) {
        self.inner.stopping.store(true, Ordering::Release);
    }

    pub(crate) fn migration_cancellation(&self) -> Arc<AtomicBool> {
        self.inner.stopping.clone()
    }

    /// Last upgrade phase; contains only stage names and committed row counts.
    pub fn upgrade_progress(&self) -> Option<audiolib_db::SnapshotUpgradeProgress> {
        *self
            .inner
            .progress
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Publish bounded in-memory startup progress without exposing database paths.
    pub fn set_upgrade_progress(&self, progress: audiolib_db::SnapshotUpgradeProgress) {
        *self
            .inner
            .progress
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(progress);
    }

    /// Mark the library as open; later [`Service::set_failed`] calls are ignored.
    pub fn set_ready(&self, state: AppState) {
        *self
            .inner
            .progress
            .write()
            .unwrap_or_else(PoisonError::into_inner) = None;
        *self
            .inner
            .readiness
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Readiness::Ready(state);
    }

    /// Record a failed open attempt unless the library is already open.
    pub fn set_failed(&self, message: impl Into<String>) {
        let mut readiness = self
            .inner
            .readiness
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if !matches!(*readiness, Readiness::Ready(_)) {
            *readiness = Readiness::Failed(message.into());
        }
    }

    /// The open library, or 503 `UNAVAILABLE` while starting or after a failed open.
    pub fn app_state(&self) -> Result<AppState, ApiError> {
        match self.readiness() {
            Readiness::Ready(state) => Ok(state),
            Readiness::Starting => Err(ApiError::unavailable(
                "The library is still opening; retry shortly",
            )),
            // The raw open error (paths, OS errors) stays in the server log only.
            Readiness::Failed(_) => Err(ApiError::unavailable(FAILED_MESSAGE)),
        }
    }

    /// Authentication and admin settings.
    pub fn security(&self) -> &Arc<Security> {
        &self.inner.security
    }

    /// Process-local release check and deployment coordinator (inactive until main starts it).
    pub fn updates(&self) -> &crate::update::UpdateManager {
        &self.inner.updates
    }
}

impl From<AppState> for Service {
    fn from(state: AppState) -> Self {
        Self::with(state.security().clone(), Readiness::Ready(state))
    }
}

/// Backoff between failed library open attempts.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Delay after the first failure; doubled after each further failure.
    pub initial: Duration,
    /// Upper bound for the delay.
    pub max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(5),
            max: Duration::from_secs(300),
        }
    }
}

/// Open the library on a dedicated OS thread (not the Tokio blocking pool, so a stuck open
/// never delays runtime shutdown), retrying failures per `retry`. On success the service
/// becomes ready (sharing the service's [`Security`]) and the revision poller starts on
/// the runtime behind `runtime`. Joining reports shutdown failures from a library
/// that finished opening after cancellation, before it could be published.
pub fn open_in_background(
    service: Service,
    config: ServerConfig,
    retry: RetryPolicy,
    runtime: tokio::runtime::Handle,
) -> std::io::Result<std::thread::JoinHandle<audiolib_core::Result<()>>> {
    let security = service.security().clone();
    std::thread::Builder::new()
        .name("audiolib-library-open".into())
        .spawn(move || {
            let mut delay = retry.initial;
            let mut attempt: u32 = 1;
            loop {
                if service.inner.stopping.load(Ordering::Acquire) {
                    return Ok(());
                }
                tracing::info!(library = %config.library_db.display(), attempt, "Opening library");
                // A panic inside the open must not leave the service `starting` forever.
                let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    AppState::open_with_progress_cancellable(
                        &config,
                        security.clone(),
                        &service.inner.stopping,
                        |stage| {
                            service.set_upgrade_progress(stage);
                        },
                    )
                }))
                .unwrap_or_else(|_| {
                    Err(audiolib_core::Error::Database(
                        "Library open panicked".into(),
                    ))
                });
                match opened {
                    Ok(state) => {
                        return publish_opened(
                            &service,
                            state,
                            &runtime,
                            &config,
                            Duration::from_secs(30),
                        );
                    }
                    Err(error) => {
                        if service.inner.stopping.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        tracing::error!(
                            library = %config.library_db.display(),
                            %error,
                            retry_secs = delay.as_secs_f64(),
                            "Cannot open library; serving /health with status error and retrying"
                        );
                        service.set_failed(error.to_string());
                    }
                }
                let until = std::time::Instant::now() + delay;
                while let Some(remaining) = until.checked_duration_since(std::time::Instant::now())
                {
                    if service.inner.stopping.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(100)));
                }
                delay = delay.saturating_mul(2).min(retry.max);
                attempt = attempt.saturating_add(1);
            }
        })
}

fn publish_opened(
    service: &Service,
    state: AppState,
    runtime: &tokio::runtime::Handle,
    config: &ServerConfig,
    shutdown_timeout: Duration,
) -> audiolib_core::Result<()> {
    if service.inner.stopping.load(Ordering::Acquire) {
        // Main publishes the update notification before storing stopping. Read it
        // only after observing that stop so the chosen drain policy cannot be stale.
        let updating = *service.updates().shutdown_watch().borrow();
        let started = std::time::Instant::now();
        let stopped = state.shutdown(shutdown_timeout);
        // Opening starts native cache writers before returning AppState. They are
        // not in the Tokio runtime and an unpublished state is invisible to main.
        // Join them here before an update can exec; propagate every shutdown error.
        let drained = if updating {
            state
                .list_waveforms()
                .drain_for_update(shutdown_timeout.saturating_sub(started.elapsed()))
        } else {
            Ok(())
        };
        return stopped.and(drained);
    }
    match state.revision() {
        Ok(revision) => tracing::info!(
            library = %config.library_db.display(),
            library_id = %revision.library_id,
            revision = revision.revision,
            roots = config.roots.len(),
            "Library opened"
        ),
        Err(error) => tracing::warn!(%error, "Library opened without revision"),
    }
    let _runtime = runtime.enter();
    spawn_revision_poller(
        state.clone(),
        Duration::from_secs(config.revision_poll_secs),
    );
    service.set_ready(state);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_rejects_unpublished_state_until_its_native_writer_drains() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("library.db");
        drop(audiolib_db::SqliteProvider::open(&database).unwrap());
        let config = ServerConfig::from_toml_str(
            "bind = '127.0.0.1:0'\nlibrary_db = 'library.db'\nlist_waveform_workers = 1\n",
            directory.path(),
        )
        .unwrap();
        let state = AppState::open(&config, None).unwrap();
        let service = Service::starting(None);
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let finished = directory.path().join("final-owned-cache-write");
        let destination = finished.clone();
        let writer = std::thread::spawn(move || {
            wait.recv().unwrap();
            std::fs::write(destination, b"finished").unwrap();
        });
        state.list_waveforms().retain_shutdown_test_worker(writer);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // Reproduce stop arriving after AppState::open spawned its native workers,
        // before the open thread publishes the state to main's readiness lookup.
        service.updates().signal_shutdown_for_test();
        service.request_stop();
        let result = publish_opened(
            &service,
            state.clone(),
            runtime.handle(),
            &config,
            Duration::ZERO,
        );
        assert!(matches!(result, Err(audiolib_core::Error::Busy(_))));
        assert!(matches!(service.readiness(), Readiness::Starting));
        assert!(
            !finished.exists(),
            "Update must not claim a clean close before the writer finishes"
        );
        release.send(()).unwrap();
        state
            .list_waveforms()
            .drain_for_update(Duration::from_secs(2))
            .unwrap();
        assert_eq!(std::fs::read(finished).unwrap(), b"finished");
    }
}
