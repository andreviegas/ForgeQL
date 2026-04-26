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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use tracing::{info, warn};

use crate::{
    ast::{index::SymbolTable, lang::LanguageRegistry},
    config::ForgeConfig,
    git::source::SourceRegistry,
    ir::{Clauses, ForgeQLIR},
    result::{
        CallDirection, CallGraphEntry, FileEntry, ForgeQLResult, MemberEntry, OutlineEntry,
        ShowContent, ShowResult, SourceLine, SuggestionEntry, SymbolMatch,
    },
    session::Session,
    transforms::TransformPlan,
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
// -----------------------------------------------------------------------
// SHOW helpers
// -----------------------------------------------------------------------

impl ForgeQLEngine {
    /// Create a new engine rooted at `data_dir`.
    ///
    /// Creates the `<data_dir>/worktrees/` directory if it does not exist.
    /// Call [`prune_orphaned_worktrees()`](Self::prune_orphaned_worktrees)
    /// explicitly if running in a long-lived service mode (e.g. MCP) where
    /// worktrees without in-memory sessions are truly orphaned.  In CLI
    /// modes (REPL, pipe, one-shot) worktrees persist across invocations,
    /// so pruning must **not** run automatically.
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

        // Auto-reconnect: if the client passes a valid alias that's no longer in
        // memory (e.g. after a server restart), silently restore the session from
        // the matching on-disk worktree before touching or querying it.
        if let Some(sid) = session_id
            && !self.sessions.contains_key(sid)
        {
            self.try_auto_reconnect(sid);
        }

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
            // --- Read-only queries ---
            ForgeQLIR::FindSymbols { clauses } => self.find_symbols(session_id, clauses),
            ForgeQLIR::FindUsages { of, clauses } => self.find_usages(session_id, of, clauses),

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

// -----------------------------------------------------------------------
// Free functions (module-private helpers)
// -----------------------------------------------------------------------

/// Load verify configuration for `source_name`, preferring an external sidecar
/// over the in-repo `.forgeql.yaml`.
///
/// **Sidecar path:** `<repo_dir>/<source_name>.forgeql.yaml` (no commit needed)\
/// **Fallback:** walk up from `worktree_path` looking for `.forgeql.yaml`
///
/// Returns `(workdir, config)` where `workdir` is the directory from which
/// VERIFY commands run — always the worktree root when the sidecar is used.
pub(crate) fn load_verify_config(
    repo_path: &Path,
    source_name: &str,
    worktree_path: &Path,
) -> Option<(PathBuf, ForgeConfig)> {
    let sidecar = repo_path
        .parent()
        .map(|p| p.join(format!("{source_name}.forgeql.yaml")));
    if let Some(sc) = sidecar.as_deref().filter(|p| p.exists()) {
        info!(%source_name, path = %sc.display(), "using sidecar .forgeql.yaml");
        return ForgeConfig::load(sc)
            .ok()
            .map(|c| (worktree_path.to_path_buf(), c));
    }
    ForgeConfig::find(worktree_path).and_then(|p| {
        let workdir = p.parent().map(Path::to_path_buf)?;
        ForgeConfig::load(&p).ok().map(|c| (workdir, c))
    })
}

/// Generate a time-based session ID for test-only local sessions.
///
/// Production sessions use the alias from `USE … AS 'alias'` as their key.
/// This helper is only needed by `register_local_session` (test feature flag).
#[cfg(feature = "test-helpers")]
pub(crate) fn generate_session_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("s{millis}")
}

/// Extract the `session_id` from `Option<&str>`, failing if absent or empty.
#[allow(clippy::missing_const_for_fn)] // bail! prevents const
pub(crate) fn require_session_id(session_id: Option<&str>) -> Result<&str> {
    match session_id {
        Some(sid) if !sid.is_empty() => Ok(sid),
        _ => bail!("session_id required — run USE <source>.<branch> first"),
    }
}

/// Determine the operation name for a mutation `ForgeQLIR` variant.
pub(crate) const fn mutation_op_name(op: &ForgeQLIR) -> &'static str {
    match op {
        ForgeQLIR::ChangeContent { .. } => "change_content",
        _ => "unknown_mutation",
    }
}

/// Detect the first numeric WHERE predicate on a non-core enrichment field.
///
/// Returns the field name (e.g. `"member_count"`, `"param_count"`) so the
/// compact renderer can show that value instead of `usages`.  Falls back
/// to `ORDER BY` field when no numeric WHERE is present.
pub(crate) fn detect_metric_hint(clauses: &Clauses) -> Option<String> {
    use crate::ir::PredicateValue;

    const CORE_FIELDS: &[&str] = &["name", "node_kind", "path", "line", "usages"];

    // Priority 1: numeric WHERE on enrichment field.
    for pred in &clauses.where_predicates {
        if matches!(pred.value, PredicateValue::Number(_))
            && !CORE_FIELDS.contains(&pred.field.as_str())
        {
            return Some(pred.field.clone());
        }
    }

    // Priority 2: ORDER BY an enrichment field.
    if let Some(ref order) = clauses.order_by
        && !CORE_FIELDS.contains(&order.field.as_str())
    {
        return Some(order.field.clone());
    }

    None
}

/// If `pat` is of the form `^literal$` where `literal` contains no regex
/// metacharacters (`.*+?[](){}|\`), return the literal substring.
///
/// This lets `MATCHES '^exact_name$'` be routed to the O(1) `name_index`
/// instead of invoking the regex engine per row.
fn extract_anchored_literal(pat: &str) -> Option<&str> {
    let inner = pat.strip_prefix('^')?.strip_suffix('$')?;
    // Reject anything with regex metacharacters — must be a pure literal.
    if inner.chars().any(|c| ".*+?[](){}|\\".contains(c)) {
        return None;
    }
    // Reject case-insensitive flag or other inline flags.
    if inner.starts_with("(?") {
        return None;
    }
    Some(inner)
}

/// Extract a required literal substring (>= 3 bytes) from a MATCHES regex
/// pattern for use as a trigram pre-filter.
fn regex_trigram_literal(pat: &str) -> Option<String> {
    crate::ast::trigram::extract_regex_literal(pat)
}

/// Extract a required literal substring (>= 3 bytes) from a SQL LIKE pattern
/// for use as a trigram pre-filter.
fn like_trigram_literal(pat: &str) -> Option<String> {
    crate::ast::trigram::extract_like_literal(pat)
}

/// Pre-filter symbol rows using secondary indexes and WHERE predicates
/// before materializing `SymbolMatch`.  Returns `(results, remaining_clauses)`
/// where `remaining_clauses` contains only the parts not yet applied.
#[allow(clippy::too_many_lines)]
pub(crate) fn find_symbols_prefilter(
    index: &SymbolTable,
    clauses: &Clauses,
    root: &std::path::Path,
    lang_configs: &[&crate::ast::lang::LanguageConfig],
) -> (Vec<SymbolMatch>, Clauses) {
    use crate::ast::index::RowRef;
    use crate::filter::{eval_predicate, like_match};
    use crate::ir::{CompareOp, PredicateValue};

    // Extract a `fql_kind = value` predicate for the fql_kind_index shortcut (preferred).
    // Extract a `node_kind = value` predicate for the kind_index shortcut (power-user fallback).
    let fql_kind_exact: Option<&str> = clauses.where_predicates.iter().find_map(|p| {
        if p.field == "fql_kind"
            && p.op == CompareOp::Eq
            && let PredicateValue::String(ref s) = p.value
        {
            Some(s.as_str())
        } else {
            None
        }
    });
    let kind_exact: Option<&str> = clauses.where_predicates.iter().find_map(|p| {
        if p.field == "node_kind"
            && p.op == CompareOp::Eq
            && let PredicateValue::String(ref s) = p.value
        {
            Some(s.as_str())
        } else {
            None
        }
    });

    // Extract a `name LIKE 'pattern'` predicate for name filtering.
    let name_like: Option<&str> = clauses.where_predicates.iter().find_map(|p| {
        if p.field == "name"
            && p.op == CompareOp::Like
            && let PredicateValue::String(ref s) = p.value
        {
            Some(s.as_str())
        } else {
            None
        }
    });

    // Fast path: `name MATCHES '^literal$'` with no regex metacharacters is
    // equivalent to an exact equality lookup in the name_index.  Using the
    // name_index directly skips the per-row regex engine entirely.
    let name_matches_anchored: Option<&str> = clauses.where_predicates.iter().find_map(|p| {
        if p.field == "name"
            && p.op == CompareOp::Matches
            && let PredicateValue::String(ref s) = p.value
        {
            extract_anchored_literal(s.as_str())
        } else {
            None
        }
    });

    // Combined: either source gives an O(1) name_index hit.
    let name_literal: Option<&str> = name_matches_anchored;
    let is_usages_pred = |p: &crate::ir::Predicate| p.field == "usages";

    // Trigram pre-filter: extract a required literal substring from MATCHES
    // or LIKE predicates that are NOT already handled by the exact name_index
    // path.  The trigram index returns a small candidate superset; the full
    // predicate is still evaluated per-candidate in `non_usages_preds`.
    let trigram_literal: Option<String> = if name_literal.is_none() {
        // Prefer the MATCHES pattern literal (usually more selective than LIKE).
        clauses
            .where_predicates
            .iter()
            .find_map(|p| {
                if p.field == "name"
                    && p.op == CompareOp::Matches
                    && let PredicateValue::String(ref s) = p.value
                {
                    return regex_trigram_literal(s.as_str());
                }
                None
            })
            .or_else(|| {
                // Fall back to LIKE literal when no MATCHES pattern exists.
                name_like.and_then(like_trigram_literal)
            })
    } else {
        None
    };

    // When no explicit kind predicate, infer raw kind(s) from enrichment fields.
    // This lets us use the kind_index instead of a full scan.
    let inferred_kinds: Option<Vec<String>> = if fql_kind_exact.is_none() && kind_exact.is_none() {
        infer_kinds_from_fields(&clauses.where_predicates, lang_configs)
    } else {
        None
    };

    // Row source priority:
    //   1. name_index  — exact anchored literal (O(1), 100% correct)
    //   2. trigram     — required substring (O(candidates), superset)
    //   3. fql_kind_index
    //   4. kind_index
    //   5. inferred kinds
    //   6. full scan
    let use_name_index = name_literal.is_some();
    // trigram_literal is only computed when name_literal is None, so
    // use_trigram already implies !use_name_index.
    let use_trigram = trigram_literal.is_some();
    // Strip a predicate only when its corresponding index actually supplied
    // the candidate rows.  Before trigram was introduced, the priority was
    // name_index → fql_kind_index → kind_index, and the strip logic only
    // checked !use_name_index.  Now that trigram sits between name_index and
    // fql_kind_index, the strip logic must also account for whether trigram
    // was used — otherwise `fql_kind` and `node_kind` predicates are silently
    // dropped even though fql_kind_index / kind_index was never consulted.
    let use_fql_kind_index = !use_name_index && !use_trigram && fql_kind_exact.is_some();
    let use_kind_index =
        !use_name_index && !use_trigram && fql_kind_exact.is_none() && kind_exact.is_some();

    let candidates: Box<dyn Iterator<Item = &crate::ast::index::IndexRow>> =
        if let Some(literal) = name_literal {
            Box::new(index.rows_by_name(literal))
        } else if let Some(ref substr) = trigram_literal {
            // trigram_candidates returns None only when substr < 3 bytes,
            // which can't happen here (extract_*_literal guarantees >= 3).
            let rows = index.trigram_candidates(substr).unwrap_or_default();
            Box::new(rows.into_iter())
        } else if let Some(fql_kind) = fql_kind_exact {
            Box::new(index.rows_by_fql_kind(fql_kind))
        } else if let Some(kind) = kind_exact {
            Box::new(index.rows_by_kind(kind))
        } else if let Some(ref kinds) = inferred_kinds {
            Box::new(
                kinds
                    .iter()
                    .flat_map(|k| index.rows_by_kind(k))
                    .collect::<Vec<_>>()
                    .into_iter(),
            )
        } else {
            Box::new(index.rows.iter())
        };

    // Collect non-usages predicates not already handled by index lookups.
    // A predicate is stripped only when its index WAS the actual candidate
    // source — stripping it otherwise would silently skip correct filtering.
    let non_usages_preds: Vec<_> = clauses
        .where_predicates
        .iter()
        .filter(|p| !is_usages_pred(p))
        // Strip fql_kind = X only when fql_kind_index supplied the candidates.
        .filter(|p| !(use_fql_kind_index && p.field == "fql_kind" && p.op == CompareOp::Eq))
        // Strip node_kind = X only when kind_index supplied the candidates.
        .filter(|p| !(use_kind_index && p.field == "node_kind" && p.op == CompareOp::Eq))
        // Strip an anchored MATCHES predicate that was resolved via name_index.
        .filter(|p| !(use_name_index && p.field == "name" && p.op == CompareOp::Matches))
        .collect();

    // Filter on raw IndexRow — no heap allocation per rejected row.
    let filtered = candidates.filter(|row| {
        if let Some(pat) = name_like
            && !like_match(index.name_of(row), pat)
        {
            return false;
        }
        if let Some(ref glob) = clauses.in_glob
            && !crate::ast::query::relative_glob_matches(index.path_of(row), glob, root)
        {
            return false;
        }
        if let Some(ref glob) = clauses.exclude_glob
            && crate::ast::query::relative_glob_matches(index.path_of(row), glob, root)
        {
            return false;
        }
        non_usages_preds
            .iter()
            .all(|p| eval_predicate(&RowRef { row, table: index }, p))
    });

    // Materialize SymbolMatch only for survivors, dedup inline.
    // When no ORDER BY / GROUP BY / usages-WHERE remains we can stop as soon
    // as we hit the LIMIT — no point scanning the remaining millions of rows.
    let has_usages_pred = clauses.where_predicates.iter().any(is_usages_pred);
    let can_early_exit = !has_usages_pred
        && clauses.order_by.is_none()
        && clauses.group_by.is_none()
        && clauses.offset.is_none();
    let early_limit = if can_early_exit {
        clauses.limit.unwrap_or(usize::MAX)
    } else {
        usize::MAX
    };

    let mut seen = HashSet::new();
    let mut results: Vec<SymbolMatch> = Vec::new();
    for def in filtered {
        if results.len() >= early_limit {
            break;
        }
        let key = (def.name_id, def.path_id, def.node_kind_id, def.line);
        if !seen.insert(key) {
            continue;
        }
        // usages_count is precomputed at index-build time; no HashMap lookup needed.
        let usages = def.usages_count as usize;
        let fql = index.fql_kind_of(def);
        let lang = index.language_of(def);
        results.push(SymbolMatch {
            name: index.name_of(def).to_owned(),
            node_kind: Some(index.node_kind_of(def).to_owned()),
            fql_kind: if fql.is_empty() {
                None
            } else {
                Some(fql.to_owned())
            },
            language: if lang.is_empty() {
                None
            } else {
                Some(lang.to_owned())
            },
            path: Some(index.path_of(def).to_owned()),
            line: Some(def.line),
            usages_count: Some(usages),
            fields: def.fields.clone(),
            count: None,
        });
    }

    // Only usages-based WHERE, GROUP/HAVING, ORDER, OFFSET, LIMIT remain.
    let remaining = Clauses {
        where_predicates: clauses
            .where_predicates
            .iter()
            .filter(|p| is_usages_pred(p))
            .cloned()
            .collect(),
        having_predicates: clauses.having_predicates.clone(),
        order_by: clauses.order_by.clone(),
        group_by: clauses.group_by.clone(),
        limit: clauses.limit,
        offset: clauses.offset,
        in_glob: None,
        exclude_glob: None,
        depth: None,
    };

    (results, remaining)
}

/// Split a qualified name like `CachedIndex::save` or `MyClass.method` into
/// `(owner, member)`.  Returns `None` for bare names without a separator.
///
/// Tries `::` first (Rust, C++), then `.` (Python, JS, Java).
/// This is language-agnostic — the separator is detected from the name itself.
fn split_qualified_name(name: &str) -> Option<(&str, &str)> {
    // Try `::` first (higher precedence — avoids false splits in `A::B.c`)
    if let Some(pos) = name.rfind("::") {
        let owner = &name[..pos];
        let member = &name[pos + 2..];
        if !owner.is_empty() && !member.is_empty() {
            return Some((owner, member));
        }
    }
    // Fall back to `.`
    if let Some(pos) = name.rfind('.') {
        let owner = &name[..pos];
        let member = &name[pos + 1..];
        if !owner.is_empty() && !member.is_empty() {
            return Some((owner, member));
        }
    }
    None
}
/// Resolve a symbol name to a single [`IndexRow`] using SHOW command clauses.
///
/// 1. Finds all definition rows matching `name` in the index.
/// 2. Filters by `IN`/`EXCLUDE` globs and `WHERE` predicates from `clauses`.
/// 3. If the surviving candidates span multiple languages, returns an error
///    asking the user to disambiguate with `WHERE language = '...'` or
///    `IN '*.ext'`.
/// 4. Returns the last matching row (preserving v1 last-write-wins semantics
///    within a single language).
#[allow(clippy::too_many_lines)]
pub(crate) fn resolve_symbol<'a>(
    index: &'a SymbolTable,
    name: &str,
    clauses: &Clauses,
    root: &Path,
) -> Result<&'a crate::ast::index::IndexRow> {
    use crate::ast::index::RowRef;
    use crate::filter::eval_predicate;

    // Qualified name resolution: split on `::` or `.` separators.
    // If the name contains a separator, look up the member name and filter
    // by the `enclosing_type` enrichment field set by MemberEnricher.
    if let Some((owner, member)) = split_qualified_name(name) {
        let candidates = index.find_all_defs(member);
        let matched: Vec<&crate::ast::index::IndexRow> = candidates
            .into_iter()
            .filter(|row| {
                row.fields
                    .get("enclosing_type")
                    .is_some_and(|et| et == owner)
            })
            .collect();
        if !matched.is_empty() {
            #[allow(clippy::expect_used)]
            return Ok(matched.last().expect("matched is non-empty"));
        }
        // Fall through: the qualified name may be resolved via body_symbol
        // redirect (C++ out-of-line definitions) or as-is in the index.
    }

    let candidates = index.find_all_defs(name);
    if candidates.is_empty() {
        let suggestions = index.suggest_similar(name, 5);
        if suggestions.is_empty() {
            bail!("symbol '{name}' not found in index");
        }
        bail!(
            "symbol '{name}' not found in index. \
             Did you mean one of: {}? \
             Use FIND symbols WHERE name LIKE \
             '%{name}%' to search.",
            suggestions.join(", ")
        );
    }

    // Single candidate — fast path, skip filtering.
    if candidates.len() == 1 {
        return Ok(candidates[0]);
    }

    let filtered: Vec<&crate::ast::index::IndexRow> = candidates
        .into_iter()
        .filter(|row| {
            if let Some(ref glob) = clauses.in_glob
                && !crate::ast::query::relative_glob_matches(index.path_of(row), glob, root)
            {
                return false;
            }
            if let Some(ref glob) = clauses.exclude_glob
                && crate::ast::query::relative_glob_matches(index.path_of(row), glob, root)
            {
                return false;
            }
            clauses
                .where_predicates
                .iter()
                .all(|p| eval_predicate(&RowRef { row, table: index }, p))
        })
        .collect();

    if filtered.is_empty() {
        use std::fmt::Write;
        let mut hint = format!(
            "symbol '{name}' exists in the index \
             but all candidates were eliminated by filters."
        );
        if let Some(ref glob) = clauses.in_glob {
            let _ = write!(hint, " IN '{glob}' excluded all matches.");
        }
        if let Some(ref glob) = clauses.exclude_glob {
            let _ = write!(hint, " EXCLUDE '{glob}' removed matches.");
        }
        if !clauses.where_predicates.is_empty() {
            hint.push_str(" WHERE predicates filtered all remaining candidates.");
        }
        let _ = write!(
            hint,
            " Try removing filters or use \
             FIND symbols WHERE name = '{name}' to see all occurrences."
        );
        bail!("{hint}");
    }

    // Prefer actual definitions (non-empty fql_kind) over reference-only
    // index rows such as scoped_identifier / qualified_identifier nodes
    // that happen to share the bare name.
    let defs: Vec<&crate::ast::index::IndexRow> = filtered
        .iter()
        .copied()
        .filter(|row| !index.fql_kind_of(row).is_empty())
        .collect();
    let best = if defs.is_empty() { &filtered } else { &defs };

    // Check cross-language ambiguity.
    let mut languages: Vec<&str> = best
        .iter()
        .filter_map(|r| {
            let lang = index.language_of(r);
            if lang.is_empty() { None } else { Some(lang) }
        })
        .collect();
    languages.sort_unstable();
    languages.dedup();

    if languages.len() > 1 {
        bail!(
            "symbol '{name}' exists in multiple languages: [{}]. \
             Use WHERE language = '...' or IN '*.ext' to disambiguate",
            languages.join(", ")
        );
    }

    // Last match — preserves v1 last-write-wins within a single language.
    // SAFETY: `best` is guaranteed non-empty by the bail above.
    #[allow(clippy::expect_used)]
    Ok(best.last().expect("filtered is non-empty"))
}

/// Like [`resolve_symbol`] but follows the `body_symbol` redirect.
///
/// If the resolved row carries a `body_symbol` field (set by the
/// `MemberEnricher` for out-of-line member function definitions), follow
/// the redirect to find the actual function body.
pub(crate) fn resolve_body_symbol<'a>(
    index: &'a SymbolTable,
    name: &str,
    clauses: &Clauses,
    root: &Path,
) -> Result<&'a crate::ast::index::IndexRow> {
    let def = resolve_symbol(index, name, clauses, root)?;
    if let Some(target) = def.fields.get("body_symbol")
        && let Some(redirected) = index.find_def(target)
    {
        return Ok(redirected);
    }
    Ok(def)
}

/// Validate the `ORDER BY` field against a result set, returning an error if
/// the field is not recognised for any item.
/// Validate that the ORDER BY field is either a known built-in field, a known
/// enrichment field, or present in at least one result item.
///
/// When the ORDER BY field is a recognised enrichment field (from any
/// registered language config), we accept it unconditionally — items that
/// lack the field will sort to the end (`field_num` returns None, which the
/// sort comparator already handles).  This allows queries like
/// `FIND symbols WHERE has_assignment_in_condition = 'true' ORDER BY lines DESC`
/// to work even when the result set contains symbol types (e.g. `if`) that
/// don't carry the `lines` enrichment field themselves.
pub(crate) fn validate_order_by_field(
    clauses: &Clauses,
    results: &[crate::result::SymbolMatch],
    lang_configs: &[&crate::ast::lang::LanguageConfig],
) -> Result<()> {
    use crate::filter::ClauseTarget as _;

    const STATIC_FIELDS: &[&str] = &[
        "name",
        "fql_kind",
        "node_kind",
        "path",
        "file",
        "line",
        "usages",
        "count",
    ];

    let Some(ref order) = clauses.order_by else {
        return Ok(());
    };
    if STATIC_FIELDS.contains(&order.field.as_str()) {
        return Ok(());
    }

    // Accept any known enrichment field — items without it sort to the end.
    if field_to_kinds(lang_configs, &order.field).is_some() {
        return Ok(());
    }

    // For dynamic fields (e.g. "type", "value", "signature"):
    // accept if at least one result item carries the field.
    if results
        .iter()
        .any(|r| r.field_num(&order.field).is_some() || r.field_str(&order.field).is_some())
    {
        return Ok(());
    }

    // If the result set is empty we cannot tell whether the field is valid;
    // skip reporting an error to avoid spurious failures.
    if results.is_empty() {
        return Ok(());
    }

    bail!(
        "unknown ORDER BY field '{}'; built-in fields: {}",
        order.field,
        STATIC_FIELDS.join(", ")
    )
}

/// Reject `WHERE text …` on FIND queries — `text` is only available on
/// commands that return source lines (SHOW body, SHOW LINES, SHOW context).
pub(crate) fn reject_text_filter(clauses: &Clauses) -> Result<()> {
    if clauses
        .where_predicates
        .iter()
        .any(|p| p.field == "text" || p.field == "content")
    {
        bail!(
            "WHERE text/content is not available on FIND queries — \
             it only works on commands that return source lines \
             (SHOW body, SHOW LINES, SHOW context)"
        );
    }
    Ok(())
}

// -----------------------------------------------------------------------
// Enrichment field → node_kind inference
// -----------------------------------------------------------------------

/// Map an enrichment field name to the `node_kind`(s) that carry it.
///
/// Returns `None` for universal fields (`naming`, `name_length`) or
/// built-in fields (`name`, `node_kind`, `path`, `line`, `usages`).
fn field_to_kinds_for_config(
    config: &crate::ast::lang::LanguageConfig,
    field: &str,
) -> Option<Vec<String>> {
    match field {
        // function_definition only — metrics, redundancy, escape, shadow,
        // unused_param, fallthrough, recursion, todo, decl_distance
        "param_count"
        | "return_count"
        | "goto_count"
        | "string_count"
        | "throw_count"
        | "is_inline"
        | "branch_count"
        | "max_condition_tests"
        | "max_paren_depth"
        | "has_repeated_condition_calls"
        | "repeated_condition_calls"
        | "null_check_count"
        | "has_escape"
        | "escape_tier"
        | "escape_vars"
        | "escape_count"
        | "escape_kinds"
        | "has_shadow"
        | "shadow_count"
        | "shadow_vars"
        | "has_unused_param"
        | "unused_param_count"
        | "unused_params"
        | "has_fallthrough"
        | "fallthrough_count"
        | "is_recursive"
        | "recursion_count"
        | "has_todo"
        | "todo_count"
        | "todo_tags"
        | "decl_distance"
        | "decl_far_count"
        | "has_unused_reassign" => Some(config.function_kinds().to_vec()),
        // comments.rs
        "comment_style" => Some(vec![config.comment_kind().to_owned()]),
        // numbers.rs
        "num_format" | "is_magic" | "num_suffix" | "has_separator" | "num_value" | "num_sign" => {
            Some(config.number_literal_kinds().to_vec())
        }
        // operators.rs
        "increment_style" | "increment_op" => Some(config.update_kinds().to_vec()),
        "compound_op" | "operand" => Some(vec![config.compound_assignment_kind().to_owned()]),
        "shift_direction" | "shift_operand" | "shift_amount" => {
            Some(config.shift_expression_kinds().to_vec())
        }
        // casts.rs — per-cast-node fields
        "cast_style" | "cast_target_type" | "cast_safety" => Some(
            config
                .cast_kind_triples()
                .iter()
                .map(|(raw_kind, _, _)| raw_kind.clone())
                .collect(),
        ),
        // casts.rs — per-function fields
        "has_cast" | "cast_count" => Some(config.function_raw_kinds.clone()),
        // control_flow.rs
        "condition_tests"
        | "paren_depth"
        | "condition_text"
        | "has_assignment_in_condition"
        | "mixed_logic"
        | "dup_logic"
        | "for_style"
        | "duplicate_condition" => Some(config.control_flow_kinds().to_vec()),
        "has_catch_all" => Some(config.switch_kinds().to_vec()),
        // metrics.rs — multiple definition kinds
        "lines" | "member_count" | "has_doc" => Some(config.definition_kinds().to_vec()),
        // metrics.rs — qualifier flags / scope.rs — is_exported
        "is_const" | "is_volatile" | "is_static" | "is_exported" => {
            let mut kinds = config.declaration_kinds().to_vec();
            kinds.extend_from_slice(config.function_kinds());
            Some(kinds)
        }
        // metrics.rs — visibility
        "visibility" => Some(config.field_kinds().to_vec()),
        // scope.rs — declaration only
        "scope" | "storage" => Some(config.declaration_kinds().to_vec()),
        // Universal / built-in → no shortcut
        _ => None,
    }
}

/// Aggregate `field_to_kinds_for_config` across all registered language configs.
fn field_to_kinds(
    configs: &[&crate::ast::lang::LanguageConfig],
    field: &str,
) -> Option<Vec<String>> {
    let mut all_kinds: Vec<String> = Vec::new();
    for config in configs {
        if let Some(kinds) = field_to_kinds_for_config(config, field) {
            for k in kinds {
                if !all_kinds.contains(&k) {
                    all_kinds.push(k);
                }
            }
        }
    }
    if all_kinds.is_empty() {
        None
    } else {
        Some(all_kinds)
    }
}

/// Inspect WHERE predicates for enrichment fields and, when all resolvable
/// fields agree on the same set of kinds, return that set.
///
/// Returns `None` when no enrichment fields are found, or when the
/// intersection of inferred kinds is empty (contradictory predicates).
fn infer_kinds_from_fields(
    predicates: &[crate::ir::Predicate],
    configs: &[&crate::ast::lang::LanguageConfig],
) -> Option<Vec<String>> {
    let mut result: Option<Vec<String>> = None;
    for pred in predicates {
        let Some(kinds) = field_to_kinds(configs, &pred.field) else {
            continue;
        };
        result = Some(match result {
            None => kinds,
            Some(current) => {
                let intersected: Vec<String> =
                    current.into_iter().filter(|k| kinds.contains(k)).collect();
                if intersected.is_empty() {
                    // Contradictory (e.g. cast_style + comment_style) — bail.
                    return None;
                }
                intersected
            }
        });
    }
    result
}

/// Convert `TransformPlan` suggestions into typed `SuggestionEntry` values.
pub(crate) fn convert_suggestions(plan: &TransformPlan) -> Vec<SuggestionEntry> {
    plan.suggestions
        .iter()
        .map(|candidate| SuggestionEntry {
            path: candidate.path.clone(),
            byte_offset: candidate.byte_offset,
            snippet: candidate.snippet.clone(),
            reason: format!("{:?}", candidate.reason),
        })
        .collect()
}

/// Convert the JSON output from `execute_show` to a typed `ShowResult`.
///
/// This is a transitional bridge: the executor currently returns
/// `serde_json::Value`.  As we refactor `ast/show.rs` to return typed
/// results directly, this function will shrink and eventually disappear.
pub(crate) fn convert_show_json(op: &ForgeQLIR, json: &serde_json::Value) -> Result<ShowResult> {
    let op_name = json
        .get("op")
        .and_then(|v| v.as_str())
        .unwrap_or("show")
        .to_string();

    let symbol = json
        .get("symbol")
        .and_then(|v| v.as_str())
        .map(String::from);

    let file = json.get("file").and_then(|v| v.as_str()).map(PathBuf::from);

    let content = convert_show_content(op, json)?;

    let start_line = json
        .get("start_line")
        .and_then(serde_json::Value::as_u64)
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
    let end_line = json
        .get("end_line")
        .and_then(serde_json::Value::as_u64)
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX));

    let metadata = json
        .get("metadata")
        .and_then(serde_json::Value::as_object)
        .cloned();

    Ok(ShowResult {
        op: op_name,
        symbol,
        file,
        content,
        start_line,
        end_line,
        total_lines: None,
        hint: None,
        metadata,
    })
}

/// Convert the inner content of a SHOW JSON response to a typed `ShowContent`.
///
/// Each SHOW variant has a different JSON shape; this function pattern-matches
/// on the `ForgeQLIR` variant to determine how to interpret the JSON.
///
/// This is a transitional bridge — the executor currently returns
/// `serde_json::Value`.  The `u64 as usize` casts are safe because line
/// numbers and byte offsets never exceed `usize::MAX` on any supported target.
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::redundant_closure_for_method_calls
)]
fn convert_show_content(op: &ForgeQLIR, json: &serde_json::Value) -> Result<ShowContent> {
    match op {
        // Line-oriented results: show_body, show_context, show_lines.
        ForgeQLIR::ShowBody { clauses, .. } => {
            let lines = extract_source_lines(json);
            Ok(ShowContent::Lines {
                lines,
                byte_start: json
                    .get("byte_start")
                    .and_then(|v| v.as_u64())
                    .map(|b| b as usize),
                depth: clauses.depth,
            })
        }

        ForgeQLIR::ShowContext { .. } | ForgeQLIR::ShowLines { .. } => {
            let lines = extract_source_lines(json);
            Ok(ShowContent::Lines {
                lines,
                byte_start: None,
                depth: None,
            })
        }

        ForgeQLIR::ShowSignature { .. } => {
            let signature = json
                .get("signature")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let line = json.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let byte_start = json.get("byte_start").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            Ok(ShowContent::Signature {
                signature,
                line,
                byte_start,
            })
        }

        ForgeQLIR::ShowOutline { .. } => {
            let entries = json
                .get("results")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|entry| {
                            Some(OutlineEntry {
                                name: entry.get("name")?.as_str()?.to_string(),
                                fql_kind: entry.get("fql_kind")?.as_str()?.to_string(),
                                path: PathBuf::from(entry.get("path")?.as_str()?),
                                line: entry.get("line")?.as_u64()? as usize,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(ShowContent::Outline { entries })
        }

        ForgeQLIR::ShowMembers { .. } => {
            let members = json
                .get("members")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| {
                            Some(MemberEntry {
                                fql_kind: m.get("fql_kind")?.as_str()?.to_string(),
                                text: m.get("text")?.as_str()?.to_string(),
                                line: m.get("line")?.as_u64()? as usize,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let byte_start = json.get("byte_start").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            Ok(ShowContent::Members {
                members,
                byte_start,
            })
        }

        ForgeQLIR::ShowCallees { .. } => {
            let direction = CallDirection::Callees;
            let entries = json
                .get("results")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|entry| {
                            Some(CallGraphEntry {
                                name: entry.get("name")?.as_str()?.to_string(),
                                path: entry
                                    .get("path")
                                    .and_then(|v| v.as_str())
                                    .map(PathBuf::from),
                                line: entry
                                    .get("line")
                                    .and_then(|v| v.as_u64())
                                    .map(|l| l as usize),
                                byte_start: entry
                                    .get("byte_start")
                                    .and_then(|v| v.as_u64())
                                    .map(|b| b as usize),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(ShowContent::CallGraph { direction, entries })
        }

        ForgeQLIR::FindFiles { clauses, .. } => {
            let results = json
                .get("results")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|entry| {
                            // FIND files results can be strings or objects with "path".
                            let path_str = entry
                                .as_str()
                                .or_else(|| entry.get("path").and_then(|v| v.as_str()))?;
                            let extension = entry
                                .get("extension")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let size = entry.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                            let count = entry
                                .get("count")
                                .and_then(|v| v.as_u64())
                                .map(|n| n as usize);
                            Some(FileEntry {
                                path: PathBuf::from(path_str),
                                depth: clauses.depth,
                                extension,
                                size,
                                count,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let total = json
                .get("count")
                .and_then(|v| v.as_u64())
                .unwrap_or(results.len() as u64) as usize;
            Ok(ShowContent::FileList {
                files: results,
                total,
            })
        }

        _ => bail!("unsupported SHOW variant: {op:?}"),
    }
}

/// Extract source lines from the JSON `"lines"` or `"results"` array.
#[allow(
    clippy::cast_possible_truncation,
    clippy::redundant_closure_for_method_calls
)]
fn extract_source_lines(json: &serde_json::Value) -> Vec<SourceLine> {
    // Different SHOW ops use different keys: "lines" or "results".
    let arr = json
        .get("lines")
        .or_else(|| json.get("results"))
        .and_then(|v| v.as_array());

    let Some(arr) = arr else {
        return Vec::new();
    };

    arr.iter()
        .filter_map(|item| {
            let line = item
                .get("line")
                .or_else(|| item.get("line_number"))
                .and_then(|v| v.as_u64())? as usize;
            let text = item
                .get("text")
                .or_else(|| item.get("content"))
                .and_then(|v| v.as_str())?
                .to_string();
            let marker = item
                .get("marker")
                .and_then(|v| v.as_str())
                .map(String::from);
            Some(SourceLine { line, text, marker })
        })
        .collect()
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::lang::CppLanguageInline;
    use crate::ir::Clauses;

    fn make_registry() -> Arc<LanguageRegistry> {
        Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]))
    }

    #[cfg(feature = "test-helpers")]
    #[test]
    fn generate_session_id_starts_with_s() {
        let id = generate_session_id();
        assert!(
            id.starts_with('s'),
            "test helper session ID must start with 's': {id}"
        );
    }

    #[cfg(feature = "test-helpers")]
    #[test]
    fn generate_session_id_unique() {
        let id1 = generate_session_id();
        // Wait 1 ms to ensure different timestamp.
        std::thread::sleep(std::time::Duration::from_millis(1));
        let id2 = generate_session_id();
        assert_ne!(
            id1, id2,
            "consecutive test-helper session IDs should differ"
        );
    }

    #[test]
    fn require_session_id_empty_fails() {
        let result = require_session_id(None);
        assert!(result.is_err());
        let result = require_session_id(Some(""));
        assert!(result.is_err());
    }

    #[test]
    fn require_session_id_valid() {
        let result = require_session_id(Some("s12345"));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "s12345");
    }

    #[test]
    fn mutation_op_name_mapping() {
        let change = ForgeQLIR::ChangeContent {
            files: vec!["f.cpp".into()],
            target: crate::ir::ChangeTarget::Delete,
            clauses: Clauses::default(),
        };
        assert_eq!(mutation_op_name(&change), "change_content");

        let unknown = ForgeQLIR::ShowSources;
        assert_eq!(mutation_op_name(&unknown), "unknown_mutation");
    }

    #[test]
    fn engine_new_creates_worktrees_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let engine = ForgeQLEngine::new(data_dir.clone(), make_registry()).unwrap();
        assert!(data_dir.join("worktrees").exists());
        assert_eq!(engine.session_count(), 0);
        assert_eq!(engine.source_count(), 0);
        assert_eq!(engine.commands_served(), 0);
    }

    #[test]
    fn engine_show_sources_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
        let result = engine.execute(None, &ForgeQLIR::ShowSources).unwrap();
        match result {
            ForgeQLResult::Query(qr) => {
                assert_eq!(qr.op, "show_sources");
                assert!(qr.results.is_empty());
            }
            other => panic!("expected Query, got: {other:?}"),
        }
    }

    #[test]
    fn engine_show_branches_requires_session() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
        let result = engine.execute(None, &ForgeQLIR::ShowBranches);
        assert!(result.is_err());
    }

    // (engine_disconnect_without_session_fails test removed — DISCONNECT eliminated)

    // (engine_disconnect_unknown_session_fails removed — DISCONNECT command eliminated)

    #[test]
    fn engine_find_symbols_without_session_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
        let op = ForgeQLIR::FindSymbols {
            clauses: Clauses::default(),
        };
        let result = engine.execute(None, &op);
        assert!(result.is_err());
    }

    #[test]
    fn convert_suggestions_from_empty_plan() {
        let plan = TransformPlan::default();
        let suggestions = convert_suggestions(&plan);
        assert!(suggestions.is_empty());
    }

    // ── validate_order_by_field ──────────────────────────────────────────────

    fn make_sym(name: &str) -> crate::result::SymbolMatch {
        crate::result::SymbolMatch {
            name: name.to_string(),
            node_kind: Some("function_definition".to_string()),
            fql_kind: Some("function".to_string()),
            language: Some("cpp".to_string()),
            path: Some(std::path::PathBuf::from("src/a.cpp")),
            line: Some(10),
            usages_count: Some(3),
            fields: std::collections::HashMap::new(),
            count: None,
        }
    }

    fn clauses_with_order(field: &str) -> Clauses {
        Clauses {
            order_by: Some(crate::ir::OrderBy {
                field: field.to_string(),
                direction: crate::ir::SortDirection::Asc,
            }),
            ..Clauses::default()
        }
    }

    #[test]
    fn validate_order_by_field_accepts_static_fields() {
        let results = vec![make_sym("foo")];
        for field in &[
            "name",
            "fql_kind",
            "node_kind",
            "path",
            "file",
            "line",
            "usages",
            "count",
        ] {
            assert!(
                validate_order_by_field(&clauses_with_order(field), &results, &[]).is_ok(),
                "expected Ok for ORDER BY {field}"
            );
        }
    }

    #[test]
    fn validate_order_by_field_rejects_unknown_field() {
        let results = vec![make_sym("foo"), make_sym("bar")];
        let err = validate_order_by_field(&clauses_with_order("invalid_field"), &results, &[]);
        assert!(err.is_err(), "expected Err for ORDER BY invalid_field");
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("invalid_field"),
            "error should mention the field name; got: {msg}"
        );
    }

    #[test]
    fn validate_order_by_field_accepts_dynamic_field_when_present() {
        let mut sym = make_sym("foo");
        sym.fields
            .insert("signature".to_string(), "void foo()".to_string());
        let results = vec![sym];
        assert!(validate_order_by_field(&clauses_with_order("signature"), &results, &[]).is_ok());
    }

    #[test]
    fn validate_order_by_field_ok_when_results_empty() {
        let results: Vec<crate::result::SymbolMatch> = Vec::new();
        // Should not error even for unknown field when result set is empty.
        assert!(validate_order_by_field(&clauses_with_order("unknown_xyz"), &results, &[]).is_ok());
    }

    #[test]
    fn validate_order_by_field_no_order_by_always_ok() {
        let results = vec![make_sym("foo")];
        assert!(validate_order_by_field(&Clauses::default(), &results, &[]).is_ok());
    }

    /// `FIND globals` now maps to `FIND symbols WHERE node_kind = 'declaration'`
    /// and correctly returns variable declarations from the index.
    ///
    /// `motor_control.cpp` declares several `static` variables at file scope
    /// (`motorPrincipal`, `motorSecundario`, `gCallbackEncendido`, `kMotorLabel`).
    /// These are `declaration` nodes in the tree-sitter AST and must now
    /// appear in results.
    #[cfg(feature = "test-helpers")]
    #[test]
    fn find_globals_returns_declaration_nodes() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let fixtures = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("tests/fixtures");
        fs::copy(
            fixtures.join("motor_control.h"),
            tmp.path().join("motor_control.h"),
        )
        .unwrap();
        fs::copy(
            fixtures.join("motor_control.cpp"),
            tmp.path().join("motor_control.cpp"),
        )
        .unwrap();

        let data_dir = tmp.path().join("data");
        let mut engine = ForgeQLEngine::new(data_dir, make_registry()).unwrap();
        let session_id = engine.register_local_session(tmp.path()).unwrap();

        // FIND globals → FIND symbols WHERE fql_kind = 'variable' WHERE scope = 'file'
        let op = crate::parser::parse("FIND globals LIMIT 200").unwrap();
        let result = engine.execute(Some(&session_id), &op[0]).unwrap();
        let results = match result {
            ForgeQLResult::Query(qr) => qr.results,
            other => panic!("expected Query, got: {other:?}"),
        };

        // All returned rows must be file-scope variable nodes.
        for r in &results {
            assert_eq!(
                r.fql_kind.as_deref(),
                Some("variable"),
                "FIND globals must only return variable nodes, got {:?} for '{}'",
                r.fql_kind,
                r.name,
            );
            assert_eq!(
                r.fields.get("scope").map(String::as_str),
                Some("file"),
                "FIND globals must only return file-scope declarations, got scope={:?} for '{}'",
                r.fields.get("scope"),
                r.name,
            );
        }

        // The known file-scope static variables should appear.
        let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
        for expected in [
            "motorPrincipal",
            "motorSecundario",
            "gCallbackEncendido",
            "kMotorLabel",
        ] {
            assert!(
                names.contains(&expected),
                "declaration '{expected}' must appear in FIND globals; got: {names:?}"
            );
        }

        // All file-scope declarations in the fixture are static.
        for r in &results {
            assert_eq!(
                r.fields.get("storage").map(String::as_str),
                Some("static"),
                "expected storage='static' for '{}'; got {:?}",
                r.name,
                r.fields.get("storage"),
            );
        }

        // Local variables must NOT appear.
        for local in ["vel", "velocidad"] {
            assert!(
                !names.contains(&local),
                "local variable '{local}' must NOT appear in FIND globals; got: {names:?}"
            );
        }
    }

    /// `FIND globals WHERE node_kind = 'enum_specifier'` must return zero results,
    /// not silently drop the `node_kind` predicate and return all variables.
    ///
    /// Regression: the `non_usages_preds` filter incorrectly stripped the
    /// `node_kind` predicate when `fql_kind_exact` was also present, because
    /// the `kind_exact.is_some()` guard didn't account for the index-selection
    /// priority (`fql_kind` wins over `node_kind`).
    #[cfg(feature = "test-helpers")]
    #[test]
    fn find_globals_with_conflicting_node_kind_returns_empty() {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let fixtures = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("tests/fixtures");
        fs::copy(
            fixtures.join("motor_control.h"),
            tmp.path().join("motor_control.h"),
        )
        .unwrap();
        fs::copy(
            fixtures.join("motor_control.cpp"),
            tmp.path().join("motor_control.cpp"),
        )
        .unwrap();

        let data_dir = tmp.path().join("data");
        let mut engine = ForgeQLEngine::new(data_dir, make_registry()).unwrap();
        let session_id = engine.register_local_session(tmp.path()).unwrap();

        // FIND globals adds fql_kind='variable' + scope='file' implicitly.
        // Adding WHERE node_kind = 'enum_specifier' must further filter,
        // not be silently dropped.
        let op = crate::parser::parse("FIND globals WHERE node_kind = 'enum_specifier' LIMIT 200")
            .unwrap();
        let result = engine.execute(Some(&session_id), &op[0]).unwrap();
        let results = match result {
            ForgeQLResult::Query(qr) => qr.results,
            other => panic!("expected Query, got: {other:?}"),
        };

        assert!(
            results.is_empty(),
            "FIND globals WHERE node_kind = 'enum_specifier' should return 0 results \
             (no variable has node_kind='enum_specifier'), got {} results: {:?}",
            results.len(),
            results.iter().map(|r| &r.name).collect::<Vec<_>>(),
        );
    }
}
