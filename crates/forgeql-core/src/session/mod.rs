/// Per-session state — Phase B of the v2 architecture.
///
/// A `Session` ties together exactly one git worktree, one user identity,
/// and one `SymbolTable` (the index of the source tree checked out in that
/// worktree). Sessions are created when a user issues `USE source.branch`
/// and destroyed when the session ends.
///
/// Index caching follows a two-phase strategy:
///   1. On first use: build the full index and persist it to disk.
///   2. On resume: reload from disk if the HEAD commit hash matches;
///      otherwise fall back to a full rebuild.
///      (True incremental re-index is deferred to Phase D.)
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, info};

use crate::ast::cache::CachedIndex;
use crate::ast::enrich::macro_table::MacroTable;
use crate::ast::index::SymbolTable;
use crate::ast::lang::LanguageRegistry;
use crate::budget::{BudgetSnapshot, BudgetState};
use crate::config::{LineBudgetConfig, VerifyStep};
use crate::workspace::Workspace;

/// Sentinel file written inside each worktree directory on every `touch()`.
///
/// Contains a single line: the Unix epoch timestamp (seconds) of the last
/// successful access.  `prune_orphaned_worktrees` reads this to decide
/// whether an ownerless worktree is still "warm" after a server restart.
const SESSION_SENTINEL: &str = ".forgeql-session";

/// Read the last-active Unix timestamp from a worktree's sentinel file.
///
/// Returns `None` if the file is missing, unreadable, or malformed.
#[must_use]
pub fn read_last_active(worktree_path: &Path) -> Option<u64> {
    let data = std::fs::read_to_string(worktree_path.join(SESSION_SENTINEL)).ok()?;
    data.trim().parse().ok()
}

// -----------------------------------------------------------------------
// Checkpoint
// -----------------------------------------------------------------------

/// A named savepoint recorded by `BEGIN TRANSACTION`.
///
/// `pre_txn_oid` is the HEAD before the checkpoint commit was created —
/// the "clean" point that `COMMIT` squashes back to.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    /// User-visible label (e.g. `"my-txn"`).
    pub name: String,
    /// Git OID of the checkpoint commit itself.
    pub oid: String,
    /// HEAD immediately before the checkpoint commit was created.
    pub pre_txn_oid: String,
}

// -----------------------------------------------------------------------
// Session
// -----------------------------------------------------------------------

/// State for one active user session.
///
/// Each session owns a git worktree and the associated symbol index.
/// Sessions cannot be shared between users; the caller is responsible for
/// managing concurrency at the registry level.
#[derive(Debug)]
pub struct Session {
    /// Session identifier — equals the alias supplied in `USE … AS 'alias'`.
    pub id: String,
    /// Identifier of the user who owns this session.
    pub user_id: String,
    /// Absolute path to the worktree's working directory.
    pub worktree_path: PathBuf,
    /// Name of the `Source` (bare repo) this session is attached to.
    pub source_name: String,
    /// Branch that is checked out in the worktree.
    pub branch: String,
    /// Custom git branch name requested via `USE … AS 'name'`.
    ///
    /// When set, this is the visible branch in the bare repo (e.g.
    /// `agent/refactor-signal-api`).  When `None`, the auto-generated
    /// `forgeql/<session_id>` name is used.
    pub custom_branch: Option<String>,
    /// Git worktree handle — the name passed to `git worktree add` and used
    /// to identify the worktree in `worktree::remove`.  May differ from `id`
    /// when a custom branch name was supplied via `USE … AS`.
    pub worktree_name: String,
    /// In-memory symbol index, populated by `build_index` or `resume_index`.
    index: Option<SymbolTable>,
    /// Macro definitions collected during the two-pass indexing pipeline.
    /// `None` until `build_index` or `resume_index` completes.
    macro_table: Option<MacroTable>,
    /// The commit hash the current `index` was built from.
    cached_commit: Option<String>,
    /// Monotonic timestamp of the last request that touched this session.
    /// Used by the TTL eviction task to detect idle sessions.
    last_active: std::time::Instant,
    /// Named checkpoint stack for the checkpoint-based transaction model.
    ///
    /// `BEGIN TRANSACTION 'label'` pushes a new entry; `ROLLBACK
    /// [TRANSACTION 'label']` pops back to (and including) the named
    /// checkpoint.  `COMMIT` squashes all checkpoint commits back to
    /// `last_clean_oid` so the branch history stays clean.
    pub checkpoints: Vec<Checkpoint>,
    /// The HEAD OID of the last "clean" commit — either the initial HEAD
    /// when the session started, or the OID produced by the most recent
    /// `COMMIT`.  `COMMIT` soft-resets to this point before creating the
    /// squashed commit.  `None` until the first `BEGIN TRANSACTION` or
    /// `COMMIT`.
    pub last_clean_oid: Option<String>,
    /// Verify steps frozen from `.forgeql.yaml` at session start (`USE` time).
    /// VERIFY build uses these instead of re-reading the file, so a CHANGE
    /// command cannot inject malicious commands by overwriting `.forgeql.yaml`.
    pub frozen_verify_steps: Option<Vec<VerifyStep>>,
    /// Working directory captured alongside `frozen_verify_steps` — the
    /// directory that contained `.forgeql.yaml` when the session was opened.
    pub frozen_workdir: Option<PathBuf>,
    /// Language support registry for tree-sitter parsing and enrichment.
    lang_registry: Arc<LanguageRegistry>,
    /// Optional line-budget tracker.  `None` when the `.forgeql.yaml` does
    /// not contain a `line_budget` section.
    budget: Option<BudgetState>,
    /// Root data directory (`~/.forgeql`) used to derive the budget file
    /// path.  Set by `init_budget`; `None` until budget is first initialised.
    budget_data_dir: Option<PathBuf>,
    /// The branch key used as the filename stem for the budget file.
    /// Differs from `branch` when branching off trunk: if `branch` is
    /// `main`/`master` this holds the `as_branch` alias instead.
    budget_branch: Option<String>,
    /// Rolling record of recent SHOW LINES reads, used to detect sequential
    /// overlapping/adjacent range reads on the same file and emit tips.
    /// Stored as `(file_path, start_line, end_line)`.
    recent_show_lines: Vec<(String, usize, usize)>,
}

impl Session {
    /// Create a new, un-indexed session.
    ///
    /// The index is initially `None`; call `build_index` or `resume_index`
    /// before querying symbols.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        user_id: impl Into<String>,
        worktree_path: PathBuf,
        source_name: impl Into<String>,
        branch: impl Into<String>,
        lang_registry: Arc<LanguageRegistry>,
    ) -> Self {
        let id_str: String = id.into();
        let worktree_name = id_str.clone();
        Self {
            id: id_str,
            user_id: user_id.into(),
            worktree_path,
            source_name: source_name.into(),
            branch: branch.into(),
            custom_branch: None,
            worktree_name,
            index: None,
            macro_table: None,
            cached_commit: None,
            last_active: std::time::Instant::now(),
            checkpoints: Vec::new(),
            last_clean_oid: None,
            frozen_verify_steps: None,
            frozen_workdir: None,
            lang_registry,
            budget: None,
            budget_data_dir: None,
            budget_branch: None,
            recent_show_lines: Vec::new(),
        }
    }
    /// Parse all source files in the worktree and build a fresh `SymbolTable`.
    ///
    /// The resulting index is persisted to `<worktree>/.forgeql-index` for
    /// future `resume_index` calls.
    ///
    /// # Errors
    /// Returns `Err` if:
    /// - the workspace cannot be created (e.g. path does not exist)
    /// - tree-sitter parsing fails fatally
    /// - the cache file cannot be written
    pub fn build_index(&mut self) -> Result<()> {
        info!(
            session = %self.id,
            path = %self.worktree_path.display(),
            "building symbol index"
        );
        let workspace = Workspace::new(&self.worktree_path)?;
        let (table, macro_table) = SymbolTable::build(&workspace, &self.lang_registry)?;

        let commit_hash = Self::get_head_oid(&self.worktree_path).unwrap_or_default();
        let cached = CachedIndex::from_table_and_macros(table, macro_table, &commit_hash);
        let cache_path = self.worktree_path.join(".forgeql-index");
        cached.save(&cache_path)?;

        let (table, macro_table) = cached.into_table_and_macros();

        debug!(
            session = %self.id,
            symbols = table.rows.len(),
            commit = %commit_hash,
            "index built and saved"
        );

        self.index = Some(table);
        self.macro_table = Some(macro_table);
        self.cached_commit = Some(commit_hash);
        Ok(())
    }

    /// Load the index from disk if it is fresh, otherwise rebuild from scratch.
    ///
    /// "Fresh" means the cached commit hash equals the current HEAD of the
    /// worktree's repository. This is an O(1) check (one `git rev-parse`).
    ///
    /// # Errors
    /// Propagates errors from `build_index` if a rebuild is needed.
    pub fn resume_index(&mut self) -> Result<()> {
        let cache_path = self.worktree_path.join(".forgeql-index");
        let head_oid = Self::get_head_oid(&self.worktree_path).unwrap_or_default();

        match CachedIndex::load(&cache_path) {
            Ok(cached) if cached.commit_hash == head_oid => {
                debug!(
                    session = %self.id,
                    commit = %head_oid,
                    "cache hit — restoring index from disk"
                );
                self.cached_commit = Some(head_oid);
                let (table, macro_table) = cached.into_table_and_macros();
                self.index = Some(table);
                self.macro_table = Some(macro_table);
            }
            Ok(cached) => {
                debug!(
                    session = %self.id,
                    cached = %cached.commit_hash,
                    head = %head_oid,
                    "cache stale — rebuilding index"
                );
                self.build_index()?;
            }
            Err(e) => {
                debug!(
                    session = %self.id,
                    error = %e,
                    "no usable cache — building fresh index"
                );
                self.build_index()?;
            }
        }

        Ok(())
    }

    /// Return a reference to the symbol index, if built.
    #[must_use]
    pub const fn index(&self) -> Option<&SymbolTable> {
        self.index.as_ref()
    }

    /// Return a mutable reference to the symbol index, if built.
    ///
    /// Used by incremental re-indexing after mutations.
    #[must_use]
    pub const fn index_mut(&mut self) -> Option<&mut SymbolTable> {
        self.index.as_mut()
    }

    /// The commit hash the current index was built from, if available.
    #[must_use]
    pub fn cached_commit(&self) -> Option<&str> {
        self.cached_commit.as_deref()
    }

    /// Incrementally re-index the given files after a mutation.
    ///
    /// Each path is purged (all stale entries removed) then re-parsed.
    /// Deleted files are purged only.
    ///
    /// # Errors
    /// Returns `Err` if the index has not been built yet, or if tree-sitter
    /// parsing fails.
    pub fn reindex_files(&mut self, paths: &[PathBuf]) -> Result<()> {
        let table = self
            .index
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("cannot reindex: session {} has no index", self.id))?;
        table.reindex_files(paths, &self.lang_registry)
    }

    /// Persist the current in-memory index to `.forgeql-index`.
    ///
    /// This round-trips through `CachedIndex` (which takes ownership of the
    /// table and gives it back via `into_table`), so the session keeps its
    /// live index after the call.
    ///
    /// # Errors
    /// Returns `Err` if no index has been built yet, or if serialisation /
    /// I/O fails.
    pub fn save_index(&mut self) -> Result<()> {
        let table = self
            .index
            .take()
            .ok_or_else(|| anyhow::anyhow!("cannot save: session {} has no index", self.id))?;
        let macro_table = self.macro_table.take().unwrap_or_default();
        let commit_hash = Self::get_head_oid(&self.worktree_path).unwrap_or_default();
        let cached = CachedIndex::from_table_and_macros(table, macro_table, &commit_hash);
        let cache_path = self.worktree_path.join(".forgeql-index");
        cached.save(&cache_path)?;
        let (table, macro_table) = cached.into_table_and_macros();
        self.index = Some(table);
        self.macro_table = Some(macro_table);
        debug!(
            session = %self.id,
            commit = %commit_hash,
            "index saved to disk"
        );
        self.cached_commit = Some(commit_hash);
        Ok(())
    }
    /// Update the last-active timestamp to now.
    ///
    /// Call this on every request that touches the session so that the TTL
    /// eviction task can accurately measure idle time.  The timestamp is
    /// also persisted to `<worktree>/.forgeql-session` so that
    /// `prune_orphaned_worktrees` can honour the TTL across restarts.
    pub fn touch(&mut self) {
        self.last_active = std::time::Instant::now();
        self.persist_last_active();
    }

    /// Seconds elapsed since the session was last active.
    #[must_use]
    pub fn idle_secs(&self) -> u64 {
        self.last_active.elapsed().as_secs()
    }

    /// Return the current HEAD commit hash of the worktree's repository.
    ///
    /// # Errors
    /// Returns `Err` if the repository cannot be opened or has no commits.
    pub fn commit_hash(&self) -> Result<String> {
        Self::get_head_oid(&self.worktree_path)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Read the OID of HEAD in the repository rooted at (or containing) `path`.
    fn get_head_oid(path: &Path) -> Result<String> {
        let repo = git2::Repository::open(path).or_else(|_| git2::Repository::open_bare(path))?;
        let head = repo.head()?;
        let oid = head.peel_to_commit()?.id().to_string();
        Ok(oid)
    }

    /// Write the current wall-clock time to the sentinel file.
    ///
    /// Best-effort — errors are silently ignored because failing to persist
    /// the timestamp must never block a user request.
    fn persist_last_active(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = std::fs::write(self.worktree_path.join(SESSION_SENTINEL), now.to_string());
    }
}

// -----------------------------------------------------------------------
// Budget integration

impl Session {
    /// Initialise the line-budget for this session.
    ///
    /// `data_dir` is the `ForgeQL` data root (`~/.forgeql`).
    /// `budget_branch` is the computed budget key — the feature branch name,
    /// derived by the engine from the `USE` command (see `use_source`).
    /// When `resumed` is `true` the persisted budget is restored from disk;
    /// otherwise a fresh budget is created.
    pub fn init_budget(
        &mut self,
        config: &LineBudgetConfig,
        resumed: bool,
        data_dir: &std::path::Path,
        budget_branch: &str,
    ) {
        self.budget_data_dir = Some(data_dir.to_path_buf());
        self.budget_branch = Some(budget_branch.to_string());
        self.budget = Some(if resumed {
            BudgetState::load(config, data_dir, &self.source_name, budget_branch)
        } else {
            BudgetState::new(config)
        });
    }

    /// Deduct `lines` from the budget and persist the new state.
    /// Returns `None` when no budget is configured.
    pub fn deduct_budget(&mut self, lines: usize) -> Option<BudgetSnapshot> {
        let data_dir = self.budget_data_dir.clone()?;
        let budget_branch = self.budget_branch.clone()?;
        let budget = self.budget.as_mut()?;
        let snap = budget.deduct(lines);
        budget.save(&data_dir, &self.source_name, &budget_branch);
        Some(snap)
    }

    /// Grant proportional budget recovery for a mutation that wrote code.
    ///
    /// Unlike `deduct_budget(0)` which triggers the rolling-window recovery,
    /// this rewards the agent 1:1 for every line written.
    pub fn reward_budget(&mut self, lines_written: usize) -> Option<BudgetSnapshot> {
        let data_dir = self.budget_data_dir.clone()?;
        let budget_branch = self.budget_branch.clone()?;
        let budget = self.budget.as_mut()?;
        let snap = budget.reward_mutation(lines_written);
        budget.save(&data_dir, &self.source_name, &budget_branch);
        Some(snap)
    }

    /// Reset the budget delta to zero for non-consuming commands.
    pub const fn reset_budget_delta(&mut self) {
        if let Some(ref mut b) = self.budget {
            b.reset_delta();
        }
    }

    /// Return `true` if a budget is active and in critical state.
    #[must_use]
    pub fn is_budget_critical(&self) -> bool {
        self.budget.as_ref().is_some_and(BudgetState::is_critical)
    }
    /// Maximum lines allowed when in critical state.
    #[must_use]
    pub fn budget_critical_max_lines(&self) -> Option<usize> {
        self.budget
            .as_ref()
            .filter(|b| b.is_critical())
            .map(BudgetState::critical_max_lines)
    }

    /// Current budget snapshot (without deducting).
    #[must_use]
    pub fn budget_snapshot(&self) -> Option<BudgetSnapshot> {
        self.budget.as_ref().map(BudgetState::snapshot)
    }

    // ---------------------------------------------------------------
    // Anti-pattern detection: sequential overlapping SHOW LINES
    // ---------------------------------------------------------------

    /// Maximum number of recent reads to track.
    const MAX_RECENT_READS: usize = 5;

    /// Record a SHOW LINES read and return a tip if a fragmentation
    /// anti-pattern is detected (3+ sequential adjacent/overlapping
    /// reads on the same file).
    pub fn record_show_lines(&mut self, file: &str, start: usize, end: usize) -> Option<String> {
        self.recent_show_lines.push((file.to_string(), start, end));
        if self.recent_show_lines.len() > Self::MAX_RECENT_READS {
            drop(self.recent_show_lines.remove(0));
        }

        self.detect_fragmentation_hint()
    }

    /// Clear the recent reads history (called on non-SHOW-LINES commands
    /// to avoid false positives when reads are interleaved with other ops).
    pub fn clear_recent_show_lines(&mut self) {
        self.recent_show_lines.clear();
    }

    /// Check the recent reads for 3+ sequential adjacent/overlapping
    /// ranges on the same file.
    fn detect_fragmentation_hint(&self) -> Option<String> {
        if self.recent_show_lines.len() < 3 {
            return None;
        }

        // Look at the last 3 entries.
        let len = self.recent_show_lines.len();
        let a = &self.recent_show_lines[len - 3];
        let b = &self.recent_show_lines[len - 2];
        let c = &self.recent_show_lines[len - 1];

        // Must all target the same file.
        if a.0 != b.0 || b.0 != c.0 {
            return None;
        }

        // Check if they form a sequential pattern:
        // each start is within 20 lines of the previous end (adjacent/overlapping).
        let b_adjacent = b.1 <= a.2 + 20;
        let c_adjacent = c.1 <= b.2 + 20;

        if b_adjacent && c_adjacent {
            Some(format!(
                "Tip: 3 sequential SHOW LINES reads on '{}'. \
                 Use SHOW body OF 'function_name' to read an entire function \
                 in one operation, or use a single wider SHOW LINES range.",
                c.0
            ))
        } else {
            None
        }
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::lang::CppLanguageInline;
    use tempfile::tempdir;

    fn make_registry() -> Arc<LanguageRegistry> {
        Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]))
    }

    /// Create a minimal git repository with one C++ file and one commit.
    /// Returns the path to the working directory (a normal, non-bare repo).
    fn make_repo_with_cpp(dir: &Path) -> PathBuf {
        let repo_path = dir.join("proj");
        let repo = git2::Repository::init(&repo_path).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        drop(cfg);

        std::fs::create_dir_all(repo_path.join("src")).unwrap();
        std::fs::write(
            repo_path.join("src/motor.cpp"),
            b"void acenderLuz() {}\nvoid apagarLuz() {}\n",
        )
        .unwrap();

        let mut index = repo.index().unwrap();
        index
            .add_path(std::path::Path::new("src/motor.cpp"))
            .unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(0, 0)).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        repo_path
    }

    #[test]
    fn session_new_has_no_index() {
        let s = Session::new(
            "s1",
            "alice",
            PathBuf::from("/tmp"),
            "motor",
            "main",
            make_registry(),
        );
        assert!(s.index().is_none());
    }

    #[test]
    fn build_index_populates_symbols() {
        let tmp = tempdir().unwrap();
        let repo_path = make_repo_with_cpp(tmp.path());
        let mut session = Session::new("s2", "alice", repo_path, "motor", "main", make_registry());

        session.build_index().unwrap();

        let index = session.index().expect("index must be present after build");
        assert!(
            !index.rows.is_empty(),
            "index must contain at least one symbol"
        );
        // The two Portuguese function names must be indexed.
        assert!(
            index.find_def("acenderLuz").is_some() || index.find_def("apagarLuz").is_some(),
            "index must contain the functions from motor.cpp"
        );
    }

    #[test]
    fn resume_index_on_cache_hit() {
        let tmp = tempdir().unwrap();
        let repo_path = make_repo_with_cpp(tmp.path());

        // Build first — writes cache.
        let mut s1 = Session::new(
            "s3",
            "alice",
            repo_path.clone(),
            "motor",
            "main",
            make_registry(),
        );
        s1.build_index().unwrap();
        let defs_count = s1.index().unwrap().rows.len();
        drop(s1); // drop to release any locks

        // Resume — should load from cache (cache hit).
        let mut s2 = Session::new("s4", "alice", repo_path, "motor", "main", make_registry());
        s2.resume_index().unwrap();
        assert_eq!(
            s2.index().unwrap().rows.len(),
            defs_count,
            "resumed index must have the same symbol count as the built one"
        );
    }

    #[test]
    fn resume_index_on_missing_cache_falls_back_to_build() {
        let tmp = tempdir().unwrap();
        let repo_path = make_repo_with_cpp(tmp.path());

        // No cache written — resume should fall back to full build.
        let mut session = Session::new("s5", "alice", repo_path, "motor", "main", make_registry());
        session.resume_index().unwrap();
        assert!(session.index().is_some());
    }

    #[test]
    fn commit_hash_returns_a_string() {
        let tmp = tempdir().unwrap();
        let repo_path = make_repo_with_cpp(tmp.path());
        let session = Session::new("s6", "alice", repo_path, "motor", "main", make_registry());
        let hash = session.commit_hash().unwrap();
        assert_eq!(hash.len(), 40, "OID must be a 40-character hex string");
    }
}
