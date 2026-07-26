//! Background jobs, and the worktree they were started against.
//!
//! A long step (`VERIFY`, `RUN`, a test gate) runs outside the command that
//! started it, so its result has to be collected later: `finish_pending` turns
//! a job snapshot into a result, and `reconcile_gate_jobs` folds finished
//! gated jobs back into the transaction state that is waiting on them.
//!
//! `check_worktree_alive` belongs with them because it guards the same
//! assumption from the other side — that the worktree a session was opened
//! against is still there when the work lands.

use std::sync::Arc;

use anyhow::Result;

use crate::{
    engine::{ForgeQLEngine, PendingGateJob},
    ir::ForgeQLIR,
    result::ForgeQLResult,
};

impl ForgeQLEngine {
    /// Shared handle to the background job registry — lets a transport wait on
    /// a job (`JobRegistry::wait`) without holding its engine lock.
    #[must_use]
    pub fn jobs_handle(&self) -> Arc<crate::jobs::JobRegistry> {
        Arc::clone(&self.jobs)
    }

    /// Convert a finished (or still-running) pending job into its final result.
    ///
    /// Reconciles gate bookkeeping first, so a gated `VERIFY build` that just
    /// completed can immediately satisfy `COMMIT`. A job still running at the
    /// wait deadline (or an unknown id) is surfaced as `JobStarted` — the
    /// caller keeps polling with `JOB STATUS`.
    pub fn finish_pending(
        &mut self,
        pending: &crate::result::PendingExecResult,
        snapshot: Option<crate::jobs::JobSnapshot>,
    ) -> ForgeQLResult {
        self.reconcile_gate_jobs();
        let started = |job_id: &str, step: &str| {
            ForgeQLResult::JobStarted(crate::result::JobStartedResult {
                id: job_id.to_string(),
                label: step.to_string(),
            })
        };
        let Some(snap) = snapshot else {
            return started(&pending.job_id, &pending.step);
        };
        if !matches!(
            snap.state,
            crate::jobs::JobState::Succeeded | crate::jobs::JobState::Failed
        ) {
            return started(&pending.job_id, &pending.step);
        }
        let success = matches!(snap.state, crate::jobs::JobState::Succeeded);
        match pending.kind {
            crate::result::PendingExecKind::Verify => {
                ForgeQLResult::VerifyBuild(crate::result::VerifyBuildResult {
                    step: pending.step.clone(),
                    success,
                    output: snap.output,
                    summary_lines: pending.summary_lines,
                    summary_direction: pending.summary_direction,
                })
            }
            crate::result::PendingExecKind::Run => ForgeQLResult::Run(crate::result::RunResult {
                step: pending.step.clone(),
                success,
                output: snap.output,
                summary_lines: pending.summary_lines,
                summary_direction: pending.summary_direction,
            }),
        }
    }

    /// Fold finished gated background jobs into their session's
    /// `satisfied_gates`. A completed gate only counts when the session's
    /// `mutation_seq` is unchanged since submission — otherwise the job tested
    /// stale sources and the gate stays unsatisfied. Failed and stale entries
    /// are dropped; running ones are kept for the next reconcile.
    pub(crate) fn reconcile_gate_jobs(&mut self) {
        let mut remaining = Vec::with_capacity(self.pending_gate_jobs.len());
        let entries: Vec<PendingGateJob> = self.pending_gate_jobs.drain(..).collect();
        for entry in entries {
            let Some(snap) = self.jobs.status(&entry.job_id) else {
                // Evicted from the registry ring — nothing left to reconcile.
                continue;
            };
            match snap.state {
                crate::jobs::JobState::Queued | crate::jobs::JobState::Running => {
                    remaining.push(entry);
                }
                crate::jobs::JobState::Failed => {}
                crate::jobs::JobState::Succeeded => {
                    if let Some(session) = self.sessions.get_mut(&entry.sid)
                        && session.mutation_seq == entry.mutation_seq_at_start
                    {
                        let _ = session.satisfied_gates.insert(entry.step);
                    }
                }
            }
        }
        self.pending_gate_jobs = remaining;
    }

    /// Guard for session-dependent operations: FIND / SHOW / mutations need a
    /// live worktree directory on disk. Source-management commands (CREATE, USE,
    /// DISCONNECT, SHOW SOURCES/BRANCHES) do not and are exempt. Errors if the
    /// session's worktree has been removed underneath us.
    pub(super) fn check_worktree_alive(&self, sid: Option<&str>, op: &ForgeQLIR) -> Result<()> {
        let Some(mk) = sid else {
            return Ok(());
        };
        let needs_worktree = matches!(
            op,
            ForgeQLIR::FindSymbols { .. }
                | ForgeQLIR::FindUsages { .. }
                | ForgeQLIR::ShowContext { .. }
                | ForgeQLIR::ShowSignature { .. }
                | ForgeQLIR::ShowOutline { .. }
                | ForgeQLIR::ShowMembers { .. }
                | ForgeQLIR::ShowBody { .. }
                | ForgeQLIR::ShowCallees { .. }
                | ForgeQLIR::ShowLines { .. }
                | ForgeQLIR::FindFiles { .. }
                | ForgeQLIR::ChangeContent { .. }
                | ForgeQLIR::FindNode { .. }
                | ForgeQLIR::ShowNode { .. }
                | ForgeQLIR::ShowMore { .. }
                | ForgeQLIR::ChangeNode { .. }
                | ForgeQLIR::ChangeNodeMatching { .. }
                | ForgeQLIR::ChangeNodesFound { .. }
                | ForgeQLIR::InsertNode { .. }
                | ForgeQLIR::DeleteNode { .. }
                | ForgeQLIR::DeleteNodesFound { .. }
                | ForgeQLIR::MoveNodesFoundTo { .. }
                | ForgeQLIR::CopyNodesFoundTo { .. }
                | ForgeQLIR::BeginTransaction { .. }
                | ForgeQLIR::Commit { .. }
                | ForgeQLIR::Rollback { .. }
                | ForgeQLIR::VerifyBuild { .. }
                | ForgeQLIR::Run { .. }
        );
        if needs_worktree
            && let Some(session) = self.sessions.get(mk)
            && !session.worktree_path.is_dir()
        {
            anyhow::bail!(
                "session '{mk}' is stale — the worktree directory \
                 '{}' no longer exists on disk.  \
                 Run USE <source>.<branch> to start a new session.",
                session.worktree_path.display()
            );
        }
        Ok(())
    }
}
