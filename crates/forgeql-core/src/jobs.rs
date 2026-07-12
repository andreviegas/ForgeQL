//! Background job system for build-class commands (`JOB START / STATUS / LIST`).
//!
//! `JOB START '<label>'` resolves a frozen verify step and submits its command
//! to a bounded worker pool, returning a short job id immediately — so a long
//! build never holds the calling request open. `JOB STATUS <id>` and `JOB LIST`
//! poll this in-memory registry.
//!
//! At most `max_concurrent` jobs (from `FORGEQL_MAX_CONCURRENT_JOBS`, default 2)
//! run at once; the rest wait `Queued` in a FIFO queue and start as slots free.
//! This is the backpressure that stops a burst of parallel heavy builds from
//! exhausting machine memory. Each job records its [`ResourceCost`] (resolved
//! from the step's `weight`) for the weight-aware admission added in a later
//! slice. Still deferred: timeout enforcement and resource-budget admission.

#![allow(clippy::module_name_repetitions)]
// Tests use unwrap/expect intentionally — the pedantic lints are for library code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::ResourceCost;

/// Cap on retained job records. The oldest *finished* jobs are evicted first so
/// a long-lived server's registry cannot grow without bound.
const MAX_RETAINED_JOBS: usize = 256;

/// Lifecycle state of a job: `Queued` → `Running` → `Succeeded`/`Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Accepted and waiting for a free worker slot (concurrency cap reached).
    Queued,
    /// The command is executing on a background thread.
    Running,
    /// The command exited 0.
    Succeeded,
    /// The command exited non-zero or could not be spawned.
    Failed,
}

impl JobState {
    /// Lowercase wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// Terminal result of a background command, handed back to the registry when the
/// worker thread finishes.
pub struct JobOutcome {
    /// Whether the command exited successfully.
    pub success: bool,
    /// Combined stdout + stderr.
    pub output: String,
}

/// A queued unit of work: runs once on a worker thread, then yields its outcome.
type BoxedJob = Box<dyn FnOnce() -> JobOutcome + Send + 'static>;

/// Full status of one job, returned by `JOB STATUS`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSnapshot {
    /// Opaque job id (`j-<hex>`).
    pub id: String,
    /// The verify-step name this job runs.
    pub label: String,
    /// Lifecycle state.
    pub state: JobState,
    /// Declared resource footprint (recorded for the future scheduler).
    pub cost: ResourceCost,
    /// Elapsed time so far (live while running, frozen once finished).
    pub elapsed_ms: u64,
    /// Combined stdout + stderr (empty until the job finishes).
    pub output: String,
}

/// One row of `JOB LIST` — the snapshot without the (potentially large) output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    /// Opaque job id (`j-<hex>`).
    pub id: String,
    /// The verify-step name this job runs.
    pub label: String,
    /// Lifecycle state.
    pub state: JobState,
    /// Elapsed time so far (live while running, frozen once finished).
    pub elapsed_ms: u64,
}

/// Internal job record.
struct Job {
    label: String,
    cost: ResourceCost,
    state: JobState,
    output: String,
    /// Set when the job actually starts running; `None` while still queued.
    started: Option<Instant>,
    /// Frozen at completion; while `Running` the elapsed time is computed live.
    elapsed_ms: u64,
    /// The pending closure, held only while `Queued`; taken when a worker slot
    /// opens. Boxed so heterogeneous job bodies share one queue.
    pending: Option<BoxedJob>,
}

impl Job {
    fn live_elapsed_ms(&self) -> u64 {
        match self.state {
            JobState::Queued => 0,
            JobState::Running => self.started.map_or(0, |s| {
                u64::try_from(s.elapsed().as_millis()).unwrap_or(u64::MAX)
            }),
            JobState::Succeeded | JobState::Failed => self.elapsed_ms,
        }
    }

    fn snapshot(&self, id: &str) -> JobSnapshot {
        JobSnapshot {
            id: id.to_string(),
            label: self.label.clone(),
            state: self.state,
            cost: self.cost,
            elapsed_ms: self.live_elapsed_ms(),
            output: self.output.clone(),
        }
    }

    fn summary(&self, id: &str) -> JobSummary {
        JobSummary {
            id: id.to_string(),
            label: self.label.clone(),
            state: self.state,
            elapsed_ms: self.live_elapsed_ms(),
        }
    }
}

/// Mutable registry interior, guarded by the [`JobRegistry`] mutex.
struct Inner {
    jobs: HashMap<String, Job>,
    /// Submission order, for `JOB LIST` and ring eviction.
    order: Vec<String>,
    /// Ids accepted but not yet running, oldest first (FIFO admission).
    queue: VecDeque<String>,
    /// Number of jobs currently executing on a worker thread.
    running: usize,
    /// Maximum jobs allowed to run at once (always >= 1).
    max_concurrent: usize,
    /// Monotonic id counter.
    seq: u64,
}

impl Inner {
    /// Evict the oldest *finished* jobs beyond the retention cap. Running jobs
    /// are never evicted — their worker thread still holds the id.
    fn evict(&mut self) {
        while self.order.len() > MAX_RETAINED_JOBS {
            let mut victim = None;
            for (i, id) in self.order.iter().enumerate() {
                if self
                    .jobs
                    .get(id)
                    .is_some_and(|j| matches!(j.state, JobState::Succeeded | JobState::Failed))
                {
                    victim = Some(i);
                    break;
                }
            }
            match victim {
                Some(i) => {
                    let id = self.order.remove(i);
                    let _ = self.jobs.remove(&id);
                }
                None => break,
            }
        }
    }
}

/// Process-wide registry of background jobs. Cheap to share via `Arc`.
pub struct JobRegistry {
    inner: Mutex<Inner>,
    /// Signalled on every job completion — lets `wait` block without polling.
    done: Condvar,
}

impl JobRegistry {
    /// Create an empty registry that runs at most `max_concurrent` jobs at
    /// once (clamped to a minimum of 1); excess submissions queue FIFO.
    #[must_use]
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                jobs: HashMap::new(),
                order: Vec::new(),
                queue: VecDeque::new(),
                running: 0,
                max_concurrent: max_concurrent.max(1),
                seq: 0,
            }),
            done: Condvar::new(),
        }
    }

    /// Construct a registry whose concurrency cap comes from the
    /// `FORGEQL_MAX_CONCURRENT_JOBS` environment variable. The default of 2
    /// allows one long gate and one quick build to overlap while still
    /// stopping a burst of heavy `JOB START` builds from exhausting memory.
    /// Missing, unparseable, or zero values fall back to 2.
    #[must_use]
    pub fn from_env() -> Self {
        let cap = std::env::var("FORGEQL_MAX_CONCURRENT_JOBS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(2);
        Self::new(cap)
    }

    /// Accept `job_fn`, record it as `Queued`, and return its id immediately.
    /// The job starts as soon as a worker slot is free — right away when the
    /// pool is below its concurrency cap, otherwise it waits FIFO in the queue.
    /// The worker updates the record to `Succeeded`/`Failed` on completion and
    /// pumps the next queued job.
    pub fn start<F>(self: &Arc<Self>, label: String, cost: ResourceCost, job_fn: F) -> String
    where
        F: FnOnce() -> JobOutcome + Send + 'static,
    {
        let id = {
            let mut inner = self.lock();
            inner.seq += 1;
            let id = format!("j-{:06x}", inner.seq);
            let _ = inner.jobs.insert(
                id.clone(),
                Job {
                    label,
                    cost,
                    state: JobState::Queued,
                    output: String::new(),
                    started: None,
                    elapsed_ms: 0,
                    pending: Some(Box::new(job_fn)),
                },
            );
            inner.order.push(id.clone());
            inner.queue.push_back(id.clone());
            inner.evict();
            id
        };
        self.pump();
        id
    }

    /// Start as many queued jobs as the concurrency cap allows. Called after a
    /// submission and after every completion. Worker threads are spawned
    /// *outside* the registry lock so a job that finishes instantly can re-enter
    /// `complete` (which re-locks) without deadlocking.
    fn pump(self: &Arc<Self>) {
        let mut to_spawn: Vec<(String, BoxedJob)> = Vec::new();
        let mut inner = self.lock();
        while inner.running < inner.max_concurrent {
            let Some(id) = inner.queue.pop_front() else {
                break;
            };
            let Some(job) = inner.jobs.get_mut(&id) else {
                continue;
            };
            let Some(job_fn) = job.pending.take() else {
                continue;
            };
            job.state = JobState::Running;
            job.started = Some(Instant::now());
            inner.running += 1;
            to_spawn.push((id, job_fn));
        }
        drop(inner);
        for (id, job_fn) in to_spawn {
            let registry = Arc::clone(self);
            let thread_id = id.clone();
            // A std thread (not a tokio task): the work runs blocking
            // `std::process` commands and is independent of the async MCP layer.
            let spawned = std::thread::Builder::new()
                .name(format!("forgeql-job-{id}"))
                .spawn(move || {
                    let outcome = job_fn();
                    registry.complete(&thread_id, &outcome);
                });
            if let Err(err) = spawned {
                self.complete(
                    &id,
                    &JobOutcome {
                        success: false,
                        output: format!("failed to spawn job worker thread: {err}"),
                    },
                );
            }
        }
    }

    /// `JOB STATUS <id>` — full snapshot, or `None` if the id is unknown.
    #[must_use]
    pub fn status(&self, id: &str) -> Option<JobSnapshot> {
        let inner = self.lock();
        inner.jobs.get(id).map(|job| job.snapshot(id))
    }

    /// Block until job `id` reaches a terminal state or `timeout` elapses.
    ///
    /// Returns the job's snapshot at wake-up time: check `state` to tell a
    /// finished job from one that is still running when the timeout fired.
    /// `None` means the id is unknown (never submitted or already evicted).
    pub fn wait(&self, id: &str, timeout: Duration) -> Option<JobSnapshot> {
        let deadline = Instant::now() + timeout;
        let mut inner = self.lock();
        loop {
            let snap = match inner.jobs.get(id) {
                None => return None,
                Some(job) => job.snapshot(id),
            };
            if matches!(snap.state, JobState::Succeeded | JobState::Failed) {
                return Some(snap);
            }
            let now = Instant::now();
            if now >= deadline {
                return Some(snap);
            }
            let (guard, _timed_out) = self
                .done
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(PoisonError::into_inner);
            inner = guard;
        }
    }

    /// `JOB LIST` — summaries in submission order (newest last).
    #[must_use]
    pub fn list(&self) -> Vec<JobSummary> {
        let inner = self.lock();
        inner
            .order
            .iter()
            .filter_map(|id| inner.jobs.get(id).map(|job| job.summary(id)))
            .collect()
    }

    fn complete(self: &Arc<Self>, id: &str, outcome: &JobOutcome) {
        let mut inner = self.lock();
        if let Some(job) = inner.jobs.get_mut(id) {
            job.state = if outcome.success {
                JobState::Succeeded
            } else {
                JobState::Failed
            };
            job.output.clone_from(&outcome.output);
            job.elapsed_ms = job.started.map_or(0, |s| {
                u64::try_from(s.elapsed().as_millis()).unwrap_or(u64::MAX)
            });
            inner.running = inner.running.saturating_sub(1);
        }
        drop(inner);
        // Wake any `wait` callers, then admit the next queued job.
        self.done.notify_all();
        self.pump();
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn medium() -> ResourceCost {
        crate::config::WeightLevel::Medium.cost()
    }

    fn wait_done(reg: &Arc<JobRegistry>, id: &str) -> JobSnapshot {
        let mut snap = reg.status(id).expect("job exists");
        for _ in 0..400 {
            if snap.state != JobState::Running {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            snap = reg.status(id).expect("job exists");
        }
        snap
    }

    #[test]
    fn job_runs_to_success_and_is_queryable() {
        let reg = Arc::new(JobRegistry::new(4));
        let id = reg.start("build".to_string(), medium(), || JobOutcome {
            success: true,
            output: "ok\n".to_string(),
        });
        let snap = wait_done(&reg, &id);
        assert_eq!(snap.state, JobState::Succeeded);
        assert_eq!(snap.label, "build");
        assert!(snap.output.contains("ok"));
        assert_eq!(snap.cost, medium());
    }

    #[test]
    fn failed_job_reports_failed_state() {
        let reg = Arc::new(JobRegistry::new(4));
        let id = reg.start("lint".to_string(), medium(), || JobOutcome {
            success: false,
            output: "boom".to_string(),
        });
        assert_eq!(wait_done(&reg, &id).state, JobState::Failed);
    }

    #[test]
    fn unknown_id_is_none_and_list_tracks_jobs() {
        let reg = Arc::new(JobRegistry::new(4));
        assert!(reg.status("j-nope").is_none());
        let _ = reg.start("a".to_string(), medium(), || JobOutcome {
            success: true,
            output: String::new(),
        });
        let _ = reg.start("b".to_string(), medium(), || JobOutcome {
            success: true,
            output: String::new(),
        });
        let list = reg.list();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn cap_of_one_runs_jobs_sequentially() {
        let reg = Arc::new(JobRegistry::new(1));
        let running = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut ids = Vec::new();
        for _ in 0..4 {
            let running = Arc::clone(&running);
            let max_seen = Arc::clone(&max_seen);
            let id = reg.start("build".to_string(), medium(), move || {
                let now = running.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                let _ = max_seen.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(15));
                let _ = running.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                JobOutcome {
                    success: true,
                    output: String::new(),
                }
            });
            ids.push(id);
        }
        for id in &ids {
            assert_eq!(wait_done(&reg, id).state, JobState::Succeeded);
        }
        assert_eq!(
            max_seen.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "cap of 1 must serialise jobs"
        );
    }

    #[test]
    fn queued_job_waits_until_a_slot_frees() {
        let reg = Arc::new(JobRegistry::new(1));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let first = reg.start("a".to_string(), medium(), move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            JobOutcome {
                success: true,
                output: String::new(),
            }
        });
        let second = reg.start("b".to_string(), medium(), || JobOutcome {
            success: true,
            output: String::new(),
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("first job should start");
        assert_eq!(
            reg.status(&first).expect("first job exists").state,
            JobState::Running
        );
        assert_eq!(
            reg.status(&second).expect("second job exists").state,
            JobState::Queued
        );
        let _ = release_tx.send(());
        assert_eq!(wait_done(&reg, &first).state, JobState::Succeeded);
        assert_eq!(wait_done(&reg, &second).state, JobState::Succeeded);
    }

    #[test]
    fn wait_returns_terminal_snapshot_or_times_out() {
        let reg = Arc::new(JobRegistry::new(1));
        let id = reg.start("slow".into(), medium(), || {
            std::thread::sleep(Duration::from_millis(150));
            JobOutcome {
                success: true,
                output: "done".into(),
            }
        });
        // Deadline shorter than the job: returns a non-terminal snapshot.
        let snap = reg.wait(&id, Duration::from_millis(1)).expect("known id");
        assert!(matches!(snap.state, JobState::Queued | JobState::Running));
        // Generous deadline: blocks until the terminal state arrives.
        let snap = reg.wait(&id, Duration::from_secs(10)).expect("known id");
        assert_eq!(snap.state, JobState::Succeeded);
        assert_eq!(snap.output, "done");
        // Unknown ids are distinguished from running ones.
        assert!(reg.wait("j-nope", Duration::from_millis(1)).is_none());
    }
}
