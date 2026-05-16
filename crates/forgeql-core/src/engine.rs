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
    ast::lang::LanguageRegistry, git::source::SourceRegistry, ir::ForgeQLIR, result::ForgeQLResult,
    session::Session,
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

/// The central `ForgeQL` dispatcher — owns all state and executes all operations.
///
/// Create one per process.  Transport layers hold a reference (typically
/// `Arc<Mutex<ForgeQLEngine>>`) and call `execute()` for every request.
pub struct ForgeQLEngine {
    /// Global catalogue of bare git repositories.
    registry: SourceRegistry,
    /// Active sessions keyed by session ID.
    sessions: HashMap<String, Session>,
    /// Root directory for bare repos and worktrees on disk.
    data_dir: PathBuf,
    /// Lifetime command counter (informational, for `/health` equivalents).
    commands_served: u64,
    /// Language support registry for tree-sitter parsing and enrichment.
    lang_registry: Arc<LanguageRegistry>,
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
        std::fs::create_dir_all(data_dir.join("worktrees"))?;
        info!(dir = %data_dir.display(), "engine: data directory ready");

        let mut registry = SourceRegistry::new(data_dir.clone());
        Self::discover_existing_sources(&data_dir, &mut registry);

        let engine = Self {
            registry,
            sessions: HashMap::new(),
            data_dir,
            commands_served: 0,
            lang_registry,
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
    /// `session_id` is `Some(id)` for operations that require an active session
    /// (queries, mutations, transactions).  Source-level operations (`CREATE
    /// SOURCE`, `SHOW SOURCES`) pass `None`.
    ///
    /// # Errors
    /// Returns `Err` for session-not-found, index-not-ready, git failures,
    /// transform planning errors, and other operational failures.
    #[allow(clippy::too_many_lines)]
    pub fn execute(&mut self, session_id: Option<&str>, op: &ForgeQLIR) -> Result<ForgeQLResult> {
        self.commands_served += 1;

        // Keep session alive on every request.
        if let Some(sid) = session_id
            && let Some(session) = self.sessions.get_mut(sid)
        {
            session.touch();
        }

        // Look up worktree root once — used to relativize paths in results.
        let worktree_root = session_id
            .and_then(|sid| self.sessions.get(sid))
            .map(|s| s.worktree_path.clone());

        // Guard: for any session-dependent operation, verify the worktree
        // directory still exists on disk.  FIND/SHOW/mutations all need a
        // live worktree; source-management commands (CREATE, USE, DISCONNECT,
        // SHOW SOURCES, SHOW BRANCHES) do not.
        if let Some(sid) = session_id {
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
                    | ForgeQLIR::BeginTransaction { .. }
                    | ForgeQLIR::Commit { .. }
                    | ForgeQLIR::Rollback { .. }
                    | ForgeQLIR::VerifyBuild { .. }
            );
            if needs_worktree
                && let Some(session) = self.sessions.get(sid)
                && !session.worktree_path.is_dir()
            {
                anyhow::bail!(
                    "session '{sid}' is stale — the worktree directory \
                     '{}' no longer exists on disk.  \
                     Run USE <source>.<branch> to start a new session.",
                    session.worktree_path.display()
                );
            }
        }

        let mut result = match op {
            // --- Source / session management ---
            ForgeQLIR::CreateSource { name, url } => self.create_source(name, url),
            ForgeQLIR::RefreshSource { name } => self.refresh_source(name),
            ForgeQLIR::UseSource {
                source,
                branch,
                as_branch,
            } => self.use_source(source, branch, as_branch),
            ForgeQLIR::ShowSources => self.show_sources(),
            ForgeQLIR::ShowBranches => self.show_branches(session_id),
            ForgeQLIR::ShowStats {
                session_id: for_session,
            } => self.show_stats(for_session.as_deref()),
            // --- Read-only queries ---
            ForgeQLIR::FindSymbols {
                backend, clauses, ..
            } => self.find_symbols(session_id, backend, clauses),
            ForgeQLIR::FindUsages {
                of,
                backend,
                clauses,
                ..
            } => self.find_usages(session_id, of, backend, clauses),

            // --- Code exposure (SHOW) ---
            ForgeQLIR::ShowContext { .. }
            | ForgeQLIR::ShowSignature { .. }
            | ForgeQLIR::ShowOutline { .. }
            | ForgeQLIR::ShowMembers { .. }
            | ForgeQLIR::ShowBody { .. }
            | ForgeQLIR::ShowCallees { .. }
            | ForgeQLIR::ShowLines { .. }
            | ForgeQLIR::FindFiles { .. } => self.exec_show(session_id, op),

            // --- Mutations ---
            ForgeQLIR::ChangeContent { .. } => self.exec_mutation(session_id, op),
            ForgeQLIR::CopyLines { .. } => self.exec_copy_lines(session_id, op),
            ForgeQLIR::MoveLines { .. } => self.exec_move_lines(session_id, op),

            // --- Checkpoint-based transactions ---
            ForgeQLIR::BeginTransaction { name } => self.exec_begin_transaction(session_id, name),
            ForgeQLIR::Commit { message } => self.exec_commit(session_id, message),
            ForgeQLIR::Rollback { name } => self.exec_rollback(session_id, name.as_deref()),
            ForgeQLIR::VerifyBuild { step } => self.exec_verify_build(session_id, step),
        }?;

        // Strip absolute worktree prefixes so results carry only relative paths.
        // This keeps MCP JSON compact and avoids leaking internal filesystem layout.
        if let Some(ref root) = worktree_root {
            result.relativize_paths(root);
        }

        // Deduct disclosed source lines from the session's line budget.
        // Always call deduct_budget even for commands that return 0 source
        // lines (FIND, transactions) so that the recovery windowing logic
        // still runs — non-consuming commands may grant a positive delta if
        // a new recovery window has opened.
        //
        // Mutations (CHANGE, COPY, MOVE) get proportional reward instead:
        // the agent earns back 1 line of budget for every line it writes.
        // This bypasses the rolling-window halving so bulk-edit tasks (e.g.
        // comment translation) remain sustainable.
        //
        // Admin / source-management commands (CreateSource, RefreshSource,
        // ShowSources, ShowBranches) are exempt: they do not read tree-sitter
        // AST data and should not participate in either deduction or recovery.
        // UseSource is already exempt because it executes without a session_id.
        let is_admin_op = matches!(
            op,
            ForgeQLIR::CreateSource { .. }
                | ForgeQLIR::RefreshSource { .. }
                | ForgeQLIR::ShowSources
                | ForgeQLIR::ShowBranches
                | ForgeQLIR::ShowStats { .. }
        );
        if !is_admin_op
            && let Some(sid) = session_id
            && let Some(session) = self.sessions.get_mut(sid)
        {
            if let ForgeQLResult::Mutation(ref m) = result {
                // Productive work: reward proportional to lines written.
                let _ = session.reward_budget(m.lines_written);
                session.clear_recent_show_lines();
            } else {
                let lines = result.source_lines_count();
                let _ = session.deduct_budget(lines);

                // Track SHOW LINES reads for anti-pattern detection.
                // On 3+ sequential adjacent reads on the same file, inject
                // a tip into the result suggesting SHOW body instead.
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

        Ok(result)
    }

    /// Number of commands served since engine creation.
    #[must_use]
    pub const fn commands_served(&self) -> u64 {
        self.commands_served
    }

    /// Number of active sessions.
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len()
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
