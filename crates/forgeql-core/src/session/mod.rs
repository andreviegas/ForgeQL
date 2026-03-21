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

use anyhow::Result;
use tracing::{debug, info};

use crate::ast::cache::CachedIndex;
use crate::ast::index::SymbolTable;
use crate::config::VerifyStep;
use crate::workspace::Workspace;

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
    /// Unique session identifier (e.g. a UUID v4 string).
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
    /// The commit hash the current `index` was built from.
    cached_commit: Option<String>,
    /// Monotonic timestamp of the last request that touched this session.
    /// Used by the TTL eviction task to detect idle sessions.
    last_active: std::time::Instant,
    /// Named checkpoint stack for the checkpoint-based transaction model.
    ///
    /// Each entry is `(label, git_oid)`.  `BEGIN TRANSACTION 'label'` pushes
    /// a new entry; `ROLLBACK [TRANSACTION 'label']` pops back to (and
    /// including) the named checkpoint.
    pub checkpoints: Vec<(String, String)>,
    /// Verify steps frozen from `.forgeql.yaml` at session start (`USE` time).
    /// VERIFY build uses these instead of re-reading the file, so a CHANGE
    /// command cannot inject malicious commands by overwriting `.forgeql.yaml`.
    pub frozen_verify_steps: Option<Vec<VerifyStep>>,
    /// Working directory captured alongside `frozen_verify_steps` — the
    /// directory that contained `.forgeql.yaml` when the session was opened.
    pub frozen_workdir: Option<PathBuf>,
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
            cached_commit: None,
            last_active: std::time::Instant::now(),
            checkpoints: Vec::new(),
            frozen_verify_steps: None,
            frozen_workdir: None,
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
        let table = SymbolTable::build(&workspace)?;

        let commit_hash = Self::get_head_oid(&self.worktree_path).unwrap_or_default();
        let cached = CachedIndex::from_table(table, &commit_hash);
        let cache_path = self.worktree_path.join(".forgeql-index");
        cached.save(&cache_path)?;

        let table = cached.into_table();

        debug!(
            session = %self.id,
            symbols = table.rows.len(),
            commit = %commit_hash,
            "index built and saved"
        );

        self.index = Some(table);
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
                self.index = Some(cached.into_table());
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
        table.reindex_files(paths)
    }

    /// Update the last-active timestamp to now.
    ///
    /// Call this on every request that touches the session so that the TTL
    /// eviction task can accurately measure idle time.
    pub fn touch(&mut self) {
        self.last_active = std::time::Instant::now();
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
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
        let s = Session::new("s1", "alice", PathBuf::from("/tmp"), "motor", "main");
        assert!(s.index().is_none());
    }

    #[test]
    fn build_index_populates_symbols() {
        let tmp = tempdir().unwrap();
        let repo_path = make_repo_with_cpp(tmp.path());
        let mut session = Session::new("s2", "alice", repo_path, "motor", "main");

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
        let mut s1 = Session::new("s3", "alice", repo_path.clone(), "motor", "main");
        s1.build_index().unwrap();
        let defs_count = s1.index().unwrap().rows.len();
        drop(s1); // drop to release any locks

        // Resume — should load from cache (cache hit).
        let mut s2 = Session::new("s4", "alice", repo_path, "motor", "main");
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
        let mut session = Session::new("s5", "alice", repo_path, "motor", "main");
        session.resume_index().unwrap();
        assert!(session.index().is_some());
    }

    #[test]
    fn commit_hash_returns_a_string() {
        let tmp = tempdir().unwrap();
        let repo_path = make_repo_with_cpp(tmp.path());
        let session = Session::new("s6", "alice", repo_path, "motor", "main");
        let hash = session.commit_hash().unwrap();
        assert_eq!(hash.len(), 40, "OID must be a 40-character hex string");
    }
}
