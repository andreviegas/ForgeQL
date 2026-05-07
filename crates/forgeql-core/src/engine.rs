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

use anyhow::{Result, bail};
use tracing::{info, warn};

use crate::{
    ast::lang::LanguageRegistry,
    config::ForgeConfig,
    git::source::SourceRegistry,
    ir::{Clauses, ForgeQLIR},
    result::{
        CallDirection, CallGraphEntry, FileEntry, ForgeQLResult, MemberEntry, OutlineEntry,
        ShowContent, ShowResult, SourceLine, SuggestionEntry,
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
pub mod warm;
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
    use crate::ir::{Backend, Clauses};

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
            backend: Backend::default(),
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
