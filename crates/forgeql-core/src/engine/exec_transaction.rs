use anyhow::Result;
use tracing::warn;

use crate::{
    git::{self as git},
    result::{
        BeginTransactionResult, CommitResult, ForgeQLResult, PendingExecKind, PendingExecResult,
        RollbackResult,
    },
    session::Checkpoint,
    verify,
};

use super::ForgeQLEngine;
use super::require_session_id;

/// Session context exposed to verify/run subprocesses (and RUN templates).
/// `FORGEQL_BUILD_DIR` is per-worktree so concurrent agents never share build
/// artifacts; consume it as `cargo --target-dir $FORGEQL_BUILD_DIR`.
fn step_env(
    session: &crate::session::Session,
    sid: &str,
    workdir: &std::path::Path,
) -> Vec<(&'static str, String)> {
    vec![
        ("FORGEQL_SESSION_ID", sid.to_string()),
        ("FORGEQL_SOURCE", session.source_name.clone()),
        ("FORGEQL_BRANCH", session.branch.clone()),
        ("FORGEQL_ALIAS", session.id.clone()),
        (
            "FORGEQL_WORKTREE",
            session.worktree_path.display().to_string(),
        ),
        (
            "FORGEQL_BUILD_DIR",
            workdir.join("target").display().to_string(),
        ),
    ]
}

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

        // Flush the columnar dirty overlay to disk so the checkpoint commit
        // captures an up-to-date `.forgeql-columnar-delta` (mirrors what
        // `flush_if_dirty` does for `.forgeql-index`).
        if let Some(session) = self.sessions.get_mut(sid)
            && let Some(columnar) = session.columnar_storage_mut()
            && let Err(err) = columnar.flush_delta()
        {
            warn!(error = %err, "BEGIN: columnar flush_delta failed (non-fatal)");
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
                created: Vec::new(),
            });
            // FT6: save AFTER push so the file reflects the full new stack.
            // The checkpoint commit tree captured the pre-push state (correct
            // for git reset --hard on ROLLBACK); we now update disk to match
            // the live in-memory state.
            if let Err(e) = crate::session::checkpoint_file::save(session, &worktree_path) {
                warn!(error = %e, "BEGIN: checkpoint file save failed (non-fatal)");
            }
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

        // Gated verify steps may have completed as background jobs — fold
        // their results into `satisfied_gates` before checking the gate.
        self.reconcile_gate_jobs();

        // Commit gate: every verify step flagged `commit_gate: true` in
        // `.forgeql.yaml` must have passed since the last mutation. When no
        // step is flagged the gate is inactive (back-compat — COMMIT as before).
        {
            let session = self.require_session(sid)?;
            let stale: Vec<String> = session
                .frozen_verify_steps
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .filter(|s| s.commit_gate && !session.satisfied_gates.contains(&s.name))
                .map(|s| s.name.clone())
                .collect();
            if !stale.is_empty() {
                let edits = session.edits_since_gate;
                anyhow::bail!(
                    "COMMIT blocked: commit-gate step(s) [{}] have not passed since your last \
                     edit ({edits} edit(s) since the last gated run). Re-run `VERIFY build` for \
                     each gated step, then COMMIT.",
                    stale.join(", ")
                );
            }
        }

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

        // Promote columnar staging segments and build the new overlay for the
        // new commit OID.  Non-fatal: on failure the session retains its stale
        // overlay; the next USE will rebuild from legacy (until PhaseFT5).
        if let Some(session) = self.sessions.get_mut(sid)
            && let Some(ctx) = session.columnar_build().cloned()
            && let Some(columnar) = session.columnar_storage_mut()
            && let Err(e) = columnar.commit_dirty(&commit_hash, &ctx)
        {
            warn!(error = %e, "COMMIT: columnar commit_dirty failed (non-fatal); stale overlay retained");
        }

        // Update the clean base for the next commit cycle.
        if let Some(session) = self.sessions.get_mut(sid) {
            session.last_clean_oid = Some(commit_hash.clone());
            // A fresh commit resets the gate's edit counter; `satisfied_gates`
            // stays intact — the committed tree is still the gated tree, so a
            // subsequent no-op COMMIT need not re-run the gate.
            session.edits_since_gate = 0;
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
            // FT6: clear checkpoint stack and remove the file — the clean
            // commit supersedes all checkpoint history.
            session.checkpoints.clear();
            crate::session::checkpoint_file::remove(&worktree_path);
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

        // Pop the checkpoint (releases the mutable borrow before reindex).
        let (label, oid, pre_txn_oid, worktree_path, created) =
            self.pop_rollback_checkpoint(sid, name)?;

        // Git-as-source-of-truth ROLLBACK: `git reset --hard <checkpoint_oid>`
        // restores the worktree (and, for legacy sessions, the matching
        // `.forgeql-index` cache) to the checkpoint state.  Columnar sessions
        // never write `.forgeql-index`, so the in-memory restore below handles
        // them instead (calling `resume_index` here would force a wasted rebuild).
        let repo = git::open(&worktree_path)?;
        git::reset_hard(&repo, &oid)?;

        // `reset --hard` restores tracked paths only. A path created inside the
        // transaction was never staged (staging is deferred to COMMIT), so the
        // reset walks straight past it and it survives — on disk and, after the
        // reindex below, in the index.
        //
        // Remove exactly what the transaction created, and nothing else. Not a
        // `git clean`: that would also destroy the user's unrelated untracked
        // files. Not "any empty parent", either — git does not track empty
        // directories, so an empty directory that was already there is not
        // restored by the reset, and deleting it would be unrecoverable. Only
        // paths in `created` are touched, deepest first so a created directory
        // is empty by the time it is reached.
        // Recorded paths are worktree-relative and reach us from the checkpoint
        // file, which is what survives a server restart — an agent can be gone
        // for hours and still ROLLBACK correctly.
        let mut created = created;
        created.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        for rel in &created {
            let path = worktree_path.join(rel);
            if !path.starts_with(&worktree_path) {
                warn!(path = %path.display(), "rollback: recorded path escapes the worktree; skipped");
                continue;
            }
            let result = if path.is_dir() {
                // `remove_dir` refuses a non-empty directory: if the agent left
                // something else in a directory we created, the directory stays.
                std::fs::remove_dir(&path)
            } else {
                std::fs::remove_file(&path)
            };
            if let Err(err) = result
                && err.kind() != std::io::ErrorKind::NotFound
            {
                warn!(error = %err, path = %path.display(), "rollback: could not remove created path");
            }
        }

        self.restore_session_after_reset(sid, &worktree_path);

        // Pop the checkpoint commit off the branch tip: BEGIN TRANSACTION made a
        // "forgeql: checkpoint '...'" commit on top of the user's clean work, so
        // `soft_reset` to `pre_txn_oid` moves the branch ref back without touching
        // the worktree.  If BEGIN had nothing to commit, oid == pre_txn_oid and
        // this is a no-op.
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

    /// Pop the checkpoint to roll back to (the named one, or the most recent),
    /// update `last_clean_oid`, and return its identity plus the worktree path.
    fn pop_rollback_checkpoint(
        &mut self,
        sid: &str,
        name: Option<&str>,
    ) -> Result<(
        String,
        String,
        String,
        std::path::PathBuf,
        Vec<std::path::PathBuf>,
    )> {
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
                .ok_or_else(|| anyhow::anyhow!("no checkpoint named '{target}' in this session"))?;
            let cp = session.checkpoints.remove(pos);
            session.checkpoints.truncate(pos);
            cp
        } else {
            // Pop the most recent checkpoint.
            session.checkpoints.pop().ok_or_else(|| {
                anyhow::anyhow!("no checkpoints available — run BEGIN TRANSACTION first")
            })?
        };

        // When the last checkpoint is popped, reset last_clean_oid so the next
        // BEGIN TRANSACTION captures a fresh pre-transaction base.  Without this,
        // BEGIN → ROLLBACK → BEGIN → ROLLBACK → BEGIN → COMMIT would squash to a
        // stale checkpoint OID (which contains .forgeql-index) instead of the
        // true clean base.
        if session.checkpoints.is_empty() {
            session.last_clean_oid = None;
        } else {
            session.last_clean_oid = Some(checkpoint.pre_txn_oid.clone());
        }

        Ok((
            checkpoint.name,
            checkpoint.oid,
            checkpoint.pre_txn_oid,
            session.worktree_path.clone(),
            checkpoint.created,
        ))
    }

    /// After `git reset --hard` to the checkpoint, restore the in-memory index,
    /// the columnar dirty overlay, and the on-disk checkpoint file to match.
    fn restore_session_after_reset(&mut self, sid: &str, worktree_path: &std::path::Path) {
        let Some(session) = self.sessions.get_mut(sid) else {
            return;
        };
        // Drop the in-memory index so resume_index reads the freshly restored
        // cache from disk rather than keeping a stale view.
        session.drop_index();
        if !session.has_columnar()
            && let Err(err) = session.resume_index()
        {
            warn!(error = %err, "rollback: resume_index failed; falling back to build_index");
            if let Err(err) = session.build_index() {
                warn!(error = %err, "rollback: index rebuild failed");
            }
        }
        // Restore the columnar dirty overlay from the just-restored delta file.
        // `git reset --hard` already rewrote `.forgeql-columnar-delta` to the
        // checkpoint state; GC orphaned staging dirs then reload into RAM.
        if let Some(columnar) = session.columnar_storage_mut()
            && let Err(e) = columnar.reload_dirty_from_delta()
        {
            warn!(error = %e, "rollback: columnar delta reload failed (non-fatal)");
        }
        // FT6: save the popped in-memory stack to disk, overwriting whatever git
        // reset --hard restored (the pre-push state from the checkpoint commit
        // tree — one entry behind in-memory after the pop), restoring the
        // file == in-memory stack invariant.  Special case: when the last
        // checkpoint was just popped and last_clean_oid is None there is no
        // active transaction state, so remove the file to avoid a spurious
        // HEAD-mismatch warning from try_restore on the next server start.
        if session.checkpoints.is_empty() && session.last_clean_oid.is_none() {
            crate::session::checkpoint_file::remove(worktree_path);
        } else if let Err(e) = crate::session::checkpoint_file::save(session, worktree_path) {
            warn!(error = %e, "rollback: checkpoint file save failed (non-fatal)");
        }
    }

    /// Run a named verify step from `.forgeql.yaml` as a standalone command.
    ///
    /// # Errors
    /// Returns `Err` if the step name is not found in `.forgeql.yaml`.
    pub(super) fn exec_verify_build(
        &mut self,
        session_id: Option<&str>,
        step_name: &str,
        args: &[String],
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let pending = self.submit_verify_job(sid, step_name, args, PendingExecKind::Verify)?;
        Ok(ForgeQLResult::PendingExec(pending))
    }

    /// Resolve a frozen verify step and submit its command to the background
    /// job pool. Shared by `VERIFY build` (whose caller then waits on the job
    /// with the engine lock released) and `JOB START` (which returns the job
    /// id immediately). A `commit_gate` step is tracked with the session's
    /// current `mutation_seq` so its completion can satisfy the commit gate —
    /// see `reconcile_gate_jobs`.
    fn submit_verify_job(
        &mut self,
        sid: &str,
        step_name: &str,
        args: &[String],
        kind: PendingExecKind,
    ) -> Result<PendingExecResult> {
        let session = self.require_session(sid)?;
        // Use the verify steps frozen at USE time — prevents config tampering
        // between session start and execution.
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
                    "verify step '{step_name}' not found in .forgeql.yaml — add it under verify_steps:"
                )
            })?;
        // Validate the supplied args against the step's declared params and
        // substitute them into the command (injection-safe: ident-typed only).
        let command = crate::config::resolve_command(&step, args)
            .map_err(|e| anyhow::anyhow!("VERIFY build '{step_name}': {e}"))?;
        let env = step_env(session, sid, &workdir);
        let mutation_seq = session.mutation_seq;
        let job_name = step.name.clone();
        let registry = std::sync::Arc::clone(&self.jobs);
        let job_id = registry.start(step.name.clone(), step.weight.resolve(), move || {
            let result = verify::run_shell(&job_name, &command, &workdir, &env, None);
            crate::jobs::JobOutcome {
                success: result.success,
                output: result.output,
            }
        });
        if step.commit_gate {
            self.pending_gate_jobs.push(super::PendingGateJob {
                job_id: job_id.clone(),
                sid: sid.to_string(),
                step: step.name.clone(),
                mutation_seq_at_start: mutation_seq,
            });
        }
        Ok(PendingExecResult {
            job_id,
            step: step.name,
            kind,
            wait_secs: step.timeout_secs,
            summary_lines: step.summary.lines,
            summary_direction: step.summary.direction,
        })
    }

    /// `JOB START '<label>' ['<arg>'…]` — run a verify step as a detached
    /// background job.
    ///
    /// Resolves the frozen verify step (same allowlist and typed-param
    /// substitution as `VERIFY build`), then runs its command on a worker
    /// thread and returns the job id immediately — the long build never blocks
    /// this request. A `commit_gate` step satisfies the commit gate when the
    /// job later completes, provided no mutation happened while it ran.
    ///
    /// # Errors
    /// Returns `Err` if the step name is not found or the arguments do not
    /// match the step's declared params.
    pub(super) fn exec_job_start(
        &mut self,
        session_id: Option<&str>,
        label: &str,
        args: &[String],
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let pending = self.submit_verify_job(sid, label, args, PendingExecKind::Verify)?;
        Ok(ForgeQLResult::JobStarted(crate::result::JobStartedResult {
            id: pending.job_id,
            label: pending.step,
        }))
    }

    /// `JOB STATUS '<id>'` — poll one background job (global; no session needed).
    ///
    /// # Errors
    /// Returns `Err` if no job with that id is known.
    pub(super) fn exec_job_status(&mut self, id: &str) -> Result<ForgeQLResult> {
        // Fold finished gated jobs into `satisfied_gates` before reporting, so
        // a poll that sees "succeeded" has also unblocked COMMIT.
        self.reconcile_gate_jobs();
        self.jobs.status(id).map_or_else(
            || Err(anyhow::anyhow!("JOB STATUS: unknown job id '{id}'")),
            |snapshot| Ok(ForgeQLResult::JobStatus(snapshot)),
        )
    }

    /// `JOB LIST` — list all known background jobs (global; no session needed).
    ///
    /// # Errors
    /// Never fails today; returns `Result` for dispatch uniformity.
    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn exec_job_list(&mut self) -> Result<ForgeQLResult> {
        self.reconcile_gate_jobs();
        Ok(ForgeQLResult::JobList(crate::result::JobListResult {
            jobs: self.jobs.list(),
        }))
    }

    /// Run a named `run_steps` template from `.forgeql.yaml` as a standalone
    /// command (outside a transaction). `Ident` args are substituted into the
    /// command; `String` args are bound to the subprocess stdin.
    ///
    /// # Errors
    /// Returns `Err` if the step name is not found, or if the supplied args do
    /// not match the template's declared params.
    pub(super) fn exec_run(
        &self,
        session_id: Option<&str>,
        step_name: &str,
        args: &[String],
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let pending = self.submit_run_job(sid, step_name, args)?;
        Ok(ForgeQLResult::PendingExec(pending))
    }

    /// Resolve a frozen `RUN` template and submit it to the background job
    /// pool; the caller waits on the job with the engine lock released.
    fn submit_run_job(
        &self,
        sid: &str,
        step_name: &str,
        args: &[String],
    ) -> Result<PendingExecResult> {
        let session = self.require_session(sid)?;
        // Use the run steps frozen at USE time — prevents config tampering
        // between session start and RUN execution.
        let frozen_steps = session.frozen_run_steps.as_deref().unwrap_or(&[]);
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
                    "RUN step '{step_name}' not found in .forgeql.yaml — add it under run_steps:"
                )
            })?;
        // Resolve the template: ident args → command tokens (injection-safe);
        // string args → subprocess stdin (never spliced into the shell).
        let (command, stdin) =
            crate::config::resolve_template(&step.name, &step.command, &step.params, args)
                .map_err(|e| anyhow::anyhow!("RUN '{step_name}': {e}"))?;
        let env = step_env(session, sid, &workdir);
        let job_name = step.name.clone();
        let registry = std::sync::Arc::clone(&self.jobs);
        // Run templates declare no `weight` — schedule them at the default cost.
        let cost = crate::config::Weight::default().resolve();
        let job_id = registry.start(step.name.clone(), cost, move || {
            let result = verify::run_shell(&job_name, &command, &workdir, &env, stdin.as_deref());
            crate::jobs::JobOutcome {
                success: result.success,
                output: result.output,
            }
        });
        Ok(PendingExecResult {
            job_id,
            step: step.name,
            kind: PendingExecKind::Run,
            wait_secs: step.timeout_secs,
            summary_lines: step.summary.lines,
            summary_direction: step.summary.direction,
        })
    }

    /// `EXPORT PATCH [LAST n]` — write the session's commits as
    /// `git am`-ready mbox files and return them (paths, sizes, sha256,
    /// inline content).
    ///
    /// The range is engine-computed: `LAST n` exports the last n commits on
    /// the session branch; without it, everything the session added over its
    /// base branch (merge-base..HEAD). `ForgeQL` runtime files are excluded
    /// from every patch (see [`git::export_patches`]), so transaction
    /// checkpoint commits drop out of the series and the export is safe to
    /// run mid-transaction. Uncommitted worktree changes are commit-less and
    /// therefore never exported — surfaced as a hint instead.
    pub(super) fn exec_export_patch(
        &self,
        session_id: Option<&str>,
        last: Option<usize>,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let session = self.require_session(sid)?;
        let worktree = session.worktree_path.clone();
        let base_branch = session.branch.clone();

        let (range_args, range) = match last {
            Some(0) => anyhow::bail!(
                "EXPORT PATCH LAST 0: nothing to export — LAST takes a positive commit count"
            ),
            Some(n) => (
                vec![format!("-{n}"), "HEAD".to_string()],
                format!("last {n} commit(s)"),
            ),
            None => {
                let base = git::merge_base_with(&worktree, &base_branch)?;
                let head = git::head_oid_of(&worktree)?;
                if base == head {
                    anyhow::bail!(
                        "nothing to export: the session branch has no commits over \
                         '{base_branch}' — run COMMIT first, or use EXPORT PATCH LAST n \
                         to export existing branch commits"
                    );
                }
                let short = base.get(..12).unwrap_or(&base);
                (vec![format!("{base}..HEAD")], format!("{short}..HEAD"))
            }
        };

        let files = git::export_patches(&worktree, &range_args)?;
        let mut content = String::new();
        for f in &files {
            content.push_str(&std::fs::read_to_string(&f.path)?);
        }

        let mut hints: Vec<String> = Vec::new();
        if files.is_empty() {
            hints.push(
                "no patches produced: every commit in the range touched only \
                 ForgeQL runtime files (e.g. transaction checkpoints)"
                    .to_string(),
            );
        }
        match git::uncommitted_source_changes(&worktree) {
            Ok(0) => {}
            Ok(n) => hints.push(format!(
                "{n} uncommitted change(s) in the worktree are not part of any \
                 commit and were not exported"
            )),
            Err(e) => warn!("EXPORT PATCH: could not check worktree status: {e}"),
        }

        Ok(ForgeQLResult::ExportPatch(
            crate::result::ExportPatchResult {
                range,
                files: files
                    .into_iter()
                    .map(|f| crate::result::PatchFileEntry {
                        path: f.path,
                        bytes: f.bytes,
                        sha256: f.sha256,
                    })
                    .collect(),
                content,
                hint: (!hints.is_empty()).then(|| hints.join("; ")),
            },
        ))
    }

    /// `SHOW DIFF [STAT]` — the session worktree's **uncommitted** diff.
    ///
    /// Mechanical: the bytes git reports, filtered and windowed. The engine
    /// neither interprets nor repairs them.
    ///
    /// Clause split, mirroring `SHOW body`: a `WHERE text …` predicate filters
    /// the diff's own **lines**; every other clause (`IN`, `EXCLUDE`, `WHERE
    /// path/status/added/removed`, `ORDER BY`, `LIMIT`, …) applies to the
    /// per-file **rows**.
    pub(super) fn exec_show_diff(
        &self,
        session_id: Option<&str>,
        stat: bool,
        of: Option<&str>,
        clauses: &crate::ir::Clauses,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let session = self.require_session(sid)?;
        let worktree = session.worktree_path.clone();

        let diff = match of {
            Some(rev) => git::commit_diff(&worktree, rev)?,
            None => git::worktree_diff(&worktree)?,
        };

        // Split the predicates: `text` targets diff lines, everything else
        // targets the file rows.
        let (text_preds, row_preds): (Vec<_>, Vec<_>) = clauses
            .where_predicates
            .iter()
            .cloned()
            .partition(|p| p.field == "text");

        let mut row_clauses = clauses.clone();
        row_clauses.where_predicates = row_preds;

        let mut rows: Vec<crate::result::DiffFileEntry> = diff
            .iter()
            .map(|f| crate::result::DiffFileEntry {
                path: f.path.clone(),
                status: f.status.to_string(),
                added: f.added,
                removed: f.removed,
            })
            .collect();
        crate::filter::apply_clauses(&mut rows, &row_clauses);

        // Keep only the hunks of files that survived the row filter, in row order.
        let mut content = String::new();
        if !stat {
            for row in &rows {
                let Some(file) = diff.iter().find(|f| f.path == row.path) else {
                    continue;
                };
                if text_preds.is_empty() {
                    content.push_str(&file.patch);
                } else {
                    // Filter the file's diff lines, exactly as SHOW body filters
                    // source lines — before any cap is applied.
                    let mut kept: Vec<crate::result::SourceLine> = file
                        .patch
                        .lines()
                        .enumerate()
                        .map(|(i, l)| crate::result::SourceLine {
                            rev: None,
                            line: i + 1,
                            text: l.to_string(),
                            marker: None,
                            node_id: None,
                            node_offset: None,
                        })
                        .collect();
                    crate::filter::apply_where_predicates(&mut kept, &text_preds);
                    for l in kept {
                        content.push_str(&l.text);
                        content.push('\n');
                    }
                }
            }
        }

        let hint = if diff.is_empty() {
            Some("worktree is clean — no uncommitted changes".to_string())
        } else if rows.is_empty() {
            Some("every changed file was filtered out by the clauses".to_string())
        } else {
            None
        };

        Ok(ForgeQLResult::ShowDiff(crate::result::ShowDiffResult {
            files: rows,
            content,
            hint,
        }))
    }
}
