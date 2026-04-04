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
            } => self.use_source(source, branch, as_branch.as_deref()),
            ForgeQLIR::ShowSources => self.show_sources(),
            ForgeQLIR::ShowBranches => self.show_branches(session_id),
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
        // source + branch + as_branch combination, reuse it — unless the
        // branch HEAD in the bare repo has moved (e.g. after REFRESH SOURCE),
        // in which case evict the stale session and fall through to create a
        // fresh one.
        //
        // We collect the decision into `resume_outcome` before mutating
        // `self.sessions` to avoid holding a shared borrow across a mutable one.
        let resume_outcome: Option<(String, Option<usize>)> = {
            if let Some((existing_id, existing_session)) = self.sessions.iter().find(|(_, s)| {
                s.source_name == source_name
                    && as_branch.map_or_else(
                        || s.branch == branch && s.custom_branch.is_none(),
                        |ab| s.custom_branch.as_deref() == Some(ab),
                    )
            }) {
                // Compare the bare repo's current branch tip to what we
                // indexed.  If `branch_head` returns None (repo unavailable
                // or branch missing) we treat the session as fresh to avoid
                // spurious evictions.
                let is_stale = self
                    .registry
                    .get(source_name)
                    .and_then(|src| git::branch_head(src.path(), branch))
                    .is_some_and(|head| {
                        existing_session.cached_commit().is_some_and(|c| c != head)
                    });
                if is_stale {
                    info!(
                        session_id = %existing_id,
                        %source_name,
                        %branch,
                        "branch HEAD moved after REFRESH — evicting stale session"
                    );
                    Some((existing_id.clone(), None))
                } else {
                    let symbols_indexed = existing_session.index().map_or(0, |idx| idx.rows.len());
                    info!(
                        session_id = %existing_id,
                        %source_name,
                        %branch,
                        "session resume — reusing existing in-memory session"
                    );
                    Some((existing_id.clone(), Some(symbols_indexed)))
                }
            } else {
                None
            }
        };
        match resume_outcome {
            Some((id, Some(symbols_indexed))) => {
                return Ok(ForgeQLResult::SourceOp(SourceOpResult {
                    op: "use_source".to_string(),
                    source_name: Some(source_name.to_string()),
                    session_id: Some(id),
                    branches: Vec::new(),
                    symbols_indexed: Some(symbols_indexed),
                    resumed: true,
                    message: as_branch.map(|ab| format!("as_branch: {ab}")),
                }));
            }
            Some((stale_id, None)) => {
                drop(self.sessions.remove(&stale_id));
                // Fall through to create a new session at the updated HEAD.
            }
            None => {
                // No existing session — fall through to create one.
            }
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

        let mut session = Session::new(
            &session_id,
            "anonymous",
            wt_path,
            source_name,
            branch,
            Arc::clone(&self.lang_registry),
        );
        session.custom_branch = as_branch.map(String::from);
        session.worktree_name = wt_name;

        // Use resume_index() so an existing disk cache at
        // <worktree>/.forgeql-index is reused when HEAD matches.
        session.resume_index()?;

        // Freeze verify config at session start — sidecar takes priority over in-repo file.
        // Any later CHANGE has no effect on VERIFY; steps are captured once here.
        if let Some((workdir, config)) =
            load_verify_config(&repo_path, source_name, &session.worktree_path)
        {
            session.frozen_workdir = Some(workdir);
            session.frozen_verify_steps = Some(config.verify_steps);
        }

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
        // Determine the session branch name (custom or auto-generated).
        let auto_branch = format!("forgeql/{wt_name}");
        let session_branch = custom_branch.as_deref().unwrap_or(&auto_branch);

        // Delete the branch if it contains no real source changes compared
        // to the base branch (ignoring ForgeQL control files).
        let disconnect_msg = match git::source_changes(&repo_path, &session.branch, session_branch)
        {
            Ok(changed) if changed.is_empty() => {
                if let Err(err) = worktree::delete_branch(&repo_path, session_branch) {
                    warn!(%session_branch, error = %err, "disconnect: branch delete failed");
                }
                info!(%session_branch, "deleted branch — no source changes");
                format!(
                    "branch {session_branch} deleted (no source changes vs {})",
                    session.branch
                )
            }
            Ok(changed) => {
                info!(%session_branch, files = ?changed, "keeping branch — has source changes");
                format!(
                    "branch {session_branch} kept — {} changed file(s): {}",
                    changed.len(),
                    changed.join(", ")
                )
            }
            Err(err) => {
                warn!(%session_branch, error = %err, "disconnect: could not diff trees, keeping branch");
                format!("branch {session_branch} kept — diff error: {err}")
            }
        };

        info!(%sid, "session disconnected and cleaned up");
        Ok(ForgeQLResult::SourceOp(SourceOpResult {
            op: "disconnect".to_string(),
            source_name: None,
            session_id: Some(sid.to_string()),
            branches: Vec::new(),
            symbols_indexed: None,
            resumed: false,
            message: Some(disconnect_msg),
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
                    fql_kind: None,
                    language: None,
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
            metric_hint: None,
        }))
    }

    /// `SHOW BRANCHES [OF 'source']` — list branches of a source.
    fn show_branches(&self, session_id: Option<&str>) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let session = self.require_session(sid)?;
        let source_name = session.source_name.clone();

        let source_ref = self
            .registry
            .get(&source_name)
            .ok_or_else(|| anyhow::anyhow!("source {source_name} not found"))?;
        let branches = source_ref.branches().unwrap_or_default();

        Ok(ForgeQLResult::SourceOp(SourceOpResult {
            op: "show_branches".to_string(),
            source_name: Some(source_name),
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
        let sid = require_session_id(session_id)?;
        let session = self.require_session(sid)?;
        let index = session
            .index()
            .ok_or_else(|| anyhow::anyhow!("session index not ready — retry USE"))?;
        let root = &session.worktree_path;

        let configs = self.lang_registry.configs();
        let (mut results, remaining) = find_symbols_prefilter(index, clauses, root, &configs);

        validate_order_by_field(&remaining, &results)?;
        crate::filter::apply_clauses(&mut results, &remaining);

        let total = results.len();
        if clauses.limit.is_none() {
            results.truncate(DEFAULT_QUERY_LIMIT);
        }

        let metric_hint = detect_metric_hint(clauses);

        Ok(ForgeQLResult::Query(QueryResult {
            op: "find_symbols".to_string(),
            results,
            total,
            metric_hint,
        }))
    }

    /// `FIND usages OF 'symbol' ...`
    fn find_usages(
        &self,
        session_id: Option<&str>,
        of: &str,
        clauses: &Clauses,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let session = self.require_session(sid)?;
        let index = session
            .index()
            .ok_or_else(|| anyhow::anyhow!("session index not ready — retry USE"))?;
        let root = &session.worktree_path;

        let sites = query::find_usages(index, of);
        let mut results: Vec<SymbolMatch> = sites
            .iter()
            .filter(|site| {
                if let Some(ref glob) = clauses.in_glob
                    && !crate::ast::query::relative_glob_matches(&site.path, glob, root)
                {
                    return false;
                }
                if let Some(ref glob) = clauses.exclude_glob
                    && crate::ast::query::relative_glob_matches(&site.path, glob, root)
                {
                    return false;
                }
                true
            })
            .map(|site| SymbolMatch {
                name: of.to_string(),
                node_kind: None,
                fql_kind: None,
                language: None,
                path: Some(site.path.clone()),
                line: Some(site.line),
                usages_count: None,
                fields: std::collections::HashMap::new(),
                count: None,
            })
            .collect();

        // Strip IN/EXCLUDE from clauses — already applied above.
        let remaining = Clauses {
            in_glob: None,
            exclude_glob: None,
            ..clauses.clone()
        };

        validate_order_by_field(&remaining, &results)?;

        crate::filter::apply_clauses(&mut results, &remaining);
        let total = results.len();
        if clauses.limit.is_none() {
            results.truncate(DEFAULT_QUERY_LIMIT);
        }

        Ok(ForgeQLResult::Query(QueryResult {
            op: "find_usages".to_string(),
            results,
            total,
            metric_hint: None,
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
    #[allow(clippy::too_many_lines)]
    fn exec_show(&self, session_id: Option<&str>, op: &ForgeQLIR) -> Result<ForgeQLResult> {
        let (workspace, index) = self.require_workspace_and_index(session_id)?;
        let root = workspace.root();

        let json = match op {
            ForgeQLIR::ShowContext { symbol, clauses } => {
                let context_lines = clauses.depth.unwrap_or(DEFAULT_CONTEXT_LINES);
                resolve_symbol(index, symbol, clauses, root)
                    .and_then(|def| show::show_context(def, &workspace, symbol, context_lines))
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowSignature { symbol, clauses } => {
                resolve_symbol(index, symbol, clauses, root)
                    .and_then(|def| {
                        show::show_signature(def, &workspace, symbol, &self.lang_registry)
                    })
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowOutline { file, .. } => show::show_outline(index, &workspace, file)
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::ShowMembers { symbol, clauses } => {
                resolve_symbol(index, symbol, clauses, root)
                    .and_then(|def| {
                        show::show_members(def, &workspace, symbol, &self.lang_registry)
                    })
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowBody { symbol, clauses } => {
                resolve_body_symbol(index, symbol, clauses, root)
                    .and_then(|def| {
                        show::show_body(
                            def,
                            &workspace,
                            symbol,
                            Some(clauses.depth.unwrap_or(DEFAULT_BODY_DEPTH)),
                            &self.lang_registry,
                        )
                    })
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowCallees { symbol, clauses } => {
                resolve_body_symbol(index, symbol, clauses, root)
                    .and_then(|def| {
                        show::show_callees(def, index, &workspace, symbol, &self.lang_registry)
                    })
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowLines {
                file,
                start_line,
                end_line,
                ..
            } => show::show_lines(&workspace, file, *start_line, *end_line)
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::FindFiles { clauses } => {
                let glob = clauses.in_glob.as_deref().unwrap_or("**");
                // IN / EXCLUDE are applied by find_files(); build typed entries
                // so the full clause pipeline (WHERE, GROUP BY, HAVING, ORDER BY,
                // LIMIT, OFFSET) can run against individual file rows.
                let raw = query::find_files(&workspace, glob, clauses.exclude_glob.as_deref());
                let mut entries: Vec<FileEntry> = raw
                    .iter()
                    .filter_map(|v| {
                        let path = v.get("path").and_then(|p| p.as_str()).map(PathBuf::from)?;
                        let extension = v
                            .get("extension")
                            .and_then(|e| e.as_str())
                            .unwrap_or("")
                            .to_string();
                        let size = v
                            .get("size")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0);
                        Some(FileEntry {
                            path,
                            extension,
                            size,
                            depth: None,
                            count: None,
                        })
                    })
                    .collect();
                // Apply the full clause pipeline.  IN / EXCLUDE are already
                // handled above so they become no-ops here; GROUP BY, HAVING,
                // WHERE, ORDER BY, LIMIT, OFFSET all run normally.
                crate::filter::apply_clauses(&mut entries, clauses);
                let max_depth = clauses.depth.unwrap_or(DEFAULT_BODY_DEPTH);
                // When GROUP BY was requested the pipeline has already
                // aggregated entries and stored per-group counts; skip the
                // depth-grouping step so those results are not disturbed.
                let results: Vec<serde_json::Value> = if clauses.group_by.is_some() {
                    entries
                        .iter()
                        .map(|fe| {
                            let mut obj = serde_json::json!({
                                "path":      fe.path.display().to_string(),
                                "extension": fe.extension,
                                "size":      fe.size,
                            });
                            if let Some(n) = fe.count {
                                obj["count"] = serde_json::Value::from(n);
                            }
                            obj
                        })
                        .collect()
                } else {
                    let file_json: Vec<serde_json::Value> = entries
                        .iter()
                        .map(|fe| {
                            serde_json::json!({
                                "path":      fe.path.display().to_string(),
                                "extension": fe.extension,
                                "size":      fe.size,
                            })
                        })
                        .collect();
                    query::group_files_by_depth(&file_json, max_depth)
                };
                let count = results.len();
                serde_json::json!({
                    "op":      "find_files",
                    "glob":    glob,
                    "depth":   max_depth,
                    "results": results,
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
        let mut show_result = convert_show_json(op, &json)?;

        // Apply the full clause pipeline (WHERE, ORDER BY, LIMIT, OFFSET, …)
        // to structured list results: outline, members, and call graph entries.
        match (&mut show_result.content, op) {
            (ShowContent::Outline { entries }, ForgeQLIR::ShowOutline { clauses, .. }) => {
                crate::filter::apply_clauses(entries, clauses);
            }
            (ShowContent::Members { members, .. }, ForgeQLIR::ShowMembers { clauses, .. }) => {
                crate::filter::apply_clauses(members, clauses);
            }
            (ShowContent::CallGraph { entries, .. }, ForgeQLIR::ShowCallees { clauses, .. }) => {
                crate::filter::apply_clauses(entries, clauses);
            }
            _ => {}
        }

        // Apply WHERE predicates to source-line results BEFORE the line cap.
        // This lets queries like `SHOW body OF 'fn' WHERE text MATCHES 'TODO'`
        // filter over the full function body, not just the first N lines.
        if let ShowContent::Lines { lines, .. } = &mut show_result.content {
            let clauses = match op {
                ForgeQLIR::ShowBody { clauses, .. }
                | ForgeQLIR::ShowLines { clauses, .. }
                | ForgeQLIR::ShowContext { clauses, .. } => Some(clauses),
                _ => None,
            };
            if let Some(clauses) = clauses {
                // Apply only WHERE predicates here; LIMIT/OFFSET are handled
                // by the line-cap logic below.
                for predicate in &clauses.where_predicates {
                    let pred = predicate.clone();
                    lines.retain(|line| crate::filter::eval_predicate(line, &pred));
                }
            }
        }

        // ----------------------------------------------------------
        // Implicit line cap for SHOW commands that return source lines.
        // When no explicit LIMIT was given, truncate to DEFAULT_SHOW_LINE_LIMIT
        // and attach a hint explaining how to paginate.
        // When LIMIT was given, honour OFFSET + LIMIT as pagination.
        // ----------------------------------------------------------
        if let ShowContent::Lines { lines, .. } = &mut show_result.content {
            let clauses = match op {
                ForgeQLIR::ShowBody { clauses, .. }
                | ForgeQLIR::ShowLines { clauses, .. }
                | ForgeQLIR::ShowContext { clauses, .. } => Some(clauses),
                _ => None,
            };
            if let Some(clauses) = clauses {
                let total = lines.len();
                let has_explicit_limit = clauses.limit.is_some();

                if has_explicit_limit {
                    // Agent explicitly requested LIMIT — honour it.
                    let offset = clauses.offset.unwrap_or(0);
                    if offset > 0 && offset < total {
                        *lines = lines.split_off(offset);
                    } else if offset >= total {
                        lines.clear();
                    }
                    let limit = clauses.limit.unwrap_or(total);
                    if lines.len() > limit {
                        lines.truncate(limit);
                    }
                } else if total > DEFAULT_SHOW_LINE_LIMIT {
                    // No explicit LIMIT and output exceeds the cap.
                    // Block the output entirely — return zero lines + guidance.
                    lines.clear();
                    show_result.total_lines = Some(total);
                    show_result.hint = Some(format!(
                        "Blocked: this SHOW command would return {total} lines \
                         (limit is {DEFAULT_SHOW_LINE_LIMIT} without an explicit LIMIT clause). \
                         Use FIND symbols WHERE to locate the exact symbol you need — \
                         it returns file path and line numbers. \
                         Then use SHOW LINES n-m OF 'file' to read only those lines. \
                         If you really need all {total} lines, re-run with LIMIT {total}.",
                    ));
                }
            }
        }

        Ok(ForgeQLResult::Show(show_result))
    }

    // ===================================================================
    // Mutations
    // ===================================================================

    /// Handle a single mutation: plan → diff → apply → reindex.
    fn exec_mutation(&mut self, session_id: Option<&str>, op: &ForgeQLIR) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;

        let mut plan = {
            let (workspace, index) = self.require_workspace_and_index(session_id)?;
            plan_from_ir(op, &RequestContext::admin(), &workspace, index)?
        };

        let op_name = mutation_op_name(op);
        let files_changed: Vec<PathBuf> =
            plan.file_edits.iter().map(|fe| fe.path.clone()).collect();
        let edit_count = plan.edit_count();
        let suggestions = convert_suggestions(&plan);

        // Merge before generating preview (compact_diff_plan reads files).
        plan.merge_by_file()?;

        // Generate a compact diff preview *before* applying (apply consumes
        // the plan). Bounded by CompactDiffConfig defaults — at most K
        // content lines per file, each ≤ W characters wide.
        let diff = match compact_diff_plan(&plan, &CompactDiffConfig::default()) {
            Ok(d) if d.is_empty() => None,
            Ok(d) => Some(d),
            Err(_) => None,
        };

        let _ = plan.apply()?;

        // Reindex touched files.
        self.reindex_session(sid, &files_changed);

        Ok(ForgeQLResult::Mutation(MutationResult {
            op: op_name.to_string(),
            applied: true,
            files_changed,
            edit_count,
            diff,
            suggestions,
        }))
    }

    // ===================================================================
    // COPY / MOVE lines
    // ===================================================================

    fn exec_copy_lines(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let (workspace, _index) = self.require_workspace_and_index(session_id)?;

        let (src, start, end, dst, at) = match op {
            ForgeQLIR::CopyLines {
                src,
                start,
                end,
                dst,
                at,
            } => (src.as_str(), *start, *end, dst.as_str(), *at),
            _ => bail!("exec_copy_lines called with wrong IR variant"),
        };

        let src_abs = workspace.root().join(src);
        let dst_abs = workspace.root().join(dst);

        let plan = match at {
            None => plan_copy_lines(src, &src_abs, start, end, &dst_abs)?,
            Some(at_line) => plan_copy_lines_at(src, &src_abs, start, end, &dst_abs, at_line)?,
        };

        self.apply_plan(sid, plan, "copy_lines")
    }

    fn exec_move_lines(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let (workspace, _index) = self.require_workspace_and_index(session_id)?;

        let (src, start, end, dst, at) = match op {
            ForgeQLIR::MoveLines {
                src,
                start,
                end,
                dst,
                at,
            } => (src.as_str(), *start, *end, dst.as_str(), *at),
            _ => bail!("exec_move_lines called with wrong IR variant"),
        };

        let src_abs = workspace.root().join(src);
        let dst_abs = workspace.root().join(dst);

        let plan = plan_move_lines(src, &src_abs, start, end, &dst_abs, at)?;
        self.apply_plan(sid, plan, "move_lines")
    }

    /// Shared plan → diff → apply → reindex helper used by COPY and MOVE.
    fn apply_plan(
        &mut self,
        sid: &str,
        mut plan: TransformPlan,
        op_name: &str,
    ) -> Result<ForgeQLResult> {
        let files_changed: Vec<PathBuf> =
            plan.file_edits.iter().map(|fe| fe.path.clone()).collect();
        let edit_count = plan.edit_count();

        plan.merge_by_file()?;

        let diff = match compact_diff_plan(&plan, &CompactDiffConfig::default()) {
            Ok(d) if d.is_empty() => None,
            Ok(d) => Some(d),
            Err(_) => None,
        };

        let _ = plan.apply()?;

        self.reindex_session(sid, &files_changed);

        Ok(ForgeQLResult::Mutation(MutationResult {
            op: op_name.to_string(),
            applied: true,
            files_changed,
            edit_count,
            diff,
            suggestions: Vec::new(),
        }))
    }
    // Checkpoint-based transactions
    // ===================================================================

    /// `BEGIN TRANSACTION 'name'` — create a named git checkpoint.
    ///
    /// Auto-commits any dirty working-tree state so the checkpoint OID
    /// always represents a complete snapshot.  The current HEAD before the
    /// checkpoint commit is recorded as `pre_txn_oid` so that `COMMIT` can
    /// squash back to a clean base.
    ///
    /// # Errors
    /// Returns `Err` if the session is missing, git open fails, or the
    /// internal savepoint commit fails.
    fn exec_begin_transaction(
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
    fn exec_commit(&mut self, session_id: Option<&str>, message: &str) -> Result<ForgeQLResult> {
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
    fn exec_rollback(
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

            // Reset last_clean_oid so the next COMMIT squashes from the
            // correct base (the state before this checkpoint existed).
            session.last_clean_oid = Some(checkpoint.pre_txn_oid.clone());

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
    fn exec_verify_build(
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
                if session.custom_branch.is_none()
                    && let Err(err) =
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
                // Honour the persisted last-active timestamp — skip worktrees
                // that were accessed within the TTL window, even though they
                // have no in-memory session (e.g. after a server restart or
                // short-lived CLI invocation).
                let wt_path = entry.path();
                if let Some(last_epoch) = read_last_active(&wt_path) {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    if now.saturating_sub(last_epoch) < SESSION_TTL_SECS {
                        debug!(%session_id, idle_secs = now.saturating_sub(last_epoch),
                               "startup: worktree still warm — skipping");
                        continue;
                    }
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
                if path.exists()
                    && let Err(err) = std::fs::remove_dir_all(&path)
                {
                    warn!(%session_id, path = %path.display(), error = %err, "remove_dir_all failed");
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

    /// Return the source name associated with the given session, if it exists.
    #[must_use]
    pub fn source_name_for_session(&self, session_id: &str) -> Option<&str> {
        self.sessions
            .get(session_id)
            .map(|s| s.source_name.as_str())
    }
    /// Incrementally re-index the given files in a session after a mutation.
    ///
    /// Errors are logged but never propagated — re-indexing is best-effort.
    fn reindex_session(&mut self, session_id: &str, paths: &[PathBuf]) {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        if let Err(err) = session.reindex_files(paths) {
            warn!(
                session = %session_id,
                error = %err,
                "reindex after mutation failed"
            );
            return;
        }
        if let Err(err) = session.save_index() {
            warn!(
                session = %session_id,
                error = %err,
                "index save after mutation failed"
            );
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
            Arc::clone(&self.lang_registry),
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

/// Load verify configuration for `source_name`, preferring an external sidecar
/// over the in-repo `.forgeql.yaml`.
///
/// **Sidecar path:** `<repo_dir>/<source_name>.forgeql.yaml` (no commit needed)\
/// **Fallback:** walk up from `worktree_path` looking for `.forgeql.yaml`
///
/// Returns `(workdir, config)` where `workdir` is the directory from which
/// VERIFY commands run — always the worktree root when the sidecar is used.
fn load_verify_config(
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

/// Detect the first numeric WHERE predicate on a non-core enrichment field.
///
/// Returns the field name (e.g. `"member_count"`, `"param_count"`) so the
/// compact renderer can show that value instead of `usages`.  Falls back
/// to `ORDER BY` field when no numeric WHERE is present.
fn detect_metric_hint(clauses: &Clauses) -> Option<String> {
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

/// Pre-filter symbol rows using secondary indexes and WHERE predicates
/// before materializing `SymbolMatch`.  Returns `(results, remaining_clauses)`
/// where `remaining_clauses` contains only the parts not yet applied.
#[allow(clippy::too_many_lines)]
fn find_symbols_prefilter(
    index: &SymbolTable,
    clauses: &Clauses,
    root: &std::path::Path,
    lang_configs: &[&crate::ast::lang::LanguageConfig],
) -> (Vec<SymbolMatch>, Clauses) {
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

    let is_usages_pred = |p: &crate::ir::Predicate| p.field == "usages";

    // When no explicit kind predicate, infer raw kind(s) from enrichment fields.
    // This lets us use the kind_index instead of a full scan.
    let inferred_kinds: Option<Vec<String>> = if fql_kind_exact.is_none() && kind_exact.is_none() {
        infer_kinds_from_fields(&clauses.where_predicates, lang_configs)
    } else {
        None
    };

    // Row source: fql_kind_index (universal) > kind_index (power-user) > inferred > full scan.
    let candidates: Box<dyn Iterator<Item = &crate::ast::index::IndexRow>> =
        if let Some(fql_kind) = fql_kind_exact {
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
    let non_usages_preds: Vec<_> = clauses
        .where_predicates
        .iter()
        .filter(|p| !is_usages_pred(p))
        .filter(|p| fql_kind_exact.is_none() || !(p.field == "fql_kind" && p.op == CompareOp::Eq))
        .filter(|p| kind_exact.is_none() || !(p.field == "node_kind" && p.op == CompareOp::Eq))
        .filter(|p| name_like.is_none() || !(p.field == "name" && p.op == CompareOp::Like))
        .collect();

    // Filter on raw IndexRow — no heap allocation per rejected row.
    let filtered = candidates.filter(|row| {
        if let Some(pat) = name_like
            && !like_match(&row.name, pat)
        {
            return false;
        }
        if let Some(ref glob) = clauses.in_glob
            && !crate::ast::query::relative_glob_matches(&row.path, glob, root)
        {
            return false;
        }
        if let Some(ref glob) = clauses.exclude_glob
            && crate::ast::query::relative_glob_matches(&row.path, glob, root)
        {
            return false;
        }
        non_usages_preds.iter().all(|p| eval_predicate(*row, p))
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
        let key = (&def.name, &def.path, &def.node_kind, def.line);
        if !seen.insert(key.to_owned()) {
            continue;
        }
        let usages_key = def.name.rsplit("::").next().unwrap_or(&def.name);
        let usages = index.usages.get(usages_key).map_or(0, Vec::len);
        results.push(SymbolMatch {
            name: def.name.clone(),
            node_kind: Some(def.node_kind.clone()),
            fql_kind: if def.fql_kind.is_empty() {
                None
            } else {
                Some(def.fql_kind.clone())
            },
            language: if def.language.is_empty() {
                None
            } else {
                Some(def.language.clone())
            },
            path: Some(def.path.clone()),
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

/// Resolve a symbol name to a single [`IndexRow`] using SHOW command clauses.
///
/// 1. Finds all definition rows matching `name` in the index.
/// 2. Filters by `IN`/`EXCLUDE` globs and `WHERE` predicates from `clauses`.
/// 3. If the surviving candidates span multiple languages, returns an error
///    asking the user to disambiguate with `WHERE language = '...'` or
///    `IN '*.ext'`.
/// 4. Returns the last matching row (preserving v1 last-write-wins semantics
///    within a single language).
fn resolve_symbol<'a>(
    index: &'a SymbolTable,
    name: &str,
    clauses: &Clauses,
    root: &Path,
) -> Result<&'a crate::ast::index::IndexRow> {
    use crate::filter::eval_predicate;

    let candidates = index.find_all_defs(name);
    if candidates.is_empty() {
        bail!("symbol '{name}' not found in index");
    }

    // Single candidate — fast path, skip filtering.
    if candidates.len() == 1 {
        return Ok(candidates[0]);
    }

    let filtered: Vec<&crate::ast::index::IndexRow> = candidates
        .into_iter()
        .filter(|row| {
            if let Some(ref glob) = clauses.in_glob
                && !crate::ast::query::relative_glob_matches(&row.path, glob, root)
            {
                return false;
            }
            if let Some(ref glob) = clauses.exclude_glob
                && crate::ast::query::relative_glob_matches(&row.path, glob, root)
            {
                return false;
            }
            clauses
                .where_predicates
                .iter()
                .all(|p| eval_predicate(*row, p))
        })
        .collect();

    if filtered.is_empty() {
        bail!("symbol '{name}' not found after applying WHERE/IN/EXCLUDE filters");
    }

    // Prefer actual definitions (non-empty fql_kind) over reference-only
    // index rows such as scoped_identifier / qualified_identifier nodes
    // that happen to share the bare name.
    let defs: Vec<&crate::ast::index::IndexRow> = filtered
        .iter()
        .copied()
        .filter(|row| !row.fql_kind.is_empty())
        .collect();
    let best = if defs.is_empty() { &filtered } else { &defs };

    // Check cross-language ambiguity.
    let mut languages: Vec<&str> = best
        .iter()
        .filter_map(|r| {
            if r.language.is_empty() {
                None
            } else {
                Some(r.language.as_str())
            }
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
fn resolve_body_symbol<'a>(
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
        // casts.rs
        "cast_style" | "cast_target_type" => Some(
            config
                .cast_kind_triples()
                .iter()
                .map(|(raw_kind, _, _)| raw_kind.clone())
                .collect(),
        ),
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
        // metrics.rs — qualifier flags
        "is_const" | "is_volatile" | "is_static" => {
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
        total_lines: None,
        hint: None,
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

    #[test]
    fn engine_disconnect_without_session_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
        let result = engine.execute(None, &ForgeQLIR::Disconnect);
        assert!(result.is_err());
    }

    #[test]
    fn engine_disconnect_unknown_session_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
        let result = engine.execute(Some("s_unknown"), &ForgeQLIR::Disconnect);
        assert!(result.is_err());
    }

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

        // FIND globals → FIND symbols WHERE node_kind = 'declaration'
        let op = crate::parser::parse("FIND globals LIMIT 200").unwrap();
        let result = engine.execute(Some(&session_id), &op[0]).unwrap();
        let results = match result {
            ForgeQLResult::Query(qr) => qr.results,
            other => panic!("expected Query, got: {other:?}"),
        };

        // All returned rows must be file-scope declaration nodes.
        for r in &results {
            assert_eq!(
                r.node_kind.as_deref(),
                Some("declaration"),
                "FIND globals must only return declaration nodes, got {:?} for '{}'",
                r.node_kind,
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
}
