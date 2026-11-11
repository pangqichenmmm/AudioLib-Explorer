// app/audiolib-desktop/src/slice_jobs.rs
//! One running and at most one latest pending Waveform selection job per worker.
use std::sync::{
    Condvar, Mutex, MutexGuard, PoisonError,
    atomic::{AtomicBool, Ordering},
};

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A worker queue that coalesces pending jobs without spawning extra threads.
///
/// Owners run `run` on their persistent worker and set their shutdown flag before
/// calling `wake_shutdown`. Running work completes normally; pending work is dropped
/// on replacement, clearing or shutdown. Closures and their captured ownership guards
/// are always run/dropped outside the predicate lock.
#[derive(Default)]
pub(super) struct LatestJobQueue {
    pending: Mutex<Option<Job>>,
    wake: Condvar,
}

impl LatestJobQueue {
    fn pending(&self) -> MutexGuard<'_, Option<Job>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replace a pending job, retaining the currently executing job unchanged.
    pub(super) fn submit(&self, job: Job) {
        let superseded = {
            let mut pending = self.pending();
            let superseded = pending.replace(job);
            self.wake.notify_one();
            superseded
        };
        // Captured controller guards can lock controller.slice or submit more
        // work from Drop; neither operation may run with the queue lock held.
        drop(superseded);
    }

    /// Execute the latest pending job until the owner's shutdown flag is set.
    /// Call from a worker thread; no thread is created by the queue itself.
    pub(super) fn run(&self, shutdown: &AtomicBool) {
        loop {
            let mut pending = self.pending();
            while pending.is_none() && !shutdown.load(Ordering::Acquire) {
                pending = self
                    .wake
                    .wait(pending)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            let job = pending.take();
            if shutdown.load(Ordering::Acquire) {
                drop(pending);
                drop(job);
                return;
            }
            drop(pending);
            if let Some(job) = job {
                job();
            }
        }
    }

    /// Notify shutdown while holding the wait predicate lock, so a worker cannot
    /// observe the old flag and start waiting after this notification was sent.
    pub(super) fn wake_shutdown(&self) {
        let _pending = self.pending();
        self.wake.notify_all();
    }

    /// Discard pending work immediately, dropping captures outside the queue lock.
    pub(super) fn clear_pending(&self) {
        let discarded = self.pending().take();
        drop(discarded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, atomic::AtomicUsize, mpsc},
        thread,
        time::Duration,
    };

    const WAIT: Duration = Duration::from_secs(5);

    struct DropChecksQueue {
        queue: Arc<LatestJobQueue>,
        count: Arc<AtomicUsize>,
    }

    impl Drop for DropChecksQueue {
        fn drop(&mut self) {
            // The real capture's Drop locks controller.slice. Re-entering this
            // queue's mutex is stricter and catches dropping under its own lock.
            let _pending = self.queue.pending();
            self.count.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn blocked_worker_executes_first_and_only_the_latest_of_one_hundred_submissions() {
        let queue = Arc::new(LatestJobQueue::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let (started, wait_started) = mpsc::channel();
        let (release, wait_release) = mpsc::channel();
        let (finished, wait_finished) = mpsc::channel();
        let first_observed = observed.clone();
        queue.submit(Box::new(move || {
            first_observed.lock().unwrap().push(0);
            started.send(()).unwrap();
            wait_release.recv_timeout(WAIT).unwrap();
        }));
        let worker_queue = queue.clone();
        let worker_shutdown = shutdown.clone();
        let worker = thread::spawn(move || worker_queue.run(&worker_shutdown));
        wait_started.recv_timeout(WAIT).unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let submit_queue = queue.clone();
        let submit_observed = observed.clone();
        let submit_drops = drops.clone();
        let (submitted, wait_submitted) = mpsc::channel();
        let submitter = thread::spawn(move || {
            for index in 1..=100 {
                let observed = submit_observed.clone();
                let finished = finished.clone();
                let guard = DropChecksQueue {
                    queue: submit_queue.clone(),
                    count: submit_drops.clone(),
                };
                submit_queue.submit(Box::new(move || {
                    let _guard = guard;
                    observed.lock().unwrap().push(index);
                    if index == 100 {
                        finished.send(()).unwrap();
                    }
                }));
            }
            submitted.send(()).unwrap();
        });
        wait_submitted
            .recv_timeout(WAIT)
            .expect("Superseded captures must drop outside the queue lock");
        submitter.join().unwrap();
        assert_eq!(drops.load(Ordering::Acquire), 99);
        release.send(()).unwrap();
        wait_finished.recv_timeout(WAIT).unwrap();
        shutdown.store(true, Ordering::Release);
        queue.wake_shutdown();
        worker.join().unwrap();
        assert_eq!(*observed.lock().unwrap(), [0, 100]);
        assert_eq!(drops.load(Ordering::Acquire), 100);
    }

    #[test]
    fn clearing_and_shutdown_drop_pending_captures_outside_the_queue_lock() {
        let queue = Arc::new(LatestJobQueue::default());
        let drops = Arc::new(AtomicUsize::new(0));
        let (done, wait_done) = mpsc::channel();
        let tested_queue = queue.clone();
        let tested_drops = drops.clone();
        let tester = thread::spawn(move || {
            for _ in 0..2 {
                let guard = DropChecksQueue {
                    queue: tested_queue.clone(),
                    count: tested_drops.clone(),
                };
                tested_queue.submit(Box::new(move || {
                    drop(guard);
                    panic!("Pending work must not execute");
                }));
                if tested_drops.load(Ordering::Acquire) == 0 {
                    tested_queue.clear_pending();
                }
            }
            tested_queue.run(&AtomicBool::new(true));
            done.send(()).unwrap();
        });
        wait_done
            .recv_timeout(WAIT)
            .expect("clear/shutdown may not drop a closure while locked");
        tester.join().unwrap();
        assert_eq!(drops.load(Ordering::Acquire), 2);
        assert!(queue.pending().is_none());
    }

    #[test]
    fn shutdown_wakes_an_idle_worker_without_a_job() {
        let queue = Arc::new(LatestJobQueue::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let (done, wait_done) = mpsc::channel();
        let worker_queue = queue.clone();
        let worker_shutdown = shutdown.clone();
        let worker = thread::spawn(move || {
            worker_queue.run(&worker_shutdown);
            done.send(()).unwrap();
        });
        shutdown.store(true, Ordering::Release);
        queue.wake_shutdown();
        wait_done.recv_timeout(WAIT).unwrap();
        worker.join().unwrap();
    }
}
