//! Background job system: a fixed worker pool with progress reporting and
//! cooperative cancellation. The UI thread only ever polls snapshots and
//! result slots, so it never blocks on work.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use thiserror::Error;

pub type JobId = u64;

/// Finished jobs kept for the Jobs panel before the oldest are dropped.
const KEEP_FINISHED: usize = 200;
/// Sentinel for "progress unknown".
const NO_PROGRESS: u32 = u32::MAX;

/// Why a job did not produce a value.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum JobError {
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
    #[error("panicked: {0}")]
    Panicked(String),
}

impl From<crate::backend::BackendError> for JobError {
    fn from(e: crate::backend::BackendError) -> Self {
        match e {
            crate::backend::BackendError::Cancelled => JobError::Cancelled,
            other => JobError::Failed(other.to_string()),
        }
    }
}

/// Lifecycle of a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Finished,
    Failed(String),
    Cancelled,
}

impl JobState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, JobState::Queued | JobState::Running)
    }

    pub fn label(&self) -> &str {
        match self {
            JobState::Queued => "Queued",
            JobState::Running => "Running",
            JobState::Finished => "Done",
            JobState::Failed(_) => "Failed",
            JobState::Cancelled => "Cancelled",
        }
    }
}

/// Handed to the job closure: progress, status text and the cancel flag.
#[derive(Clone)]
pub struct JobContext {
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
    status: Arc<Mutex<String>>,
    notify: Option<Notifier>,
}

impl Default for JobContext {
    fn default() -> Self {
        Self::detached()
    }
}

impl JobContext {
    /// A context not attached to any job system (synchronous callers, tests).
    pub fn detached() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(AtomicU32::new(NO_PROGRESS)),
            status: Arc::new(Mutex::new(String::new())),
            notify: None,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// `Err(JobError::Cancelled)` when cancellation was requested.
    pub fn check(&self) -> Result<(), JobError> {
        if self.is_cancelled() {
            Err(JobError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Progress as a fraction in `0..=1`.
    pub fn set_fraction(&self, f: f32) {
        let f = if f.is_finite() {
            f.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let old = self.progress.swap(f.to_bits(), Ordering::Relaxed);
        // Repaint at most every ~1% to keep the UI cheap.
        let old_f = if old == NO_PROGRESS {
            -1.0
        } else {
            f32::from_bits(old)
        };
        if (f - old_f).abs() >= 0.01 || f >= 1.0 {
            self.poke();
        }
    }

    /// Progress as `done / total` (no-op when `total == 0`).
    pub fn set_progress(&self, done: u64, total: u64) {
        if total > 0 {
            self.set_fraction((done as f64 / total as f64) as f32);
        }
    }

    pub fn progress(&self) -> Option<f32> {
        let bits = self.progress.load(Ordering::Relaxed);
        (bits != NO_PROGRESS).then(|| f32::from_bits(bits))
    }

    pub fn set_status(&self, s: impl Into<String>) {
        if let Ok(mut g) = self.status.lock() {
            *g = s.into();
        }
        self.poke();
    }

    pub fn status(&self) -> String {
        self.status.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn poke(&self) {
        if let Some(n) = &self.notify {
            n();
        }
    }
}

type Notifier = Arc<dyn Fn() + Send + Sync>;
type Task = Box<dyn FnOnce() + Send>;

struct JobRecord {
    id: JobId,
    name: String,
    ctx: JobContext,
    state: Mutex<JobState>,
    created: Instant,
    started: Mutex<Option<Instant>>,
    finished: Mutex<Option<Instant>>,
}

impl JobRecord {
    fn set_state(&self, s: JobState) {
        let terminal = s.is_terminal();
        if let Ok(mut g) = self.state.lock() {
            *g = s;
        }
        let now = Some(Instant::now());
        if terminal {
            if let Ok(mut g) = self.finished.lock() {
                *g = now;
            }
        } else if let Ok(mut g) = self.started.lock() {
            *g = now;
        }
    }

    fn state(&self) -> JobState {
        self.state
            .lock()
            .map(|g| g.clone())
            .unwrap_or(JobState::Failed("poisoned".into()))
    }
}

/// Read-only view of a job for the UI.
#[derive(Clone, Debug)]
pub struct JobSnapshot {
    pub id: JobId,
    pub name: String,
    pub state: JobState,
    pub progress: Option<f32>,
    pub status: String,
    pub elapsed: Duration,
}

struct Shared {
    jobs: Mutex<Vec<Arc<JobRecord>>>,
    queue: Mutex<VecDeque<Task>>,
    wake: Condvar,
    shutdown: AtomicBool,
    next_id: AtomicU64,
    notify: Mutex<Option<Notifier>>,
}

/// Worker pool. Cheap to clone; all clones share the same pool.
#[derive(Clone)]
pub struct JobSystem {
    shared: Arc<Shared>,
}

impl JobSystem {
    /// Starts `workers` threads (clamped to `1..=32`).
    pub fn new(workers: usize) -> Self {
        let shared = Arc::new(Shared {
            jobs: Mutex::new(Vec::new()),
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
            next_id: AtomicU64::new(1),
            notify: Mutex::new(None),
        });
        for i in 0..workers.clamp(1, 32) {
            let shared = Arc::clone(&shared);
            let spawned = std::thread::Builder::new()
                .name(format!("studio-worker-{i}"))
                .spawn(move || worker_loop(&shared));
            if let Err(e) = spawned {
                log::error!("could not start worker thread {i}: {e}");
            }
        }
        Self { shared }
    }

    /// A pool sized for the machine (2 to 8 workers).
    pub fn with_default_workers() -> Self {
        let n = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self::new(n.clamp(2, 8))
    }

    /// Called (from worker threads) whenever progress or state changes;
    /// the app passes `egui::Context::request_repaint`.
    pub fn set_notifier(&self, f: impl Fn() + Send + Sync + 'static) {
        if let Ok(mut g) = self.shared.notify.lock() {
            *g = Some(Arc::new(f));
        }
    }

    fn notifier(&self) -> Option<Notifier> {
        self.shared.notify.lock().ok().and_then(|g| g.clone())
    }

    /// Queues `f` and returns a handle to its result.
    pub fn spawn<T, F>(&self, name: impl Into<String>, f: F) -> JobHandle<T>
    where
        T: Send + 'static,
        F: FnOnce(&JobContext) -> Result<T, JobError> + Send + 'static,
    {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let name = name.into();
        let notify = self.notifier();
        let ctx = JobContext {
            notify: notify.clone(),
            ..JobContext::detached()
        };
        let record = Arc::new(JobRecord {
            id,
            name: name.clone(),
            ctx: ctx.clone(),
            state: Mutex::new(JobState::Queued),
            created: Instant::now(),
            started: Mutex::new(None),
            finished: Mutex::new(None),
        });
        let slot: Arc<Mutex<Option<Result<T, JobError>>>> = Arc::new(Mutex::new(None));
        let handle = JobHandle {
            id,
            ctx: ctx.clone(),
            slot: Arc::clone(&slot),
        };

        let rec = Arc::clone(&record);
        let task: Task = Box::new(move || {
            let result = if rec.ctx.is_cancelled() {
                Err(JobError::Cancelled)
            } else {
                rec.set_state(JobState::Running);
                log::debug!("job {} '{}' started", rec.id, rec.name);
                match catch_unwind(AssertUnwindSafe(|| f(&rec.ctx))) {
                    Ok(r) => r,
                    Err(p) => Err(JobError::Panicked(crate::crash::panic_message(p.as_ref()))),
                }
            };
            let result = match result {
                Ok(_) if rec.ctx.is_cancelled() => Err(JobError::Cancelled),
                other => other,
            };
            let state = match &result {
                Ok(_) => JobState::Finished,
                Err(JobError::Cancelled) => JobState::Cancelled,
                Err(e) => JobState::Failed(e.to_string()),
            };
            match &state {
                JobState::Failed(e) => log::warn!("job {} '{}' failed: {e}", rec.id, rec.name),
                s => log::debug!("job {} '{}' {}", rec.id, rec.name, s.label()),
            }
            if let Ok(mut g) = slot.lock() {
                *g = Some(result);
            }
            rec.set_state(state);
            if let Some(n) = &notify {
                n();
            }
        });

        if let Ok(mut jobs) = self.shared.jobs.lock() {
            jobs.push(record);
            prune_finished(&mut jobs);
        }
        if let Ok(mut q) = self.shared.queue.lock() {
            q.push_back(task);
        }
        self.shared.wake.notify_one();
        handle
    }

    pub fn snapshot(&self) -> Vec<JobSnapshot> {
        let Ok(jobs) = self.shared.jobs.lock() else {
            return Vec::new();
        };
        jobs.iter()
            .map(|r| {
                let state = r.state();
                let started = r.started.lock().ok().and_then(|g| *g);
                let finished = r.finished.lock().ok().and_then(|g| *g);
                let elapsed = match (started, finished) {
                    (Some(s), Some(f)) => f.saturating_duration_since(s),
                    (Some(s), None) => s.elapsed(),
                    _ => r.created.elapsed(),
                };
                JobSnapshot {
                    id: r.id,
                    name: r.name.clone(),
                    progress: if state == JobState::Finished {
                        Some(1.0)
                    } else {
                        r.ctx.progress()
                    },
                    status: r.ctx.status(),
                    state,
                    elapsed,
                }
            })
            .collect()
    }

    pub fn cancel(&self, id: JobId) {
        if let Ok(jobs) = self.shared.jobs.lock()
            && let Some(r) = jobs.iter().find(|r| r.id == id)
        {
            r.ctx.cancel();
            log::info!("cancel requested for job {id} '{}'", r.name);
        }
    }

    pub fn cancel_all(&self) {
        if let Ok(jobs) = self.shared.jobs.lock() {
            for r in jobs.iter() {
                r.ctx.cancel();
            }
        }
    }

    /// Drops finished, failed and cancelled jobs from the list.
    pub fn clear_finished(&self) {
        if let Ok(mut jobs) = self.shared.jobs.lock() {
            jobs.retain(|r| !r.state().is_terminal());
        }
    }

    /// Queued plus running jobs.
    pub fn active_count(&self) -> usize {
        self.shared
            .jobs
            .lock()
            .map(|j| j.iter().filter(|r| !r.state().is_terminal()).count())
            .unwrap_or(0)
    }

    /// Cancels everything and stops the workers after their current task.
    pub fn shutdown(&self) {
        self.cancel_all();
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
    }
}

fn prune_finished(jobs: &mut Vec<Arc<JobRecord>>) {
    let finished = jobs.iter().filter(|r| r.state().is_terminal()).count();
    let mut excess = finished.saturating_sub(KEEP_FINISHED);
    if excess > 0 {
        jobs.retain(|r| {
            if excess > 0 && r.state().is_terminal() {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }
}

fn worker_loop(shared: &Shared) {
    loop {
        let task = {
            let Ok(mut q) = shared.queue.lock() else {
                return;
            };
            loop {
                if shared.shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(t) = q.pop_front() {
                    break t;
                }
                q = match shared.wake.wait(q) {
                    Ok(q) => q,
                    Err(_) => return,
                };
            }
        };
        task();
    }
}

/// Typed handle to a job's eventual result.
pub struct JobHandle<T> {
    id: JobId,
    ctx: JobContext,
    slot: Arc<Mutex<Option<Result<T, JobError>>>>,
}

impl<T> JobHandle<T> {
    pub fn id(&self) -> JobId {
        self.id
    }

    pub fn cancel(&self) {
        self.ctx.cancel();
    }

    pub fn progress(&self) -> Option<f32> {
        self.ctx.progress()
    }

    pub fn is_done(&self) -> bool {
        self.slot.lock().map(|g| g.is_some()).unwrap_or(true)
    }

    /// Takes the result if the job has finished.
    pub fn try_take(&self) -> Option<Result<T, JobError>> {
        self.slot.lock().ok().and_then(|mut g| g.take())
    }

    /// Blocks (polling) until done or `timeout` elapses. Not for the UI thread.
    pub fn wait(&self, timeout: Duration) -> Option<Result<T, JobError>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(r) = self.try_take() {
                return Some(r);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(10);

    #[test]
    fn runs_and_returns() {
        let js = JobSystem::new(2);
        let h = js.spawn("sum", |ctx| {
            let mut s = 0u64;
            for i in 0..=100 {
                s += i;
                ctx.set_progress(i, 100);
            }
            ctx.set_status("done");
            Ok(s)
        });
        assert_eq!(h.wait(T), Some(Ok(5050)));
        let snap = js.snapshot();
        assert_eq!(snap.len(), 1);
        // State is set right after the slot is filled.
        let deadline = Instant::now() + T;
        while js.snapshot()[0].state != JobState::Finished && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let s = &js.snapshot()[0];
        assert_eq!(s.state, JobState::Finished);
        assert_eq!(s.progress, Some(1.0));
        assert_eq!(s.status, "done");
        js.clear_finished();
        assert!(js.snapshot().is_empty());
        js.shutdown();
    }

    #[test]
    fn cancellation_is_cooperative() {
        let js = JobSystem::new(1);
        let started = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&started);
        let h = js.spawn("spin", move |ctx| -> Result<(), JobError> {
            s2.store(true, Ordering::SeqCst);
            loop {
                ctx.check()?;
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        // Queued behind the spinner on a single worker; cancelled before it runs.
        let queued = js.spawn("queued", |_| Ok(1));
        queued.cancel();
        while !started.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        js.cancel(h.id());
        assert_eq!(h.wait(T), Some(Err(JobError::Cancelled)));
        assert_eq!(queued.wait(T), Some(Err(JobError::Cancelled)));
        js.shutdown();
    }

    #[test]
    fn failures_and_panics_are_captured() {
        let js = JobSystem::new(2);
        let a = js.spawn("fail", |_| -> Result<(), JobError> {
            Err(JobError::Failed("bad header at 0x10".into()))
        });
        let b = js.spawn("panic", |_| -> Result<(), JobError> { panic!("kaboom") });
        assert_eq!(
            a.wait(T),
            Some(Err(JobError::Failed("bad header at 0x10".into())))
        );
        match b.wait(T) {
            Some(Err(JobError::Panicked(m))) => assert!(m.contains("kaboom")),
            other => panic!("unexpected {other:?}"),
        }
        // The pool survives a panicking job.
        assert_eq!(js.spawn("after", |_| Ok(7)).wait(T), Some(Ok(7)));
        js.shutdown();
    }

    #[test]
    fn notifier_fires_and_progress_clamps() {
        let js = JobSystem::new(1);
        let hits = Arc::new(AtomicU64::new(0));
        let h2 = Arc::clone(&hits);
        js.set_notifier(move || {
            h2.fetch_add(1, Ordering::Relaxed);
        });
        let h = js.spawn("p", |ctx| {
            ctx.set_fraction(2.0);
            ctx.set_fraction(f32::NAN);
            Ok(ctx.progress())
        });
        assert_eq!(h.wait(T), Some(Ok(Some(0.0))));
        assert!(hits.load(Ordering::Relaxed) >= 2);
        js.shutdown();
    }

    #[test]
    fn active_count_tracks_jobs() {
        let js = JobSystem::new(1);
        let gate = Arc::new(AtomicBool::new(false));
        let g2 = Arc::clone(&gate);
        let h = js.spawn("wait", move |_| {
            while !g2.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        });
        assert_eq!(js.active_count(), 1);
        gate.store(true, Ordering::SeqCst);
        assert_eq!(h.wait(T), Some(Ok(())));
        let deadline = Instant::now() + T;
        while js.active_count() != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(js.active_count(), 0);
        js.shutdown();
    }
}
