//! The `USE` pipeline: attach a session to a source branch, creating the
//! worktree or resuming one that already exists.
//!
//! `use_source` is the only entry point. The other five functions are steps of
//! it — resuming a live worktree, restoring session state on reconnect,
//! configuring the columnar build, finalising the response, and loading the
//! index — and each is private and called only from `use_source`, which is why
//! they can all move together without any visibility change.

use std::sync::Arc;

use anyhow::Result;
use tracing::info;

use crate::{
    git::{self as git, worktree},
    result::{ForgeQLResult, SourceOpResult},
    session::{Session, SessionCoords, liveness::WorktreeClaim},
};

use crate::engine::{ForgeQLEngine, load_verify_config};

impl ForgeQLEngine {
    /// `USE source.branch [AS 'custom-branch']` — create or resume a session.
    #[allow(clippy::too_many_lines)]
    pub(in crate::engine) fn use_source(
        &mut self,
        user_id: &str,
        source_name: &str,
        branch: &str,
        as_branch: &str,
    ) -> Result<ForgeQLResult> {
        // Construct session identity — single source of truth for map key,
        // git branch name, and worktree path derivations.
        let coords = SessionCoords::new(user_id, source_name, branch, as_branch);
        if let Err(msg) = coords.validate() {
            return Err(crate::error::ForgeError::InvalidInput(msg).into());
        }

        let budget_branch = coords.budget_branch();

        info!(%source_name, %branch, ?as_branch, %budget_branch, "starting session");

        // Session resume: reuse an in-memory session for this source + branch +
        // alias when one exists and its branch HEAD has not moved; otherwise a
        // stale session is evicted and we fall through to create a fresh one.
        if let Some(result) = self.try_resume_session(&coords, source_name, branch, as_branch)? {
            return Ok(result);
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

        // The session token returned to callers is the full coords.to_session_id()
        // value, which also serves as the HashMap key (map_key delegates to
        // to_session_id).  Callers echo this opaque token back on every request;
        // the engine decodes it into SessionCoords via from_session_id().
        let session_token = coords.to_session_id(); // returned to caller & used as map key
        // All path and branch name derivations go through `SessionCoords`
        // so the layout can be changed in one place — see session/coords.rs.
        let wt_name = coords.worktree_dir();
        let git_branch = coords.git_branch();
        // Worktree lives under the per-user subdir: data_dir/worktrees/{user}/{dir}.
        let wt_path = coords.worktree_path(&self.data_dir);
        // Ensure the per-user worktree subdirectory exists before creating the worktree.
        if let Some(parent) = wt_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let wt_existed = wt_path.exists();
        // Claim the worktree before git is allowed to touch it. A peer engine
        // process sweeping for orphans at startup reads a half-populated
        // directory exactly like an abandoned one; the claim is what tells them
        // apart, and it only works if it is taken first.
        let worktree_claim = WorktreeClaim::acquire(&wt_path)?;
        let wt_info = worktree::create(&repo_path, &wt_name, branch, &wt_path, Some(&git_branch))?;
        // `base_commit` is the commit actually checked out (truthful even when
        // an existing worktree or session branch was reused); `upstream_head`
        // is what the requested base resolved to right now — recorded on the
        // session so a later re-USE can detect the base moving under REFRESH.
        let base_commit = wt_info.base_commit;
        let upstream_head = wt_info.upstream_head;
        // Host tooling built against the pre-user-segment layout resolves
        // worktrees/{dir}; keep a compatibility symlink there so container
        // runners and mount scripts keep working (see ensure_legacy_link).
        worktree::ensure_legacy_link(&coords.legacy_worktree_path(&self.data_dir), &wt_path);
        // Keep never-committed runtime artifacts out of git status and host
        // pre-commit hooks for every worktree of this source.
        crate::git::ensure_runtime_excludes(&repo_path);
        let mut session = Session::from_coords(&coords, wt_path, &Arc::clone(&self.lang_registry));
        session.custom_branch = Some(git_branch);
        session.worktree_name = wt_name;
        session.upstream_observed = upstream_head;
        // The claim now lives as long as the session does; dropping the session
        // — or losing the process — is what releases the worktree again.
        session.worktree_claim = Some(worktree_claim);

        // Load config once — before resume_index so shadow-write is configured
        // before the first build.  The same config is then used to freeze the
        // verify steps and initialise the budget below.
        let maybe_config = load_verify_config(&repo_path, source_name, &session.worktree_path);

        // Configure columnar when a `.forgeql.yaml` is present (always-on).
        if maybe_config.is_some() {
            Self::configure_columnar_build(&mut session, &repo_path);
        }

        // Load the session index (warm path reads the columnar overlay from
        // disk; cold path loads the legacy table for the shadow-writer).
        self.load_session_index(&mut session)?;

        // Restore checkpoint stack (FT6) and reindex dirty files on reconnect (FT7).
        Self::restore_session_on_reconnect(&mut session, wt_existed);

        // Freeze verify config at session start — sidecar takes priority over in-repo file.
        // Any later CHANGE has no effect on VERIFY; steps are captured once here.
        if let Some((workdir, config)) = maybe_config {
            session.frozen_workdir = Some(workdir);
            if let Some(ref budget_cfg) = config.line_budget {
                // Sweep expired budget files before initialising the budget
                // for this session — clean up abandoned branches for free.
                crate::budget::sweep_expired(&self.data_dir);
                session.init_budget(budget_cfg, wt_existed, &self.data_dir, budget_branch);
            }
            session.frozen_output_config = Some(config.output);
            session.frozen_verify_steps = Some(config.verify_steps);
            session.frozen_run_steps = Some(config.run_steps);
        }

        Ok(self.finalize_use_source(
            session,
            &coords,
            source_name,
            session_token,
            wt_existed,
            base_commit,
        ))
    }

    /// Configure columnar shadow-write on `session` when a `.forgeql.yaml` is
    /// present. The store layout and content-hash choice live on
    /// [`ColumnarBuildContext::for_bare_repo`], so this and the background
    /// warmer cannot drift apart — they key the same shared-open cache.
    fn configure_columnar_build(session: &mut Session, repo_path: &std::path::Path) {
        session.set_columnar_build(crate::storage::ColumnarBuildContext::for_bare_repo(
            repo_path,
        ));
    }

    /// Restore the checkpoint stack from disk (FT6) and, for a resumed worktree,
    /// reindex any files modified on disk but not captured in a checkpoint (FT7).
    /// Both steps degrade gracefully — failures are logged and ignored.
    fn restore_session_on_reconnect(session: &mut Session, wt_existed: bool) {
        // FT6: restore checkpoint stack from disk if the file is present and
        // the stored HEAD matches the current worktree HEAD.  Both conditions
        // must hold to guarantee the stack is consistent with git state.
        // On mismatch or any error, the session starts with an empty stack.
        {
            // Clone the path first to avoid holding an immutable borrow on
            // `session` while also passing `&mut session` to `try_restore`.
            let worktree = session.worktree_path.clone();
            let current_head = crate::session::Session::get_head_oid(&worktree).unwrap_or_default();
            crate::session::checkpoint_file::try_restore(session, &worktree, &current_head);
            // The FOUND set outlives the process too: an agent may FIND, hand the
            // session to another agent (or wait out a restart), and only then
            // sweep. The set is re-gated against live revs at mutation time, so
            // restoring it can only re-offer a target — never authorise a stale
            // one.
            session.found_set = crate::session::found_set::try_restore(&worktree);
        }
        // FT7: on reconnect, reindex any tracked files that were modified on
        // disk but not captured in a checkpoint commit.  Non-fatal — if the git
        // diff fails (e.g. detached HEAD), log a warning and continue with the
        // cached index (graceful degradation to pre-FT7 behaviour).
        //
        // The list is joined with any paths whose staged delta state the
        // columnar loader had to drop (previous-generation delta after an
        // upgrade, or a missing staging segment): those files are NOT dirty in
        // git terms — their edits may sit in checkpoint commits — but their
        // index rows were lost with the delta and only a re-index restores
        // them.
        if wt_existed {
            let mut paths = match git::diff_head_to_worktree(&session.worktree_path) {
                Ok(paths) => paths,
                Err(e) => {
                    tracing::warn!("reconnect: git diff HEAD failed (non-fatal): {e}");
                    Vec::new()
                }
            };
            if let Some(columnar) = session.columnar_storage_mut() {
                for p in columnar.take_pending_reindex_paths() {
                    if !paths.contains(&p) {
                        paths.push(p);
                    }
                }
            }
            if !paths.is_empty() {
                tracing::info!(count = paths.len(), "reconnect: reindexing dirty file(s)",);
                if let Err(e) = session.reindex_files(&paths) {
                    tracing::warn!("reconnect: reindex_files failed (non-fatal): {e}",);
                }
            }
        }
    }

    /// Finalise a freshly built session: record index stats, register it in the
    /// live session map, clear any pending-session entry, and build the
    /// `use_source` result. Consumes `session`.
    /// Finalise a freshly built session: record index stats, register it in the
    /// live session map, clear any pending-session entry, and build the
    /// `use_source` result. Consumes `session`.
    fn finalize_use_source(
        &mut self,
        mut session: Session,
        coords: &SessionCoords,
        source_name: &str,
        session_token: String,
        wt_existed: bool,
        base_commit: Option<String>,
    ) -> ForgeQLResult {
        // PhaseFT5: prefer columnar stats; fall back to legacy table.
        let symbols_indexed = session.engine().index_stats().map_or_else(
            || session.index().map_or(0, |idx| idx.rows.len()),
            |s| s.rows,
        );

        // Write the initial timestamp so background pruners see this worktree as active.
        session.touch();
        let map_key = session_token.clone();
        // Read before the insert moves the session: the fallback note rides on
        // the USE response, not only in the server log.
        let index_fallback = session.index_fallback.clone();
        drop(self.sessions.insert(map_key.clone(), session));

        // If this session was previously registered as pending (from
        // restore_sessions_from_disk), remove it now that it is fully active.
        drop(self.pending_sessions.remove(&map_key));

        let mut message = if wt_existed {
            format!(
                "resumed existing worktree for {} — uncommitted changes preserved",
                coords.git_branch()
            )
        } else {
            format!("created new worktree for {}", coords.git_branch())
        };
        if let Some(note) = index_fallback {
            message = format!("{message}. {note}");
        }

        ForgeQLResult::SourceOp(SourceOpResult {
            op: "use_source".to_string(),
            source_name: Some(source_name.to_string()),
            session_id: Some(session_token),
            branches: Vec::new(),
            symbols_indexed: Some(symbols_indexed),
            resumed: wt_existed,
            base_commit,
            message: Some(message),
        })
    }

    /// Reuse an in-memory session for this source + branch + alias when one
    /// exists and is still valid. Returns `Some(result)` to short-circuit
    /// `use_source` (the caller returns it), or `None` to create a fresh session.
    ///
    /// A stale session — whose indexed commit differs from the bare repo's
    /// current branch HEAD (e.g. after REFRESH SOURCE) — is evicted before
    /// returning `None`. An alias already bound to a different source or user is
    /// an error rather than a silent rebind.
    fn try_resume_session(
        &mut self,
        coords: &SessionCoords,
        source_name: &str,
        branch: &str,
        as_branch: &str,
    ) -> Result<Option<ForgeQLResult>> {
        // Decide before mutating self.sessions to avoid holding a shared borrow
        // across a mutable one. The alias is the session key, so this is O(1).
        // (session_id, Some(ResumedInfo)) to resume,
        // (session_id, None) to evict the stale entry and rebuild.
        struct ResumedInfo {
            symbols_indexed: usize,
            base_commit: Option<String>,
            index_fallback: Option<String>,
        }
        type ResumeOutcome = Option<(String, Option<ResumedInfo>)>;
        let resume_outcome: ResumeOutcome = {
            if let Some((existing_id, existing_session)) =
                self.sessions.get_key_value(&coords.map_key())
            {
                if existing_session.source_name != source_name
                    || existing_session.user_id != coords.user
                {
                    return Err(crate::error::ForgeError::InvalidInput(format!(
                        "alias '{as_branch}' is already bound to source '{}' (user '{}') — \
                         choose a different alias or DROP SESSION '{as_branch}' first",
                        existing_session.source_name, existing_session.user_id,
                    ))
                    .into());
                }
                // Fail CLOSED: resume only when we can positively prove the
                // requested base still resolves to the same commit this
                // session observed at creation. `resolve_base_commit` covers
                // branch names AND commit-hex bases (a hex is not a branch, so
                // the old `branch_head` check returned `None` there and the
                // eviction never fired — the incident shape: REFRESH moved the
                // base, re-USE resumed the pre-REFRESH session). If either
                // side is unknown, evict and rebuild rather than risk silently
                // serving old content. The comparison target is the observed
                // upstream, NOT `cached_commit`: the worktree HEAD moves with
                // every session commit and would evict healthy work sessions.
                let is_stale = {
                    let live_base = self
                        .registry
                        .get(source_name)
                        .and_then(|src| crate::git::resolve_base_commit(src.path(), branch));
                    match (live_base, existing_session.upstream_observed.as_deref()) {
                        (Some(head), Some(observed)) => observed != head,
                        _ => true,
                    }
                };
                if is_stale {
                    info!(
                        session_id = %existing_id,
                        %source_name,
                        %branch,
                        "requested base moved (or could not be verified) — evicting stale session"
                    );
                    Some((existing_id.clone(), None))
                } else {
                    let symbols_indexed = existing_session.engine().index_stats().map_or_else(
                        || existing_session.index().map_or(0, |idx| idx.rows.len()),
                        |s| s.rows,
                    );
                    info!(
                        session_id = %existing_id,
                        %source_name,
                        %branch,
                        "session resume — reusing existing in-memory session"
                    );
                    // Report the commit the session's index was actually built
                    // from — never a fresh resolution of the requested base.
                    // (A fresh resolution told an agent "base = new head"
                    // while the resumed session was still serving old files.)
                    let actual_base = existing_session.cached_commit().map(str::to_string);
                    Some((
                        existing_id.clone(),
                        Some(ResumedInfo {
                            symbols_indexed,
                            base_commit: actual_base,
                            index_fallback: existing_session.index_fallback.clone(),
                        }),
                    ))
                }
            } else {
                None
            }
        };
        match resume_outcome {
            Some((id, Some(info))) => {
                let mut message = format!("resumed in-memory session for {}", coords.git_branch());
                if let Some(note) = info.index_fallback {
                    message = format!("{message}. {note}");
                }
                Ok(Some(ForgeQLResult::SourceOp(SourceOpResult {
                    op: "use_source".to_string(),
                    source_name: Some(source_name.to_string()),
                    session_id: Some(id),
                    branches: Vec::new(),
                    symbols_indexed: Some(info.symbols_indexed),
                    resumed: true,
                    base_commit: info.base_commit,
                    message: Some(message),
                })))
            }
            Some((stale_id, None)) => {
                drop(self.sessions.remove(&stale_id));
                Ok(None)
            }
            None => Ok(None),
        }
    }

    /// Load the session index. Warm path: when a columnar overlay already exists
    /// for the current HEAD and opens cleanly, read it from disk and skip loading
    /// the multi-GB legacy table. Cold path: load the legacy table so the
    /// shadow-writer can build the overlay, then install columnar and drop legacy.
    fn load_session_index(&self, session: &mut Session) -> Result<()> {
        // Every attach recomputes the fallback note below; a stale one from an
        // earlier failed attach must not outlive the open that healed it.
        session.index_fallback = None;
        // Ask the shared cache, and KEEP what it hands back. Binding the entry
        // is the whole point: it must outlive the `warm_or_open` below, which
        // is what turns this probe's decode into that session's decode rather
        // than a second one. Nothing else holds an entry — the cache tracks
        // them weakly and retains nothing — so letting this fall out of scope
        // as a temporary frees it immediately and makes the first session on a
        // commit decode the committed half twice.
        let warm_entry = session.columnar_build().and_then(|ctx| {
            let commit =
                crate::session::Session::get_head_oid(&session.worktree_path).unwrap_or_default();
            let path = ctx.overlay_path_for(&commit);
            if !path.exists() {
                return None;
            }
            crate::storage::columnar::ColumnarStorage::shared_open(ctx, &path).ok()
        });
        let columnar_warm = warm_entry.is_some();

        if !columnar_warm {
            // Cold path: load legacy index (reuses the on-disk cache when HEAD matches).
            session.resume_index()?;
        }

        let Some(ctx) = session.columnar_build().cloned() else {
            // Columnar not configured — legacy was loaded above.
            return Ok(());
        };
        let commit =
            crate::session::Session::get_head_oid(&session.worktree_path).unwrap_or_default();
        // Warm path passes None (overlay from disk); cold path passes the legacy
        // storage for shadow-write.
        let legacy = if columnar_warm {
            None
        } else {
            session.legacy_storage()
        };
        let input = crate::storage::columnar::BuildInput {
            table: legacy.and_then(|l| l.table()),
            prebuilt_segment_map: session.prebuilt_segment_map.clone(),
        };
        match crate::storage::columnar::ColumnarStorage::warm_or_open(
            &ctx,
            input,
            session.worktree_path.clone(),
            &commit,
            Arc::clone(&self.lang_registry),
        ) {
            Ok(storage) => {
                session.install_columnar(Box::new(storage));
                session.drop_legacy_index();
                // The warm path skips `resume_index`/`build_index` entirely, so
                // record here which commit this index snapshot serves — the
                // resume staleness check and the resumed-USE `base_commit`
                // echo both read it.
                session.note_index_commit(commit.clone());
            }
            Err(e) => {
                tracing::warn!(%commit, "columnar warm_or_open failed (non-fatal): {e}");
                // The in-memory index is the fallback, and what the cold path
                // built is not it: with columnar configured, `build_index`
                // hands `SymbolTable::build` the inline segment context, and
                // that path returns an EMPTY table by design — the columnar
                // engine never reads it after build. On this path the table is
                // the whole answer surface, so an absent or empty one is
                // rebuilt for real, without the inline context, before this
                // session serves zero rows as a success.
                let fallback_failed = if session.index().is_none_or(|t| t.rows.is_empty()) {
                    let failed = session.build_fallback_index().err();
                    if let Some(re) = &failed {
                        tracing::warn!("columnar fallback index build failed: {re}");
                    }
                    failed
                } else {
                    None
                };
                // The refusal names its own repair, and the warn above reaches
                // only the server log — record it on the session so every USE
                // response for this session carries it to the agent.
                session.index_fallback = Some(fallback_failed.map_or_else(
                    || {
                        format!(
                            "columnar index unavailable — serving from the \
                             complete in-memory index (slower): {e:#}"
                        )
                    },
                    |re| {
                        format!(
                            "columnar index unavailable ({e:#}) and the in-memory \
                             fallback failed too ({re:#}) — this session may hold \
                             no index; re-run USE or re-create the source"
                        )
                    },
                ));
            }
        }
        Ok(())
    }
}
