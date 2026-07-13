/// `ForgeQLEngine` — the single dispatcher and state owner for all `ForgeQL` operations.
///
/// This is the core entry point for the entire `ForgeQL` system.  Every operation
/// — queries, mutations, source management, transactions — goes through
/// `engine.execute()`.  Transport layers (MCP, REPL, pipe) are thin wrappers
/// that parse input, call `execute()`, and format the `ForgeQLResult`.
///
/// # Architecture
///
/// ```text
///                 ┌────────────┐
///                 │  Transport  │   MCP stdio / REPL / pipe / one-shot
///                 └─────┬──────┘
///                       │ ForgeQLIR
///                       ▼
///              ┌────────────────┐
///              │ ForgeQLEngine  │   Owns state: registry, sessions, data_dir
///              │   execute()    │   Single match on ForgeQLIR
///              └────────────────┘
///                       │
///          ┌────────────┼────────────┐
///          ▼            ▼            ▼
///     ast/query     transforms     git/
///     ast/show      workspace    worktree
/// ```
///
/// # Thread safety
///
/// `ForgeQLEngine` is **not** `Send` or `Sync`.  The async transport layer
/// wraps it in `Arc<Mutex<ForgeQLEngine>>` and calls `execute()` under the
/// lock.  Git and tree-sitter operations are CPU-bound, so holding the lock
/// for the duration of an `execute()` call is correct.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tracing::{info, warn};

use crate::{
    ast::lang::LanguageRegistry,
    git::source::SourceRegistry,
    ir::ForgeQLIR,
    result::ForgeQLResult,
    session::{Session, SessionCoords},
};

// -----------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------

/// How long (in seconds) a session may be idle before `evict_idle_sessions`
/// removes it.
pub const SESSION_TTL_SECS: u64 = 48 * 60 * 60; // 48 hours (generous for dev)

// -----------------------------------------------------------------------
// ForgeQLEngine
// -----------------------------------------------------------------------

/// Implicit row cap for `FIND` queries that specify no `LIMIT` clause.
///
/// Prevents runaway token consumption when the agent issues a broad query
/// such as `FIND symbols` on a large codebase.  The agent can always
/// override with an explicit `LIMIT N` clause.  When the cap fires,
/// `total > results.len()` signals that more rows are available.
pub const DEFAULT_QUERY_LIMIT: usize = 20;

/// Default collapse depth for `SHOW body OF`.
///
/// `0` = signature only (return type, name, parameters); the body is
/// replaced with `{ ... }`.  Higher values reveal nested structure
/// progressively.
pub const DEFAULT_BODY_DEPTH: usize = 0;

/// Default number of context lines shown by `SHOW context OF`.
pub const DEFAULT_CONTEXT_LINES: usize = 5;

/// Implicit line cap for `SHOW` commands that return source lines
/// (`show_body`, `show_lines`, `show_context`) when no `LIMIT` clause
/// is specified.
///
/// Prevents large functions or line ranges from flooding the agent's
/// context window.  The agent can override with an explicit `LIMIT N`.
/// When the cap fires, a `hint` field explains how to paginate.
pub const DEFAULT_SHOW_LINE_LIMIT: usize = 40;

/// Metadata recorded for a session that exists on disk but has not yet been
/// promoted to a full in-memory session.
///
/// Populated by [`ForgeQLEngine::restore_sessions_from_disk`] at MCP startup
/// and consumed (removed) the first time the agent issues a `USE` command
/// that matches this entry.  Holding only metadata avoids loading the full
/// columnar index at startup.
pub struct PendingSession {
    /// Original user identity from the `.forgeql-session` sentinel file.
    pub user: String,
    /// Source name (e.g. `"zephyr-andre"`).
    pub source: String,
    /// Source branch (e.g. `"zephyr-main"`).
    pub branch: String,
    /// Session alias (e.g. `"tests"`).
    pub alias: String,
    /// Worktree directory name — used by the startup sweep to protect live
    /// worktrees from accidental pruning before they are promoted.
    pub worktree_name: String,
}

/// The central `ForgeQL` dispatcher — owns all state and executes all operations.
///
/// Create one per process.  Transport layers hold a reference (typically
/// `Arc<Mutex<ForgeQLEngine>>`) and call `execute()` for every request.
pub struct ForgeQLEngine {
    /// Global catalogue of bare git repositories.
    registry: SourceRegistry,
    /// Active sessions keyed by session ID.
    sessions: HashMap<String, Session>,
    /// Sessions that exist on disk but have not yet been loaded into memory.
    /// Populated at startup by `restore_sessions_from_disk`; cleared on first USE.
    pending_sessions: HashMap<String, PendingSession>,
    /// Root directory for bare repos and worktrees on disk.
    data_dir: PathBuf,
    /// Lifetime command counter (informational, for `/health` equivalents).
    commands_served: u64,
    /// Language support registry for tree-sitter parsing and enrichment.
    lang_registry: Arc<LanguageRegistry>,
    /// Background build-job registry (`JOB START / STATUS / LIST`), shared with
    /// worker threads via `Arc`.
    jobs: Arc<crate::jobs::JobRegistry>,
    /// Gated verify jobs whose completion has not yet been folded into their
    /// session's `satisfied_gates` — see `reconcile_gate_jobs`.
    pending_gate_jobs: Vec<PendingGateJob>,
}

/// A gated verify step running as a background job, awaiting reconciliation
/// into its session's `satisfied_gates` once it completes.
pub(crate) struct PendingGateJob {
    /// Job id in the background registry.
    pub(crate) job_id: String,
    /// Internal session map key the gate belongs to.
    pub(crate) sid: String,
    /// The `commit_gate` verify-step name.
    pub(crate) step: String,
    /// The session's `mutation_seq` when the job was submitted. The gate is
    /// only satisfied when the counter is unchanged at completion — an edit
    /// made while the job ran means it tested stale sources.
    pub(crate) mutation_seq_at_start: u64,
}

// -----------------------------------------------------------------------
// Sub-modules — each owns a slice of the `impl ForgeQLEngine` methods.
// -----------------------------------------------------------------------

mod exec_change;
mod exec_find;
mod exec_session;
mod exec_show;
mod exec_source;
mod exec_transaction;
pub mod warm;

pub mod convert;
pub mod helpers;
#[cfg(test)]
mod tests;

// Re-export helpers so sub-modules can continue using `use super::func`.
#[cfg(feature = "test-helpers")]
pub(crate) use helpers::generate_session_id;
pub(crate) use helpers::{
    detect_metric_hint, load_verify_config, mutation_op_name, reject_text_filter,
    require_session_id,
};

// Re-export converters for sub-modules.
pub(crate) use convert::{convert_show_json, convert_suggestions};

impl ForgeQLEngine {
    /// Create a new engine rooted at `data_dir`.
    ///
    /// Creates the `<data_dir>/worktrees/` directory if it does not exist.
    /// Call [`restore_sessions_from_disk()`](Self::restore_sessions_from_disk)
    /// once at MCP server startup to prune expired worktrees and restore live
    /// sessions into memory.  In CLI modes (REPL, pipe, one-shot) do not call
    /// it — worktrees persist across invocations and sessions should not be
    /// re-indexed on every invocation.
    ///
    /// # Errors
    /// Returns `Err` if the worktree directory cannot be created.
    pub fn new(data_dir: PathBuf, lang_registry: Arc<LanguageRegistry>) -> Result<Self> {
        std::fs::create_dir_all(SessionCoords::worktrees_root(&data_dir))?;
        info!(dir = %data_dir.display(), "engine: data directory ready");

        let mut registry = SourceRegistry::new(data_dir.clone());
        Self::discover_existing_sources(&data_dir, &mut registry);

        let engine = Self {
            registry,
            sessions: HashMap::new(),
            pending_sessions: HashMap::new(),
            data_dir,
            commands_served: 0,
            lang_registry,
            jobs: Arc::new(crate::jobs::JobRegistry::from_env()),
            pending_gate_jobs: Vec::new(),
        };
        Ok(engine)
    }

    /// Scan `data_dir` for existing `*.git` bare repositories and register them.
    ///
    /// This makes sources survive process restarts without requiring
    /// `CREATE SOURCE` again — the bare repo on disk is the source of truth.
    fn discover_existing_sources(data_dir: &Path, registry: &mut SourceRegistry) {
        let entries = match std::fs::read_dir(data_dir) {
            Ok(entries) => entries,
            Err(err) => {
                warn!(%err, "cannot scan data_dir for existing sources");
                return;
            }
        };

        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(source_name) = name.strip_suffix(".git") else {
                continue;
            };
            if source_name.is_empty() {
                continue;
            }

            match registry.register(source_name, path.clone()) {
                Ok(source) => {
                    info!(
                        name = source.name(),
                        path = %source.path().display(),
                        "discovered existing source",
                    );
                }
                Err(err) => {
                    warn!(
                        name = source_name,
                        %err,
                        "failed to register discovered source",
                    );
                }
            }
        }
    }

    // ===================================================================
    // Public API
    // ===================================================================

    /// The single entry point for all `ForgeQL` operations.
    ///
    /// `user_id` is the authenticated user identity for this request.  Obtain
    /// it by calling [`crate::auth::auth`] at the entry point (MCP handler,
    /// CLI runner, session restorer) — never hard-code a literal here.
    ///
    /// `coords` carries the full session identity for session-dependent
    /// operations (FIND, SHOW, mutations, transactions).  Pass `None` for
    /// session-independent operations (`CREATE SOURCE`, `REFRESH SOURCE`,
    /// `SHOW SOURCES`, `USE`).  Build a `SessionCoords` from the opaque
    /// token returned by `USE` via [`SessionCoords::from_session_id`].
    ///
    /// # Errors
    /// Returns `Err` for session-not-found, index-not-ready, git failures,
    /// transform planning errors, and other operational failures.
    #[allow(clippy::too_many_lines)]
    pub fn execute(
        &mut self,
        user_id: &str,
        coords: Option<&SessionCoords>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        self.commands_served += 1;

        // Derive the internal HashMap key directly from the SessionCoords.
        // All key construction is now centralised inside `SessionCoords::map_key`
        // so adding fields to SessionCoords never requires touching this function.
        let map_key: Option<String> = coords.map(SessionCoords::map_key);
        let sid: Option<&str> = map_key.as_deref();

        // Keep session alive on every request.
        if let Some(mk) = sid
            && let Some(session) = self.sessions.get_mut(mk)
        {
            session.touch();
        }

        // Look up worktree root once — used to relativize paths in results.
        let worktree_root = sid
            .and_then(|mk| self.sessions.get(mk))
            .map(|s| s.worktree_path.clone());

        // Guard: for any session-dependent operation, verify the worktree
        // directory still exists on disk.  FIND/SHOW/mutations all need a
        // live worktree; source-management commands (CREATE, USE, DISCONNECT,
        // SHOW SOURCES, SHOW BRANCHES) do not.
        // Guard: session-dependent ops need a live worktree on disk.
        self.check_worktree_alive(sid, op)?;

        // Content-addressed freshness gate (BUG-001/BUG-002): addressable-node
        // operations resolve a node_id to an exact line range. Reindex the
        // single target file first when its committed segment is stale vs disk,
        // so we never serve or mutate a stale line. One file → O(1), so broad
        // FIND/SHOW scans are unaffected.
        if let Some(mk) = sid {
            let target_node: Option<String> = match op {
                ForgeQLIR::FindNode { node_id }
                | ForgeQLIR::ChangeNode { node_id, .. }
                | ForgeQLIR::ChangeNodeMatching { node_id, .. }
                | ForgeQLIR::InsertNode { node_id, .. }
                | ForgeQLIR::DeleteNode { node_id, .. }
                | ForgeQLIR::ShowNode { node_id, .. } => Some(node_id.clone()),
                _ => None,
            };
            if let Some(node_id) = target_node {
                self.ensure_node_file_fresh(mk, &node_id);
            }
        }
        let mut result = self.dispatch_op(user_id, sid, op)?;

        // Strip absolute worktree prefixes so results carry only relative paths.
        // This keeps MCP JSON compact and avoids leaking internal filesystem layout.
        if let Some(ref root) = worktree_root {
            result.relativize_paths(root);
        }

        // Update the session line budget based on the result (see apply_budget).
        self.apply_budget(sid, op, &mut result);

        Ok(result)
    }

    /// Execute an op and synchronously wait out any pending background
    /// execution (`VERIFY build` / `RUN` now run on the job pool).
    ///
    /// Single-tenant callers (CLI, REPL, pipe mode) use this; multi-tenant
    /// transports do the same wait manually so they can release their engine
    /// lock while the job runs.
    ///
    /// # Errors
    /// Same failure modes as [`Self::execute`].
    pub fn execute_blocking(
        &mut self,
        user_id: &str,
        coords: Option<&SessionCoords>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        match self.execute(user_id, coords, op)? {
            ForgeQLResult::PendingExec(pending) => {
                let snapshot = self.jobs.wait(
                    &pending.job_id,
                    std::time::Duration::from_secs(pending.wait_secs),
                );
                Ok(self.finish_pending(&pending, snapshot))
            }
            other => Ok(other),
        }
    }

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
    fn check_worktree_alive(&self, sid: Option<&str>, op: &ForgeQLIR) -> Result<()> {
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
                | ForgeQLIR::ChangeNodesLast { .. }
                | ForgeQLIR::InsertNode { .. }
                | ForgeQLIR::DeleteNode { .. }
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

    /// Dispatch a parsed operation to its handler. Pure routing — the
    /// surrounding session/worktree guards, path relativization, and budget
    /// accounting live in `execute`.
    fn dispatch_op(
        &mut self,
        user_id: &str,
        sid: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        match op {
            // --- Source / session management ---
            ForgeQLIR::CreateSource { name, url } => self.create_source(name, url),
            ForgeQLIR::RefreshSource { name } => self.refresh_source(name),
            ForgeQLIR::Vacuum {
                source,
                keep,
                all,
                apply,
            } => self.vacuum(source.as_deref(), *keep, *all, *apply),
            ForgeQLIR::UseSource {
                source,
                branch,
                as_branch,
            } => self.use_source(user_id, source, branch, as_branch),
            ForgeQLIR::ShowSources => self.show_sources(),
            ForgeQLIR::ShowBranches => self.show_branches(sid),
            ForgeQLIR::ShowStats {
                session_id: for_session,
            } => {
                // SHOW STATS 'token' — the token is the full to_session_id() value
                // which equals map_key(), so it works for the sessions lookup directly.
                self.show_stats(for_session.as_deref())
            }
            // --- Read-only queries ---
            ForgeQLIR::FindNode { node_id } => self.find_node(sid, node_id),
            ForgeQLIR::FindSymbols {
                backend, clauses, ..
            } => self.find_symbols(sid, backend, clauses),
            ForgeQLIR::FindUsages {
                of,
                backend,
                clauses,
                ..
            } => self.find_usages(sid, of, backend, clauses),
            // --- Code exposure (SHOW) ---
            ForgeQLIR::ShowNode { .. } => self.exec_show_node(sid, op),
            ForgeQLIR::ShowMore { .. } => self.exec_show_more(sid, op),
            ForgeQLIR::ShowContext { .. }
            | ForgeQLIR::ShowSignature { .. }
            | ForgeQLIR::ShowOutline { .. }
            | ForgeQLIR::ShowMembers { .. }
            | ForgeQLIR::ShowBody { .. }
            | ForgeQLIR::ShowCallees { .. }
            | ForgeQLIR::ShowLines { .. }
            | ForgeQLIR::FindFiles { .. } => self.exec_show(sid, op),
            // --- Mutations ---
            ForgeQLIR::ChangeContent { .. } => self.exec_mutation(sid, op, true),
            ForgeQLIR::ChangeNode { .. } => self.exec_change_node(sid, op),
            ForgeQLIR::ChangeNodeMatching { .. } => self.exec_change_node_matching(sid, op),
            ForgeQLIR::ChangeNodesLast { .. } => self.exec_change_nodes_last(sid, op),
            ForgeQLIR::InsertNode { .. } => self.exec_insert_node(sid, op),
            ForgeQLIR::DeleteNode { .. } => self.exec_delete_node(sid, op),
            ForgeQLIR::MoveNode { .. } => self.exec_move_node(sid, op),
            ForgeQLIR::CopyLines { .. } => self.exec_copy_lines(sid, op),
            ForgeQLIR::MoveLines { .. } => self.exec_move_lines(sid, op),
            // --- Checkpoint-based transactions ---
            ForgeQLIR::BeginTransaction { name } => self.exec_begin_transaction(sid, name),
            ForgeQLIR::Commit { message } => self.exec_commit(sid, message),
            ForgeQLIR::Rollback { name } => self.exec_rollback(sid, name.as_deref()),
            ForgeQLIR::VerifyBuild { step, args } => self.exec_verify_build(sid, step, args),
            ForgeQLIR::Run { step, args } => self.exec_run(sid, step, args),
            ForgeQLIR::Undo { last } => self.exec_undo(sid, *last),
            ForgeQLIR::JobStart { label, args } => self.exec_job_start(sid, label, args),
            ForgeQLIR::JobStatus { id } => self.exec_job_status(id),
            ForgeQLIR::JobList => self.exec_job_list(),
            ForgeQLIR::ExportPatch { last } => self.exec_export_patch(sid, *last),
            ForgeQLIR::ShowDiff { stat, clauses } => self.exec_show_diff(sid, *stat, clauses),
        }
    }

    /// Apply line-budget accounting for one executed op. Mutations earn back a
    /// line per line written; read ops deduct disclosed source lines (and run
    /// the SHOW LINES anti-pattern tracker). Admin / source-management commands
    /// read no AST data and are exempt from both deduction and recovery.
    fn apply_budget(&mut self, sid: Option<&str>, op: &ForgeQLIR, result: &mut ForgeQLResult) {
        let is_admin_op = matches!(
            op,
            ForgeQLIR::CreateSource { .. }
                | ForgeQLIR::RefreshSource { .. }
                | ForgeQLIR::Vacuum { .. }
                | ForgeQLIR::ShowSources
                | ForgeQLIR::ShowBranches
                | ForgeQLIR::ShowStats { .. }
        );
        if is_admin_op {
            return;
        }
        let Some(mk) = sid else {
            return;
        };
        let Some(session) = self.sessions.get_mut(mk) else {
            return;
        };
        if let ForgeQLResult::Mutation(m) = &*result {
            // Productive work: reward proportional to lines written.
            let _ = session.reward_budget(m.lines_written);
            session.clear_recent_show_lines();
        } else {
            let lines = result.source_lines_count();
            let _ = session.deduct_budget(lines);
            // Track SHOW LINES reads for anti-pattern detection: on 3+ sequential
            // adjacent reads of the same file, inject a tip suggesting SHOW body.
            if let ForgeQLIR::ShowLines {
                file,
                start_line,
                end_line,
                ..
            } = op
            {
                if let Some(tip) = session.record_show_lines(file, *start_line, *end_line) {
                    result.inject_hint(&tip);
                }
            } else {
                session.clear_recent_show_lines();
            }
        }
    }

    /// Content-addressed freshness gate for addressable-node operations.
    ///
    /// Resolves `node_id` to its file — the path is reliable even when the
    /// segment's line data is stale — and, if the committed segment no longer
    /// matches the file on disk, reindexes just that one file so the operation
    /// resolves against fresh line/byte data. Best-effort: any failure falls
    /// through to normal dispatch, which surfaces the proper error.
    ///
    /// Scope is a single file → one content hash, so broad FIND/SHOW scans are
    /// never penalised. This is the structural guarantee that a node op never
    /// serves or mutates a stale line — see BUG-001 (CHANGE NODE corruption)
    /// and BUG-002 (FIND NODE misresolution).
    fn ensure_node_file_fresh(&mut self, session_id: &str, node_id: &str) {
        // Phase 1 (shared borrow): resolve the target file and check freshness.
        let stale_abs_path = {
            let Ok(session) = self.require_session(session_id) else {
                return;
            };
            let root = session.worktree_path.clone();
            let Ok(engine) = session.engine_for(&crate::ir::Backend::Default) else {
                return;
            };
            let Ok(Some(node)) = engine.find_node(node_id, &root) else {
                return;
            };
            let rel = node
                .path
                .strip_prefix(&root)
                .unwrap_or(&node.path)
                .to_path_buf();
            if engine.is_path_fresh(&rel, &root) {
                return;
            }
            root.join(&rel)
        };
        // Phase 2 (mutable borrow): reindex the single stale file so the next
        // find_node resolves against fresh content. Best-effort (logs on error).
        self.reindex_session(session_id, &[stale_abs_path]);
    }

    /// Number of commands served since engine creation.
    #[must_use]
    pub const fn commands_served(&self) -> u64 {
        self.commands_served
    }

    /// Number of active sessions (in-memory) plus pending sessions (on-disk, not yet loaded).
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len() + self.pending_sessions.len()
    }

    /// Return the current budget snapshot for a session.
    /// Returns `None` if no budget is active OR if the last operation was an
    /// admin-exempt command (`CreateSource`, `RefreshSource`, `ShowSources`, `ShowBranches`)
    /// — those commands should not appear in the budget log.
    #[must_use]
    pub fn budget_status(&self, session_id: &str) -> Option<crate::budget::BudgetSnapshot> {
        self.sessions
            .get(session_id)
            .and_then(Session::budget_snapshot)
    }

    /// Worktree root for a loaded session, used by transports to locate the
    /// session's `SHOW MORE` buffer. `None` when the session is not in memory.
    #[must_use]
    pub fn session_worktree(&self, session_id: &str) -> Option<std::path::PathBuf> {
        self.sessions
            .get(session_id)
            .map(|s| s.worktree_path.clone())
    }

    /// Inline output cap (lines) for a loaded session, used by transports to
    /// window over-cap CSV output into the `SHOW MORE` buffer. Falls back to
    /// the configured default when the session is not resident in memory.
    #[must_use]
    pub fn session_inline_cap(&self, session_id: &str) -> usize {
        self.sessions.get(session_id).map_or_else(
            || crate::config::OutputConfig::default().show_lines,
            |s| s.output_config().show_lines,
        )
    }

    /// Return `Some(snapshot)` only for non-admin ops, `None` for admin-exempt commands.
    #[must_use]
    pub fn budget_status_for_op(
        &self,
        session_id: &str,
        op: &ForgeQLIR,
    ) -> Option<crate::budget::BudgetSnapshot> {
        let is_admin = matches!(
            op,
            ForgeQLIR::CreateSource { .. }
                | ForgeQLIR::RefreshSource { .. }
                | ForgeQLIR::ShowSources
                | ForgeQLIR::ShowBranches
        );
        if is_admin {
            None
        } else {
            self.budget_status(session_id)
        }
    }
    /// Number of registered sources.
    #[must_use]
    pub fn source_count(&self) -> usize {
        self.registry.len()
    }

    /// The data directory path.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // PathBuf::as_path is not const
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}
