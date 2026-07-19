/// Per-session state — Phase B of the v2 architecture.
///
/// A `Session` ties together exactly one git worktree, one user identity,
/// and one `StorageEngine` (the index of the source tree checked out in
/// that worktree). Sessions are created when a user issues `USE source.branch`
/// and destroyed when the session ends.
///
/// Index caching follows a two-phase strategy:
///   1. On first use: build the full index and persist it to disk.
///   2. On resume: reload from disk if the HEAD commit hash matches;
///      otherwise fall back to a full rebuild.
///      (True incremental re-index is deferred to Phase D.)
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::ast::index::{OrdinalTombstones, SymbolTable};
use crate::ast::lang::LanguageRegistry;
use crate::ast::parse_cache::ParseCache;
use crate::budget::{BudgetSnapshot, BudgetState};
use crate::config::{LineBudgetConfig, RunStep, VerifyStep};
use crate::storage::{BackendSet, LegacyMemoryStorage, StorageEngine};
use crate::workspace::Workspace;

pub mod checkpoint_file;
pub mod coords;
pub mod found_set;

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
    /// Configure columnar shadow-write.
    ///
    /// Must be called **before** `build_index` / `resume_index`.  When set,
    /// each full build writes one segment per source file to
    /// `<segments_dir>/<provider_id>/<content-hex>/` and builds an overlay
    /// at `<overlays_dir>/<provider_id>/<commit>.bin`.
    pub fn set_columnar_build(&mut self, ctx: crate::storage::ColumnarBuildContext) {
        self.columnar_build = Some(ctx);
    }

    /// Columnar build context, if shadow-write was enabled at session creation.
    #[must_use]
    pub const fn columnar_build(&self) -> Option<&crate::storage::ColumnarBuildContext> {
        self.columnar_build.as_ref()
    }

    /// Inline output caps for this session, frozen from `.forgeql.yaml` at
    /// `USE` time.  Falls back to [`OutputConfig::default`] when no config was
    /// found (`find_limit` = 20, `show_lines` = 40).
    #[must_use]
    pub fn output_config(&self) -> crate::config::OutputConfig {
        self.frozen_output_config.unwrap_or_default()
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
        // PhaseFT5: build and persist always operate on the legacy backend
        // explicitly; after the route-flip `default_engine_mut()` returns
        // columnar (which has no `build` or `persist_to_cache` semantics).
        let legacy = self
            .backends
            .legacy_storage_mut()
            .ok_or_else(|| anyhow::anyhow!("no legacy backend"))?;

        // Columnar inline fast-path: build a SegmentBuildCtx so SymbolTable::build
        // writes segments inline per-file (with per-file post_pass), skipping the
        // 2-minute sequential merge. Passed to build_with_seg_ctx (not stored on legacy).
        let worktree_root = self.worktree_path.clone();
        let (seg_ctx, inline_state) = self.columnar_build.as_ref().map_or_else(
            || (None, None),
            |ctx| {
                let (sc, state) = ctx.make_inline_ctx(&worktree_root);
                (Some(sc), Some(state))
            },
        );

        legacy.build_with_seg_ctx(&workspace, seg_ctx.as_ref())?;

        // After build (all rayon threads done), extract the inline segment_map
        // and store it on the Session for warm_or_open to consume.
        if let Some(state) = inline_state {
            let map = std::sync::Arc::try_unwrap(state).map_or_else(
                |arc| {
                    arc.segment_map
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                },
                |s| s.segment_map.into_inner().unwrap_or_default(),
            );
            self.prebuilt_segment_map = Some(map);
        }

        // When columnar is configured, the legacy SymbolTable is a transient
        // build artefact used only to shadow-write segments and build the
        // overlay. It is freed by `drop_legacy_index()` immediately after
        // `warm_or_open` completes, so writing it to `.forgeql-index` wastes
        // I/O and produces a file that is never read on future sessions
        // (the warm path skips `resume_index()` when an overlay exists).
        if self.columnar_build.is_none() {
            let commit_hash = Self::get_head_oid(&self.worktree_path).unwrap_or_default();
            legacy.persist_to_cache(&self.worktree_path, &commit_hash, &self.source_name)?;
            debug!(
                session = %self.id,
                commit = %commit_hash,
                "index built and saved"
            );
            self.cached_commit = Some(commit_hash);
        } else {
            debug!(
                session = %self.id,
                "index built in-memory (columnar configured — skipping .forgeql-index write)"
            );
        }

        self.index_dirty = false;
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
        let head_oid = Self::get_head_oid(&self.worktree_path).unwrap_or_default();

        // PhaseFT5: legacy must be loaded explicitly; `default_engine_mut()`
        // now returns columnar once installed, which has no cache semantics.
        let loaded = self
            .backends
            .legacy_storage_mut()
            .map(|l| l.load_from_cache(&self.worktree_path, &head_oid, &self.source_name))
            .transpose()?
            .unwrap_or(false);

        if loaded {
            debug!(
                session = %self.id,
                commit = %head_oid,
                "cache hit — restoring index from disk"
            );
            self.cached_commit = Some(head_oid);
            self.index_dirty = false;
        } else {
            debug!(
                session = %self.id,
                "cache miss — building fresh index"
            );
            self.build_index()?;
        }

        Ok(())
    }

    /// Return a reference to the legacy `SymbolTable`, if the engine holds one.
    /// Provided for SHOW / exec paths that still work directly with the table.
    /// Returns `None` for non-legacy backends, or before the index is built.
    #[must_use]
    pub fn index(&self) -> Option<&SymbolTable> {
        self.backends.legacy_storage().and_then(|l| l.table())
    }

    /// `true` when an index has been built (or loaded from cache) for this
    /// session.  Used by callers that need to distinguish "no index yet"
    /// from "empty index" — e.g. ROLLBACK's smart-rollback path.
    #[must_use]
    pub fn has_index(&self) -> bool {
        self.backends.default_engine().has_index()
    }

    /// Return a reference to the legacy backend, if present.
    ///
    /// Used by call sites that need `&SymbolTable` directly (e.g. on-demand
    /// overlay builds in `exec_source`).  Returns `None` in Phase 09+ when
    /// the default backend is no longer legacy.
    #[must_use]
    pub const fn legacy_storage(&self) -> Option<&crate::storage::LegacyMemoryStorage> {
        self.backends.legacy_storage()
    }

    /// The commit hash the current index was built from, if available.
    #[must_use]
    pub fn cached_commit(&self) -> Option<&str> {
        self.cached_commit.as_deref()
    }

    /// Return a reference to the default (legacy) storage engine.
    #[must_use]
    pub fn engine(&self) -> &dyn StorageEngine {
        self.backends.default_engine()
    }

    /// Return a mutable reference to the default (legacy) storage engine.
    #[must_use]
    pub fn engine_mut(&mut self) -> &mut dyn StorageEngine {
        self.backends.default_engine_mut()
    }

    /// Return a reference to the storage engine to use for a given backend selector.
    ///
    /// - [`Backend::Default`] and [`Backend::Legacy`] → the legacy in-memory engine.
    /// - [`Backend::Columnar`] → the columnar engine, if one is installed.
    ///   Returns an error when no columnar engine has been installed.
    ///
    /// # Errors
    /// Returns `Err` if `backend` is [`Backend::Columnar`] and no columnar engine
    /// has been installed for this session.
    pub fn engine_for(&self, backend: &crate::ir::Backend) -> Result<&dyn StorageEngine> {
        self.backends.engine_for(backend)
    }

    /// Returns `true` if a columnar backend is installed on this session.
    #[must_use]
    pub fn has_columnar(&self) -> bool {
        self.backends.has_columnar()
    }

    /// Install (or replace) the columnar storage backend.
    ///
    /// In production this is called by `exec_source` when an overlay file is
    /// found on disk. In tests it can be called directly via
    /// [`ForgeQLEngine::install_columnar_for_session`].
    pub fn install_columnar(&mut self, columnar: Box<dyn StorageEngine>) {
        self.backends.set_columnar(columnar);
    }

    /// Free the legacy `SymbolTable` from memory.
    ///
    /// Called immediately after `install_columnar` (`PhaseFT5`) so that the
    /// legacy RAM is released once columnar is the default engine.
    pub fn drop_legacy_index(&mut self) {
        if let Some(legacy) = self.backends.legacy_storage_mut() {
            legacy.drop_stored_index();
        }
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
        // A node-removal verb may have staged tombstones for this reindex; take
        // them (so they never leak into a later mutation) and hand them to both
        // backends. The tombstoned root ordinals stop a byte-identical
        // surviving sibling from adopting a just-deleted node's handle.
        let tombstones = std::mem::take(&mut self.pending_tombstones);
        // PhaseFT5: target both backends explicitly.
        // Legacy may have no table after `drop_legacy_index()` — treat as non-fatal.
        if let Some(legacy) = self.backends.legacy_storage_mut()
            && let Err(e) = legacy.reindex_files_tombstoned(paths, &tombstones)
        {
            tracing::warn!("legacy reindex_files (non-fatal): {e}");
        }
        if let Some(columnar) = self.backends.columnar_engine_mut()
            && let Err(e) = columnar.reindex_files_tombstoned(paths, &tombstones)
        {
            tracing::warn!("columnar reindex_files failed (non-fatal): {e}");
        }
        self.index_dirty = true;
        Ok(())
    }

    /// Persist the current in-memory index to `.forgeql-index`.
    ///
    /// # Errors
    /// Returns `Err` if no index has been built yet, or if serialisation /
    /// I/O fails.
    pub fn save_index(&mut self) -> Result<()> {
        let commit_hash = Self::get_head_oid(&self.worktree_path).unwrap_or_default();
        // PhaseFT5: persist explicitly via legacy; `default_engine_mut()` now
        // returns columnar when installed.
        if let Some(legacy) = self.backends.legacy_storage_mut() {
            legacy.persist_to_cache(&self.worktree_path, &commit_hash, &self.source_name)?;
        }
        debug!(
            session = %self.id,
            commit = %commit_hash,
            "index saved to disk"
        );
        self.cached_commit = Some(commit_hash);
        self.index_dirty = false;
        Ok(())
    }

    /// Save the index to disk if it has been modified since the last save.
    ///
    /// Cheap no-op when `index_dirty` is `false`.
    ///
    /// # Errors
    /// Propagates `save_index` errors when a flush actually happens.
    pub fn flush_if_dirty(&mut self) -> Result<()> {
        if self.index_dirty {
            if self.backends.has_columnar() {
                // PhaseFT5: columnar sessions manage their delta file at
                // BEGIN TRANSACTION time (git-tracked).  Nothing to flush here.
            } else {
                self.save_index()?;
            }
        }
        Ok(())
    }

    /// Mark the in-memory index as having diverged from the on-disk cache.
    pub const fn mark_index_dirty(&mut self) {
        self.index_dirty = true;
    }

    /// Drop the in-memory index without saving.  Used by `ROLLBACK` so
    /// the next `resume_index` reads the freshly-restored
    /// `.forgeql-index` from disk instead of keeping a stale view.
    pub fn drop_index(&mut self) {
        self.backends.default_engine_mut().drop_stored_index();
        self.cached_commit = None;
        self.index_dirty = false;
    }

    /// Mutable access to the columnar storage backend, if installed.
    ///
    /// Returns `None` when the columnar backend is not enabled for this session.
    pub fn columnar_storage_mut(&mut self) -> Option<&mut dyn crate::storage::StorageEngine> {
        self.backends.columnar_engine_mut()
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
