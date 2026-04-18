#![allow(unused_imports)]
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use tracing::{debug, info, warn};

use crate::{
    ast::{index::SymbolTable, lang::LanguageRegistry, query, show},
    config::ForgeConfig,
    context::RequestContext,
    git::{
        self as git,
        source::{Source, SourceRegistry},
        worktree,
    },
    ir::{Clauses, ForgeQLIR},
    result::{
        BeginTransactionResult, CallDirection, CallGraphEntry, CommitResult, FileEntry,
        ForgeQLResult, MemberEntry, MutationResult, OutlineEntry, QueryResult, RollbackResult,
        ShowContent, ShowResult, SourceLine, SourceOpResult, SuggestionEntry, SymbolMatch,
        VerifyBuildResult,
    },
    session::{Checkpoint, Session, read_last_active},
    transforms::copy_move::{plan_copy_lines, plan_copy_lines_at, plan_move_lines},
    transforms::diff::{CompactDiffConfig, compact_diff_plan},
    transforms::{TransformPlan, plan_from_ir},
    verify,
    workspace::Workspace,
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

        let repo = git::open(&worktree_path)?;

        // Record the HEAD *before* the checkpoint commit — this is the
        // "clean" point that COMMIT will squash back to.
        let pre_txn_oid = git::head_oid(&repo)?;

        // Auto-commit dirty state so the checkpoint OID is a complete snapshot.
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
        let (label, oid, _pre_txn_oid, worktree_path) = {
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
                    anyhow::anyhow!("no checkpoints available \u{2014} run BEGIN TRANSACTION first")
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

        // Git reset --hard to the checkpoint OID.
        let repo = git::open(&worktree_path)?;
        git::reset_hard(&repo, &oid)?;

        // Try disk-cache first; fall back to full rebuild if stale/missing.
        if let Some(session) = self.sessions.get_mut(sid)
            && session.resume_index().is_err()
            && let Err(err) = session.build_index()
        {
            warn!(error = %err, "rollback: index rebuild failed");
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
