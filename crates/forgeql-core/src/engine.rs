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
    coach_api::{Clause, Coach, CommandEvent, ErrKind, Outcome, Verb},
    error::{ForgeError, RejectionKind},
    git::source::SourceRegistry,
    ir::{Clauses, ForgeQLIR},
    result::ForgeQLResult,
    session::{Session, SessionCoords},
};

// -----------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------

/// How long (in seconds) a session may be idle before `evict_idle_sessions`
/// removes it.
pub const SESSION_TTL_SECS: u64 = 48 * 60 * 60; // 48 hours (generous for dev)

/// Idle seconds before a work-free session is reclaimed.
///
/// A session with no commits over its base and no uncommitted changes is
/// reclaimed after this instead of [`SESSION_TTL_SECS`], so review and probe
/// worktrees self-clean quickly. Overridable via `FORGEQL_SHORT_SESSION_TTL_SECS`.
pub const SHORT_SESSION_TTL_SECS: u64 = 2 * 60 * 60; // 2 hours

/// The short idle TTL, honoring the `FORGEQL_SHORT_SESSION_TTL_SECS` override.
#[must_use]
pub fn short_session_ttl_secs() -> u64 {
    std::env::var("FORGEQL_SHORT_SESSION_TTL_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(SHORT_SESSION_TTL_SECS)
}

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
    /// Optional onboarding coach — observes every command and may return a
    /// hint. `None` unless a product entry point injects one via `set_coach`;
    /// the engine's own constructor never builds one.
    coach: Option<Box<dyn Coach>>,
}

/// The result of an `execute` call.
///
/// Pairs the command's outcome with any coaching hint produced for it. The
/// pairing is structural: a hint always travels with the command that produced
/// it, so it cannot be delivered late, lost on an early error return, or
/// stapled to another session's command. The hint is a plain `String` at this
/// boundary — front-ends deliver it without needing the coach's vocabulary.
pub struct ExecOutcome {
    /// The command's result, success or error.
    pub result: Result<ForgeQLResult>,
    /// A coaching hint to deliver alongside the response, if any.
    pub coach: Option<String>,
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
            coach: None,
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
    ) -> ExecOutcome {
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

        // Guard: session-dependent ops need a live worktree on disk. This is an
        // infrastructure precondition, not a coachable command outcome, so it
        // returns without observing.
        if let Err(e) = self.check_worktree_alive(sid, op) {
            return ExecOutcome {
                result: Err(e),
                coach: None,
            };
        }

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

        let dispatched = self.dispatch_op(user_id, sid, op);

        // The coach observes both the success and the failure path; the hint it
        // returns travels back paired with the result, so it can ride an error
        // response and can never leak onto the next command.
        let coach = if self.coach.is_some() {
            self.observe_command(coords, op, &dispatched)
        } else {
            None
        };

        let mut result = match dispatched {
            Ok(result) => result,
            Err(e) => {
                return ExecOutcome {
                    result: Err(e),
                    coach,
                };
            }
        };

        // Strip absolute worktree prefixes so results carry only relative paths.
        // This keeps MCP JSON compact and avoids leaking internal filesystem layout.
        if let Some(ref root) = worktree_root {
            result.relativize_paths(root);
        }

        // Update the session line budget based on the result (see apply_budget).
        self.apply_budget(sid, op, &mut result);

        ExecOutcome {
            result: Ok(result),
            coach,
        }
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
    ) -> ExecOutcome {
        let ExecOutcome { result, coach } = self.execute(user_id, coords, op);
        let result = match result {
            Ok(ForgeQLResult::PendingExec(pending)) => {
                let snapshot = self.jobs.wait(
                    &pending.job_id,
                    std::time::Duration::from_secs(pending.wait_secs),
                );
                Ok(self.finish_pending(&pending, snapshot))
            }
            other => other,
        };
        ExecOutcome { result, coach }
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
            ForgeQLIR::ShowCommits { clauses } => self.exec_show_commits(sid, clauses),
            ForgeQLIR::ShowVersion => Ok(Self::show_version()),
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
            | ForgeQLIR::ShowLines { .. } => self.exec_show(sid, op),
            // FIND files is a SHOW op internally, but it is still a FIND: it
            // arms LAST like the other two.
            ForgeQLIR::FindFiles { .. } => self.exec_find_files(sid, op),

            // --- Mutations ---
            ForgeQLIR::ChangeContent { .. } => self.exec_mutation(sid, op, true),
            ForgeQLIR::ChangeNode { .. } => self.exec_change_node(sid, op),
            ForgeQLIR::ChangeNodeMatching { .. } => self.exec_change_node_matching(sid, op),
            ForgeQLIR::ChangeNodesFound { .. } => self.exec_change_nodes_found(sid, op),
            ForgeQLIR::InsertNode { .. } => self.exec_insert_node(sid, op),
            ForgeQLIR::InsertNodeFor { .. } => self.exec_insert_node_for(sid, op),
            ForgeQLIR::DeleteNode { .. } => self.exec_delete_node(sid, op),
            ForgeQLIR::DeleteNodesFound { .. } => self.exec_delete_nodes_found(sid, op),
            ForgeQLIR::MoveNode { .. } => self.exec_move_node(sid, op),
            ForgeQLIR::MoveNodeTo { .. } => self.exec_move_node_to(sid, op, true),
            ForgeQLIR::CopyNodeTo { .. } => self.exec_move_node_to(sid, op, false),
            ForgeQLIR::MoveNodesFoundTo { .. } => self.exec_move_nodes_found_to(sid, op, true),
            ForgeQLIR::CopyNodesFoundTo { .. } => self.exec_move_nodes_found_to(sid, op, false),
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
            ForgeQLIR::ShowDiff { stat, of, clauses } => {
                self.exec_show_diff(sid, *stat, of.as_deref(), clauses)
            }
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
                | ForgeQLIR::ShowVersion
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
    /// admin-exempt command (`CreateSource`, `RefreshSource`, `ShowSources`, `ShowBranches`, `ShowVersion`)
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
                | ForgeQLIR::ShowVersion
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

    /// Inject an onboarding coach. Product entry points call this after
    /// construction; `ForgeQLEngine::new` never builds one, so library
    /// embedders and the test suites stay coach-free and deterministic.
    pub fn set_coach(&mut self, coach: Box<dyn Coach>) {
        self.coach = Some(coach);
    }

    /// Hand the just-executed command to the coach — on both the success and
    /// the failure path — returning any hint it produces so the caller can pair
    /// it with the result.
    fn observe_command(
        &mut self,
        coords: Option<&SessionCoords>,
        op: &ForgeQLIR,
        dispatched: &Result<ForgeQLResult>,
    ) -> Option<String> {
        let coords = coords?;
        let outcome = match dispatched {
            Ok(result) => Outcome::Ok {
                capped: result.output_capped(),
                truncated: result.output_truncated(),
            },
            Err(err) => Outcome::Err(Self::classify_rejection(err)),
        };
        let ev = CommandEvent {
            coords,
            verb: Self::verb_of(op),
            clauses: Self::clauses_of(op),
            outcome,
            cmd_index: self.commands_served,
        };
        self.coach
            .as_mut()
            .and_then(|c| c.observe(&ev))
            .map(|hint| hint.text)
    }

    /// Map an op to its coarse coach verb.
    const fn verb_of(op: &ForgeQLIR) -> Verb {
        match op {
            ForgeQLIR::UseSource { .. } => Verb::Use,
            ForgeQLIR::FindSymbols { .. }
            | ForgeQLIR::FindUsages { .. }
            | ForgeQLIR::FindFiles { .. }
            | ForgeQLIR::FindNode { .. } => Verb::Find,
            ForgeQLIR::ShowSources
            | ForgeQLIR::ShowBranches
            | ForgeQLIR::ShowVersion
            | ForgeQLIR::ShowStats { .. }
            | ForgeQLIR::ShowNode { .. }
            | ForgeQLIR::ShowContext { .. }
            | ForgeQLIR::ShowSignature { .. }
            | ForgeQLIR::ShowOutline { .. }
            | ForgeQLIR::ShowMembers { .. }
            | ForgeQLIR::ShowBody { .. }
            | ForgeQLIR::ShowCallees { .. }
            | ForgeQLIR::ShowLines { .. }
            | ForgeQLIR::ShowMore { .. }
            | ForgeQLIR::ShowDiff { .. } => Verb::Show,
            ForgeQLIR::ChangeNode { .. }
            | ForgeQLIR::ChangeNodeMatching { .. }
            | ForgeQLIR::ChangeNodesFound { .. }
            | ForgeQLIR::ChangeContent { .. } => Verb::Change,
            ForgeQLIR::InsertNode { .. } | ForgeQLIR::InsertNodeFor { .. } => Verb::Insert,
            ForgeQLIR::DeleteNode { .. } | ForgeQLIR::DeleteNodesFound { .. } => Verb::Delete,
            ForgeQLIR::MoveNode { .. }
            | ForgeQLIR::MoveNodeTo { .. }
            | ForgeQLIR::MoveNodesFoundTo { .. }
            | ForgeQLIR::MoveLines { .. } => Verb::Move,
            ForgeQLIR::CopyNodeTo { .. }
            | ForgeQLIR::CopyNodesFoundTo { .. }
            | ForgeQLIR::CopyLines { .. } => Verb::Copy,
            ForgeQLIR::BeginTransaction { .. } => Verb::Begin,
            ForgeQLIR::Commit { .. } => Verb::Commit,
            ForgeQLIR::Rollback { .. } => Verb::Rollback,
            ForgeQLIR::VerifyBuild { .. } => Verb::Verify,
            ForgeQLIR::JobStart { .. } | ForgeQLIR::JobStatus { .. } | ForgeQLIR::JobList => {
                Verb::Job
            }
            ForgeQLIR::Undo { .. } => Verb::Undo,
            _ => Verb::Other,
        }
    }

    /// Presence of read-verb clauses (WHERE, IN, LIMIT, DEPTH, …). Mutation
    /// clauses are added when the curriculum begins consuming them.
    fn clauses_of(op: &ForgeQLIR) -> Vec<Clause> {
        let (ForgeQLIR::FindSymbols { clauses, .. }
        | ForgeQLIR::FindUsages { clauses, .. }
        | ForgeQLIR::FindFiles { clauses, .. }
        | ForgeQLIR::ShowNode { clauses, .. }
        | ForgeQLIR::ShowContext { clauses, .. }
        | ForgeQLIR::ShowSignature { clauses, .. }
        | ForgeQLIR::ShowOutline { clauses, .. }
        | ForgeQLIR::ShowMembers { clauses, .. }
        | ForgeQLIR::ShowBody { clauses, .. }
        | ForgeQLIR::ShowCallees { clauses, .. }
        | ForgeQLIR::ShowLines { clauses, .. }
        | ForgeQLIR::ShowMore { clauses, .. }) = op
        else {
            return Vec::new();
        };
        Self::clauses_present(clauses)
    }

    /// Translate a populated `Clauses` into presence flags for the coach.
    fn clauses_present(c: &Clauses) -> Vec<Clause> {
        let mut v = Vec::new();
        if !c.where_predicates.is_empty() {
            v.push(Clause::Where);
        }
        if !c.having_predicates.is_empty() {
            v.push(Clause::Having);
        }
        if c.in_glob.is_some() {
            v.push(Clause::In);
        }
        if !c.exclude_globs.is_empty() {
            v.push(Clause::Exclude);
        }
        if c.order_by.is_some() {
            v.push(Clause::OrderBy);
        }
        if c.group_by.is_some() {
            v.push(Clause::GroupBy);
        }
        if c.limit.is_some() {
            v.push(Clause::Limit);
        }
        if c.offset.is_some() {
            v.push(Clause::Offset);
        }
        if c.depth.is_some() {
            v.push(Clause::Depth);
        }
        v
    }

    /// Classify a type-erased engine error into the coach taxonomy. Parse
    /// failures never reach here — the transport rejects them before `execute`.
    fn classify_rejection(err: &anyhow::Error) -> ErrKind {
        match err.downcast_ref::<ForgeError>() {
            Some(ForgeError::Rejection { kind, .. }) => match kind {
                RejectionKind::RevMismatch => ErrKind::RevMismatch,
                RejectionKind::NodeNotFound => ErrKind::NodeNotFound,
                RejectionKind::NoFoundSet => ErrKind::NoFoundSet,
                RejectionKind::FoundTruncated => ErrKind::FoundTruncated,
                RejectionKind::FoundRefused => ErrKind::FoundRefused,
                // NoSession is a precondition/handshake denial, out of coach scope.
                RejectionKind::NoSession => ErrKind::Other,
            },
            Some(ForgeError::DslParse(attempted)) => ErrKind::ParseError {
                attempted: attempted.clone(),
            },
            _ => ErrKind::Other,
        }
    }
}
