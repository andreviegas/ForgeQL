//! `ForgeQLEngine` — the single dispatcher and state owner for all `ForgeQL` operations.
//!
//! This is the core entry point for the entire `ForgeQL` system.  Every operation
//! — queries, mutations, source management, transactions — goes through
//! `engine.execute()`.  Transport layers (MCP, REPL, pipe) are thin wrappers
//! that parse input, call `execute()`, and format the `ForgeQLResult`.
//!
//! # Architecture
//!
//! ```text
//!                 ┌────────────┐
//!                 │  Transport  │   MCP stdio / REPL / pipe / one-shot
//!                 └─────┬──────┘
//!                       │ ForgeQLIR
//!                       ▼
//!              ┌────────────────┐
//!              │ ForgeQLEngine  │   Owns state: registry, sessions, data_dir
//!              │   execute()    │   Single match on ForgeQLIR
//!              └────────────────┘
//!                       │
//!          ┌────────────┼────────────┐
//!          ▼            ▼            ▼
//!     ast/query     transforms     git/
//!     ast/show      workspace    worktree
//! ```
//!
//! # Thread safety
//!
//! `ForgeQLEngine` is **not** `Send` or `Sync`.  The async transport layer
//! wraps it in `Arc<Mutex<ForgeQLEngine>>` and calls `execute()` under the
//! lock.  Git and tree-sitter operations are CPU-bound, so holding the lock
//! for the duration of an `execute()` call is correct.
//!
//! # Modules
//!
//! What stays in this file is the state and the single dispatch path; the rest
//! is one module per concern.
//!
//! - `exec_*` — one module per verb family, each owning its own handlers
//! - `coach_report` — what the engine reports to the onboarding coach, which
//!   observes an already-decided outcome and never steers one
//! - `construct` — bringing an engine up and rediscovering existing sources
//! - `jobs` — collecting background work, and the worktree guard around it
//! - `status` — read-only views: counters, budgets, worktree paths
//! - `warm`, `convert`, `helpers` — shared plumbing
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::{
    ast::lang::LanguageRegistry,
    coach_api::Coach,
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

mod coach_report;
mod construct;
mod exec_change;
mod exec_find;
mod exec_session;
mod exec_show;
mod exec_source;
mod exec_transaction;
mod jobs;
mod status;
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
        self.apply_budget(sid, op, &result);

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
            ForgeQLIR::ChangeContent { clauses, .. } => {
                // The parser accepts the universal clause block here, and
                // nothing downstream reads it: the line range a `CHANGE FILE`
                // rewrites lives in its `ChangeTarget`, not in `clauses`. A
                // clause on a mutation is therefore accepted and read by
                // nothing — and it is a mutation, so answering as though a
                // filter had been applied would edit more than was asked.
                crate::filter::reject_clause_block("CHANGE FILE", clauses)?;
                self.exec_mutation(sid, op, true)
            }
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
    /// line per line written; read ops deduct disclosed source lines. Admin /
    /// source-management commands read no AST data and are exempt from both
    /// deduction and recovery.
    fn apply_budget(&mut self, sid: Option<&str>, op: &ForgeQLIR, result: &ForgeQLResult) {
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
        if let ForgeQLResult::Mutation(m) = result {
            // Productive work: reward proportional to lines written.
            let _ = session.reward_budget(m.lines_written);
        } else {
            let lines = result.source_lines_count();
            let _ = session.deduct_budget(lines);
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
}
