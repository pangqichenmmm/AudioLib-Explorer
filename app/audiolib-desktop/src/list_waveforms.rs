// app/audiolib-desktop/src/list_waveforms.rs
//! List thumbnail Waveforms of the result table (doc/api/README.md §9.3).
//!
//! One low-priority background worker answers the visible rows (plus one screen above
//! and below) through the [`DataProvider`]: it first looks stored list Waveforms up
//! (cheap, never decodes), then computes missing ones one file at a time (local mode),
//! and finally, when the user enabled "pre-generate list Waveforms", backfills the
//! library at a bounded rate. Rows that scroll out of the window are dropped from the
//! queues and an on-demand decode of such a row is cancelled.
//!
//! In server mode ([`ListWaveformSource::Server`]) the lookup is one `POST /waveforms`
//! for up to 200 ids and the client never decodes: `pending` ids are asked again after
//! the server's `retry_after`, only while still visible and at most
//! [`MAX_PENDING_RETRIES`] times; `root_offline` rows keep a stale copy drawn with the
//! offline tint until [`ListWaveforms::roots_online`] reports their root back; `missing`
//! and `error` are not retried; a busy server (`503 BUSY`) defers every lookup by its
//! `Retry-After`.
//!
//! Zero-latency rules: the worker never runs on the event loop or in the audio
//! callback; it takes no new decode (and, in server mode with the "hold list-waveform
//! batch requests" setting on, sends no new `POST /waveforms`, deferring rather than
//! dropping them) during the first [`PREBUFFER_YIELD`] after the latest audition start
//! (Load/Play/Seek), uses at
//! most one decode or request at a time, and the backfill pauses for the whole audition
//! and while a scan runs.
use audiolib_core::{
    DataProvider, Error, LIST_WAVEFORM_COLUMNS, ListWaveform, ListWaveformSource,
    ListWaveformState, MAX_LIST_WAVEFORM_IDS,
};
use slint::{Rgba8Pixel, SharedPixelBuffer};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// No new decode starts this long after an audition (Load/Play/Seek) starts.
pub(crate) const PREBUFFER_YIELD: Duration = Duration::from_secs(3);
/// The backfill resumes this long after the last audition ended.
pub(crate) const BACKFILL_RESUME_DELAY: Duration = Duration::from_secs(5);
/// Backfill starts at most one file per interval (5 files per second).
pub(crate) const BACKFILL_INTERVAL: Duration = Duration::from_millis(200);
/// Ids per lookup request.
const LOOKUP_BATCH: usize = 64;
/// Ids per backlog query.
const BACKLOG_BATCH: usize = 256;
/// Results kept in memory for redisplay without a lookup.
const CACHE_CAPACITY: usize = 4096;
/// Re-evaluation interval of an idle worker (provider availability, scans).
const IDLE_POLL: Duration = Duration::from_secs(1);
/// Server mode: a still-visible `pending` id is asked again at most this many times.
pub(crate) const MAX_PENDING_RETRIES: u32 = 5;
/// Server mode: retry interval when a reply with `pending` items gives no `retry_after`.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(2);
/// Server mode: pause after a failed request (unreachable server, protocol error).
const ERROR_BACKOFF: Duration = Duration::from_secs(5);

/// What a result row shows in its Waveform column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ListWaveformCell {
    /// Columns to draw; zero columns (an empty file) draw a center line.
    Ready(ListWaveform),
    /// Missing file or undecodable: the "no Waveform" placeholder.
    Unavailable,
    /// The record's root is offline (server mode): a stale copy, if any, drawn with the
    /// offline tint.
    Offline(Option<ListWaveform>),
}

/// The desktop side the worker needs; implemented by the controller.
pub(crate) trait ListWaveformHost: Send + Sync + 'static {
    /// The provider to use now, or `None` while library work must not run (library
    /// operations, source changes, closing).
    fn list_waveform_provider(&self) -> Option<Arc<dyn DataProvider>>;
    /// Whether a scan is running or paused (the backfill yields to it).
    fn list_waveform_scanning(&self) -> bool;
    /// Publish results; the host checks `generation` on the event loop.
    fn deliver_list_waveforms(&self, generation: u64, cells: Vec<(i64, ListWaveformCell)>);
    /// The provider's list Waveform source changed (the table shows the column only for
    /// a source other than `Unavailable`).
    fn list_waveform_source_changed(&self, source: ListWaveformSource);
}

#[derive(Debug)]
pub(crate) enum Task {
    Lookup {
        generation: u64,
        ids: Vec<i64>,
        cancel: Arc<AtomicBool>,
        server: bool,
    },
    Compute {
        generation: u64,
        id: i64,
        cancel: Arc<AtomicBool>,
        on_demand: bool,
    },
    Backlog {
        generation: u64,
        after: i64,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Exit,
    Wait(Option<Duration>),
    Run,
}

struct Current {
    id: i64,
    cancel: Arc<AtomicBool>,
    on_demand: bool,
}

#[derive(Default)]
struct Backfill {
    active: bool,
    cursor: i64,
    queue: VecDeque<i64>,
    last_start: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct State {
    generation: u64,
    /// Wanted ids in priority order (visible first) and as a set.
    window: Vec<i64>,
    in_window: HashSet<i64>,
    /// The rows on screen (the start of `window`).
    visible: HashSet<i64>,
    cache: HashMap<i64, ListWaveformCell>,
    cache_order: VecDeque<i64>,
    /// Cached ids to look up again (files may have changed, e.g. after a scan).
    verify: HashSet<i64>,
    /// Window ids already looked up in this generation.
    looked_up: HashSet<i64>,
    /// Looked-up `Pending` ids waiting for a local decode.
    compute: HashSet<i64>,
    /// Ids whose decode ended without a result; not retried while they stay in view.
    attempted: HashSet<i64>,
    current: Option<Current>,
    busy: bool,
    suspended: bool,
    shutdown: bool,
    playing: bool,
    playback_start: Option<Instant>,
    playback_end: Option<Instant>,
    pregenerate: bool,
    backfill: Backfill,
    task: Option<Task>,
    /// Server mode: `pending` answers received per id.
    pending_answers: HashMap<i64, u32>,
    /// Server mode: ids to ask again once `retry_due` passed (only while visible).
    retry_wait: HashSet<i64>,
    retry_due: Option<Instant>,
    /// Server mode: ids whose root was offline, with the root name.
    offline: HashMap<i64, String>,
    /// Server mode: no request before this (a busy or failing server).
    defer_until: Option<Instant>,
    /// Server mode: hold new batches during the first [`PREBUFFER_YIELD`] after the latest
    /// audition start (client setting, default off).
    hold_after_audition: bool,
}

impl State {
    fn remember(&mut self, id: i64, cell: ListWaveformCell) {
        if self.cache.insert(id, cell).is_none() {
            self.cache_order.push_back(id);
        }
        while self.cache.len() > CACHE_CAPACITY {
            let Some(oldest) = self.cache_order.pop_front() else {
                break;
            };
            // Keep what is on screen.
            if self.in_window.contains(&oldest) {
                self.cache_order.push_back(oldest);
                if self.cache_order.len() <= self.in_window.len() {
                    break;
                }
                continue;
            }
            self.cache.remove(&oldest);
            self.verify.remove(&oldest);
        }
    }

    fn cancel_current(&mut self, only_on_demand_outside_window: bool) {
        if let Some(current) = &self.current
            && (!only_on_demand_outside_window
                || (current.on_demand && !self.in_window.contains(&current.id)))
        {
            current.cancel.store(true, Ordering::Release);
        }
    }

    /// Choose the next task (stored in `self.task`) or how long to wait. Pure policy:
    /// `now`, `scanning` and `source` come from the caller.
    pub(crate) fn next_step(
        &mut self,
        now: Instant,
        scanning: bool,
        source: ListWaveformSource,
    ) -> Step {
        if self.shutdown {
            return Step::Exit;
        }
        if self.suspended || source == ListWaveformSource::Unavailable {
            return Step::Wait(Some(IDLE_POLL));
        }
        let prebuffering = self
            .playback_start
            .map(|start| now.saturating_duration_since(start))
            .filter(|elapsed| *elapsed < PREBUFFER_YIELD)
            .map(|elapsed| PREBUFFER_YIELD - elapsed);
        if source == ListWaveformSource::Server {
            return self.next_server_step(now, prebuffering);
        }
        // 1. Cheap lookups of the window, visible rows first.
        let ids: Vec<i64> = self
            .window
            .iter()
            .copied()
            .filter(|id| {
                !self.looked_up.contains(id)
                    && (!self.cache.contains_key(id) || self.verify.contains(id))
            })
            .take(LOOKUP_BATCH)
            .collect();
        if !ids.is_empty() {
            self.looked_up.extend(&ids);
            self.task = Some(self.lookup_task(ids, false));
            return Step::Run;
        }
        // 2. On-demand decodes of window rows, in window order.
        if let Some(id) = self
            .window
            .iter()
            .copied()
            .find(|id| self.compute.contains(id) && !self.attempted.contains(id))
        {
            if let Some(remaining) = prebuffering {
                return Step::Wait(Some(remaining));
            }
            self.compute.remove(&id);
            self.task = Some(self.compute_task(id, true));
            return Step::Run;
        }
        // 3. Rate-limited backfill, only while nothing is auditioned or scanned.
        if !(self.pregenerate && self.backfill.active) {
            return Step::Wait(None);
        }
        if scanning || self.playing {
            return Step::Wait(Some(IDLE_POLL));
        }
        if let Some(remaining) = prebuffering {
            return Step::Wait(Some(remaining));
        }
        if let Some(end) = self.playback_end {
            let elapsed = now.saturating_duration_since(end);
            if elapsed < BACKFILL_RESUME_DELAY {
                return Step::Wait(Some(BACKFILL_RESUME_DELAY - elapsed));
            }
        }
        while let Some(id) = self.backfill.queue.front().copied() {
            if self.cache.contains_key(&id) && !self.verify.contains(&id) {
                self.backfill.queue.pop_front();
                continue;
            }
            if let Some(last) = self.backfill.last_start {
                let elapsed = now.saturating_duration_since(last);
                if elapsed < BACKFILL_INTERVAL {
                    return Step::Wait(Some(BACKFILL_INTERVAL - elapsed));
                }
            }
            self.backfill.queue.pop_front();
            self.backfill.last_start = Some(now);
            self.task = Some(self.compute_task(id, false));
            return Step::Run;
        }
        self.task = Some(Task::Backlog {
            generation: self.generation,
            after: self.backfill.cursor,
        });
        Step::Run
    }

    /// Server mode: one `POST /waveforms` for still-visible ids due for a retry and window
    /// ids not asked yet. With [`State::hold_after_audition`] the batch is held (not
    /// dropped) until [`PREBUFFER_YIELD`] after the latest audition start.
    fn next_server_step(&mut self, now: Instant, prebuffering: Option<Duration>) -> Step {
        let prebuffering = prebuffering.filter(|_| self.hold_after_audition);
        if let Some(until) = self.defer_until {
            if now < until {
                return Step::Wait(Some(until - now));
            }
            self.defer_until = None;
        }
        let waiting: Vec<i64> = self
            .window
            .iter()
            .copied()
            .filter(|id| self.visible.contains(id) && self.retry_wait.contains(id))
            .collect();
        let due = self.retry_due.is_none_or(|due| due <= now);
        let fresh = self.window.iter().copied().filter(|id| {
            !self.looked_up.contains(id)
                && (!self.cache.contains_key(id) || self.verify.contains(id))
        });
        let fresh: Vec<i64> = fresh.collect();
        let mut ids: Vec<i64> = if due { waiting.clone() } else { Vec::new() };
        for id in fresh {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        ids.truncate(MAX_LIST_WAVEFORM_IDS);
        if ids.is_empty() {
            return match self.retry_due {
                Some(due) if !waiting.is_empty() => {
                    Step::Wait(Some(due.saturating_duration_since(now)))
                }
                _ => Step::Wait(None),
            };
        }
        if let Some(remaining) = prebuffering {
            return Step::Wait(Some(remaining));
        }
        if due && !waiting.is_empty() {
            self.retry_due = None;
        }
        for id in &ids {
            self.retry_wait.remove(id);
            self.looked_up.insert(*id);
        }
        self.task = Some(self.lookup_task(ids, true));
        Step::Run
    }

    fn lookup_task(&mut self, ids: Vec<i64>, server: bool) -> Task {
        let cancel = Arc::new(AtomicBool::new(false));
        self.current = Some(Current {
            id: ids.first().copied().unwrap_or_default(),
            cancel: cancel.clone(),
            on_demand: false,
        });
        Task::Lookup {
            generation: self.generation,
            ids,
            cancel,
            server,
        }
    }

    fn compute_task(&mut self, id: i64, on_demand: bool) -> Task {
        let cancel = Arc::new(AtomicBool::new(false));
        self.current = Some(Current {
            id,
            cancel: cancel.clone(),
            on_demand,
        });
        Task::Compute {
            generation: self.generation,
            id,
            cancel,
            on_demand,
        }
    }

    /// Apply a finished task; returns the cells to publish.
    pub(crate) fn finish(&mut self, outcome: Outcome) -> Vec<(i64, ListWaveformCell)> {
        self.finish_at(outcome, Instant::now())
    }

    pub(crate) fn finish_at(
        &mut self,
        outcome: Outcome,
        now: Instant,
    ) -> Vec<(i64, ListWaveformCell)> {
        self.current = None;
        let mut cells = Vec::new();
        match outcome {
            Outcome::Lookup {
                generation,
                ids,
                server,
                result,
            } => {
                if generation != self.generation {
                    return cells;
                }
                let batch = match result {
                    Ok(batch) => batch,
                    Err(error) => {
                        // Retried once the rows scroll out and back (or after a scan); a
                        // failing server is left alone for a moment.
                        if server && !matches!(error, Error::Cancelled) {
                            tracing::debug!(%error, "list Waveform request failed");
                            self.defer_until = Some(now + ERROR_BACKOFF);
                        }
                        if matches!(error, Error::Cancelled) {
                            for id in &ids {
                                self.looked_up.remove(id);
                            }
                        }
                        return cells;
                    }
                };
                if let Some(wait) = batch.deferred {
                    // Busy: nothing answered, ask again about all of them later.
                    for id in &ids {
                        self.looked_up.remove(id);
                    }
                    self.defer_until = Some(now + wait);
                    return cells;
                }
                let retry_after = batch.retry_after.unwrap_or(DEFAULT_RETRY_AFTER);
                for item in batch.items {
                    if !ids.contains(&item.id) {
                        continue;
                    }
                    self.verify.remove(&item.id);
                    let id = item.id;
                    let cell = match item.state {
                        ListWaveformState::Ready { waveform } => ListWaveformCell::Ready(waveform),
                        ListWaveformState::Missing | ListWaveformState::Failed { .. } => {
                            ListWaveformCell::Unavailable
                        }
                        ListWaveformState::Pending { previous } if server => {
                            let answers = self.pending_answers.entry(id).or_default();
                            *answers += 1;
                            if *answers <= MAX_PENDING_RETRIES {
                                self.retry_wait.insert(id);
                                self.retry_due = Some(now + retry_after);
                            }
                            match previous {
                                // §9.3: keep showing the old copy.
                                Some(previous) => ListWaveformCell::Ready(previous),
                                None => continue,
                            }
                        }
                        ListWaveformState::RootOffline { root, previous } if server => {
                            self.offline.insert(id, root);
                            ListWaveformCell::Offline(previous)
                        }
                        ListWaveformState::Pending { previous }
                        | ListWaveformState::RootOffline { previous, .. } => {
                            if self.in_window.contains(&id) {
                                self.compute.insert(id);
                            }
                            if let Some(previous) = previous {
                                cells.push((id, ListWaveformCell::Ready(previous)));
                            }
                            continue;
                        }
                    };
                    if !matches!(cell, ListWaveformCell::Offline(_)) {
                        self.offline.remove(&id);
                    }
                    self.remember(id, cell.clone());
                    cells.push((id, cell));
                }
            }
            Outcome::Compute {
                generation,
                id,
                on_demand,
                result,
            } => {
                if generation != self.generation {
                    return cells;
                }
                let cell = match result {
                    Ok(ListWaveformState::Ready { waveform }) => ListWaveformCell::Ready(waveform),
                    Ok(ListWaveformState::Missing | ListWaveformState::Failed { .. }) => {
                        ListWaveformCell::Unavailable
                    }
                    Err(Error::Cancelled) => {
                        // Scrolled out (or suspended): look it up again when it returns.
                        self.looked_up.remove(&id);
                        return cells;
                    }
                    Ok(_) | Err(_) => {
                        // The file changed while decoding, or I/O failed: not retried
                        // while the row stays in view.
                        self.attempted.insert(id);
                        return cells;
                    }
                };
                self.verify.remove(&id);
                if on_demand || self.in_window.contains(&id) {
                    self.remember(id, cell.clone());
                }
                cells.push((id, cell));
            }
            Outcome::Backlog { generation, result } => {
                if generation != self.generation {
                    return cells;
                }
                match result {
                    Ok(ids) if !ids.is_empty() => {
                        self.backfill.cursor = *ids.last().unwrap();
                        self.backfill.queue.extend(ids);
                    }
                    // Done (or the library refused): the next scan starts a new pass.
                    _ => self.backfill.active = false,
                }
            }
        }
        cells
    }
}

/// Result of a [`Task`] run without the state lock.
pub(crate) enum Outcome {
    Lookup {
        generation: u64,
        ids: Vec<i64>,
        server: bool,
        result: audiolib_core::Result<audiolib_core::ListWaveformBatch>,
    },
    Compute {
        generation: u64,
        id: i64,
        on_demand: bool,
        result: audiolib_core::Result<ListWaveformState>,
    },
    Backlog {
        generation: u64,
        result: audiolib_core::Result<Vec<i64>>,
    },
}

/// Blocking: run one task against `provider`.
pub(crate) fn run_task(provider: &dyn DataProvider, task: Task) -> Outcome {
    match task {
        Task::Lookup {
            generation,
            ids,
            cancel,
            server,
        } => Outcome::Lookup {
            result: provider.list_waveforms(&ids, cancel),
            generation,
            ids,
            server,
        },
        Task::Compute {
            generation,
            id,
            cancel,
            on_demand,
        } => Outcome::Compute {
            generation,
            id,
            on_demand,
            result: provider.compute_list_waveform(id, cancel),
        },
        Task::Backlog { generation, after } => Outcome::Backlog {
            generation,
            result: provider.list_waveform_backlog(after, BACKLOG_BATCH),
        },
    }
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// Handle owned by the controller; cheap to clone.
#[derive(Clone)]
pub(crate) struct ListWaveforms {
    shared: Arc<Shared>,
}

impl ListWaveforms {
    pub(crate) fn new(pregenerate: bool) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    pregenerate,
                    backfill: Backfill {
                        active: pregenerate,
                        ..Backfill::default()
                    },
                    ..State::default()
                }),
                wake: Condvar::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn update(&self, change: impl FnOnce(&mut State)) {
        let mut state = self.lock();
        change(&mut state);
        self.shared.wake.notify_all();
    }

    /// Server mode: start with the "hold list-waveform batch requests" setting.
    pub(crate) fn with_hold_after_audition(self, enabled: bool) -> Self {
        self.set_hold_after_audition(enabled);
        self
    }

    /// The "hold list-waveform batch requests during the first 3 s after audition start"
    /// setting changed. Cheap and non-blocking; safe on the event loop.
    pub(crate) fn set_hold_after_audition(&self, enabled: bool) {
        self.update(|state| state.hold_after_audition = enabled);
    }

    /// Start the worker thread; it ends on [`ListWaveforms::shutdown`] or when the host
    /// is dropped.
    pub(crate) fn start(&self, host: Weak<dyn ListWaveformHost>) {
        let service = self.clone();
        std::thread::Builder::new()
            .name("audiolib-list-waveforms".into())
            .spawn(move || {
                // Lowest CPU and I/O priority: never compete with audition decoding.
                audiolib_windows::enter_background_thread_mode();
                service.run(host);
            })
            .expect("spawn list Waveform worker");
    }

    fn run(&self, host: Weak<dyn ListWaveformHost>) {
        let mut reported = None;
        loop {
            let Some(strong) = host.upgrade() else { return };
            let provider = strong.list_waveform_provider();
            let scanning = strong.list_waveform_scanning();
            // Blocking on first use for a server (`GET /capabilities`): on this thread.
            let source = provider
                .as_ref()
                .map_or(ListWaveformSource::Unavailable, |provider| {
                    provider.list_waveform_source()
                });
            if reported != Some(source) {
                reported = Some(source);
                strong.list_waveform_source_changed(source);
            }
            drop(strong);
            let mut state = self.lock();
            match state.next_step(Instant::now(), scanning, source) {
                Step::Exit => return,
                Step::Wait(timeout) => {
                    // Never sleep holding the provider: an idle worker must not keep
                    // the library database open once its host is gone (on Windows the
                    // open file blocks deleting or replacing the library directory).
                    drop(provider);
                    let timeout = timeout.unwrap_or(IDLE_POLL).min(IDLE_POLL);
                    drop(
                        self.shared
                            .wake
                            .wait_timeout(state, timeout)
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                    );
                }
                Step::Run => {
                    let (Some(task), Some(provider)) = (state.task.take(), provider) else {
                        continue;
                    };
                    state.busy = true;
                    drop(state);
                    let outcome = run_task(provider.as_ref(), task);
                    drop(provider);
                    let (generation, cells) = {
                        let mut state = self.lock();
                        state.busy = false;
                        let cells = state.finish(outcome);
                        self.shared.wake.notify_all();
                        (state.generation, cells)
                    };
                    if !cells.is_empty()
                        && let Some(host) = host.upgrade()
                    {
                        host.deliver_list_waveforms(generation, cells);
                    }
                }
            }
        }
    }

    /// Current generation; results of older generations are stale.
    pub(crate) fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Replace the wanted ids (the `visible` rows on screen first, then the rows one
    /// screen below and above). Unsent work for other ids is dropped (including server
    /// retries) and their on-demand decode cancelled; a sent request is not cancelled.
    pub(crate) fn set_window(&self, ids: Vec<i64>, visible: usize) {
        self.update(|state| {
            let wanted: HashSet<i64> = ids.iter().copied().collect();
            state.looked_up.retain(|id| wanted.contains(id));
            state.compute.retain(|id| wanted.contains(id));
            state.attempted.retain(|id| wanted.contains(id));
            state.retry_wait.retain(|id| wanted.contains(id));
            state.visible = ids.iter().take(visible).copied().collect();
            state.window = ids;
            state.in_window = wanted;
            state.cancel_current(true);
        });
    }

    /// Cached cell of `id`, if any (for rows being (re)published).
    pub(crate) fn cached(&self, id: i64) -> Option<ListWaveformCell> {
        self.lock().cache.get(&id).cloned()
    }

    /// A different library, source or snapshot: forget everything and stop.
    pub(crate) fn invalidate(&self) {
        self.update(|state| {
            state.generation += 1;
            state.cancel_current(false);
            state.window.clear();
            state.in_window.clear();
            state.visible.clear();
            state.pending_answers.clear();
            state.retry_wait.clear();
            state.retry_due = None;
            state.offline.clear();
            state.defer_until = None;
            state.cache.clear();
            state.cache_order.clear();
            state.verify.clear();
            state.looked_up.clear();
            state.compute.clear();
            state.attempted.clear();
            state.backfill = Backfill {
                active: state.pregenerate,
                ..Backfill::default()
            };
        });
    }

    /// A scan finished: cached cells stay visible but are looked up again (files may have
    /// changed), and an enabled pre-generation starts a new pass.
    pub(crate) fn scan_finished(&self) {
        self.update(|state| {
            state.generation += 1;
            state.verify = state.cache.keys().copied().collect();
            state.looked_up.clear();
            state.compute.clear();
            state.attempted.clear();
            state.pending_answers.clear();
            state.retry_wait.clear();
            state.backfill = Backfill {
                active: state.pregenerate,
                ..Backfill::default()
            };
        });
    }

    /// Server mode: `/roots` reports these roots online; rows that were `root_offline`
    /// there are asked again (while in the window).
    pub(crate) fn roots_online(&self, online: &[String]) {
        let mut state = self.lock();
        let back: Vec<i64> = state
            .offline
            .iter()
            .filter(|(_, root)| online.contains(root))
            .map(|(id, _)| *id)
            .collect();
        if back.is_empty() {
            return;
        }
        for id in back {
            state.offline.remove(&id);
            state.looked_up.remove(&id);
            state.verify.insert(id);
        }
        self.shared.wake.notify_all();
    }

    /// The "pre-generate list Waveforms" setting changed. Turning it off lets a running
    /// decode finish but starts no new one.
    pub(crate) fn set_pregenerate(&self, enabled: bool) {
        self.update(|state| {
            if enabled && !state.pregenerate {
                state.backfill = Backfill {
                    active: true,
                    ..Backfill::default()
                };
            }
            if !enabled {
                state.backfill = Backfill::default();
            }
            state.pregenerate = enabled;
        });
    }

    /// An audition is starting (Load, Play or Seek): no new decode for
    /// [`PREBUFFER_YIELD`]. Cheap and non-blocking; safe on the event loop.
    pub(crate) fn note_audition_start(&self) {
        self.update(|state| state.playback_start = Some(Instant::now()));
    }

    /// Observed transport state from the playback status worker.
    pub(crate) fn observe_playback(&self, playing: bool) {
        let mut state = self.lock();
        if playing == state.playing {
            return;
        }
        let now = Instant::now();
        if playing {
            state.playback_start = Some(now);
        } else {
            state.playback_end = Some(now);
        }
        state.playing = playing;
        self.shared.wake.notify_all();
    }

    /// Stop taking work (library operations) and cancel a running decode.
    pub(crate) fn suspend(&self) {
        self.update(|state| {
            state.suspended = true;
            state.cancel_current(false);
        });
    }

    pub(crate) fn resume(&self) {
        self.update(|state| state.suspended = false);
    }

    /// Wait until no task runs, at most `timeout`; returns whether the worker is idle.
    pub(crate) fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock();
        while state.busy {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self
                .shared
                .wake
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        true
    }

    pub(crate) fn shutdown(&self) {
        self.update(|state| {
            state.shutdown = true;
            state.cancel_current(false);
        });
    }
}

/// Window ids for rows `first .. first + count` of `ids`: the visible rows, then one
/// screen below, then one screen above (nearest first).
pub(crate) fn window_ids(ids: &[i64], first: usize, count: usize) -> Vec<i64> {
    let len = ids.len();
    let first = first.min(len);
    let visible_end = first.saturating_add(count).min(len);
    let below_end = visible_end.saturating_add(count).min(len);
    let above_start = first.saturating_sub(count);
    let mut window = Vec::with_capacity(below_end - above_start);
    window.extend_from_slice(&ids[first..visible_end]);
    window.extend_from_slice(&ids[visible_end..below_end]);
    window.extend(ids[above_start..first].iter().rev());
    window
}

/// Logical size of the list Waveform image (cell 120 px wide, 18 px tall in a 22 px row).
pub(crate) const THUMBNAIL_WIDTH: u32 = 120;
pub(crate) const THUMBNAIL_HEIGHT: u32 = 18;
/// Plain grey bars.
const WAVE_COLOR: Rgba8Pixel = Rgba8Pixel {
    r: 150,
    g: 150,
    b: 150,
    a: 255,
};

/// Rasterize `waveform` as filled min/max bars at `scale` physical pixels per logical
/// pixel, so the table shows it 1:1 at the window scale factor. Each pixel column takes
/// the envelope union of the Waveform columns it covers. `None` for 0 columns (centre line).
pub(crate) fn render_list_waveform(
    waveform: &ListWaveform,
    scale: f32,
) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let columns = waveform.columns().min(LIST_WAVEFORM_COLUMNS as usize);
    if columns == 0 {
        return None;
    }
    let scale = if scale.is_finite() {
        scale.clamp(1.0, 4.0)
    } else {
        1.0
    };
    let width = ((THUMBNAIL_WIDTH as f32 * scale).round() as u32).max(1);
    let height = ((THUMBNAIL_HEIGHT as f32 * scale).round() as u32).max(1);
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    let pixels = buffer.make_mut_slice();
    let row = |value: i16| -> u32 {
        let scaled = (32767.0 - f32::from(value.max(-32767))) / 65534.0 * (height - 1) as f32;
        (scaled.round() as u32).min(height - 1)
    };
    for x in 0..width as usize {
        let first = x * columns / width as usize;
        let last = ((x + 1) * columns)
            .div_ceil(width as usize)
            .clamp(first + 1, columns);
        let (mut low, mut high) = (i16::MAX, i16::MIN);
        for column in first..last {
            let (a, b) = waveform.column(column).unwrap_or((0, 0));
            low = low.min(a.min(b));
            high = high.max(a.max(b));
        }
        for y in row(high)..=row(low) {
            pixels[y as usize * width as usize + x] = WAVE_COLOR;
        }
    }
    Some(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiolib_core::{ListWaveformBatch, ListWaveformItem};

    fn waveform(columns: usize) -> ListWaveform {
        ListWaveform::new(1, [-100, 100].repeat(columns)).unwrap()
    }

    fn state_with_window(ids: &[i64]) -> (ListWaveforms, State) {
        let service = ListWaveforms::new(false);
        service.set_window(ids.to_vec(), ids.len());
        let state = std::mem::take(&mut *service.lock());
        (service, state)
    }

    fn take_task(state: &mut State) -> Task {
        state.task.take().expect("task")
    }

    #[test]
    fn window_is_visible_rows_then_below_then_above() {
        let ids: Vec<i64> = (0..20).collect();
        assert_eq!(window_ids(&ids, 5, 3), vec![5, 6, 7, 8, 9, 10, 4, 3, 2]);
        assert_eq!(window_ids(&ids, 0, 4), vec![0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(window_ids(&ids, 18, 4), vec![18, 19, 17, 16, 15, 14]);
        assert!(window_ids(&[], 3, 4).is_empty());
    }

    #[test]
    fn lookups_precede_decodes_which_yield_to_audition_start() {
        let (_service, mut state) = state_with_window(&[1, 2, 3]);
        let now = Instant::now();
        let local = ListWaveformSource::Local;
        assert_eq!(state.next_step(now, false, local), Step::Run);
        let Task::Lookup {
            generation, ids, ..
        } = take_task(&mut state)
        else {
            panic!("lookup first")
        };
        assert_eq!(ids, vec![1, 2, 3]);
        let cells = state.finish(Outcome::Lookup {
            generation,
            ids: ids.clone(),
            server: false,
            result: Ok(ListWaveformBatch {
                items: vec![
                    ListWaveformItem {
                        id: 1,
                        state: ListWaveformState::Ready {
                            waveform: waveform(128),
                        },
                    },
                    ListWaveformItem {
                        id: 2,
                        state: ListWaveformState::Pending { previous: None },
                    },
                    ListWaveformItem {
                        id: 3,
                        state: ListWaveformState::Missing,
                    },
                ],
                retry_after: None,
                deferred: None,
            }),
        });
        assert_eq!(
            cells,
            vec![
                (1, ListWaveformCell::Ready(waveform(128))),
                (3, ListWaveformCell::Unavailable)
            ]
        );
        // An audition started 1 s ago: the decode waits for the rest of the 3 s window.
        state.playback_start = Some(now - Duration::from_secs(1));
        assert_eq!(
            state.next_step(now, false, local),
            Step::Wait(Some(Duration::from_secs(2)))
        );
        let later = now + Duration::from_secs(2);
        assert_eq!(state.next_step(later, false, local), Step::Run);
        let Task::Compute { id, on_demand, .. } = take_task(&mut state) else {
            panic!("decode")
        };
        assert_eq!((id, on_demand), (2, true));
        // A server source never decodes on the client.
        let (_service, mut server) = state_with_window(&[9]);
        assert_eq!(
            server.next_step(now, false, ListWaveformSource::Server),
            Step::Run
        );
        take_task(&mut server);
        server.compute.insert(9);
        assert_eq!(
            server.next_step(now, false, ListWaveformSource::Server),
            Step::Wait(None)
        );
        assert_eq!(
            server.next_step(now, false, ListWaveformSource::Unavailable),
            Step::Wait(Some(IDLE_POLL))
        );
    }

    #[test]
    fn scrolling_away_drops_unsent_work_and_cancels_the_running_decode() {
        let service = ListWaveforms::new(false);
        service.set_window(vec![1, 2], usize::MAX);
        let now = Instant::now();
        let cancel = {
            let mut state = service.lock();
            assert_eq!(
                state.next_step(now, false, ListWaveformSource::Local),
                Step::Run
            );
            let Task::Lookup {
                generation, ids, ..
            } = take_task(&mut state)
            else {
                panic!()
            };
            state.finish(Outcome::Lookup {
                generation,
                ids,
                server: false,
                result: Ok(ListWaveformBatch {
                    items: [1, 2]
                        .map(|id| ListWaveformItem {
                            id,
                            state: ListWaveformState::Pending { previous: None },
                        })
                        .to_vec(),
                    retry_after: None,
                    deferred: None,
                }),
            });
            assert_eq!(
                state.next_step(now, false, ListWaveformSource::Local),
                Step::Run
            );
            let Task::Compute { id, cancel, .. } = take_task(&mut state) else {
                panic!()
            };
            assert_eq!(id, 1);
            cancel
        };
        service.set_window(vec![7], usize::MAX);
        assert!(
            cancel.load(Ordering::Acquire),
            "scrolled-out decode is cancelled"
        );
        let mut state = service.lock();
        assert!(
            state.compute.is_empty(),
            "unsent decode of row 2 is dropped"
        );
        let generation = state.generation;
        assert!(
            state
                .finish(Outcome::Compute {
                    generation,
                    id: 1,
                    on_demand: true,
                    result: Err(Error::Cancelled),
                })
                .is_empty()
        );
        assert_eq!(
            state.next_step(now, false, ListWaveformSource::Local),
            Step::Run
        );
        let Task::Lookup { ids, .. } = take_task(&mut state) else {
            panic!()
        };
        assert_eq!(ids, vec![7]);
    }

    #[test]
    fn backfill_is_rate_limited_and_yields_to_scans_and_auditions() {
        let service = ListWaveforms::new(true);
        let mut state = std::mem::take(&mut *service.lock());
        let now = Instant::now();
        let local = ListWaveformSource::Local;
        assert_eq!(
            state.next_step(now, true, local),
            Step::Wait(Some(IDLE_POLL))
        );
        state.playing = true;
        assert_eq!(
            state.next_step(now, false, local),
            Step::Wait(Some(IDLE_POLL))
        );
        state.playing = false;
        state.playback_end = Some(now - Duration::from_secs(1));
        assert_eq!(
            state.next_step(now, false, local),
            Step::Wait(Some(Duration::from_secs(4)))
        );
        let now = now + Duration::from_secs(4);
        assert_eq!(state.next_step(now, false, local), Step::Run);
        let Task::Backlog { generation, after } = take_task(&mut state) else {
            panic!()
        };
        assert_eq!(after, 0);
        state.finish(Outcome::Backlog {
            generation,
            result: Ok(vec![10, 11]),
        });
        assert_eq!(state.next_step(now, false, local), Step::Run);
        let Task::Compute { id, on_demand, .. } = take_task(&mut state) else {
            panic!()
        };
        assert_eq!((id, on_demand), (10, false));
        state.current = None;
        assert_eq!(
            state.next_step(now + Duration::from_millis(50), false, local),
            Step::Wait(Some(Duration::from_millis(150)))
        );
        let next = now + BACKFILL_INTERVAL;
        assert_eq!(state.next_step(next, false, local), Step::Run);
        take_task(&mut state);
        assert_eq!(
            state.next_step(next + BACKFILL_INTERVAL, false, local),
            Step::Run
        );
        let Task::Backlog { generation, after } = take_task(&mut state) else {
            panic!()
        };
        assert_eq!(after, 11);
        state.finish(Outcome::Backlog {
            generation,
            result: Ok(Vec::new()),
        });
        assert!(
            !state.backfill.active,
            "the pass ends at the end of the backlog"
        );
        assert_eq!(state.next_step(next, false, local), Step::Wait(None));
    }

    #[test]
    fn enabled_pregeneration_with_empty_backlog_idles_without_hanging() {
        // Fresh start with the new-install default (pregenerate on): worker is ready,
        // but an empty library finishes the pass and waits — no busy loop.
        let service = ListWaveforms::new(true);
        let mut state = std::mem::take(&mut *service.lock());
        assert!(state.pregenerate && state.backfill.active);
        let now = Instant::now();
        let local = ListWaveformSource::Local;
        assert_eq!(state.next_step(now, false, local), Step::Run);
        let Task::Backlog { generation, after } = take_task(&mut state) else {
            panic!("empty library still asks for a backlog page")
        };
        assert_eq!(after, 0);
        state.finish(Outcome::Backlog {
            generation,
            result: Ok(Vec::new()),
        });
        assert!(
            !state.backfill.active,
            "empty backlog ends the pass so the worker does not hang"
        );
        assert_eq!(state.next_step(now, false, local), Step::Wait(None));
    }

    #[test]
    fn scan_finished_restarts_pregeneration_pass_when_enabled() {
        let service = ListWaveforms::new(true);
        {
            let mut state = service.lock();
            // Simulate a completed first pass.
            state.backfill.active = false;
            state.backfill.cursor = 99;
            state.backfill.queue.push_back(1);
        }
        service.scan_finished();
        let state = service.lock();
        assert!(state.pregenerate);
        assert!(
            state.backfill.active,
            "a finished local scan starts a new pre-generation pass"
        );
        assert_eq!(state.backfill.cursor, 0);
        assert!(state.backfill.queue.is_empty());
    }

    #[test]
    fn unavailable_or_suspended_source_idles_without_starting_backfill_work() {
        let service = ListWaveforms::new(true);
        let mut state = std::mem::take(&mut *service.lock());
        let now = Instant::now();
        assert_eq!(
            state.next_step(now, false, ListWaveformSource::Unavailable),
            Step::Wait(Some(IDLE_POLL))
        );
        assert!(state.task.is_none(), "Unavailable must not enqueue work");
        state.suspended = true;
        assert_eq!(
            state.next_step(now, false, ListWaveformSource::Local),
            Step::Wait(Some(IDLE_POLL))
        );
        assert!(state.task.is_none(), "suspended must not enqueue work");
    }

    #[test]
    fn stale_generations_are_dropped_and_scans_reverify_cached_rows() {
        let service = ListWaveforms::new(false);
        service.set_window(vec![1], usize::MAX);
        {
            let mut state = service.lock();
            state.remember(1, ListWaveformCell::Ready(waveform(2)));
        }
        let old = service.generation();
        service.scan_finished();
        assert_eq!(
            service.cached(1),
            Some(ListWaveformCell::Ready(waveform(2)))
        );
        let mut state = service.lock();
        assert!(
            state
                .finish(Outcome::Compute {
                    generation: old,
                    id: 1,
                    on_demand: true,
                    result: Ok(ListWaveformState::Missing),
                })
                .is_empty()
        );
        assert_eq!(
            state.next_step(Instant::now(), false, ListWaveformSource::Local),
            Step::Run
        );
        let Task::Lookup { ids, .. } = take_task(&mut state) else {
            panic!()
        };
        assert_eq!(ids, vec![1]);
        drop(state);
        service.invalidate();
        assert_eq!(service.cached(1), None);
    }

    #[test]
    fn suspension_cancels_and_idle_wait_returns() {
        let service = ListWaveforms::new(false);
        let cancel = Arc::new(AtomicBool::new(false));
        service.lock().current = Some(Current {
            id: 1,
            cancel: cancel.clone(),
            on_demand: false,
        });
        service.suspend();
        assert!(cancel.load(Ordering::Acquire));
        assert_eq!(
            service
                .lock()
                .next_step(Instant::now(), false, ListWaveformSource::Local),
            Step::Wait(Some(IDLE_POLL))
        );
        assert!(service.wait_idle(Duration::from_millis(10)));
        service.lock().busy = true;
        assert!(!service.wait_idle(Duration::from_millis(10)));
        service.shutdown();
        assert_eq!(
            service
                .lock()
                .next_step(Instant::now(), false, ListWaveformSource::Local),
            Step::Exit
        );
    }

    fn server_lookup(state: &mut State, now: Instant) -> (u64, Vec<i64>) {
        assert_eq!(
            state.next_step(now, false, ListWaveformSource::Server),
            Step::Run
        );
        let Task::Lookup {
            generation,
            ids,
            server,
            ..
        } = take_task(state)
        else {
            panic!("server lookup")
        };
        assert!(server);
        (generation, ids)
    }

    fn answer(
        state: &mut State,
        now: Instant,
        (generation, ids): (u64, Vec<i64>),
        items: Vec<(i64, ListWaveformState)>,
        retry_after: Option<Duration>,
    ) -> Vec<(i64, ListWaveformCell)> {
        state.finish_at(
            Outcome::Lookup {
                generation,
                ids,
                server: true,
                result: Ok(ListWaveformBatch {
                    items: items
                        .into_iter()
                        .map(|(id, state)| ListWaveformItem { id, state })
                        .collect(),
                    retry_after,
                    deferred: None,
                }),
            },
            now,
        )
    }

    #[test]
    fn server_pending_rows_are_retried_while_visible_at_most_five_times() {
        let service = ListWaveforms::new(false);
        // Rows 1 and 2 are on screen, 3 is one screen below.
        service.set_window(vec![1, 2, 3], 2);
        service.set_hold_after_audition(true);
        let mut state = std::mem::take(&mut *service.lock());
        let mut now = Instant::now();
        // With the hold setting on, no POST during the first 3 s of an audition.
        state.playback_start = Some(now - Duration::from_secs(1));
        assert_eq!(
            state.next_step(now, false, ListWaveformSource::Server),
            Step::Wait(Some(Duration::from_secs(2)))
        );
        now += Duration::from_secs(2);
        let request = server_lookup(&mut state, now);
        assert_eq!(request.1, vec![1, 2, 3], "one batch, visible rows first");
        let pending = || ListWaveformState::Pending { previous: None };
        let cells = answer(
            &mut state,
            now,
            request,
            vec![
                (1, pending()),
                (
                    2,
                    ListWaveformState::Pending {
                        previous: Some(waveform(4)),
                    },
                ),
                (3, pending()),
            ],
            Some(Duration::from_secs(3)),
        );
        assert_eq!(cells, vec![(2, ListWaveformCell::Ready(waveform(4)))]);
        assert_eq!(
            state.next_step(now, false, ListWaveformSource::Server),
            Step::Wait(Some(Duration::from_secs(3)))
        );
        // Row 2 scrolls off screen (still in the window): only row 1 is retried.
        state.visible = HashSet::from([1]);
        for retry in 1..=MAX_PENDING_RETRIES {
            now += Duration::from_secs(3);
            let request = server_lookup(&mut state, now);
            assert_eq!(request.1, vec![1], "retry {retry}");
            answer(
                &mut state,
                now,
                request,
                vec![(1, pending())],
                Some(Duration::from_secs(3)),
            );
        }
        now += Duration::from_secs(30);
        assert_eq!(
            state.next_step(now, false, ListWaveformSource::Server),
            Step::Wait(None),
            "no sixth retry"
        );
        // A newly visible row is asked about at once; ready rows are not retried.
        state.window.push(4);
        state.in_window.insert(4);
        let request = server_lookup(&mut state, now);
        assert_eq!(request.1, vec![4]);
        let cells = answer(
            &mut state,
            now,
            request,
            vec![(
                4,
                ListWaveformState::Ready {
                    waveform: waveform(128),
                },
            )],
            None,
        );
        assert_eq!(cells, vec![(4, ListWaveformCell::Ready(waveform(128)))]);
        assert_eq!(
            state.next_step(now, false, ListWaveformSource::Server),
            Step::Wait(None)
        );
    }

    #[test]
    fn server_batches_are_not_held_after_audition_start_by_default() {
        let service = ListWaveforms::new(false);
        service.set_window(vec![1, 2, 3], 2);
        let mut state = std::mem::take(&mut *service.lock());
        assert!(!state.hold_after_audition, "default off");
        let now = Instant::now();
        state.playback_start = Some(now);
        let request = server_lookup(&mut state, now);
        assert_eq!(request.1, vec![1, 2, 3], "sent at once, visible rows first");
    }

    #[test]
    fn held_server_batches_wait_for_the_latest_audition_start_and_are_not_dropped() {
        let service = ListWaveforms::new(false).with_hold_after_audition(true);
        service.set_window(vec![1, 2, 3], 2);
        let mut state = std::mem::take(&mut *service.lock());
        assert!(state.hold_after_audition);
        let mut now = Instant::now();
        state.playback_start = Some(now);
        assert_eq!(
            state.next_step(now, false, ListWaveformSource::Server),
            Step::Wait(Some(PREBUFFER_YIELD))
        );
        assert!(
            state.task.is_none() && state.looked_up.is_empty(),
            "nothing sent"
        );
        // A second audition 2 s later restarts the hold from that start.
        now += Duration::from_secs(2);
        state.playback_start = Some(now);
        now += Duration::from_millis(2500);
        assert_eq!(
            state.next_step(now, false, ListWaveformSource::Server),
            Step::Wait(Some(Duration::from_millis(500)))
        );
        // Rows scrolled in while held join the deferred batch; nothing was dropped.
        state.window.push(4);
        state.in_window.insert(4);
        now += Duration::from_millis(500);
        let request = server_lookup(&mut state, now);
        assert_eq!(request.1, vec![1, 2, 3, 4], "deferred, visible rows first");
        // Turning the setting off releases a held batch at once.
        let service = ListWaveforms::new(false).with_hold_after_audition(true);
        service.set_window(vec![7], 1);
        service.set_hold_after_audition(false);
        let mut state = std::mem::take(&mut *service.lock());
        let now = Instant::now();
        state.playback_start = Some(now);
        assert_eq!(server_lookup(&mut state, now).1, vec![7]);
    }

    #[test]
    fn server_busy_errors_and_offline_roots_follow_their_rules() {
        let service = ListWaveforms::new(false);
        service.set_window(vec![1, 2, 3, 4], 4);
        let mut now = Instant::now();
        let mut state = service.lock();
        let (generation, ids) = server_lookup(&mut state, now);
        // 503 BUSY: nothing answered, everything is asked again after Retry-After.
        assert!(
            state
                .finish_at(
                    Outcome::Lookup {
                        generation,
                        ids,
                        server: true,
                        result: Ok(ListWaveformBatch {
                            deferred: Some(Duration::from_secs(4)),
                            ..ListWaveformBatch::default()
                        }),
                    },
                    now,
                )
                .is_empty()
        );
        assert_eq!(
            state.next_step(now, false, ListWaveformSource::Server),
            Step::Wait(Some(Duration::from_secs(4)))
        );
        now += Duration::from_secs(4);
        let request = server_lookup(&mut state, now);
        assert_eq!(request.1, vec![1, 2, 3, 4]);
        let cells = answer(
            &mut state,
            now,
            request,
            vec![
                (1, ListWaveformState::Missing),
                (
                    2,
                    ListWaveformState::Failed {
                        message: "Cannot decode".into(),
                    },
                ),
                (
                    3,
                    ListWaveformState::RootOffline {
                        root: "SFX".into(),
                        previous: Some(waveform(8)),
                    },
                ),
                (
                    4,
                    ListWaveformState::RootOffline {
                        root: "Other".into(),
                        previous: None,
                    },
                ),
            ],
            None,
        );
        assert_eq!(
            cells,
            vec![
                (1, ListWaveformCell::Unavailable),
                (2, ListWaveformCell::Unavailable),
                (3, ListWaveformCell::Offline(Some(waveform(8)))),
                (4, ListWaveformCell::Offline(None)),
            ]
        );
        // Missing, error and offline rows are not retried on their own.
        now += Duration::from_secs(60);
        assert_eq!(
            state.next_step(now, false, ListWaveformSource::Server),
            Step::Wait(None)
        );
        drop(state);
        // `/roots` shows SFX online again: only its rows are asked again.
        service.roots_online(&["SFX".into()]);
        let mut state = service.lock();
        let request = server_lookup(&mut state, now);
        assert_eq!(request.1, vec![3]);
        let cells = answer(
            &mut state,
            now,
            request,
            vec![(
                3,
                ListWaveformState::Ready {
                    waveform: waveform(16),
                },
            )],
            None,
        );
        assert_eq!(cells, vec![(3, ListWaveformCell::Ready(waveform(16)))]);
        assert!(!state.offline.contains_key(&3) && state.offline.contains_key(&4));
        // A failed request (unreachable server) backs off; the rows wait for a new window.
        state.window.push(5);
        state.in_window.insert(5);
        state.visible.insert(5);
        let (generation, ids) = server_lookup(&mut state, now);
        state.finish_at(
            Outcome::Lookup {
                generation,
                ids,
                server: true,
                result: Err(Error::Database("unreachable".into())),
            },
            now,
        );
        assert_eq!(
            state.next_step(now, false, ListWaveformSource::Server),
            Step::Wait(Some(ERROR_BACKOFF))
        );
        assert_eq!(
            state.next_step(now + ERROR_BACKOFF, false, ListWaveformSource::Server),
            Step::Wait(None)
        );
    }

    #[test]
    fn thumbnails_fill_the_cell_at_the_window_scale_and_empty_files_none() {
        assert!(render_list_waveform(&waveform(0), 1.0).is_none());
        let full = ListWaveform::new(1, vec![-32767, 32767, 0, 0]).unwrap();
        let image = render_list_waveform(&full, 1.0).unwrap();
        let (w, h) = (THUMBNAIL_WIDTH as usize, THUMBNAIL_HEIGHT as usize);
        assert_eq!((image.width(), image.height()), (w as u32, h as u32));
        let pixels = image.as_slice();
        let filled = |x: usize| (0..h).filter(|y| pixels[y * w + x].a > 0).count();
        assert_eq!(filled(0), h, "full-scale column fills the cell");
        assert_eq!(filled(w - 1), 1, "silence keeps a one-pixel line");
        assert!(
            pixels
                .iter()
                .all(|p| p.a == 0 || (p.r == p.g && p.g == p.b)),
            "plain grey"
        );
        let double = render_list_waveform(&waveform(125), 2.0).unwrap();
        assert_eq!(
            (double.width(), double.height()),
            (2 * w as u32, 2 * h as u32)
        );
    }
}
