// crates/audiolib-remote/src/poll.rs
//! Background revision check for server mode.
//!
//! [`RevisionCheck::classify`] compares one `GET /library` result with the snapshot the
//! client has open and decides whether an update is available, the library identity
//! changed, or the server is offline/starting. [`PollSchedule`] turns each outcome into
//! the delay before the next check (fixed interval when healthy, exponential backoff
//! while offline, short retries while the server reports `503` during startup). Both are
//! pure so the desktop's decision logic is unit tested without a network.
use crate::client::{RemoteClient, RemoteLibrary, RemoteRoot};
use crate::error::RemoteError;
use crate::sync::snapshot_schema_error;
use std::time::{Duration, Instant};

/// Longest delay between checks while the server stays unreachable.
pub const MAX_POLL_BACKOFF: Duration = Duration::from_secs(15 * 60);
/// Quick retries (seconds) after a slow answer (response timeout or `408`) before the
/// ordinary backoff starts.
pub const SLOW_RETRY_STEPS: [u64; 3] = [2, 5, 10];
/// Longest delay between checks while the server answers slowly (shorter than offline).
pub const MAX_SLOW_BACKOFF: Duration = Duration::from_secs(2 * 60);
/// First retry delay while the server answers `503` (library still opening).
pub const STARTING_RETRY: Duration = Duration::from_secs(10);
/// Minimum delay after the server rejected the token (settings changes wake the poller).
pub const UNAUTHORIZED_RETRY: Duration = Duration::from_secs(5 * 60);
/// Longest time automatic sync waits for a library whose revision keeps changing (a
/// batched server scan): at most one full snapshot download per this period.
pub const AUTO_SYNC_MAX_DEFER: Duration = Duration::from_secs(15 * 60);

/// Identity of the snapshot currently opened by the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedIdentity {
    pub library_id: String,
    pub revision: u64,
}

/// Outcome of one background revision check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RevisionCheck {
    /// Same library identity and revision as the open snapshot.
    UpToDate { revision: u64 },
    /// Same library, different server revision: a newer snapshot is available.
    NewRevision { cached: u64, server: u64 },
    /// The server now serves a different library identity (database moved or replaced).
    LibraryChanged {
        cached_id: String,
        server_id: String,
        server_revision: u64,
    },
    /// No snapshot cached yet (empty placeholder shown) and the server is reachable.
    NotSynced {
        server_id: String,
        server_revision: u64,
    },
    /// Unreachable (connection refused, DNS, connect timeout): keep using the cached
    /// snapshot.
    Offline(String),
    /// Connected, but the answer timed out (response timeout or `408`): the server is slow
    /// (a large library), not offline. Keep using the cached snapshot and retry quickly.
    Slow(String),
    /// `503 UNAVAILABLE`: the server is still opening its library; retry soon, never
    /// earlier than the server's `Retry-After` (seconds).
    Starting {
        message: String,
        retry_after: Option<u64>,
    },
    /// The server rejected the token.
    Unauthorized(String),
    /// Any other failure (protocol error, server error, unsupported schema).
    Failed(String),
}

impl RevisionCheck {
    /// Classify a `GET /library` result against the open snapshot. Failure messages are
    /// localized for the desktop language and never contain secrets.
    pub fn classify(
        cached: Option<&CachedIdentity>,
        result: Result<RemoteLibrary, RemoteError>,
        chinese: bool,
    ) -> Self {
        let library = match result {
            Ok(library) => library,
            Err(error) => {
                let message = error.localized(chinese);
                return match error {
                    RemoteError::Timeout(_) => Self::Slow(message),
                    error if error.is_offline() => Self::Offline(message),
                    RemoteError::Starting { retry_after, .. } => Self::Starting {
                        message,
                        retry_after,
                    },
                    error if error.is_unauthorized() => Self::Unauthorized(message),
                    _ => Self::Failed(message),
                };
            }
        };
        if let Some(message) = snapshot_schema_error(library.schema_version, chinese) {
            return Self::Failed(message);
        }
        if let Err(error) = crate::compatibility::library(&library) {
            return Self::Failed(error.localized(chinese));
        }
        match cached {
            None => Self::NotSynced {
                server_id: library.library_id,
                server_revision: library.revision,
            },
            Some(cached) if cached.library_id != library.library_id => Self::LibraryChanged {
                cached_id: cached.library_id.clone(),
                server_id: library.library_id,
                server_revision: library.revision,
            },
            Some(cached) if cached.revision != library.revision => Self::NewRevision {
                cached: cached.revision,
                server: library.revision,
            },
            Some(cached) => Self::UpToDate {
                revision: cached.revision,
            },
        }
    }

    /// A newer snapshot (or a first one) can be downloaded.
    pub fn update_available(&self) -> bool {
        matches!(
            self,
            Self::NewRevision { .. } | Self::LibraryChanged { .. } | Self::NotSynced { .. }
        )
    }

    /// The server answered the check (successfully).
    pub fn reachable(&self) -> bool {
        matches!(
            self,
            Self::UpToDate { .. }
                | Self::NewRevision { .. }
                | Self::LibraryChanged { .. }
                | Self::NotSynced { .. }
        )
    }
}

/// `GET /library` through `client` and classify the result.
pub fn check_revision(
    client: &RemoteClient,
    cached: Option<&CachedIdentity>,
    chinese: bool,
) -> RevisionCheck {
    let result = client.library().and_then(|library| {
        crate::compatibility::live_library(client, &library, None)?;
        Ok(library)
    });
    RevisionCheck::classify(cached, result, chinese)
}

/// [`check_revision`] plus `GET /roots`. `/roots` never touches the NAS and answers at once
/// even when `/library` (statistics over the whole table) is slow, so root changes are
/// still picked up while the library check times out. Roots are skipped when the server
/// cannot be reached or rejected the request.
pub fn check_revision_with_roots(
    client: &RemoteClient,
    cached: Option<&CachedIdentity>,
    chinese: bool,
) -> (RevisionCheck, Option<Vec<RemoteRoot>>) {
    let check = check_revision(client, cached, chinese);
    let roots = if check.reachable() || matches!(check, RevisionCheck::Slow(_)) {
        client.roots().ok()
    } else {
        None
    };
    (check, roots)
}

/// Delay policy for background revision checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollSchedule {
    interval: Duration,
    failures: u32,
}

impl PollSchedule {
    /// `interval_secs == 0` disables periodic checks.
    pub fn new(interval_secs: u32) -> Self {
        Self {
            interval: Duration::from_secs(u64::from(interval_secs)),
            failures: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        !self.interval.is_zero()
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Consecutive failed checks since the last successful one.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Record `outcome` and return the delay before the next automatic check, or `None`
    /// when periodic checks are disabled.
    ///
    /// * reachable: reset the backoff, wait one interval;
    /// * slow answer: [`SLOW_RETRY_STEPS`] (2, 5, 10 s), then interval × 2^(n-4) capped at
    ///   [`MAX_SLOW_BACKOFF`];
    /// * offline / other failures: interval × 2^(n-1), capped at [`MAX_POLL_BACKOFF`]
    ///   (never shorter than the interval);
    /// * starting (`503`): [`STARTING_RETRY`] × 2^(n-1), never earlier than the server's
    ///   `Retry-After`, capped at the interval;
    /// * unauthorized: at least [`UNAUTHORIZED_RETRY`] (saving new settings re-checks).
    pub fn next_delay(&mut self, outcome: &RevisionCheck) -> Option<Duration> {
        if outcome.reachable() {
            self.failures = 0;
        } else {
            self.failures = self.failures.saturating_add(1);
        }
        if !self.enabled() {
            return None;
        }
        let exponent = self.failures.saturating_sub(1).min(16);
        Some(match outcome {
            RevisionCheck::Starting { retry_after, .. } => STARTING_RETRY
                .saturating_mul(1 << exponent)
                .max(Duration::from_secs(retry_after.unwrap_or(0)))
                .min(self.interval)
                .max(Duration::from_secs(1)),
            RevisionCheck::Unauthorized(_) => self
                .interval
                .max(UNAUTHORIZED_RETRY)
                .min(MAX_POLL_BACKOFF.max(self.interval)),
            RevisionCheck::Slow(_) => {
                let step = self.failures.saturating_sub(1) as usize;
                match SLOW_RETRY_STEPS.get(step) {
                    Some(seconds) => Duration::from_secs(*seconds),
                    None => self
                        .interval
                        .saturating_mul(1 << (step - SLOW_RETRY_STEPS.len()).min(16))
                        .min(MAX_SLOW_BACKOFF)
                        .max(Duration::from_secs(1)),
                }
            }
            RevisionCheck::Offline(_) | RevisionCheck::Failed(_) => self
                .interval
                .saturating_mul(1 << exponent)
                .min(MAX_POLL_BACKOFF.max(self.interval)),
            _ => self.interval,
        })
    }
}

/// When automatic sync (`auto_sync = true`) should download a new snapshot.
///
/// Every revision change means a full snapshot download (about 160 MB for 85 000
/// records), and a batched server scan bumps the revision every few seconds for a long
/// time. Syncing on each poll would download the whole library every interval, so a new
/// revision of the same library is synced once it stayed unchanged for at least one poll
/// interval (the server went quiet; an extra check soon after, e.g. Retry, does not count),
/// or when changes kept coming for [`AUTO_SYNC_MAX_DEFER`]. A different library identity,
/// or no snapshot yet, syncs at once. Manual sync is never gated.
#[derive(Clone, Debug)]
pub struct AutoSyncGate {
    /// How long a revision must stay unchanged to count as settled (the poll interval).
    quiet: Duration,
    /// Server revision seen by the previous check that reported a new revision.
    last_seen: Option<u64>,
    /// When `last_seen` was first reported.
    seen_since: Option<Instant>,
    /// When the current run of changing revisions was first deferred.
    deferred_since: Option<Instant>,
}

impl AutoSyncGate {
    /// Gate whose revisions settle after staying unchanged for `quiet` (the poll interval).
    pub fn new(quiet: Duration) -> Self {
        Self {
            quiet,
            last_seen: None,
            seen_since: None,
            deferred_since: None,
        }
    }

    /// The poll interval changed.
    pub fn set_quiet(&mut self, quiet: Duration) {
        self.quiet = quiet;
    }

    /// Record one check; `true` when an automatic sync should start now.
    pub fn observe(&mut self, check: &RevisionCheck, now: Instant) -> bool {
        match check {
            RevisionCheck::NewRevision { server, .. } => {
                let repeated = self.last_seen == Some(*server);
                let seen_since = match self.seen_since {
                    Some(since) if repeated => since,
                    _ => now,
                };
                self.last_seen = Some(*server);
                self.seen_since = Some(seen_since);
                let settled = repeated && now.saturating_duration_since(seen_since) >= self.quiet;
                let since = *self.deferred_since.get_or_insert(now);
                let due = settled || now.saturating_duration_since(since) >= AUTO_SYNC_MAX_DEFER;
                if due {
                    self.deferred_since = None;
                }
                due
            }
            RevisionCheck::LibraryChanged { .. } | RevisionCheck::NotSynced { .. } => {
                self.reset();
                true
            }
            RevisionCheck::UpToDate { .. } => {
                self.reset();
                false
            }
            // Failures say nothing about whether the library is still changing.
            _ => false,
        }
    }

    /// A new revision is known but its automatic sync is waiting for changes to stop.
    pub fn deferring(&self) -> bool {
        self.deferred_since.is_some()
    }

    fn reset(&mut self) {
        self.last_seen = None;
        self.seen_since = None;
        self.deferred_since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_sync_waits_until_the_revision_settles_or_the_cap() {
        let minute = Duration::from_secs(60);
        let mut gate = AutoSyncGate::new(minute);
        let start = Instant::now();
        let new = |server| RevisionCheck::NewRevision { cached: 1, server };
        // A batched scan: the revision changes on every poll.
        assert!(!gate.observe(&new(5), start));
        assert!(gate.deferring());
        assert!(!gate.observe(&new(9), start + Duration::from_secs(60)));
        // An offline check in between changes nothing.
        assert!(!gate.observe(&RevisionCheck::Offline("x".into()), start));
        assert!(!gate.observe(&new(14), start + Duration::from_secs(120)));
        // The scan stopped: same revision on the next poll.
        assert!(gate.observe(&new(14), start + Duration::from_secs(180)));
        assert!(!gate.deferring());
        // Changes that never stop still sync once per AUTO_SYNC_MAX_DEFER.
        let mut gate = AutoSyncGate::new(minute);
        let mut revision = 20;
        let mut synced = Vec::new();
        for minute in 0..40u64 {
            revision += 3;
            if gate.observe(&new(revision), start + Duration::from_secs(minute * 60)) {
                synced.push(minute);
            }
        }
        // The next deferral starts with the first change after a sync.
        assert_eq!(synced, [15, 31]);
        // Up to date resets; another library or no snapshot syncs at once.
        assert!(!gate.observe(&RevisionCheck::UpToDate { revision: 3 }, start));
        assert!(!gate.deferring());
        let changed = RevisionCheck::LibraryChanged {
            cached_id: "a".into(),
            server_id: "b".into(),
            server_revision: 2,
        };
        assert!(gate.observe(&changed, start));
        let first = RevisionCheck::NotSynced {
            server_id: "b".into(),
            server_revision: 2,
        };
        assert!(gate.observe(&first, start));
    }

    #[test]
    fn an_early_extra_check_does_not_settle_a_revision() {
        let minute = Duration::from_secs(60);
        let mut gate = AutoSyncGate::new(minute);
        let start = Instant::now();
        let new = |server| RevisionCheck::NewRevision { cached: 1, server };
        assert!(!gate.observe(&new(5), start));
        // Retry (or a settings change) checks again 5 s later: same revision, too soon.
        assert!(!gate.observe(&new(5), start + Duration::from_secs(5)));
        assert!(gate.deferring());
        // Counted from the first sighting, not from the extra check.
        assert!(gate.observe(&new(5), start + minute));
        // A shorter interval applies to the next run.
        gate.set_quiet(Duration::from_secs(10));
        assert!(!gate.observe(&new(8), start + minute * 2));
        assert!(gate.observe(&new(8), start + minute * 2 + Duration::from_secs(10)));
    }
    fn library(id: &str, revision: u64, schema: i64) -> RemoteLibrary {
        RemoteLibrary {
            library_id: id.into(),
            revision,
            schema_version: schema,
            ..Default::default()
        }
    }

    fn cached(id: &str, revision: u64) -> CachedIdentity {
        CachedIdentity {
            library_id: id.into(),
            revision,
        }
    }

    #[test]
    fn classify_compares_identity_and_revision() {
        let current = cached("lib-1", 7);
        assert_eq!(
            RevisionCheck::classify(Some(&current), Ok(library("lib-1", 7, 3)), false),
            RevisionCheck::UpToDate { revision: 7 }
        );
        let newer = RevisionCheck::classify(Some(&current), Ok(library("lib-1", 9, 3)), false);
        assert_eq!(
            newer,
            RevisionCheck::NewRevision {
                cached: 7,
                server: 9
            }
        );
        assert!(newer.update_available() && newer.reachable());
        // A lower revision (e.g. server restored from an older copy) still resyncs.
        assert!(
            RevisionCheck::classify(Some(&current), Ok(library("lib-1", 3, 3)), false)
                .update_available()
        );
        let moved = RevisionCheck::classify(Some(&current), Ok(library("lib-2", 7, 3)), false);
        assert_eq!(
            moved,
            RevisionCheck::LibraryChanged {
                cached_id: "lib-1".into(),
                server_id: "lib-2".into(),
                server_revision: 7
            }
        );
        assert!(moved.update_available());
        let first = RevisionCheck::classify(None, Ok(library("lib-1", 1, 3)), false);
        assert!(matches!(first, RevisionCheck::NotSynced { .. }) && first.update_available());
        let future = RevisionCheck::classify(Some(&current), Ok(library("lib-1", 8, 99)), true);
        assert!(matches!(future, RevisionCheck::Failed(ref text) if text.contains("schema 99")));
        assert!(!future.update_available() && !future.reachable());
    }

    #[test]
    fn polling_requires_a_format_declaration_for_schema_6() {
        let current = cached("lib-1", 7);
        for version in [2, 3, 4, 5, 6, 7] {
            let check =
                RevisionCheck::classify(Some(&current), Ok(library("lib-1", 7, version)), false);
            if (3..=5).contains(&version) {
                assert_eq!(check, RevisionCheck::UpToDate { revision: 7 });
                assert!(check.reachable());
            } else {
                assert!(matches!(&check, RevisionCheck::Failed(message)
                        if version == 6
                            || message.contains(&format!("schema {version}"))
                            || message.contains("format")));
                assert!(!check.reachable() && !check.update_available());
            }
        }
    }

    #[test]
    fn classify_maps_failures_without_treating_starting_as_auth_failure() {
        let current = cached("lib-1", 7);
        let check = |error| RevisionCheck::classify(Some(&current), Err(error), false);
        assert!(matches!(
            check(RemoteError::Unreachable("down".into())),
            RevisionCheck::Offline(_)
        ));
        assert!(matches!(
            check(RemoteError::Timeout("slow".into())),
            RevisionCheck::Slow(_)
        ));
        let starting = check(RemoteError::from_response(
            503,
            r#"{"error_code":"UNAVAILABLE","message":"library is opening"}"#,
            "GET /library",
            Some(5),
        ));
        assert!(
            matches!(starting, RevisionCheck::Starting { ref message, retry_after: Some(5) } if message.contains("starting"))
        );
        assert!(matches!(
            check(RemoteError::from_status(503, "", "GET /library")),
            RevisionCheck::Starting {
                retry_after: None,
                ..
            }
        ));
        assert!(matches!(
            check(RemoteError::from_status(401, "", "GET /library")),
            RevisionCheck::Unauthorized(_)
        ));
        // No token configured and the server wants one: shown as a token problem.
        assert!(matches!(
            check(RemoteError::TokenRequired("GET /library".into())),
            RevisionCheck::Unauthorized(ref message) if message.contains("requires an access token")
        ));
        // A `503 BUSY` (stream cap) is neither a starting server nor a token problem.
        let busy = check(RemoteError::from_response(
            503,
            r#"{"error_code":"BUSY","message":"too many audio streams"}"#,
            "GET /library",
            Some(1),
        ));
        assert!(
            matches!(busy, RevisionCheck::Failed(ref message) if message.contains("busy")),
            "{busy:?}"
        );
        assert!(matches!(
            check(RemoteError::from_status(
                500,
                r#"{"error_code":"INTERNAL","message":"boom"}"#,
                "GET /library"
            )),
            RevisionCheck::Failed(_)
        ));
    }

    #[test]
    fn schedule_backs_off_while_offline_and_resets_on_success() {
        let mut schedule = PollSchedule::new(60);
        let ok = RevisionCheck::UpToDate { revision: 1 };
        let offline = RevisionCheck::Offline("down".into());
        assert_eq!(schedule.next_delay(&ok), Some(Duration::from_secs(60)));
        let delays: Vec<u64> = (0..7)
            .map(|_| schedule.next_delay(&offline).unwrap().as_secs())
            .collect();
        assert_eq!(delays, [60, 120, 240, 480, 900, 900, 900]);
        assert_eq!(schedule.failures(), 7);
        assert_eq!(schedule.next_delay(&ok), Some(Duration::from_secs(60)));
        assert_eq!(schedule.failures(), 0);
        // An interval longer than the backoff cap is never shortened.
        let mut slow = PollSchedule::new(3600);
        assert_eq!(slow.next_delay(&offline), Some(Duration::from_secs(3600)));
        assert_eq!(slow.next_delay(&offline), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn schedule_retries_starting_server_quickly_and_auth_failures_slowly() {
        let mut schedule = PollSchedule::new(60);
        let starting = RevisionCheck::Starting {
            message: "503".into(),
            retry_after: Some(5),
        };
        let delays: Vec<u64> = (0..4)
            .map(|_| schedule.next_delay(&starting).unwrap().as_secs())
            .collect();
        assert_eq!(delays, [10, 20, 40, 60]);
        // A longer Retry-After is honoured (still capped at the interval).
        let mut patient = PollSchedule::new(60);
        let later = RevisionCheck::Starting {
            message: "503".into(),
            retry_after: Some(30),
        };
        assert_eq!(patient.next_delay(&later), Some(Duration::from_secs(30)));
        assert_eq!(patient.next_delay(&later), Some(Duration::from_secs(30)));
        assert_eq!(patient.next_delay(&later), Some(Duration::from_secs(40)));
        let unauthorized = RevisionCheck::Unauthorized("401".into());
        assert_eq!(
            schedule.next_delay(&unauthorized),
            Some(Duration::from_secs(300))
        );
        let mut disabled = PollSchedule::new(0);
        assert!(!disabled.enabled());
        assert_eq!(disabled.next_delay(&starting), None);
        assert_eq!(
            disabled.next_delay(&RevisionCheck::UpToDate { revision: 1 }),
            None
        );
    }

    #[test]
    fn a_slow_answer_is_not_offline_and_retries_quickly_then_backs_off_to_two_minutes() {
        let slow = RevisionCheck::classify(
            None,
            Err(RemoteError::Timeout("http://h/api/v1/library".into())),
            true,
        );
        assert!(matches!(slow, RevisionCheck::Slow(_)), "{slow:?}");
        assert!(!slow.reachable() && !slow.update_available());
        let down = RevisionCheck::classify(
            None,
            Err(RemoteError::Unreachable(
                "http://h: connection failed".into(),
            )),
            true,
        );
        assert!(matches!(down, RevisionCheck::Offline(_)), "{down:?}");

        let mut schedule = PollSchedule::new(60);
        let delays: Vec<u64> = (0..7)
            .map(|_| schedule.next_delay(&slow).unwrap().as_secs())
            .collect();
        assert_eq!(delays, [2, 5, 10, 60, 120, 120, 120]);
        // A good answer resets; the offline backoff keeps its own, longer curve.
        schedule.next_delay(&RevisionCheck::UpToDate { revision: 1 });
        assert_eq!(schedule.failures(), 0);
        let offline: Vec<u64> = (0..3)
            .map(|_| schedule.next_delay(&down).unwrap().as_secs())
            .collect();
        assert_eq!(offline, [60, 120, 240]);
        assert!(MAX_SLOW_BACKOFF < MAX_POLL_BACKOFF);
    }

    #[test]
    fn roots_are_refreshed_even_when_the_library_check_is_slow() {
        use crate::test_server::{TestResponse, TestServer};
        let server = TestServer::start(|request| match request.path.as_str() {
            "/api/v1/library" => TestResponse::json(
                408,
                r#"{"error_code":"TIMEOUT","message":"request took longer than 30 s"}"#,
            ),
            "/api/v1/roots" => TestResponse::json(
                200,
                r#"[{"name":"New","server_path":"/Volumes/New","webdav_url":null}]"#,
            ),
            _ => TestResponse::json(404, "{}"),
        });
        let client = RemoteClient::new(&server.url, "t").unwrap();
        let (check, roots) = check_revision_with_roots(&client, None, false);
        assert!(matches!(check, RevisionCheck::Slow(_)), "{check:?}");
        let roots = roots.expect("/roots is independent of /library");
        assert_eq!(roots[0].name, "New");

        // A server that cannot be reached has no roots either: offline, nothing to show.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let down = RemoteClient::new(&format!("http://127.0.0.1:{port}"), "t").unwrap();
        let (check, roots) = check_revision_with_roots(&down, None, false);
        assert!(matches!(check, RevisionCheck::Offline(_)), "{check:?}");
        assert!(roots.is_none());
    }
}
