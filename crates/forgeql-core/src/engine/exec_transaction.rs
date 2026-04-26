use anyhow::Result;
use tracing::warn;

use crate::{
    git::{self as git},
    result::{
        BeginTransactionResult, CommitResult, ForgeQLResult, RollbackResult, VerifyBuildResult,
    },
    session::Checkpoint,
    verify,
};

use super::ForgeQLEngine;
use super::require_session_id;

impl ForgeQLEngine {
    pub(super) fn exec_begin_transaction(
        &mut self,
        session_id: Option<&str>,
        name: &str,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let worktree_path = self.require_session(sid)?.worktree_path.clone();

        // CRITICAL: flush the in-memory index to `.forgeql-index` BEFORE
        // staging.  The checkpoint commit deliberately includes the cache
        // file (see `git::CHECKPOINT_EXCLUDED`) so that ROLLBACK can
        // restore a guaranteed-correct cache via `git reset --hard`.  If
        // we don't flush first, the checkpoint captures a stale cache and
        // ROLLBACK would `resume_index` into a stale view.
        if let Some(session) = self.sessions.get_mut(sid)
            && let Err(err) = session.flush_if_dirty()
        {
            warn!(error = %err, "BEGIN: flush_if_dirty failed; checkpoint cache may be stale");
        }

        let repo = git::open(&worktree_path)?;

        // Record the HEAD *before* the checkpoint commit — this is the
        // "clean" point that COMMIT will squash back to.
        let pre_txn_oid = git::head_oid(&repo)?;

        // Auto-commit dirty state (worktree files + freshly-saved cache)
        // so the checkpoint OID is a complete snapshot.
        // Ignore errors from stage_and_commit (e.g. nothing to commit).
        let checkpoint_msg = format!("forgeql: checkpoint '{name}'");
        let _ = git::stage_and_commit(&repo, &checkpoint_msg);
        let oid = git::head_oid(&repo)?;

        if let Some(session) = self.sessions.get_mut(sid) {
            // Set last_clean_oid once per commit cycle — this is the base
            // for the next COMMIT squash.
            if session.last_clean_oid.is_none() {
                session.last_clean_oid = Some(pre_txn_oid.clone());
            }
            session.checkpoints.push(Checkpoint {
                name: name.to_string(),
                oid: oid.clone(),
                pre_txn_oid,
            });
        }

        Ok(ForgeQLResult::BeginTransaction(BeginTransactionResult {
            name: name.to_string(),
            checkpoint_oid: oid,
        }))
    }

    /// `COMMIT MESSAGE 'msg'` — squash checkpoint commits and create a clean
    /// user-facing git commit.
    ///
    /// If `BEGIN TRANSACTION` was called since the last `COMMIT`, the
    /// checkpoint commits are squashed: a new commit is created with
    /// parent = `last_clean_oid` and the current working-tree state,
    /// updating the session branch ref directly by name.  This avoids
    /// `git reset --soft` which can detach HEAD in linked worktrees.
    ///
    /// # Errors
    /// Returns `Err` if the session is missing, git open or commit fails.
    pub(super) fn exec_commit(
        &mut self,
        session_id: Option<&str>,
        message: &str,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let worktree_path = self.require_session(sid)?.worktree_path.clone();
        let last_clean = self
            .sessions
            .get(sid)
            .and_then(|s| s.last_clean_oid.clone());

        let repo = git::open(&worktree_path)?;

        let commit_hash = if let Some(ref clean_oid) = last_clean {
            // Squash checkpoint commits: create a single commit whose parent
            // is the pre-transaction base, updating the branch ref by name.
            git::squash_commit_on_branch(&repo, clean_oid, message)?
        } else {
            // No transaction context — simple commit on HEAD.
            git::stage_and_commit_clean(&repo, message)?;
            git::head_oid(&repo)?
        };

        // Update the clean base for the next commit cycle.
        if let Some(session) = self.sessions.get_mut(sid) {
            session.last_clean_oid = Some(commit_hash.clone());
            // Always save after COMMIT: HEAD just moved so the cache's
            // `commit_hash` field is now stale even when no reindex
            // happened.  Mark dirty first to force the flush.
            // `stage_and_commit_clean` strips `.forgeql-index` from the
            // user-facing commit, so the on-disk file's content is purely
            // a runtime cache untracked by published history.
            session.mark_index_dirty();
            if let Err(err) = session.flush_if_dirty() {
                warn!(error = %err, "COMMIT: post-commit flush failed; cache will rebuild on next USE");
            }
        }

        Ok(ForgeQLResult::Commit(CommitResult {
            message: message.to_string(),
            commit_hash,
        }))
    }

    // ===================================================================
    // Session lifecycle helpers
    // ===================================================================

    /// Revert to a named checkpoint via `git reset --hard`.
    ///
    /// If `name` is given, reverts to that specific checkpoint (and pops all
    /// checkpoints created after it).  If `name` is `None`, reverts to the
    /// most recent checkpoint on the stack.
    ///
    /// After reset, `last_clean_oid` is updated to the checkpoint's
    /// `pre_txn_oid` so the next `COMMIT` squashes from the correct base.
    ///
    /// # Errors
    /// Returns `Err` if no matching checkpoint exists, git open fails, or
    /// the reset itself fails.
    pub(super) fn exec_rollback(
        &mut self,
        session_id: Option<&str>,
        name: Option<&str>,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;

        // Pop the checkpoint (releases mutable borrow before reindex).
        // Pop the checkpoint (releases mutable borrow before reindex).
        let (label, oid, pre_txn_oid, worktree_path) = {
            let session = self
                .sessions
                .get_mut(sid)
                .ok_or_else(|| anyhow::anyhow!("session '{sid}' not found"))?;

            let checkpoint = if let Some(target) = name {
                // Find the named checkpoint and pop everything from that point onward.
                let pos = session
                    .checkpoints
                    .iter()
                    .rposition(|cp| cp.name == target)
                    .ok_or_else(|| {
                        anyhow::anyhow!("no checkpoint named '{target}' in this session")
                    })?;
                let cp = session.checkpoints.remove(pos);
                session.checkpoints.truncate(pos);
                cp
            } else {
                // Pop the most recent checkpoint.
                session.checkpoints.pop().ok_or_else(|| {
                    anyhow::anyhow!("no checkpoints available — run BEGIN TRANSACTION first")
                })?
            };

            // When the last checkpoint is popped, reset last_clean_oid so the
            // next BEGIN TRANSACTION captures a fresh pre-transaction base.
            // Without this, a sequence like BEGIN → ROLLBACK → BEGIN → ROLLBACK
            // → BEGIN → COMMIT would squash to a stale checkpoint OID (which
            // contains .forgeql-index) instead of the true clean base.
            if session.checkpoints.is_empty() {
                session.last_clean_oid = None;
            } else {
                session.last_clean_oid = Some(checkpoint.pre_txn_oid.clone());
            }

            (
                checkpoint.name,
                checkpoint.oid,
                checkpoint.pre_txn_oid,
                session.worktree_path.clone(),
            )
        };

        // Git-as-source-of-truth ROLLBACK.
        //
        // The checkpoint commit captured by `BEGIN` deliberately includes
        // `.forgeql-index` (see `git::CHECKPOINT_EXCLUDED`).  After
        // `git reset --hard <checkpoint_oid>` the worktree contains both
        // the file state AND the matching cache file from that point in
        // time, so `resume_index` is guaranteed to cache-hit and restore
        // a provably-correct index in O(deserialize) instead of O(rebuild).
        //
        // This is intentionally simpler (and more trustworthy) than
        // computing a diff and incrementally reindexing — git is the
        // authoritative source of all worktree state, including the
        // index cache.
        let repo = git::open(&worktree_path)?;
        git::reset_hard(&repo, &oid)?;

        if let Some(session) = self.sessions.get_mut(sid) {
            // Drop the in-memory index so resume_index reads the freshly
            // restored cache from disk rather than keeping a stale view.
            session.drop_index();
            if let Err(err) = session.resume_index() {
                warn!(error = %err, "rollback: resume_index failed; falling back to build_index");
                if let Err(err) = session.build_index() {
                    warn!(error = %err, "rollback: index rebuild failed");
                }
            }
        }

        // Pop the checkpoint commit off the branch tip.
        //
        // `BEGIN TRANSACTION` creates a "forgeql: checkpoint '...'" commit on
        // top of the user's clean work so it can include `.forgeql-index` in
        // the snapshot.  After `reset_hard` restores the worktree to that
        // checkpoint, HEAD still points to the checkpoint commit — which then
        // shows up in `git log` as a spurious entry.
        //
        // `soft_reset` to `pre_txn_oid` moves the branch ref back to the
        // commit that existed before BEGIN was called, without touching the
        // worktree.  `.forgeql-index` therefore remains on disk for the
        // already-completed `resume_index` call above.
        //
        // Edge case: if `stage_and_commit` inside BEGIN had nothing to commit
        // (worktree was already clean), `oid == pre_txn_oid` and this is a
        // no-op.
        if oid != pre_txn_oid
            && let Err(err) = git::soft_reset(&repo, &pre_txn_oid)
        {
            warn!(error = %err, "rollback: soft_reset to pre_txn_oid failed; checkpoint commit remains in history");
        }

        Ok(ForgeQLResult::Rollback(RollbackResult {
            name: label,
            reset_to_oid: oid,
        }))
    }

    /// Run a named verify step from `.forgeql.yaml` as a standalone command.
    ///
    /// # Errors
    /// Returns `Err` if the step name is not found in `.forgeql.yaml`.
    pub(super) fn exec_verify_build(
        &self,
        session_id: Option<&str>,
        step_name: &str,
    ) -> Result<ForgeQLResult> {
        let session = self.require_session(require_session_id(session_id)?)?;
        // Use the verify steps frozen at USE time — prevents config tampering
        // between session start and VERIFY execution.
        let frozen_steps = session.frozen_verify_steps.as_deref().unwrap_or(&[]);
        let workdir = session
            .frozen_workdir
            .clone()
            .unwrap_or_else(|| session.worktree_path.clone());
        let step = frozen_steps
            .iter()
            .find(|s| s.name == step_name)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "VERIFY step '{step_name}' not found in .forgeql.yaml — add it under verify_steps:"
                )
            })?;
        let result = verify::run_standalone(&step, &workdir);
        Ok(ForgeQLResult::VerifyBuild(VerifyBuildResult {
            step: result.step,
            success: result.success,
            output: result.output,
        }))
    }
}
