use anyhow::Result;
use std::path::{Path, PathBuf};
#[cfg(feature = "test-helpers")]
use std::sync::Arc;
use tracing::{info, warn};

use crate::{
    auth::{AuthContext, auth},
    git::worktree,
    ir::Backend,
    session::{Session, SessionCoords, SessionSentinel, read_sentinel},
    storage::StorageEngine,
    workspace::Workspace,
};

use super::ForgeQLEngine;
use super::PendingSession;
#[cfg(feature = "test-helpers")]
use super::generate_session_id;
use super::{SESSION_TTL_SECS, require_session_id};

impl ForgeQLEngine {
    pub fn evict_idle_sessions(&mut self) {
        let expired_ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.idle_secs() > session.ttl_secs.unwrap_or(SESSION_TTL_SECS)
            })
            .map(|(id, _)| id.clone())
            .collect();

        for id in expired_ids {
            if let Some(session) = self.sessions.remove(&id) {
                info!(%id, "TTL eviction: removing idle session");
                let repo_path = self.data_dir.join(format!("{}.git", session.source_name));
                // Unity rule: a worktree and its branch are deleted together or
                // kept together. A session is kept only when its named branch
                // carries real committed work (something to review); research
                // sessions that changed nothing are reclaimed whole at the TTL.
                let keep_for_review = match session.custom_branch.as_deref() {
                    // No USE … AS branch → auto scratch, never reviewable.
                    None => false,
                    // Keep iff the branch diff vs its base has non-control changes.
                    Some(branch) => {
                        match crate::git::source_changes(&repo_path, &session.branch, branch) {
                            Ok(changed) => !changed.is_empty(),
                            // Be conservative on error: keep, so work is never lost.
                            Err(e) => {
                                warn!(%id, %e, "TTL eviction: source_changes failed — keeping branch");
                                true
                            }
                        }
                    }
                };
                if keep_for_review {
                    info!(
                        %id,
                        branch = ?session.custom_branch,
                        "TTL eviction: worktree+branch retained for review (has unmerged work)"
                    );
                    continue;
                }
                // No reviewable work → delete the whole unit (worktree + branch).
                if let Err(err) = worktree::remove_with_branch(
                    &repo_path,
                    &session.worktree_path,
                    &session.worktree_name,
                    session.custom_branch.as_deref(),
                ) {
                    warn!(
                        worktree = %session.worktree_name,
                        error = %err,
                        "TTL eviction: worktree cleanup failed"
                    );
                }
            }
        }
    }

    /// Restore live sessions from disk and prune expired worktrees.
    ///
    /// Call this **once** at MCP server startup (before accepting requests).
    /// It replaces the old `prune_orphaned_worktrees` + `try_auto_reconnect`
    /// pair with a single pass:
    ///
    /// - Scans `<data_dir>/worktrees/` for all worktree directories.
    /// - Reads each worktree's `.forgeql-session` sentinel file.
    /// - **Prunes** any worktree whose TTL has expired.
    /// - **Registers** every warm worktree as a [`PendingSession`] — metadata
    ///   only; no columnar index is loaded until the agent issues a `USE`
    ///   command for that session.
    ///
    /// This is intentionally lazy: on a shared server with many developers,
    /// eagerly loading every columnar index at startup would exhaust RAM before
    /// the first request is served.  The full session (worktree checkout +
    /// columnar overlay) is loaded in `use_source` when the agent actually
    /// reconnects.
    ///
    /// After this call, `require_session` is a pure O(1) map lookup with no
    /// fallback disk scan.  Do **not** call in CLI modes — worktrees persist
    /// across invocations and sessions should not be re-indexed on every
    /// invocation.
    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    pub fn restore_sessions_from_disk(&mut self) {
        let wt_dir = SessionCoords::worktrees_root(&self.data_dir);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // Worktrees live in a per-user subdirectory:
        //   data_dir/worktrees/{user}/{source}.{branch}.{alias}/
        // Scan two levels: user dirs → worktree dirs inside each.  Old flat-layout
        // worktrees that pre-date this layout are ignored and remain on disk.
        let Ok(user_entries) = std::fs::read_dir(&wt_dir) else {
            return;
        };

        let mut registered = 0u32;
        let mut pruned = 0u32;

        for user_entry in user_entries.flatten() {
            let user_dir = user_entry.path();
            // file_type() from read_dir does NOT follow symlinks. The
            // compatibility links that USE leaves at worktrees/{dir} (see
            // git::worktree::ensure_legacy_link) must never be traversed
            // here: following one would scan a worktree's own contents as
            // if they were session dirs and prune every subdirectory that
            // lacks a session sentinel.
            if !user_entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(wt_entries) = std::fs::read_dir(&user_dir) else {
                continue;
            };
            for entry in wt_entries.flatten() {
                let wt_path = entry.path();
                if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                    continue;
                }
                let wt_name = entry.file_name().to_string_lossy().to_string();
                self.restore_one_worktree(&wt_path, &wt_name, now, &mut registered, &mut pruned);
            }
        }

        self.prune_stale_git_worktrees();

        info!(
            registered,
            pruned, "startup: session restore complete (lazy — indexes load on first USE)"
        );
    }

    /// Restore (or prune) a single worktree dir based on its sentinel: prune on
    /// missing/expired sentinels, register as pending on full metadata, leave
    /// old-format sentinels for the agent to reconnect.  Bumps the counters.
    fn restore_one_worktree(
        &mut self,
        wt_path: &std::path::Path,
        wt_name: &str,
        now: u64,
        registered: &mut u32,
        pruned: &mut u32,
    ) {
        // Defense in depth: only ever prune something that is actually a git
        // worktree (contains a `.git` entry). A scanner bug or a stray
        // directory under worktrees/ must never lead to deleting arbitrary
        // content.
        if !wt_path.join(".git").exists() {
            warn!(%wt_name, "startup: not a git worktree, leaving untouched");
            return;
        }
        match read_sentinel(wt_path) {
            None => {
                // No readable sentinel — orphan from an older version or a
                // partially created worktree.  Prune unconditionally.
                info!(%wt_name, "startup: no sentinel, pruning");
                self.prune_single_worktree(wt_path, wt_name);
                *pruned += 1;
            }
            Some(sentinel)
                if now.saturating_sub(sentinel.last_active_secs)
                    >= sentinel.ttl_secs.unwrap_or(SESSION_TTL_SECS) =>
            {
                info!(%wt_name, "startup: TTL expired, pruning");
                self.prune_single_worktree(wt_path, wt_name);
                *pruned += 1;
            }
            Some(SessionSentinel {
                user,
                source: Some(source),
                branch: Some(branch),
                alias: Some(alias),
                ..
            }) => {
                // Warm worktree with full metadata — register as pending.  The
                // index loads lazily when the agent issues USE for this session.
                let user = user
                    .as_deref()
                    .unwrap_or_else(|| auth(AuthContext::Session))
                    .to_owned();
                let coords = SessionCoords::new(&user, &source, &branch, &alias);
                let session_key = coords.map_key();
                let worktree_name = coords.worktree_dir();
                info!(%user, %source, %branch, %alias, "startup: session registered as pending");
                drop(self.pending_sessions.insert(
                    session_key,
                    PendingSession {
                        user,
                        source,
                        branch,
                        alias,
                        worktree_name,
                    },
                ));
                *registered += 1;
            }
            Some(_) => {
                // Old-format sentinel (timestamp only) — cannot recover
                // source/branch/alias.  Leave on disk; the agent will re-issue
                // USE when it next connects.
                info!(%wt_name, "startup: old-format sentinel, leaving for agent reconnect");
            }
        }
    }

    /// Pass 2: sweep git worktree metadata entries whose checkout path is gone
    /// (handles a crash-interrupted prune from a previous run).  In-memory and
    /// pending sessions are protected from pruning.
    fn prune_stale_git_worktrees(&self) {
        let Ok(repo_entries) = std::fs::read_dir(&self.data_dir) else {
            return;
        };
        let live_wt_names: std::collections::HashSet<&str> = self
            .sessions
            .values()
            .map(|s| s.worktree_name.as_str())
            .chain(
                self.pending_sessions
                    .values()
                    .map(|p| p.worktree_name.as_str()),
            )
            .collect();

        for re in repo_entries.flatten() {
            let rpath = re.path();
            if rpath.extension().is_none_or(|ext| ext != "git") {
                continue;
            }
            let Ok(wts) = worktree::list(&rpath) else {
                continue;
            };
            for wt in wts {
                if live_wt_names.contains(wt.name.as_str()) {
                    continue;
                }
                if !wt.path.exists() {
                    info!(wt_name = %wt.name, "startup: pruning stale git worktree metadata");
                    // `wt.branch` carries the real branch name even though the
                    // checkout is gone, so a custom `fql/…` session branch is
                    // deleted by its true name rather than the legacy fallback.
                    if let Err(e) = worktree::remove_with_branch(
                        &rpath,
                        &wt.path,
                        &wt.name,
                        wt.branch.as_deref(),
                    ) {
                        warn!(wt_name = %wt.name, %e, "stale worktree/branch prune failed");
                    }
                }
            }
        }
    }

    /// Remove a single worktree directory and its git metadata from all bare repos.
    fn prune_single_worktree(&self, wt_path: &Path, wt_name: &str) {
        crate::session::teardown_worktree(&self.data_dir, wt_path, wt_name);
    }

    // ===================================================================
    // Internal helpers
    // ===================================================================

    /// Resolve `session_id` to a `Workspace` + `&dyn StorageEngine` pair.
    ///
    /// # Errors
    /// Returns `Err` if the session is not found, the index is not ready,
    /// or the workspace cannot be created.
    pub(super) fn require_workspace_and_engine(
        &self,
        session_id: Option<&str>,
    ) -> Result<(Workspace, &dyn StorageEngine)> {
        let session = self.require_session(require_session_id(session_id)?)?;
        anyhow::ensure!(session.has_index(), "session index not ready — retry USE");
        let workspace = Workspace::new(&session.worktree_path)?;
        Ok((workspace, session.engine()))
    }

    /// Backend-aware variant of `require_workspace_and_engine`.
    ///
    /// Routes through `Session::engine_for(backend)` instead of always using
    /// the default legacy engine.
    ///
    /// # Errors
    /// Returns `Err` if the session is not found, the index is not ready,
    /// the workspace cannot be created, or the requested backend is not
    /// installed (e.g. `Backend::Columnar` before Phase 03).
    pub(super) fn require_workspace_and_engine_for(
        &self,
        session_id: Option<&str>,
        backend: &Backend,
    ) -> Result<(Workspace, &dyn StorageEngine)> {
        let session = self.require_session(require_session_id(session_id)?)?;
        anyhow::ensure!(session.has_index(), "session index not ready — retry USE");
        let workspace = Workspace::new(&session.worktree_path)?;
        Ok((workspace, session.engine_for(backend)?))
    }
    /// Look up a session by ID.
    ///
    /// # Errors
    /// Returns `Err` if no session with this ID exists.
    pub(super) fn require_session(&self, session_id: &str) -> Result<&Session> {
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
    pub(super) fn reindex_session(&mut self, session_id: &str, paths: &[PathBuf]) {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        if let Err(err) = session.reindex_files(paths) {
            warn!(
                session = %session_id,
                error = %err,
                "reindex after mutation failed"
            );
        }
        // Note: no save_index here. The on-disk cache is only flushed at
        // meaningful boundaries — BEGIN, COMMIT, TTL eviction, shutdown —
        // because (a) most mutations are followed by more mutations, not
        // by a daemon restart, and (b) on Zephyr the serialize+write costs
        // ~17s per call.  `Session::index_dirty` tracks divergence and
        // `flush_if_dirty` is called from those four boundary points.
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
        let alias = generate_session_id();
        let coords = SessionCoords::new(auth(AuthContext::Tester), "local", "test-branch", &alias);
        let map_key = coords.map_key();
        let mut session = Session::new(
            &alias,
            "test-user",
            workspace_root.to_path_buf(),
            "local",       // synthetic source name
            "test-branch", // synthetic branch name (not main/master to allow budget tests)
            &Arc::clone(&self.lang_registry),
        );

        // Honor a workspace `.forgeql.yaml` (when present) so local sessions
        // freeze the same verify steps — including commit-gate flags — as a real
        // `USE`.  Absent config → nothing frozen (back-compat).
        if let Some((workdir, config)) =
            super::load_verify_config(workspace_root, "local", workspace_root)
        {
            session.frozen_workdir = Some(workdir);
            session.frozen_output_config = Some(config.output);
            session.frozen_verify_steps = Some(config.verify_steps);
            session.frozen_run_steps = Some(config.run_steps);
        }

        // Restore the checkpoint stack from disk, exactly as the real `USE`
        // reconnect path does (`restore_session_on_reconnect`). A local session
        // that skipped this could not see what a restarted server sees — and a
        // test harness that is quietly more forgetful than production hides
        // precisely the bugs a restart causes.
        let current_head = Session::get_head_oid(workspace_root).unwrap_or_default();
        crate::session::checkpoint_file::try_restore(&mut session, workspace_root, &current_head);
        session.last_set = crate::session::last_set::try_restore(workspace_root);
        session.build_index()?;

        session.touch();
        drop(self.sessions.insert(map_key, session));
        Ok(coords.to_session_id())
    }

    /// Like [`register_local_session`] but uses an explicit `user_id` for the
    /// session map key.  Use this when the test exercises a specific entry-point
    /// user (e.g. `auth(AuthContext::Mcp)`) so the registered session is
    /// discoverable by that code path.
    ///
    /// # Errors
    /// Returns `Err` if the workspace cannot be created or indexing fails.
    #[cfg(feature = "test-helpers")]
    pub fn register_local_session_for(
        &mut self,
        user_id: &str,
        workspace_root: &Path,
    ) -> Result<String> {
        let alias = generate_session_id();
        let coords = SessionCoords::new(user_id, "local", "test-branch", &alias);
        let map_key = coords.map_key();
        let mut session = Session::new(
            &alias,
            "test-user",
            workspace_root.to_path_buf(),
            "local",
            "test-branch",
            &Arc::clone(&self.lang_registry),
        );
        session.build_index()?;
        session.touch();
        drop(self.sessions.insert(map_key, session));
        Ok(coords.to_session_id())
    }

    /// Activate a line budget for an existing session.
    ///
    /// Test-only helper — in production, the budget is initialized during `USE`.
    #[cfg(feature = "test-helpers")]
    pub fn init_session_budget(
        &mut self,
        session_id: &str,
        config: &crate::config::LineBudgetConfig,
    ) {
        let data_dir = self.data_dir.clone();
        // session_id is a full to_session_id() token — it equals the map key.
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.init_budget(config, false, &data_dir, "test-branch");
        }
    }

    /// Install a columnar storage backend on an existing session.
    ///
    /// Test-only helper — in production the columnar engine is installed during
    /// `USE` when an overlay file is found on disk.
    #[cfg(feature = "test-helpers")]
    pub fn install_columnar_for_session(
        &mut self,
        session_id: &str,
        storage: Box<dyn crate::storage::StorageEngine>,
    ) {
        // session_id is a full to_session_id() token — it equals the map key.
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.install_columnar(storage);
        }
    }

    /// Returns `true` if the session has a columnar backend installed.
    #[cfg(feature = "test-helpers")]
    #[must_use]
    pub fn session_has_columnar(&self, session_id: &str) -> bool {
        // session_id is a full to_session_id() token — it equals the map key.
        self.sessions
            .get(session_id)
            .is_some_and(Session::has_columnar)
    }
    /// Returns `true` if the legacy symbol table is `None` for the given session.
    #[cfg(feature = "test-helpers")]
    #[must_use]
    pub fn session_legacy_table_is_none(&self, session_id: &str) -> bool {
        self.sessions
            .get(session_id)
            .and_then(|s| s.legacy_storage())
            .is_none_or(|l| l.table().is_none())
    }

    /// Return `index_stats().rows` for the session's default engine.
    ///
    /// Returns `None` if the session does not exist or `index_stats()` is `None`.
    ///
    /// Test-only helper for `PhaseFT5` gate tests.
    #[cfg(feature = "test-helpers")]
    #[must_use]
    pub fn session_index_stats_rows(&self, session_id: &str) -> Option<usize> {
        // session_id is a full to_session_id() token — it equals the map key.
        self.sessions
            .get(session_id)
            .and_then(|s| s.engine().index_stats())
            .map(|st| st.rows)
    }

    /// Register a local session and build **both** backends from the same
    /// `build_index` call via columnar shadow-write.
    ///
    /// After this returns, the session has:
    /// - A legacy `LegacyMemoryStorage` built by `build_index`.
    /// - A `ColumnarStorage` built from the same segments written during
    ///   `build_index` (shadow-write path), so both backends index identical
    ///   symbol data.
    ///
    /// `segments_dir` and `overlays_dir` are temporary directories owned by
    /// the caller (e.g., subdirs of a `TempDir`).
    ///
    /// # Errors
    /// Returns an error if `build_index` or the overlay open fails.
    #[cfg(feature = "test-helpers")]
    pub fn register_local_session_with_columnar(
        &mut self,
        workspace_root: &Path,
        segments_dir: &Path,
        overlays_dir: &Path,
    ) -> Result<String> {
        use crate::storage::columnar::overlay::Overlay;
        use crate::storage::columnar::{ColumnarStorage, SegmentReader};

        let alias = crate::engine::exec_session::generate_session_id();
        let coords = SessionCoords::new(auth(AuthContext::Tester), "local", "test-branch", &alias);
        let mut session = Session::new(
            &alias,
            "test-user",
            workspace_root.to_path_buf(),
            "local",
            "test-branch",
            &Arc::clone(&self.lang_registry),
        );

        // Enable shadow-write: segments → `segments_dir/unknown/hex/`.
        // provider_id "test" maps to "unknown" in build_index's static match.
        let hash_fn: crate::storage::HashFn = Arc::new(|content: &[u8]| {
            use std::hash::Hasher as _;
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash_slice(content, &mut h);
            h.finish().to_le_bytes().to_vec()
        });
        session.set_columnar_build(crate::storage::ColumnarBuildContext::new(
            segments_dir.to_path_buf(),
            overlays_dir.to_path_buf(),
            "test",
            hash_fn,
        ));

        // Build the index.  This writes the legacy SymbolTable, segments, and
        // (because columnar_overlays_dir is set) the overlay file.
        session.build_index()?;

        // The commit OID for a non-git dir is "" (get_head_oid returns Err,
        // build_index falls back to unwrap_or_default → "").
        // overlay_path = overlays_dir/{provider_id}/{commit}.bin
        //              = overlays_dir/test/.bin  (provider_id="test", commit="")
        let provider_id = "test";
        let overlay_path = overlays_dir.join(provider_id).join(".bin");

        if overlay_path.exists() {
            let overlay = Overlay::open(&overlay_path).map_err(|e| {
                anyhow::anyhow!("register_local_session_with_columnar: overlay open: {e}")
            })?;
            let segs: Vec<Arc<SegmentReader>> = overlay
                .segments()
                .iter()
                .filter_map(|meta| {
                    let seg_path = segments_dir
                        .join(provider_id)
                        .join(format!("{}.fqsf", &meta.hex_content_id));
                    SegmentReader::open(&seg_path).ok().map(Arc::new)
                })
                .collect();
            let columnar = ColumnarStorage::new(
                workspace_root.to_path_buf(),
                segs,
                overlay,
                Arc::clone(&self.lang_registry),
            );
            session.install_columnar(Box::new(columnar));
            session.drop_legacy_index();
        }
        session.touch();
        drop(self.sessions.insert(coords.map_key(), session));
        Ok(coords.to_session_id())
    }
}
