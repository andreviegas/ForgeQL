//! Per-session state.
//!
//! A `Session` ties together exactly one git worktree, one user identity,
//! and one `StorageEngine` (the index of the source tree checked out in
//! that worktree). Sessions are created when a user issues `USE source.branch`
//! and destroyed when the session ends.
//!
//! Index caching follows a two-phase strategy:
//!   1. On first use: build the full index and persist it to disk.
//!   2. On resume: reload from disk if the HEAD commit hash matches;
//!      otherwise fall back to a full rebuild.
//!
//! Resume is wholesale, but within a live session only the files a mutation
//! touched are re-indexed.
//!
//! A session outlives the process that created it. That is the constraint this
//! module exists to honour: state a later command depends on is written as it
//! changes, not held in memory and lost to a restart.
//!
//! Submodules:
//! - `budget` — the per-session line budget
//! - `checkpoint_file` — the checkpoint stack, persisted so it survives a restart
//! - `coords` — `SessionCoords`, the identity everything else is derived from
//! - `found_set` — the `FOUND` set a `FIND` arms, and the rev that gates it
//! - `indexing` — building, persisting and resuming the index, and the
//!   storage backends that serve it
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tracing::warn;

use crate::ast::index::OrdinalTombstones;
use crate::ast::lang::LanguageRegistry;
use crate::ast::parse_cache::ParseCache;
use crate::budget::BudgetState;
use crate::config::{RunStep, VerifyStep};
use crate::storage::{BackendSet, LegacyMemoryStorage};

mod budget;
pub mod checkpoint_file;
pub mod coords;
pub mod found_set;
mod indexing;

pub use coords::SessionCoords;

/// Sentinel file written inside each worktree directory on every `touch()`.
///
/// Format: `key=value` lines.  Required key: `timestamp` (Unix epoch, seconds).
/// Optional keys written by [`Session::touch`]: `source`, `branch`, `alias`,
/// `user`.  [`restore_sessions_from_disk`] reads these to restore sessions
/// after a server restart without opening the git repo.
///
/// [`restore_sessions_from_disk`]: crate::engine::ForgeQLEngine::restore_sessions_from_disk
const SESSION_SENTINEL: &str = ".forgeql-session";

/// Parsed contents of a worktree's sentinel file.
///
/// All fields except `last_active_secs` are `None` when the file was written
/// by an older server version that stored only a bare timestamp.
#[derive(Debug)]
pub struct SessionSentinel {
    /// Unix epoch timestamp (seconds) of the last access.
    pub last_active_secs: u64,
    /// Registered source name (bare repo name), e.g. `"pisco-firmware"`.
    pub source: Option<String>,
    /// Branch that is checked out in the worktree.
    pub branch: Option<String>,
    /// User-chosen session alias from `USE … AS 'alias'`.
    pub alias: Option<String>,
    /// User identity that owns this session.
    pub user: Option<String>,
    /// Per-session TTL override in seconds, from `FORGEQL_SESSION_TTL_SECS`
    /// at session creation. `None` falls back to the global `SESSION_TTL_SECS`.
    pub ttl_secs: Option<u64>,
}

/// Read and parse the sentinel file from a worktree directory.
///
/// Returns `None` if the file is missing, unreadable, or the `timestamp`
/// key cannot be parsed.
#[must_use]
pub fn read_sentinel(worktree_path: &Path) -> Option<SessionSentinel> {
    let data = std::fs::read_to_string(worktree_path.join(SESSION_SENTINEL)).ok()?;
    let mut timestamp: Option<u64> = None;
    let mut source: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut alias: Option<String> = None;
    let mut user: Option<String> = None;
    let mut ttl_secs: Option<u64> = None;

    for line in data.lines() {
        if let Some((key, val)) = line.split_once('=') {
            match key {
                "timestamp" => timestamp = val.parse().ok(),
                "source" => source = Some(val.to_string()),
                "branch" => branch = Some(val.to_string()),
                "alias" => alias = Some(val.to_string()),
                "user" => user = Some(val.to_string()),
                "ttl" => ttl_secs = val.parse().ok(),
                _ => {}
            }
        } else if timestamp.is_none() {
            // Backward compat: old files stored just a bare integer.
            timestamp = line.trim().parse().ok();
        }
    }

    Some(SessionSentinel {
        last_active_secs: timestamp?,
        source,
        branch,
        alias,
        user,
        ttl_secs,
    })
}

/// Tear down a worktree: git worktree, session branch, and directory.
///
/// Best-effort and panic-free (every step logs on failure), so it is safe
/// to call from `Drop` guards and test teardown.
///
/// `wt_name` is the worktree directory name as produced by
/// [`SessionCoords::worktree_dir`]. This is the single implementation shared by
/// startup pruning and explicit caller-driven cleanup.
pub fn teardown_worktree(data_dir: &Path, wt_path: &Path, wt_name: &str) {
    // The session branch follows the `fql/{user}/{source}/{branch}/{alias}`
    // scheme and CANNOT be reconstructed from `wt_name` (which flattens every
    // `/` to `-` and omits the user). Read the actual checked-out branch from
    // the live worktree now, before `remove` deletes the working directory —
    // otherwise the branch is orphaned in the bare repo.
    let session_branch = crate::git::worktree::branch_of_worktree(wt_path);

    if let Ok(repo_entries) = std::fs::read_dir(data_dir) {
        for re in repo_entries.flatten() {
            let rpath = re.path();
            if rpath.extension().is_some_and(|ext| ext == "git") {
                // Single teardown path: removes the worktree and deletes its
                // branch together so the branch is never orphaned. The branch was
                // read from the live HEAD above (before removal); the helper falls
                // back to the legacy `forgeql/<wt_name>` name when HEAD was
                // detached or unreadable.
                if let Err(e) = crate::git::worktree::remove_with_branch(
                    &rpath,
                    wt_path,
                    wt_name,
                    session_branch.as_deref(),
                ) {
                    warn!(%wt_name, repo = %rpath.display(), %e, "teardown: worktree/branch cleanup failed");
                }
            }
        }
    }
    if wt_path.exists()
        && let Err(e) = std::fs::remove_dir_all(wt_path)
    {
        warn!(path = %wt_path.display(), %e, "teardown: remove_dir_all failed");
    }
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
    /// Paths created since this checkpoint was pushed, **worktree-relative**.
    ///
    /// ROLLBACK is `git reset --hard`, which restores tracked paths only.
    /// Staging is deferred to COMMIT, so a path created inside the transaction
    /// is still untracked and the reset leaves it behind — on disk and in the
    /// index. These are removed explicitly.
    ///
    /// Persisted with the rest of the stack on every append, not just at BEGIN:
    /// a session outlives the server, and the ROLLBACK that consumes this list
    /// may run in a process that has restarted since the file was created. A
    /// list held only in RAM would be silently empty after a restart, and the
    /// created files would survive the rollback — the exact bug the list exists
    /// to prevent.
    ///
    /// Only the topmost frame records: a nested BEGIN stages everything created
    /// so far, so below it `reset --hard` already handles them. Only paths the
    /// engine itself created are listed — an empty directory that was already
    /// there is not ours to delete, and git would not restore it.
    pub created: Vec<std::path::PathBuf>,
}

// -----------------------------------------------------------------------
// Session
// -----------------------------------------------------------------------

/// State for one active user session.
///
/// Each session owns a git worktree and the associated `StorageEngine`.
/// Sessions cannot be shared between users; the caller is responsible for
/// managing concurrency at the registry level.
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
    /// All storage backends for this session.
    ///
    /// Encapsulates the legacy (always-present) and the optional columnar
    /// backend. `engine_for(&Backend)` delegates to `backends.engine_for`.
    /// Phase 09 will flip the default to columnar inside `BackendSet` without
    /// touching this field or any caller of `engine()` / `engine_for()`.
    backends: BackendSet,
    /// The commit hash the current `index` was built from.
    cached_commit: Option<String>,
    /// `true` when in-memory `index` has diverged from the on-disk
    /// `.forgeql-index` cache (i.e. since the last `save_index`).
    ///
    /// Set by `reindex_files` after every mutation; cleared by
    /// `save_index`.  Used by `BEGIN`, `COMMIT`, and TTL eviction to
    /// decide whether to flush before relying on git as the source of
    /// truth — `BEGIN`'s checkpoint commit must contain a fresh cache
    /// so `ROLLBACK` can restore it via `git reset --hard` and trust it.
    index_dirty: bool,
    /// Monotonic timestamp of the last request that touched this session.
    /// Used by the TTL eviction task to detect idle sessions.
    last_active: std::time::Instant,
    /// Per-session TTL override (seconds) captured from
    /// `FORGEQL_SESSION_TTL_SECS` at session creation. `None` falls back to the
    /// global `SESSION_TTL_SECS`. Lets a short-lived test fleet self-reclaim its
    /// worktrees on a tight TTL without affecting unrelated sessions.
    pub ttl_secs: Option<u64>,
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
    /// Run-step templates frozen from `.forgeql.yaml` at session start, mirroring
    /// `frozen_verify_steps` — a later CHANGE cannot tamper a `RUN` template.
    pub frozen_run_steps: Option<Vec<RunStep>>,
    /// Working directory captured alongside `frozen_verify_steps` — the
    /// directory that contained `.forgeql.yaml` when the session was opened.
    pub frozen_workdir: Option<PathBuf>,
    /// Inline output caps frozen from `.forgeql.yaml` at session start, mirroring
    /// `frozen_verify_steps`.  `None` until the first `USE` that finds a config;
    /// callers fall back to `OutputConfig::default()`.
    pub frozen_output_config: Option<crate::config::OutputConfig>,
    /// Commit-gate tracking — names of `commit_gate` verify steps that have
    /// passed since the last mutation. Cleared by every mutation; a name is
    /// inserted when its gated `VERIFY build` succeeds. `COMMIT` requires every
    /// gated step in `frozen_verify_steps` to be present here.
    pub satisfied_gates: std::collections::HashSet<String>,
    /// Mutations applied since the last successful gated `VERIFY build`,
    /// surfaced only to enrich the COMMIT-blocked message.
    pub edits_since_gate: usize,
    /// Monotonic count of mutations over this session's lifetime — never reset
    /// (unlike `edits_since_gate`). Snapshotted when a gated job starts so its
    /// completion can prove no edit happened while the job was running.
    pub mutation_seq: u64,
    /// The set the most recent FIND armed — the target of every `… NODE[S] LAST`
    /// verb. `None` until the first FIND, and again after any mutation: a
    /// mutation shifts line numbers, so a set that outlived it points at the
    /// wrong code.
    pub found_set: Option<found_set::FoundSet>,
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
    /// Columnar build configuration — populated by `exec_source` / `warm.rs`
    /// when a `.forgeql.yaml` is present for this source.
    /// Replaces the four flat `columnar_segments_dir`, `columnar_provider_id`,
    /// `columnar_hash_fn`, and `columnar_overlays_dir` fields.
    pub(crate) columnar_build: Option<crate::storage::ColumnarBuildContext>,
    /// Per-session LRU parse cache for `SHOW` operations.
    ///
    /// Amortises repeated tree-sitter parses of the same source file within
    /// a session. Keyed by SHA-1 content hash so stale entries are bypassed
    /// automatically after `CHANGE FILE` commands. Capacity: 32 entries.
    pub(crate) parse_cache: Mutex<ParseCache>,
    /// Inline columnar segment map produced by `build_index`'s inline-emit fast-path.
    /// Read by `exec_source::load_session_index` / `warm::warm_snapshot` and handed to
    /// `ColumnarStorage::warm_or_open` via `BuildInput` (skips the `ShadowWriter` pass).
    /// Lives here rather than on the legacy backend so columnar build output is not
    /// stashed on the legacy storage type.
    pub(crate) prebuilt_segment_map: Option<std::collections::HashMap<std::path::PathBuf, Vec<u8>>>,
    /// Removed **root** ordinals per worktree-relative path, staged by a
    /// node-removal verb (`DELETE NODE` whole-node, `MOVE NODE` away) and consumed by
    /// the very next `reindex_files`, which tombstones them in the ordinal
    /// remapper so a byte-identical surviving sibling cannot adopt a deleted
    /// node's handle. Transient: `reindex_files` takes it, so it is
    /// empty for every non-removal mutation and never persisted.
    pub(crate) pending_tombstones: OrdinalTombstones,
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
        lang_registry: &Arc<LanguageRegistry>,
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
            backends: BackendSet::new(LegacyMemoryStorage::new(Arc::clone(lang_registry))),
            cached_commit: None,
            index_dirty: false,
            last_active: std::time::Instant::now(),
            ttl_secs: std::env::var("FORGEQL_SESSION_TTL_SECS")
                .ok()
                .and_then(|v| v.trim().parse().ok()),
            checkpoints: Vec::new(),
            last_clean_oid: None,
            frozen_verify_steps: None,
            frozen_run_steps: None,
            frozen_workdir: None,
            frozen_output_config: None,
            satisfied_gates: std::collections::HashSet::new(),
            edits_since_gate: 0,
            mutation_seq: 0,
            found_set: None,
            budget: None,
            budget_data_dir: None,
            budget_branch: None,
            columnar_build: None,
            parse_cache: Mutex::new(ParseCache::with_capacity(32)),
            prebuilt_segment_map: None,
            pending_tombstones: OrdinalTombstones::new(),
        }
    }

    /// Construct a `Session` from a `SessionCoords` — convenience factory that
    /// avoids threading `id`, `user_id`, `source_name`, and `branch` separately
    /// when a `SessionCoords` is already available.
    #[must_use]
    pub fn from_coords(
        coords: &SessionCoords,
        worktree_path: PathBuf,
        lang_registry: &Arc<LanguageRegistry>,
    ) -> Self {
        Self::new(
            &coords.alias,
            &coords.user,
            worktree_path,
            &coords.source,
            &coords.branch,
            lang_registry,
        )
    }
    /// Inline output caps for this session, frozen from `.forgeql.yaml` at
    /// `USE` time.  Falls back to [`OutputConfig::default`] when no config was
    /// found (`find_limit` = 20, `show_lines` = 40).
    #[must_use]
    pub fn output_config(&self) -> crate::config::OutputConfig {
        self.frozen_output_config.unwrap_or_default()
    }

    /// Update the last-active timestamp to now.
    ///
    /// Call this on every request that touches the session so that the TTL
    /// eviction task can accurately measure idle time.  The timestamp and
    /// session metadata are also persisted to `<worktree>/.forgeql-session`
    /// so that [`ForgeQLEngine::restore_sessions_from_disk`] can restore
    /// live sessions after a server restart without requiring git repo
    /// traversal or directory-name parsing.
    ///
    /// [`ForgeQLEngine::restore_sessions_from_disk`]: crate::engine::ForgeQLEngine::restore_sessions_from_disk
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
    pub(crate) fn get_head_oid(path: &Path) -> Result<String> {
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
        let ttl_line = self
            .ttl_secs
            .map(|ttl| format!("ttl={ttl}\n"))
            .unwrap_or_default();
        let contents = format!(
            "timestamp={now}\nsource={}\nbranch={}\nalias={}\nuser={}\n{ttl_line}",
            self.source_name, self.branch, self.id, self.user_id,
        );
        let _ = std::fs::write(self.worktree_path.join(SESSION_SENTINEL), contents);
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
    fn read_sentinel_parses_ttl_when_present() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join(SESSION_SENTINEL),
            "timestamp=100\nsource=s\nbranch=b\nalias=a\nuser=anonymous\nttl=3600\n",
        )
        .unwrap();
        let sentinel = read_sentinel(dir).expect("sentinel must parse");
        assert_eq!(sentinel.ttl_secs, Some(3600));
        assert_eq!(sentinel.last_active_secs, 100);
    }

    #[test]
    fn read_sentinel_ttl_absent_is_none() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join(SESSION_SENTINEL),
            "timestamp=100\nsource=s\nbranch=b\nalias=a\nuser=anonymous\n",
        )
        .unwrap();
        let sentinel = read_sentinel(dir).expect("sentinel must parse");
        assert_eq!(sentinel.ttl_secs, None);
    }

    #[test]
    fn session_new_has_no_index() {
        let s = Session::new(
            "s1",
            "alice",
            PathBuf::from("/tmp"),
            "motor",
            "main",
            &make_registry(),
        );
        assert!(s.index().is_none());
    }

    #[test]
    fn build_index_populates_symbols() {
        let tmp = tempdir().unwrap();
        let repo_path = make_repo_with_cpp(tmp.path());
        let mut session = Session::new("s2", "alice", repo_path, "motor", "main", &make_registry());

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
            &make_registry(),
        );
        s1.build_index().unwrap();
        let defs_count = s1.index().unwrap().rows.len();
        drop(s1); // drop to release any locks

        // Resume — should load from cache (cache hit).
        let mut s2 = Session::new("s4", "alice", repo_path, "motor", "main", &make_registry());
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
        let mut session = Session::new("s5", "alice", repo_path, "motor", "main", &make_registry());
        session.resume_index().unwrap();
        assert!(session.index().is_some());
    }

    #[test]
    fn commit_hash_returns_a_string() {
        let tmp = tempdir().unwrap();
        let repo_path = make_repo_with_cpp(tmp.path());
        let session = Session::new("s6", "alice", repo_path, "motor", "main", &make_registry());
        let hash = session.commit_hash().unwrap();
        assert_eq!(hash.len(), 40, "OID must be a 40-character hex string");
    }
}
