use std::path::{Path, PathBuf};
#[cfg(feature = "test-helpers")]
use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::{
    ast::index::SymbolTable,
    git::worktree,
    session::{Session, read_last_active},
    workspace::Workspace,
};

use super::ForgeQLEngine;
#[cfg(feature = "test-helpers")]
use super::generate_session_id;
use super::{SESSION_TTL_SECS, require_session_id};

impl ForgeQLEngine {
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
    pub(super) fn require_workspace_and_index(
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
    pub(super) fn require_session(&self, session_id: &str) -> Result<&Session> {
        self.sessions.get(session_id).ok_or_else(|| {
            anyhow::anyhow!("session '{session_id}' not found — run USE <source>.<branch> first")
        })
    }

    /// Attempt to silently restore a session from disk after a server restart.
    ///
    /// When the alias (= `session_id`) is not in memory but a matching worktree
    /// directory exists on disk, this reads the persisted `.forgeql-meta` file
    /// (written by `use_source` at session creation time) to recover the
    /// source name and branch, then re-executes `USE` transparently.
    ///
    /// On success the session is restored exactly as if the client had re-issued
    /// the `USE` command.  On any failure the call is a silent no-op and the
    /// subsequent `require_session` will return the normal "not found" error.
    pub(super) fn try_auto_reconnect(&mut self, alias: &str) {
        let wt_dir = self.data_dir.join("worktrees");
        let target_suffix = format!(".{alias}");

        // Scan for a worktree directory whose name ends with .{alias}.
        // Since the new naming format is "{source}.{branch}.{alias}", multiple
        // sources could in principle have a worktree ending in the same alias.
        // We pick the first one whose git metadata still resolves cleanly —
        // the use_source() retry will validate the (source, branch, alias)
        // tuple end-to-end.
        let wt_path = std::fs::read_dir(&wt_dir).map_or(None, |entries| {
            entries
                .flatten()
                .find(|e| e.file_name().to_string_lossy().ends_with(&target_suffix))
                .map(|e| e.path())
        });

        let Some(wt_path) = wt_path else {
            debug!(%alias, "auto-reconnect: no matching worktree directory on disk");
            return;
        };

        // Derive source_name from the git worktree's link to its bare repo.
        // For a linked worktree, `repo.path()` returns
        // `<data_dir>/<source>.git/worktrees/<wt_name>/`, so going up two
        // parents gives us the bare repo directory `<source>.git`.
        let source_name: String = match git2::Repository::open(&wt_path) {
            Ok(repo) => {
                // .../source.git/worktrees/wt_name/ → .../source.git
                let Some(bare) = repo.path().parent().and_then(Path::parent) else {
                    debug!(%alias, "auto-reconnect: cannot derive bare repo path");
                    return;
                };
                let Some(stem) = bare.file_stem().and_then(std::ffi::OsStr::to_str) else {
                    debug!(%alias, path = %bare.display(), "auto-reconnect: cannot derive source name");
                    return;
                };
                String::from(stem)
            }
            Err(err) => {
                debug!(%alias, %err, "auto-reconnect: cannot open worktree repo");
                return;
            }
        };

        // Directory name layout: "{source}.{branch}.{alias}".
        // Strip the known prefix and suffix to recover the branch component.
        // Branch may itself contain '.' so we strip both ends rather than split.
        let dir_name = wt_path.file_name().unwrap_or_default().to_string_lossy();
        let source_prefix = format!("{source_name}.");
        let Some(branch) = dir_name
            .strip_prefix(&source_prefix)
            .and_then(|rest| rest.strip_suffix(&target_suffix))
        else {
            debug!(
                %alias, %dir_name, %source_name,
                "auto-reconnect: directory name does not match the source.branch.alias layout — likely a legacy pre-0.38.2 layout, skipping",
            );
            return;
        };

        match self.use_source(&source_name, branch, alias) {
            Ok(_) => info!(%alias, %source_name, %branch, "session auto-reconnected from disk"),
            Err(err) => warn!(%alias, %err, "auto-reconnect attempt failed"),
        }
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
        let session_id = generate_session_id();
        let mut session = Session::new(
            &session_id,
            "test-user",
            workspace_root.to_path_buf(),
            "local",       // synthetic source name
            "test-branch", // synthetic branch name (not main/master to allow budget tests)
            Arc::clone(&self.lang_registry),
        );
        session.build_index()?;

        let sid = session_id.clone();
        session.touch();
        drop(self.sessions.insert(session_id, session));
        Ok(sid)
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
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.init_budget(config, false, &data_dir, "test-branch");
        }
    }
}
