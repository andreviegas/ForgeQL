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

        // Content-addressed freshness gate: an op that names a node, a file or
        // a symbol resolves the file(s) it is about to read or write first, and
        // any of them whose bytes no longer match the indexed content — HEAD
        // advanced, a file reverted, a formatter or build step rewrote it
        // outside ForgeQL — is re-indexed before dispatch, so no line is served
        // or mutated from a stale span and a rev read before the rewrite is
        // refused. Scoped to the named files: a broad FIND/SHOW scan names
        // none and pays nothing. Cost and coverage: `ensure_files_fresh`.
        let reindexed: Vec<PathBuf> = match sid.map(|mk| self.ensure_files_fresh(mk, op)) {
            None => Vec::new(),
            Some((paths, Ok(()))) => paths,
            // A file that changed outside ForgeQL and could not be brought
            // fresh: refuse rather than answer from the old rows. Whatever the
            // gate re-indexed before it stopped is named in the refusal too —
            // those files are stamped fresh now, so no later command would
            // report them, and the refusal is the only place they can be said.
            Some((paths, Err(e))) => {
                let e = if paths.is_empty() {
                    e
                } else {
                    note_reindexed_on_error(e, &paths)
                };
                return ExecOutcome {
                    result: Err(e),
                    coach: None,
                };
            }
        };

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
                // A refusal issued right after the gate re-indexed a file must
                // say so too: a `rev_mismatch` on a rewritten file IS the
                // rewrite, and the agent can only tell if the error carries it.
                let e = if reindexed.is_empty() {
                    e
                } else {
                    note_reindexed_on_error(e, &reindexed)
                };
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

        // Say so when the gate re-indexed a file: the lines and revs in this
        // answer are current, and the agent's earlier reads of it are not.
        if !reindexed.is_empty() {
            result.note_reindexed_outside_forgeql(&reindexed);
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

    /// Dispatch a parsed operation to its handler, after the two clause checks
    /// that are the same question for every verb: an unusable regex pattern,
    /// and a value outside a set the engine itself owns. Both read the op's
    /// clauses through `ir::clauses_of`, so a verb added to that function is
    /// covered without being named here, and both storage backends are covered
    /// because neither has run yet. Everything else is routing — the
    /// surrounding session/worktree guards, path relativization, and budget
    /// accounting live in `execute`, and the per-verb field checks live where
    /// each verb executes.
    fn dispatch_op(
        &mut self,
        user_id: &str,
        sid: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        crate::filter::reject_invalid_patterns(op)?;
        crate::filter::reject_unknown_enum_values(op)?;
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

    /// Which files `op` is about to read or write, by workspace-relative path.
    ///
    /// Node handles resolve through `find_node` — the path is reliable even
    /// when a segment's line data is stale — `SHOW outline OF '<node_id>'`,
    /// the subtree form, is one of them; file verbs resolve through the
    /// workspace; symbol verbs run the same lookup, with the same clause
    /// split, on the same backend that the verb itself runs — a read carrying
    /// `USING 'legacy'` is resolved through the legacy engine here too, since
    /// gating the file the default engine names would check a file the command
    /// never reads — and `SHOW body` and `SHOW callees` follow a `body_symbol`
    /// redirect exactly as the verb does, so the file gated is the file read,
    /// and a lookup that changes in one place and not the other fails the
    /// suite that reads through every verb. Freshness itself is then asked of
    /// the default engine — the columnar one whenever the source has an index
    /// — so a `USING 'legacy'` read of an indexed file is checked, hinted and
    /// refused exactly as a default-served one is, and the re-index refreshes
    /// both backends; only a path the default engine holds no content id for
    /// answers `Unknown`, which stamps nothing and refuses nothing. The `FOUND`
    /// verbs name every member's file. Everything else — `FIND symbols`,
    /// `FIND usages`, `FIND files`, the session and source verbs, `SHOW MORE`,
    /// `SHOW DIFF`, the raw-text file verbs — names no indexed span and is
    /// not gated: a `FIND` row can carry a stale line for a file rewritten
    /// outside `ForgeQL` until that file is read or edited, and `UNDO` and
    /// `ROLLBACK` restore whole files from `ForgeQL`'s own snapshots over
    /// whatever such a rewrite left since.
    ///
    /// The reach is what the STALE index can still name, and no more: a
    /// symbol a rewrite introduced or renamed resolves to nothing here, so
    /// the verb answers "no symbol matches" with nothing re-indexed until the
    /// file is read by handle or by path; a directory or glob `SHOW outline`
    /// names the directory, not the files it will list, so those are not
    /// checked; a file created outside `ForgeQL` has no index to be stale
    /// (`path_freshness` answers `Unknown` for a path it holds no content id
    /// for) and stays unindexed until `ForgeQL` itself writes it or the next
    /// attach rebuilds the index — a reconnect re-indexes only the tracked
    /// files `git diff HEAD` lists, never an untracked one; a file deleted or
    /// made unreadable outside `ForgeQL` keeps its rows until `ForgeQL` next
    /// writes or re-indexes it (`path_freshness` answers `Unknown` for a file
    /// it cannot read, and an `Unknown` is never stamped as verified) and a
    /// read of it fails on the missing bytes. Each is stated beside the
    /// promise in the docs.
    #[expect(
        clippy::too_many_lines,
        reason = "One explicit arm per IR variant is the point: a variant added later must fail to compile here until it is classified as gated or not, and splitting the match would hide that"
    )]
    fn files_named_by(&self, session_id: &str, op: &ForgeQLIR) -> Vec<PathBuf> {
        use crate::filter::clauses_for_lookup;
        use crate::ir::Backend;
        use crate::result::{CallGraphEntry, MemberEntry, SourceLine};
        let Ok(session) = self.require_session(session_id) else {
            return Vec::new();
        };
        let root = session.worktree_path.clone();
        let rel = |abs: &std::path::Path| abs.strip_prefix(&root).unwrap_or(abs).to_path_buf();
        // Resolve on the engine the verb will read through, not always the
        // default one: `USING 'legacy'` resolves its symbol in the legacy
        // backend, so gating the file the default engine names would check a
        // file the command never reads. Freshness is then asked of the default
        // engine, which holds the per-file content ids — a legacy-served read
        // of an indexed file is gated exactly like any other, since a re-index
        // refreshes both backends.
        let of_node = |backend: &Backend, id: &str| -> Option<PathBuf> {
            let engine = session.engine_for(backend).ok()?;
            let base = crate::node_id::split_node_offset(id).map_or(id, |(base, _)| base);
            engine
                .find_node(base, &root)
                .ok()
                .flatten()
                .map(|node| rel(&node.path))
        };
        let of_file = |file: &str| -> Option<PathBuf> {
            let workspace = crate::workspace::Workspace::new(&root).ok()?;
            let abs = workspace.safe_path(file).ok()?;
            Some(workspace.relative(&abs))
        };
        let of_location = |found: Result<Option<crate::storage::SymbolLocation>>| {
            found.ok().flatten().map(|loc| rel(&loc.path))
        };
        let one = |path: Option<PathBuf>| path.into_iter().collect::<Vec<_>>();
        match op {
            ForgeQLIR::FindNode { node_id }
            | ForgeQLIR::ChangeNode { node_id, .. }
            | ForgeQLIR::ChangeNodeMatching { node_id, .. }
            | ForgeQLIR::InsertNode { node_id, .. }
            | ForgeQLIR::DeleteNode { node_id, .. }
            | ForgeQLIR::ShowNode { node_id, .. } => one(of_node(&Backend::Default, node_id)),
            ForgeQLIR::MoveNodeTo { src_id, .. } | ForgeQLIR::CopyNodeTo { src_id, .. } => {
                one(of_node(&Backend::Default, src_id))
            }
            ForgeQLIR::MoveNode { src_id, dst_id, .. } => of_node(&Backend::Default, src_id)
                .into_iter()
                .chain(of_node(&Backend::Default, dst_id))
                .collect(),
            ForgeQLIR::ShowLines { file, .. } => one(of_file(file)),
            // `SHOW outline OF '<node_id>'` names a subtree by handle, not by
            // path: resolve the handle first, and a plain path as itself.
            ForgeQLIR::ShowOutline { file, backend, .. } => {
                one(of_node(backend, file).or_else(|| of_file(file)))
            }
            ForgeQLIR::ShowBody {
                symbol,
                clauses,
                backend,
                ..
            } => one(of_location(session.engine_for(backend).and_then(|e| {
                e.resolve_body_symbol(symbol, &clauses_for_lookup::<SourceLine>(clauses), &root)
            }))),
            ForgeQLIR::ShowCallees {
                symbol,
                clauses,
                backend,
                ..
            } => one(of_location(session.engine_for(backend).and_then(|e| {
                e.resolve_body_symbol(
                    symbol,
                    &clauses_for_lookup::<CallGraphEntry>(clauses),
                    &root,
                )
            }))),
            ForgeQLIR::ShowContext {
                symbol,
                clauses,
                backend,
                ..
            } => one(of_location(session.engine_for(backend).and_then(|e| {
                e.resolve_symbol(symbol, &clauses_for_lookup::<SourceLine>(clauses), &root)
            }))),
            ForgeQLIR::ShowSignature {
                symbol,
                clauses,
                backend,
                ..
            } => one(of_location(
                session
                    .engine_for(backend)
                    .and_then(|e| e.resolve_symbol(symbol, clauses, &root)),
            )),
            ForgeQLIR::ShowMembers {
                symbol,
                clauses,
                backend,
                ..
            } => one(of_location(session.engine_for(backend).and_then(|e| {
                e.resolve_type_symbol(symbol, &clauses_for_lookup::<MemberEntry>(clauses), &root)
            }))),
            ForgeQLIR::ChangeNodesFound { .. }
            | ForgeQLIR::DeleteNodesFound { .. }
            | ForgeQLIR::MoveNodesFoundTo { .. }
            | ForgeQLIR::CopyNodesFoundTo { .. } => session
                .found_set
                .as_ref()
                .map(|set| {
                    set.members
                        .iter()
                        .filter(|m| !m.path.ends_with('/') && !root.join(&m.path).is_dir())
                        .map(|m| PathBuf::from(&m.path))
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect()
                })
                .unwrap_or_default(),
            // Named explicitly, not with a wildcard: a variant added later
            // fails to compile here until it is classified as gated or not,
            // instead of slipping through ungated. These name no indexed span
            // — or, for `Undo`/`Rollback`, restore whole files from ForgeQL's
            // own snapshots — and are stated as not checked in the docs.
            ForgeQLIR::CreateSource { .. }
            | ForgeQLIR::RefreshSource { .. }
            | ForgeQLIR::Vacuum { .. }
            | ForgeQLIR::UseSource { .. }
            | ForgeQLIR::ShowSources
            | ForgeQLIR::ShowBranches
            | ForgeQLIR::ShowCommits { .. }
            | ForgeQLIR::ShowVersion
            | ForgeQLIR::ShowStats { .. }
            | ForgeQLIR::FindSymbols { .. }
            | ForgeQLIR::FindUsages { .. }
            | ForgeQLIR::FindFiles { .. }
            | ForgeQLIR::ShowMore { .. }
            | ForgeQLIR::ShowDiff { .. }
            | ForgeQLIR::ChangeContent { .. }
            | ForgeQLIR::CopyLines { .. }
            | ForgeQLIR::MoveLines { .. }
            | ForgeQLIR::InsertNodeFor { .. }
            | ForgeQLIR::BeginTransaction { .. }
            | ForgeQLIR::Commit { .. }
            | ForgeQLIR::Rollback { .. }
            | ForgeQLIR::VerifyBuild { .. }
            | ForgeQLIR::Run { .. }
            | ForgeQLIR::Undo { .. }
            | ForgeQLIR::JobStart { .. }
            | ForgeQLIR::JobStatus { .. }
            | ForgeQLIR::JobList
            | ForgeQLIR::ExportPatch { .. } => Vec::new(),
        }
    }

    /// The content-addressed freshness gate, run once per command before
    /// dispatch. Returns the workspace-relative paths it re-indexed together
    /// with its outcome: on a refusal the paths re-indexed before it stopped
    /// travel back with the error, because they are stamped fresh by then and
    /// the refusal is the only answer that can still name them.
    ///
    /// Cost per command: resolving what the command names, which is the same
    /// lookup the verb itself then runs (`resolve_symbol`,
    /// `resolve_body_symbol`, `resolve_type_symbol`, `find_node`) — a gated
    /// command performs that one lookup twice, on the same index tier, with
    /// nothing newly scanned; one `stat` per file so named (see
    /// `files_named_by`); a read-and-hash of that file the first time this
    /// session names it and whenever its size or mtime moved since the last
    /// check; and a single-file re-index when the hash differs from the
    /// indexed content — after which the naming is repeated, since a re-index
    /// can change which file a symbol resolves to, until a pass re-indexes
    /// nothing. Mapping an answer's lines to handles
    /// (`innermost_nodes_for_lines`) hashes the file once more, uncached, so a
    /// read that renders handles and a mutation's boundary diff pay a second
    /// hash of that file. A file whose bytes changed while both its size and
    /// its mtime stayed put is not seen until either moves — the precheck
    /// trusts the stamp, and that is the one thing it cannot tell. Best-effort
    /// throughout: a lookup that fails here fails again in dispatch, which is
    /// where it is reported.
    fn ensure_files_fresh(
        &mut self,
        session_id: &str,
        op: &ForgeQLIR,
    ) -> (Vec<PathBuf>, Result<()>) {
        let mut reindexed: Vec<PathBuf> = Vec::new();
        // A re-index can move what the op names: a symbol verb is gated on
        // the file the STALE index resolved the symbol to, and the fresh index
        // may resolve it elsewhere — a duplicated name whose first holder the
        // rewrite removed. So the pass repeats until it names nothing new to
        // re-index. It ends: a pass continues only when it re-indexed a file
        // not seen before, and the files are finite.
        loop {
            let mut moved = false;
            for rel in self.files_named_by(session_id, op) {
                if reindexed.contains(&rel) {
                    continue;
                }
                // A file that could not be brought fresh stops the gate — but
                // the files already re-indexed go back with the error, not
                // lost with it: they are stamped fresh now, so the refusal is
                // the last chance to name them.
                match self.ensure_file_fresh(session_id, &rel) {
                    Ok(true) => {
                        reindexed.push(rel);
                        moved = true;
                    }
                    Ok(false) => {}
                    Err(e) => return (reindexed, Err(e)),
                }
            }
            if !moved {
                break;
            }
        }
        (reindexed, Ok(()))
    }

    /// Bring one file's index up to date with its bytes on disk. `Ok(false)`
    /// when nothing had to be done, `Ok(true)` when the file had changed
    /// outside `ForgeQL` and was re-indexed — and verified fresh afterwards —
    /// and `Err` when it had changed and the re-index did not leave it fresh:
    /// the caller refuses the command with that error.
    fn ensure_file_fresh(&mut self, session_id: &str, rel: &std::path::Path) -> Result<bool> {
        use crate::storage::PathFreshness;
        let Some(session) = self.sessions.get_mut(session_id) else {
            return Ok(false);
        };
        let root = session.worktree_path.clone();
        let abs = root.join(rel);
        let stamp = crate::session::FileStamp::of(&abs);
        if stamp.is_some() && session.fresh_stamps.get(rel) == stamp.as_ref() {
            return Ok(false);
        }
        let verdict = session
            .engine_for(&crate::ir::Backend::Default)
            .map_or(PathFreshness::Unknown, |engine| {
                engine.path_freshness(rel, &root)
            });
        match verdict {
            PathFreshness::Verified => {
                if let Some(stamp) = stamp {
                    let _ = session.fresh_stamps.insert(rel.to_path_buf(), stamp);
                }
                return Ok(false);
            }
            // Nothing to compare — no content id, or the file cannot be read:
            // nothing stale can be served, and nothing verified can be stamped.
            // A stamp here would vouch, once the file is readable again with
            // the same size and mtime, for bytes no hash ever saw.
            PathFreshness::Unknown => return Ok(false),
            PathFreshness::Stale => {}
        }
        // Stale: re-index this one file through the route every mutation
        // uses, so handles survive (the ordinal remapper matches the new parse
        // against the old rows) and the rev handed back afterwards is current.
        let outcome = self.try_reindex_session(session_id, std::slice::from_ref(&abs));
        // The stamp is taken BEFORE the verifying hash, as on the fresh path
        // above: a stamp records the bytes the hash is about to read, so a
        // rewrite landing between the two moves the stat and the next command
        // hashes again. Taken after the hash, it would vouch for bytes the
        // hash never saw.
        let verified = crate::session::FileStamp::of(&abs);
        // The notice that follows — "the lines and revs here are current" — is
        // checked, not assumed: the file is hashed against the index once more,
        // and a re-index that did not leave it verified fresh refuses the
        // command with its reason rather than answering from the old rows
        // under a notice that says the opposite.
        let Some(session) = self.sessions.get_mut(session_id) else {
            return Ok(false);
        };
        let now = session
            .engine_for(&crate::ir::Backend::Default)
            .map_or(PathFreshness::Unknown, |engine| {
                engine.path_freshness(rel, &root)
            });
        if now != PathFreshness::Verified {
            // Either the re-index failed and named its own cause, or it
            // reported success and left the file still not fresh. Those are
            // different failures and the refusal says which one happened.
            let reason = outcome.err().map_or_else(
                || {
                    "the re-index reported success but the file is still not fresh: \
                     its re-indexed content id does not match the bytes on disk, \
                     or the file cannot be read"
                        .to_string()
                },
                |e| format!("{e:#}"),
            );
            anyhow::bail!(
                "{} changed on disk outside ForgeQL and could not be re-indexed ({reason}); \
                 the index still describes the old bytes, so this command is refused rather \
                 than answered from them",
                rel.display()
            );
        }
        if let Some(stamp) = verified {
            let _ = session.fresh_stamps.insert(rel.to_path_buf(), stamp);
        }
        Ok(true)
    }
}

/// The re-index notice carried on an error the way an answer carries it. A
/// structured rejection keeps its JSON payload — that payload is what a
/// self-healing caller parses — and gains a `reindexed` field; any other
/// error gains the notice as a trailing sentence. The error's type is kept
/// for a rejection, so the transports still recognise it as self-healing.
fn note_reindexed_on_error(err: anyhow::Error, paths: &[PathBuf]) -> anyhow::Error {
    let notice = ForgeQLResult::reindexed_before_refusal_notice(paths);
    match err.downcast::<crate::error::ForgeError>() {
        Ok(crate::error::ForgeError::Rejection { kind, payload }) => {
            let payload = match serde_json::from_str::<serde_json::Value>(&payload) {
                Ok(serde_json::Value::Object(mut fields)) => {
                    let _ =
                        fields.insert("reindexed".to_owned(), serde_json::Value::String(notice));
                    serde_json::Value::Object(fields).to_string()
                }
                _ => format!("{payload} {notice}"),
            };
            crate::error::ForgeError::Rejection { kind, payload }.into()
        }
        Ok(other) => anyhow::anyhow!("{other:#} {notice}"),
        Err(err) => anyhow::anyhow!("{err:#} {notice}"),
    }
}
