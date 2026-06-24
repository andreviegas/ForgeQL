//! Background job system for build-class commands (`JOB START / STATUS / LIST`).
//!
//! Slice 1 of the server-side job scheduler. `JOB START '<label>'` resolves a
//! frozen verify step, runs its command on a background thread, and returns a
//! short job id immediately — so a long build never holds the calling request
//! open. `JOB STATUS <id>` and `JOB LIST` poll this in-memory registry.
//!
//! Deliberately NOT in this slice: queueing, concurrency caps, timeout
//! enforcement, resource-budget admission. Each job still records its
//! [`ResourceCost`] (resolved from the step's `weight`) so the scheduler added
//! in later slices can consume it without a data migration.

#![allow(clippy::module_name_repetitions)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::config::ResourceCost;

/// Cap on retained job records. The oldest *finished* jobs are evicted first so
/// a long-lived server's registry cannot grow without bound.
const MAX_RETAINED_JOBS: usize = 256;

/// Lifecycle state of a job. Slice 1 has no `Queued` (jobs run immediately); the
/// queue arrives in Slice 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
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
    started: Instant,
    /// Frozen at completion; while `Running` the elapsed time is computed live.
    elapsed_ms: u64,
}

impl Job {
    fn live_elapsed_ms(&self) -> u64 {
        match self.state {
            JobState::Running => {
                u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
            }
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
#[derive(Default)]
struct Inner {
    jobs: HashMap<String, Job>,
    /// Submission order, for `JOB LIST` and ring eviction.
    order: Vec<String>,
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
                    .is_some_and(|j| j.state != JobState::Running)
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
#[derive(Default)]
pub struct JobRegistry {
    inner: Mutex<Inner>,
}

impl JobRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn `job_fn` on a background thread, record a `Running` job, and return
    /// its id immediately. The worker updates the record to `Succeeded`/`Failed`
    /// on completion.
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
                    state: JobState::Running,
                    output: String::new(),
                    started: Instant::now(),
                    elapsed_ms: 0,
                },
            );
            inner.order.push(id.clone());
            inner.evict();
            id
        };

        let registry = Arc::clone(self);
        let thread_id = id.clone();
        // A std thread (not a tokio task): the work runs blocking `std::process`
        // commands, and this primitive is independent of the async MCP layer.
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
        id
    }

    /// `JOB STATUS <id>` — full snapshot, or `None` if the id is unknown.
    #[must_use]
    pub fn status(&self, id: &str) -> Option<JobSnapshot> {
        let inner = self.lock();
        inner.jobs.get(id).map(|job| job.snapshot(id))
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

    fn complete(&self, id: &str, outcome: &JobOutcome) {
        let mut inner = self.lock();
        if let Some(job) = inner.jobs.get_mut(id) {
            job.state = if outcome.success {
                JobState::Succeeded
            } else {
                JobState::Failed
            };
            job.output.clone_from(&outcome.output);
            job.elapsed_ms = u64::try_from(job.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
        let reg = Arc::new(JobRegistry::new());
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
        let reg = Arc::new(JobRegistry::new());
        let id = reg.start("lint".to_string(), medium(), || JobOutcome {
            success: false,
            output: "boom".to_string(),
        });
        assert_eq!(wait_done(&reg, &id).state, JobState::Failed);
    }

    #[test]
    fn unknown_id_is_none_and_list_tracks_jobs() {
        let reg = Arc::new(JobRegistry::new());
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
}
