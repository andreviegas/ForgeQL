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

use anyhow::{bail, Result};
use tracing::{info, warn};

use crate::{
    ast::{index::SymbolTable, query, show},
    config::ForgeConfig,
    context::RequestContext,
    git::{
        self as git,
        source::{Source, SourceRegistry},
        worktree,
    },
    ir::{Clauses, ForgeQLIR},
    result::{
        CallDirection, CallGraphEntry, FileEntry, ForgeQLResult, MemberEntry, MutationResult,
        OutlineEntry, QueryResult, RollbackResult, ShowContent, ShowResult, SourceLine,
        SourceOpResult, SuggestionEntry, SymbolMatch, TransactionResult,
    },
    session::Session,
    transforms::{plan_from_ir, TransformPlan},
    verify,
    workspace::Workspace,
};

// -----------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------

/// How long (in seconds) a session may be idle before `evict_idle_sessions`
/// removes it.
const SESSION_TTL_SECS: u64 = 2 * 60 * 60; // 2 hours

// -----------------------------------------------------------------------
// ForgeQLEngine
// -----------------------------------------------------------------------

/// Implicit row cap for `FIND` queries that specify no `LIMIT` clause.
///
/// Prevents runaway token consumption when the agent issues a broad query
/// such as `FIND symbols` on a large codebase.  The agent can always
/// override with an explicit `LIMIT N` clause.  When the cap fires,
/// `total > results.len()` signals that more rows are available.
const DEFAULT_QUERY_LIMIT: usize = 20;

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
}

// -----------------------------------------------------------------------
// SHOW helpers
// -----------------------------------------------------------------------

/// Apply LIMIT and OFFSET from `clauses` to the named JSON array field
/// inside a SHOW result value.
///
/// Used by `ShowOutline` (`"results"`) and `ShowMembers` (`"members"`) so
/// that agents can paginate or preview large lists without receiving the
/// entire member/declaration set.
fn apply_show_list_clauses(
    json: &mut serde_json::Value,
    array_key: &str,
    clauses: &crate::ir::Clauses,
) {
    let offset = clauses.offset.unwrap_or(0);
    let limit = clauses.limit;

    if offset == 0 && limit.is_none() {
        return; // nothing to do
    }

    let Some(arr) = json.get_mut(array_key).and_then(|v| v.as_array_mut()) else {
        return;
    };

    if offset > 0 {
        let skip = offset.min(arr.len());
        drop(arr.drain(..skip));
    }
    if let Some(max) = limit {
        arr.truncate(max);
    }
}

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
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(data_dir.join("worktrees"))?;
        info!(dir = %data_dir.display(), "engine: data directory ready");

        let mut registry = SourceRegistry::new(data_dir.clone());
        Self::discover_existing_sources(&data_dir, &mut registry);

        let engine = Self {
            registry,
            sessions: HashMap::new(),
            data_dir,
            commands_served: 0,
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
    pub fn execute(&mut self, session_id: Option<&str>, op: &ForgeQLIR) -> Result<ForgeQLResult> {
        self.commands_served += 1;

        // Keep session alive on every request.
        if let Some(sid) = session_id {
            if let Some(session) = self.sessions.get_mut(sid) {
                session.touch();
            }
        }

        // Look up worktree root once — used to relativize paths in results.
        let worktree_root = session_id
            .and_then(|sid| self.sessions.get(sid))
            .map(|s| s.worktree_path.clone());

        let mut result = match op {
            // --- Source / session management ---
            ForgeQLIR::CreateSource { name, url } => self.create_source(name, url),
            ForgeQLIR::RefreshSource { name } => self.refresh_source(name),
            ForgeQLIR::UseSource {
                source,
                branch,
                as_branch,
            } => self.use_source(source, branch, as_branch.as_deref()),
            ForgeQLIR::ShowSources => self.show_sources(),
            ForgeQLIR::ShowBranches { source } => self.show_branches(source.as_deref()),
            ForgeQLIR::Disconnect => self.disconnect(session_id),

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

            // --- Composite operations ---
            ForgeQLIR::Transaction {
                name,
                ops,
                message,
                verify,
            } => {
                self.exec_transaction(session_id, name, ops, verify.as_deref(), message.as_deref())
            }
            ForgeQLIR::Rollback { name } => self.exec_rollback(session_id, name.as_deref()),
        }?;

        // Strip absolute worktree prefixes so results carry only relative paths.
        // This keeps MCP JSON compact and avoids leaking internal filesystem layout.
        if let Some(ref root) = worktree_root {
            result.relativize_paths(root);
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

    // ===================================================================
    // Source / session management
    // ===================================================================

    /// `CREATE SOURCE 'name' FROM 'url'` — idempotent bare-clone.
    fn create_source(&mut self, name: &str, url: &str) -> Result<ForgeQLResult> {
        info!(%name, %url, "creating source");

        // Idempotent: if already registered in-memory, return immediately.
        if let Some(source) = self.registry.get(name) {
            let branches = source.branches().unwrap_or_default();
            return Ok(ForgeQLResult::SourceOp(SourceOpResult {
                op: "create_source".to_string(),
                source_name: Some(source.name().to_string()),
                session_id: None,
                branches,
                symbols_indexed: None,
                resumed: true,
                message: Some("source already registered".to_string()),
            }));
        }

        let repo_path = self.data_dir.join(format!("{name}.git"));
        let already_on_disk = repo_path.exists();

        // If the bare repo exists on disk (e.g. after server restart),
        // reopen it instead of re-cloning.
        let source = if already_on_disk {
            info!(name, "bare repo already on disk — reopening");
            Source::open(name, repo_path)?
        } else {
            Source::clone_from(name, url, &self.data_dir)?
        };

        let registered = self.registry.insert(source)?;
        let branches = registered.branches().unwrap_or_default();

        Ok(ForgeQLResult::SourceOp(SourceOpResult {
            op: "create_source".to_string(),
            source_name: Some(registered.name().to_string()),
            session_id: None,
            branches,
            symbols_indexed: None,
            resumed: already_on_disk,
            message: None,
        }))
    }

    /// `REFRESH SOURCE 'name'` — fetch all remotes on an existing bare repo.
    fn refresh_source(&self, name: &str) -> Result<ForgeQLResult> {
        info!(%name, "refreshing source");

        let source = self.registry.get(name).ok_or_else(|| {
            anyhow::anyhow!("source '{name}' not found — run CREATE SOURCE first")
        })?;
        let repo_path = source.path().to_path_buf();

        let reopened = Source::open(name, repo_path)?;
        let branches = reopened.fetch_all()?;

        Ok(ForgeQLResult::SourceOp(SourceOpResult {
            op: "refresh_source".to_string(),
            source_name: Some(name.to_string()),
            session_id: None,
            branches,
            symbols_indexed: None,
            resumed: false,
            message: None,
        }))
    }

    /// `USE source.branch [AS 'custom-branch']` — create or resume a session.
    fn use_source(
        &mut self,
        source_name: &str,
        branch: &str,
        as_branch: Option<&str>,
    ) -> Result<ForgeQLResult> {
        info!(%source_name, %branch, ?as_branch, "starting session");

        // Session resume: if an in-memory session already exists for this
        // source + branch + as_branch combination, return it immediately.
        if let Some((existing_id, existing_session)) = self.sessions.iter().find(|(_, s)| {
            s.source_name == source_name
                && as_branch.map_or_else(
                    || s.branch == branch && s.custom_branch.is_none(),
                    |ab| s.custom_branch.as_deref() == Some(ab),
                )
        }) {
            let symbols_indexed = existing_session.index().map_or(0, |idx| idx.rows.len());
            info!(
                session_id = %existing_id,
                %source_name,
                %branch,
                "session resume — reusing existing in-memory session"
            );
            return Ok(ForgeQLResult::SourceOp(SourceOpResult {
                op: "use_source".to_string(),
                source_name: Some(source_name.to_string()),
                session_id: Some(existing_id.clone()),
                branches: Vec::new(),
                symbols_indexed: Some(symbols_indexed),
                resumed: true,
                message: as_branch.map(|ab| format!("as_branch: {ab}")),
            }));
        }

        // Verify source exists.
        let repo_path = self
            .registry
            .get(source_name)
            .ok_or_else(|| {
                anyhow::anyhow!("source '{source_name}' not found — run CREATE SOURCE first")
            })?
            .path()
            .to_path_buf();

        let session_id = generate_session_id();
        // Worktree folder name: sanitize '/' → '-' for filesystem safety.
        let wt_name = as_branch.map_or_else(|| session_id.clone(), |s| s.replace('/', "-"));
        let wt_path = self.data_dir.join("worktrees").join(&wt_name);

        drop(worktree::create(
            &repo_path, &wt_name, branch, &wt_path, as_branch,
        )?);

        let mut session = Session::new(&session_id, "anonymous", wt_path, source_name, branch);
        session.custom_branch = as_branch.map(String::from);
        session.worktree_name = wt_name;

        // Use resume_index() so an existing disk cache at
        // <worktree>/.forgeql-index is reused when HEAD matches.
        session.resume_index()?;

        let symbols_indexed = session.index().map_or(0, |idx| idx.rows.len());
        let sid = session_id.clone();
        drop(self.sessions.insert(session_id, session));

        Ok(ForgeQLResult::SourceOp(SourceOpResult {
            op: "use_source".to_string(),
            source_name: Some(source_name.to_string()),
            session_id: Some(sid),
            branches: Vec::new(),
            symbols_indexed: Some(symbols_indexed),
            resumed: false,
            message: as_branch.map(|ab| format!("as_branch: {ab}")),
        }))
    }

    /// `DISCONNECT` — remove the session, delete its worktree and session branch.
    fn disconnect(&mut self, session_id: Option<&str>) -> Result<ForgeQLResult> {
        let sid = session_id.ok_or_else(|| {
            anyhow::anyhow!("session_id required — run USE <source>.<branch> first")
        })?;
        if sid.is_empty() {
            bail!("session_id required — run USE <source>.<branch> first");
        }

        let session = self
            .sessions
            .remove(sid)
            .ok_or_else(|| anyhow::anyhow!("session '{sid}' not found"))?;

        let repo_path = self.data_dir.join(format!("{}.git", session.source_name));
        let wt_name = &session.worktree_name;
        let custom_branch = &session.custom_branch;

        if let Err(err) = worktree::remove(&repo_path, wt_name) {
            warn!(%wt_name, error = %err, "disconnect: worktree remove failed");
        }
        // Only auto-delete the forgeql/* branch.  Named branches (from USE … AS)
        // are intentionally kept for senior developer review.
        if custom_branch.is_none() {
            if let Err(err) = worktree::delete_session_branch(&repo_path, wt_name) {
                warn!(%wt_name, error = %err, "disconnect: branch delete failed");
            }
        }

        info!(%sid, "session disconnected and cleaned up");
        Ok(ForgeQLResult::SourceOp(SourceOpResult {
            op: "disconnect".to_string(),
            source_name: None,
            session_id: Some(sid.to_string()),
            branches: Vec::new(),
            symbols_indexed: None,
            resumed: false,
            message: None,
        }))
    }

    /// `SHOW SOURCES` — list all registered sources.
    #[allow(clippy::unnecessary_wraps)] // uniform Result return across all ops
    fn show_sources(&self) -> Result<ForgeQLResult> {
        let mut results: Vec<SymbolMatch> = self
            .registry
            .names()
            .iter()
            .filter_map(|name| {
                self.registry.get(name).map(|source| SymbolMatch {
                    name: source.name().to_string(),
                    node_kind: Some("source".to_string()),
                    path: Some(source.path().to_path_buf()),
                    line: None,
                    usages_count: None,
                    fields: source
                        .origin_url()
                        .map(|url| {
                            std::collections::HashMap::from([("url".to_string(), url.to_string())])
                        })
                        .unwrap_or_default(),
                    count: None,
                })
            })
            .collect();
        results.sort_by(|a, b| a.name.cmp(&b.name));
        let total = results.len();

        Ok(ForgeQLResult::Query(QueryResult {
            op: "show_sources".to_string(),
            results,
            total,
        }))
    }

    /// `SHOW BRANCHES [OF 'source']` — list branches of a source.
    fn show_branches(&self, source: Option<&str>) -> Result<ForgeQLResult> {
        let name =
            source.ok_or_else(|| anyhow::anyhow!("SHOW BRANCHES requires OF '<source_name>'"))?;

        let source_ref = self
            .registry
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("source '{name}' not found"))?;
        let branches = source_ref.branches().unwrap_or_default();

        Ok(ForgeQLResult::SourceOp(SourceOpResult {
            op: "show_branches".to_string(),
            source_name: Some(name.to_string()),
            session_id: None,
            branches,
            symbols_indexed: None,
            resumed: false,
            message: None,
        }))
    }

    // ===================================================================
    // Read-only queries
    // ===================================================================

    /// `FIND symbols WHERE name LIKE 'pattern' ...`
    fn find_symbols(&self, session_id: Option<&str>, clauses: &Clauses) -> Result<ForgeQLResult> {
        let index = self.require_index(session_id)?;

        // Scan all symbols; apply_clauses handles name filtering via WHERE predicates.
        let defs = query::find_symbols_like(index, "%");

        let mut results: Vec<SymbolMatch> = defs
            .into_iter()
            .map(|def| {
                let usages = index.usages.get(&def.name).map_or(0, Vec::len);
                SymbolMatch {
                    name: def.name.clone(),
                    node_kind: Some(def.node_kind.clone()),
                    path: Some(def.path.clone()),
                    line: Some(def.line),
                    usages_count: Some(usages),
                    fields: def.fields.clone(),
                    count: None,
                }
            })
            .collect();

        // Validate ORDER BY field before applying clauses so that typos
        // produce a clear error instead of silently returning default order.
        validate_order_by_field(clauses, &results)?;

        crate::filter::apply_clauses(&mut results, clauses);
        // Capture total BEFORE the implicit cap so the agent can see when
        // more rows are available (total > results.len() in the response).
        let total = results.len();
        if clauses.limit.is_none() {
            results.truncate(DEFAULT_QUERY_LIMIT);
        }

        Ok(ForgeQLResult::Query(QueryResult {
            op: "find_symbols".to_string(),
            results,
            total,
        }))
    }

    /// `FIND usages OF 'symbol' ...`
    fn find_usages(
        &self,
        session_id: Option<&str>,
        of: &str,
        clauses: &Clauses,
    ) -> Result<ForgeQLResult> {
        let index = self.require_index(session_id)?;

        let sites = query::find_usages(index, of);
        let mut results: Vec<SymbolMatch> = sites
            .iter()
            .map(|site| SymbolMatch {
                name: of.to_string(),
                node_kind: None,
                path: Some(site.path.clone()),
                line: Some(site.line),
                usages_count: None,
                fields: std::collections::HashMap::new(),
                count: None,
            })
            .collect();

        validate_order_by_field(clauses, &results)?;

        crate::filter::apply_clauses(&mut results, clauses);
        let total = results.len();
        if clauses.limit.is_none() {
            results.truncate(DEFAULT_QUERY_LIMIT);
        }

        Ok(ForgeQLResult::Query(QueryResult {
            op: "find_usages".to_string(),
            results,
            total,
        }))
    }

    // ===================================================================
    // Code exposure — SHOW commands
    // ===================================================================

    /// Handle all SHOW variants and FIND files, calling `ast::show` functions
    /// directly and converting the JSON result to a typed `ShowResult`.
    ///
    /// Formerly delegated through `executor::execute_show`; the JSON bridge
    /// is now inlined here to eliminate the middle layer.
    fn exec_show(&self, session_id: Option<&str>, op: &ForgeQLIR) -> Result<ForgeQLResult> {
        let (workspace, index) = self.require_workspace_and_index(session_id)?;

        let json = match op {
            ForgeQLIR::ShowContext { symbol, clauses } => {
                let file = clauses.in_glob.as_deref();
                let context_lines = clauses.depth.unwrap_or(5);
                show::show_context(index, &workspace, symbol, file, context_lines)
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowSignature { symbol, .. } => {
                show::show_signature(index, &workspace, symbol)
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowOutline { file, clauses } => {
                let mut json = show::show_outline(index, &workspace, file)
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
                apply_show_list_clauses(&mut json, "results", clauses);
                json
            }
            ForgeQLIR::ShowMembers { symbol, clauses } => {
                let mut json = show::show_members(index, &workspace, symbol)
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
                apply_show_list_clauses(&mut json, "members", clauses);
                json
            }
            ForgeQLIR::ShowBody { symbol, clauses } => {
                show::show_body(index, &workspace, symbol, Some(clauses.depth.unwrap_or(0)))
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowCallees { symbol, .. } => show::show_callees(index, &workspace, symbol)
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::ShowLines {
                file,
                start_line,
                end_line,
                ..
            } => show::show_lines(&workspace, file, *start_line, *end_line)
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::FindFiles { clauses } => {
                let glob = clauses.in_glob.as_deref().unwrap_or("**");
                let results = query::find_files(&workspace, glob, clauses.exclude_glob.as_deref());
                let max_depth = clauses.depth.unwrap_or(0);
                let grouped = query::group_files_by_depth(&results, max_depth);
                let count = grouped.len();
                serde_json::json!({
                    "op":      "find_files",
                    "glob":    glob,
                    "depth":   max_depth,
                    "results": grouped,
                    "count":   count,
                })
            }
            other => serde_json::json!({ "error": format!("not a show op: {other:?}") }),
        };

        // Check for error responses.
        if let Some(err) = json.get("error").and_then(|v| v.as_str()) {
            bail!("{err}");
        }

        // Convert the JSON value to a typed ShowResult.
        let show_result = convert_show_json(op, &json)?;
        Ok(ForgeQLResult::Show(show_result))
    }

    // ===================================================================
    // Mutations
    // ===================================================================

    /// Handle a single mutation: plan → apply → reindex.
    fn exec_mutation(&mut self, session_id: Option<&str>, op: &ForgeQLIR) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;

        let plan = {
            let (workspace, index) = self.require_workspace_and_index(session_id)?;
            plan_from_ir(op, &RequestContext::admin(), &workspace, index)?
        };

        let op_name = mutation_op_name(op);
        let files_changed: Vec<PathBuf> =
            plan.file_edits.iter().map(|fe| fe.path.clone()).collect();
        let edit_count = plan.edit_count();
        let suggestions = convert_suggestions(&plan);

        let _ = plan.apply()?;

        // Reindex touched files.
        self.reindex_session(sid, &files_changed);

        Ok(ForgeQLResult::Mutation(MutationResult {
            op: op_name.to_string(),
            applied: true,
            files_changed,
            edit_count,
            diff: None,
            suggestions,
        }))
    }

    // ===================================================================
    // Composite operations
    // ===================================================================

    /// `BEGIN TRANSACTION 'name' ... COMMIT MESSAGE '...'`
    ///
    /// Plans all inner ops, applies atomically, reindexes, and optionally
    /// commits to git.
    fn exec_transaction(
        &mut self,
        session_id: Option<&str>,
        name: &str,
        ops: &[ForgeQLIR],
        verify: Option<&str>,
        message: Option<&str>,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;

        // Step 1: Plan all ops (pure, no I/O beyond reading source files).
        let (combined_plan, worktree_path) = {
            let (workspace, index) = self.require_workspace_and_index(session_id)?;
            let ctx = RequestContext::admin();
            let mut combined = TransformPlan::default();
            for op in ops {
                let plan = plan_from_ir(op, &ctx, &workspace, index)?;
                combined.file_edits.extend(plan.file_edits);
            }
            let wt_path = self.require_session(sid)?.worktree_path.clone();
            (combined, wt_path)
        };

        let files_changed: Vec<PathBuf> = combined_plan
            .file_edits
            .iter()
            .map(|fe| fe.path.clone())
            .collect();

        // Step 2: Apply all file edits atomically.
        let apply_result = combined_plan.apply()?;
        // Clone originals now — needed for the session rollback slot after apply.
        let originals = apply_result.originals.clone();

        // Step 3: Run VERIFY if requested.
        // `verify::run_step` consumes `apply_result` and calls rollback on failure.
        if let Some(step_name) = verify {
            let step = ForgeConfig::find(&worktree_path)
                .and_then(|p| ForgeConfig::load(&p).ok())
                .and_then(|cfg| cfg.verify_steps.into_iter().find(|s| s.name == step_name))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "VERIFY step '{step_name}' not found in .forgeql.yaml \u{2014} add it under verify_steps:"
                    )
                })?;
            if verify::run_step(&step, apply_result).is_err() {
                // Files already rolled back by run_step; discard staged originals.
                return Ok(ForgeQLResult::Transaction(TransactionResult {
                    name: name.to_string(),
                    committed: false,
                    steps: Vec::new(),
                    commit_hash: None,
                    message: message.map(String::from),
                    verify_step: Some(step_name.to_string()),
                    verified: Some(false),
                }));
            }
        } else {
            // No verify — release apply_result (files stay modified on disk).
            drop(apply_result);
        }

        // Step 4: Reindex touched files.
        self.reindex_session(sid, &files_changed);

        // Step 5: Store originals in session for a subsequent ROLLBACK command.
        if let Some(session) = self.sessions.get_mut(sid) {
            session.last_rollback_data = Some(originals);
        }

        // Step 6: Git commit (only when COMMIT MESSAGE clause is present).
        let mut commit_hash = None;
        if let Some(msg) = message {
            match git::open(&worktree_path) {
                Err(err) => {
                    warn!(error = %err, "transaction: transforms applied but git open failed");
                }
                Ok(repo) => {
                    match git::stage_paths_and_commit(&repo, &worktree_path, &files_changed, msg) {
                        Ok(oid) => {
                            commit_hash = Some(oid);
                        }
                        Err(err) => {
                            warn!(error = %err, "transaction: transforms applied but git commit failed");
                        }
                    }
                }
            }
        }

        Ok(ForgeQLResult::Transaction(TransactionResult {
            name: name.to_string(),
            committed: commit_hash.is_some(),
            steps: Vec::new(),
            commit_hash,
            message: message.map(String::from),
            verify_step: verify.map(String::from),
            verified: verify.map(|_| true),
        }))
    }

    // ===================================================================
    // Session lifecycle helpers
    // ===================================================================

    /// Undo the last applied transaction in the session by restoring the
    /// original file bytes saved in `session.last_rollback_data`.
    ///
    /// # Errors
    /// Returns `Err` if the session has no rollback data (no transaction
    /// has been applied yet) or if any file write fails.
    fn exec_rollback(
        &mut self,
        session_id: Option<&str>,
        name: Option<&str>,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;

        // Take rollback data (releases borrow so we can call reindex_session after).
        let originals = {
            let session = self
                .sessions
                .get_mut(sid)
                .ok_or_else(|| anyhow::anyhow!("session '{sid}' not found"))?;
            session.last_rollback_data.take()
        }
        .ok_or_else(|| {
            anyhow::anyhow!(
            "no rollback data available \u{2014} no transaction has been applied in this session"
        )
        })?;

        let files_restored: Vec<PathBuf> = originals.keys().cloned().collect();

        for (path, bytes) in &originals {
            crate::workspace::file_io::write_atomic(path, bytes)?;
        }

        self.reindex_session(sid, &files_restored);

        Ok(ForgeQLResult::Rollback(RollbackResult {
            name: name.unwrap_or("last").to_string(),
            files_restored,
        }))
    }

    /// Evict sessions that have been idle longer than `SESSION_TTL_SECS`.
    ///
    /// Call this periodically from the transport layer (e.g. a background
    /// tokio task every 5 minutes).
    // The branching for worktree + branch cleanup is inherent to the eviction logic.
    #[allow(clippy::cognitive_complexity)]
    pub fn evict_idle_sessions(&mut self) {
        let expired_ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.idle_secs() > SESSION_TTL_SECS)
            .map(|(id, _)| id.clone())
            .collect();

        for id in expired_ids {
            if let Some(session) = self.sessions.remove(&id) {
                info!(%id, "TTL eviction: removing idle session");
                let repo_path = self.data_dir.join(format!("{}.git", session.source_name));
                if let Err(err) = worktree::remove(&repo_path, &session.worktree_name) {
                    warn!(
                        worktree = %session.worktree_name,
                        error = %err,
                        "TTL eviction: worktree remove failed"
                    );
                }
                // Named branches (from USE … AS) are kept for review.
                if session.custom_branch.is_none() {
                    if let Err(err) =
                        worktree::delete_session_branch(&repo_path, &session.worktree_name)
                    {
                        warn!(
                            worktree = %session.worktree_name,
                            error = %err,
                            "TTL eviction: branch delete failed"
                        );
                    }
                }
            }
        }
    }

    /// Remove orphaned worktrees left over from a previous engine run.
    ///
    /// Called automatically by `new()`.  Scans `<data_dir>/worktrees/` and
    /// removes directories not belonging to any live session.
    #[allow(clippy::cognitive_complexity)]
    /// Prune worktrees whose session IDs are not in the live `sessions` map.
    ///
    /// Call this in long-lived service modes (MCP server) where an orphaned
    /// worktree directory means the session is truly gone.  **Do not** call
    /// in CLI modes — worktrees persist across invocations and legitimate
    /// sessions would be destroyed.
    pub fn prune_orphaned_worktrees(&self) {
        let wt_dir = self.data_dir.join("worktrees");
        let live_ids: Vec<&str> = self.sessions.keys().map(String::as_str).collect();

        // Pass 1: checkout directories still present under data/worktrees/.
        if let Ok(entries) = std::fs::read_dir(&wt_dir) {
            for entry in entries.flatten() {
                let session_id = entry.file_name().to_string_lossy().to_string();
                if live_ids.contains(&session_id.as_str()) {
                    continue;
                }
                info!(%session_id, "startup: pruning orphaned worktree");
                if let Ok(repo_entries) = std::fs::read_dir(&self.data_dir) {
                    for re in repo_entries.flatten() {
                        let rpath = re.path();
                        if rpath.extension().is_some_and(|ext| ext == "git") {
                            if let Err(err) = worktree::remove(&rpath, &session_id) {
                                warn!(%session_id, repo = %rpath.display(), error = %err, "git prune failed");
                            }
                            if let Err(err) = worktree::delete_session_branch(&rpath, &session_id) {
                                warn!(%session_id, repo = %rpath.display(), error = %err, "branch delete failed");
                            }
                        }
                    }
                }
                let path = entry.path();
                if path.exists() {
                    if let Err(err) = std::fs::remove_dir_all(&path) {
                        warn!(%session_id, path = %path.display(), error = %err, "remove_dir_all failed");
                    }
                }
            }
        }

        // Pass 2: git worktree metadata entries whose checkout path is gone.
        let Ok(repo_entries) = std::fs::read_dir(&self.data_dir) else {
            return;
        };
        for re in repo_entries.flatten() {
            let rpath = re.path();
            if rpath.extension().is_none_or(|ext| ext != "git") {
                continue;
            }
            let Ok(wts) = worktree::list(&rpath) else {
                continue;
            };
            for wt in wts {
                if live_ids.contains(&wt.name.as_str()) {
                    continue;
                }
                if !wt.path.exists() {
                    info!(session_id = %wt.name, "startup: pruning stale git worktree metadata");
                    if let Err(err) = worktree::remove(&rpath, &wt.name) {
                        warn!(session_id = %wt.name, error = %err, "stale metadata prune failed");
                    }
                    if let Err(err) = worktree::delete_session_branch(&rpath, &wt.name) {
                        warn!(session_id = %wt.name, error = %err, "stale branch delete failed");
                    }
                }
            }
        }
    }

    // ===================================================================
    // Internal helpers
    // ===================================================================

    /// Resolve `session_id` to a reference to the session's `SymbolTable`.
    ///
    /// # Errors
    /// Returns `Err` if the session is not found or the index is not ready.
    fn require_index(&self, session_id: Option<&str>) -> Result<&SymbolTable> {
        let session = self.require_session(require_session_id(session_id)?)?;
        session
            .index()
            .ok_or_else(|| anyhow::anyhow!("session index not ready — retry USE"))
    }

    /// Resolve `session_id` to a `Workspace` + `SymbolTable` pair.
    ///
    /// # Errors
    /// Returns `Err` if the session is not found, the index is not ready,
    /// or the workspace cannot be created.
    fn require_workspace_and_index(
        &self,
        session_id: Option<&str>,
    ) -> Result<(Workspace, &SymbolTable)> {
        let session = self.require_session(require_session_id(session_id)?)?;
        let index = session
            .index()
            .ok_or_else(|| anyhow::anyhow!("session index not ready — retry USE"))?;
        let workspace = Workspace::new(&session.worktree_path)?;
        Ok((workspace, index))
    }

    /// Look up a session by ID.
    ///
    /// # Errors
    /// Returns `Err` if no session with this ID exists.
    fn require_session(&self, session_id: &str) -> Result<&Session> {
        self.sessions.get(session_id).ok_or_else(|| {
            anyhow::anyhow!("session '{session_id}' not found — run USE <source>.<branch> first")
        })
    }

    /// Incrementally re-index the given files in a session after a mutation.
    ///
    /// Errors are logged but never propagated — re-indexing is best-effort.
    fn reindex_session(&mut self, session_id: &str, paths: &[PathBuf]) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            if let Err(err) = session.reindex_files(paths) {
                warn!(
                    session = %session_id,
                    error = %err,
                    "reindex after mutation failed"
                );
            }
        }
    }

    // ===================================================================
    // Test helpers (available only in test builds)
    // ===================================================================

    /// Register a local session pointing at an arbitrary directory.
    ///
    /// This bypasses git (no bare repo, no worktree creation) and builds a
    /// fresh `SymbolTable` from the files in `workspace_root`.  Useful for
    /// integration tests that just need an engine with an indexed session.
    ///
    /// Returns the auto-generated session ID.
    ///
    /// # Errors
    /// Returns `Err` if the workspace cannot be created or indexing fails.
    #[cfg(feature = "test-helpers")]
    pub fn register_local_session(&mut self, workspace_root: &Path) -> Result<String> {
        let session_id = generate_session_id();
        let mut session = Session::new(
            &session_id,
            "test-user",
            workspace_root.to_path_buf(),
            "local", // synthetic source name
            "main",  // synthetic branch name
        );
        session.build_index()?;

        let sid = session_id.clone();
        drop(self.sessions.insert(session_id, session));
        Ok(sid)
    }
}

// -----------------------------------------------------------------------
// Free functions (module-private helpers)
// -----------------------------------------------------------------------

/// Generate a time-based session ID.
fn generate_session_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("s{millis}")
}

/// Extract the `session_id` from `Option<&str>`, failing if absent or empty.
#[allow(clippy::missing_const_for_fn)] // bail! prevents const
fn require_session_id(session_id: Option<&str>) -> Result<&str> {
    match session_id {
        Some(sid) if !sid.is_empty() => Ok(sid),
        _ => bail!("session_id required — run USE <source>.<branch> first"),
    }
}

/// Determine the operation name for a mutation `ForgeQLIR` variant.
const fn mutation_op_name(op: &ForgeQLIR) -> &'static str {
    match op {
        ForgeQLIR::ChangeContent { .. } => "change_content",
        _ => "unknown_mutation",
    }
}

/// Validate the `ORDER BY` field against a result set, returning an error if
/// the field is not recognised for any item.
///
/// This runs before `apply_clauses` (against the full unfiltered set) so that
/// callers get a clear diagnostic instead of silently receiving default order.
/// Static built-in fields (`name`, `kind`, `node_kind`, `path`, `file`,
/// `line`, `usages`, `count`) are always accepted even when the result set is
/// empty.  Unknown dynamic fields are accepted when at least one item carries
/// them.
fn validate_order_by_field(
    clauses: &Clauses,
    results: &[crate::result::SymbolMatch],
) -> Result<()> {
    use crate::filter::ClauseTarget as _;

    const STATIC_FIELDS: &[&str] = &[
        "name",
        "kind",
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

/// Convert `TransformPlan` suggestions into typed `SuggestionEntry` values.
fn convert_suggestions(plan: &TransformPlan) -> Vec<SuggestionEntry> {
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
fn convert_show_json(op: &ForgeQLIR, json: &serde_json::Value) -> Result<ShowResult> {
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

    Ok(ShowResult {
        op: op_name,
        symbol,
        file,
        content,
        start_line,
        end_line,
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
                                kind: entry.get("kind")?.as_str()?.to_string(),
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
                                kind: m.get("kind")?.as_str()?.to_string(),
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
                            Some(FileEntry {
                                path: PathBuf::from(path_str),
                                depth: clauses.depth,
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
    use crate::ir::Clauses;

    #[test]
    fn generate_session_id_starts_with_s() {
        let id = generate_session_id();
        assert!(id.starts_with('s'), "session ID must start with 's': {id}");
    }

    #[test]
    fn generate_session_id_unique() {
        let id1 = generate_session_id();
        // Wait 1 ms to ensure different timestamp.
        std::thread::sleep(std::time::Duration::from_millis(1));
        let id2 = generate_session_id();
        assert_ne!(id1, id2, "consecutive session IDs should differ");
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
        let engine = ForgeQLEngine::new(data_dir.clone()).unwrap();
        assert!(data_dir.join("worktrees").exists());
        assert_eq!(engine.session_count(), 0);
        assert_eq!(engine.source_count(), 0);
        assert_eq!(engine.commands_served(), 0);
    }

    #[test]
    fn engine_show_sources_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf()).unwrap();
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
    fn engine_show_branches_requires_source() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf()).unwrap();
        let result = engine.execute(None, &ForgeQLIR::ShowBranches { source: None });
        assert!(result.is_err());
    }

    #[test]
    fn engine_disconnect_without_session_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf()).unwrap();
        let result = engine.execute(None, &ForgeQLIR::Disconnect);
        assert!(result.is_err());
    }

    #[test]
    fn engine_disconnect_unknown_session_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf()).unwrap();
        let result = engine.execute(Some("s_unknown"), &ForgeQLIR::Disconnect);
        assert!(result.is_err());
    }

    #[test]
    fn engine_find_symbols_without_session_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf()).unwrap();
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
            "kind",
            "node_kind",
            "path",
            "file",
            "line",
            "usages",
            "count",
        ] {
            assert!(
                validate_order_by_field(&clauses_with_order(field), &results).is_ok(),
                "expected Ok for ORDER BY {field}"
            );
        }
    }

    #[test]
    fn validate_order_by_field_rejects_unknown_field() {
        let results = vec![make_sym("foo"), make_sym("bar")];
        let err = validate_order_by_field(&clauses_with_order("invalid_field"), &results);
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
        assert!(validate_order_by_field(&clauses_with_order("signature"), &results).is_ok());
    }

    #[test]
    fn validate_order_by_field_ok_when_results_empty() {
        let results: Vec<crate::result::SymbolMatch> = Vec::new();
        // Should not error even for unknown field when result set is empty.
        assert!(validate_order_by_field(&clauses_with_order("unknown_xyz"), &results).is_ok());
    }

    #[test]
    fn validate_order_by_field_no_order_by_always_ok() {
        let results = vec![make_sym("foo")];
        assert!(validate_order_by_field(&Clauses::default(), &results).is_ok());
    }

    /// Regression test for Bug #10: static file-scope variables must NOT
    /// appear in `FIND globals` results.
    ///
    /// `motor_control.cpp` declares several `static` variables at file scope
    /// (`motorPrincipal`, `motorSecundario`, `gCallbackEncendido`, `kMotorLabel`).
    /// Before the fix, their `is_global` flag was incorrectly set to `true`
    /// because the indexer only filtered out `extern`, not `static`.
    #[cfg(feature = "test-helpers")]
    #[test]
    fn find_globals_excludes_static_variables() {
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
        let mut engine = ForgeQLEngine::new(data_dir).unwrap();
        let session_id = engine.register_local_session(tmp.path()).unwrap();

        let op = ForgeQLIR::FindSymbols {
            clauses: Clauses::default(),
        };
        let result = engine.execute(Some(&session_id), &op).unwrap();
        let names: Vec<String> = match result {
            ForgeQLResult::Query(qr) => qr.results.into_iter().map(|r| r.name).collect(),
            other => panic!("expected Query, got: {other:?}"),
        };

        // These are declared `static` — internal linkage, not globals.
        let statics = [
            "motorPrincipal",
            "motorSecundario",
            "gCallbackEncendido",
            "kMotorLabel",
        ];
        for s in &statics {
            assert!(
                !names.contains(&s.to_string()),
                "static variable '{s}' must not appear in FIND globals results; got: {names:?}"
            );
        }
    }
}
