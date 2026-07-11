use std::sync::Arc;

use anyhow::Result;
use tracing::info;

use crate::storage::git_sha1_provider::git_blob_sha1;

use crate::{
    git::{self as git, source::Source, worktree},
    result::{ForgeQLResult, QueryResult, SessionStats, ShowContent, SourceOpResult, SymbolMatch},
    session::{Session, SessionCoords},
};

use super::ForgeQLEngine;
use super::{load_verify_config, require_session_id};

impl ForgeQLEngine {
    pub(super) fn create_source(&mut self, name: &str, url: &str) -> Result<ForgeQLResult> {
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

        // Write a commented template sidecar config the first time this source
        // is created, so newcomers get a ready-to-edit file without any extra
        // setup.  The call is idempotent and non-fatal.
        let template_msg = crate::config::ForgeConfig::write_sidecar_template(&self.data_dir, name)
            .map(|p| {
                format!(
                    "config template written to '{}' — review and adjust before running VERIFY",
                    p.display()
                )
            });

        // Phase 05 Task 9: spawn background warmer when configured.
        // Defaults are disabled, so this is a no-op out of the box.
        if let Some((_, ref cfg)) =
            load_verify_config(registered.path(), registered.name(), registered.path())
        {
            let policy = cfg.columnar.warm_on_create.clone();
            if policy.enabled {
                match super::warm::pick_warm_targets(registered, &policy) {
                    Ok(targets) => super::warm::spawn_warmer(
                        registered.path().to_path_buf(),
                        registered.name().to_string(),
                        targets,
                        self.data_dir.clone(),
                        Arc::clone(&self.lang_registry),
                    ),
                    Err(e) => tracing::warn!(
                        %name,
                        "warm_on_create: pick_warm_targets failed (non-fatal): {e}"
                    ),
                }
            }
        }

        Ok(ForgeQLResult::SourceOp(SourceOpResult {
            op: "create_source".to_string(),
            source_name: Some(registered.name().to_string()),
            session_id: None,
            branches,
            symbols_indexed: None,
            resumed: already_on_disk,
            message: template_msg,
        }))
    }

    /// `REFRESH SOURCE 'name'` — fetch all remotes on an existing bare repo.
    pub(super) fn refresh_source(&self, name: &str) -> Result<ForgeQLResult> {
        info!(%name, "refreshing source");

        let source = self.registry.get(name).ok_or_else(|| {
            anyhow::anyhow!("source '{name}' not found — run CREATE SOURCE first")
        })?;
        let repo_path = source.path().to_path_buf();

        let reopened = Source::open(name, repo_path.clone())?;

        // Snapshot branch HEADs before fetch — used to compute the moved set
        // for Phase 05 Task 9 selective warming.
        let before = reopened.branch_heads().unwrap_or_default();
        let branches = reopened.fetch_all()?;
        let after = reopened.branch_heads().unwrap_or_default();

        // Phase 05 Task 9: warm only branches whose HEAD moved.  Empty diff
        // = empty target list = no thread spawned.
        if let Some((_, ref cfg)) = load_verify_config(&repo_path, name, &repo_path) {
            let policy = cfg.columnar.warm_on_refresh.clone();
            if policy.enabled {
                let moved: Vec<super::warm::WarmTarget> = after
                    .iter()
                    .filter(|(b, sha)| before.get(*b) != Some(*sha))
                    .map(|(b, sha)| super::warm::WarmTarget {
                        branch: b.clone(),
                        commit_sha: sha.clone(),
                    })
                    .collect();
                if !moved.is_empty() {
                    super::warm::spawn_warmer(
                        repo_path.clone(),
                        name.to_string(),
                        moved,
                        self.data_dir.clone(),
                        Arc::clone(&self.lang_registry),
                    );
                }
            }
        }

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

    /// Scan every in-scope source for stale columnar-cache version directories,
    /// optionally deleting the selected ones, and return the full (uncapped)
    /// [`gc::VacuumReport`]. Powers both the `VACUUM` DSL verb and `forgeql gc`.
    ///
    /// Previews by default (`apply = false`): entries are classified but nothing
    /// is removed. Classification ignores the provider prefix and keys purely on
    /// `<N>` versus `ENRICH_VER`. `source = None` scans every registered source.
    ///
    /// # Errors
    /// Returns an error if `source` is `Some(name)` and no source with that name
    /// is registered.
    pub fn vacuum_report(
        &self,
        source: Option<&str>,
        keep: usize,
        all: bool,
        apply: bool,
    ) -> Result<crate::storage::columnar::gc::VacuumReport> {
        use crate::storage::columnar::gc;

        let names: Vec<String> = match source {
            Some(name) => {
                if self.registry.get(name).is_none() {
                    anyhow::bail!("source '{name}' not found — run SHOW SOURCES to list sources");
                }
                vec![name.to_string()]
            }
            None => self
                .registry
                .names()
                .iter()
                .map(ToString::to_string)
                .collect(),
        };

        let opts = gc::VacuumOptions { keep, all };
        let mut report = gc::VacuumReport {
            source_count: names.len(),
            applied: apply,
            ..Default::default()
        };

        for name in &names {
            let Some(src) = self.registry.get(name) else {
                continue;
            };
            let forgeql = src.path().join("forgeql");

            // Both cache roots share one plan so KEEP/version rules apply per repo.
            let mut dirs = gc::scan_cache_root(&forgeql.join("overlays"));
            dirs.extend(gc::scan_cache_root(&forgeql.join("segments")));
            if dirs.is_empty() {
                continue;
            }

            let to_delete: std::collections::HashSet<usize> =
                gc::plan_deletions(&dirs, opts).into_iter().collect();

            for (i, d) in dirs.iter().enumerate() {
                let selected = to_delete.contains(&i);
                let mut action = if selected {
                    gc::VacuumAction::Delete
                } else {
                    gc::VacuumAction::Keep
                };
                if selected {
                    if apply {
                        match std::fs::remove_dir_all(&d.path) {
                            Ok(()) => {
                                report.delete_count += 1;
                                report.delete_bytes += d.size_bytes;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    path = %d.path.display(),
                                    error = %e,
                                    "vacuum: failed to remove version dir"
                                );
                                action = gc::VacuumAction::Error;
                                report.errors += 1;
                            }
                        }
                    } else {
                        report.delete_count += 1;
                        report.delete_bytes += d.size_bytes;
                    }
                }

                report.entries.push(gc::VacuumEntry {
                    source: name.clone(),
                    name: d.name.clone(),
                    path: d.path.clone(),
                    version: d.version,
                    class: d.class,
                    action,
                    size_bytes: d.size_bytes,
                });
            }
        }

        // Deletions first, then by source and name, for a stable report.
        report.entries.sort_by(|a, b| {
            (a.action != gc::VacuumAction::Delete, &a.source, &a.name).cmp(&(
                b.action != gc::VacuumAction::Delete,
                &b.source,
                &b.name,
            ))
        });

        Ok(report)
    }

    /// `VACUUM [SOURCE 'name'] [KEEP n] [ALL] [APPLY]` — reclaim disk space by
    /// removing stale columnar cache version directories. See [`ForgeQLIR::Vacuum`].
    ///
    /// Thin DSL wrapper over [`Self::vacuum_report`]: it renders the report as a
    /// `QueryResult` (one CSV row per directory, capped like any FIND, with the
    /// reclaimable totals carried in `hint`).
    pub(super) fn vacuum(
        &self,
        source: Option<&str>,
        keep: usize,
        all: bool,
        apply: bool,
    ) -> Result<ForgeQLResult> {
        use crate::storage::columnar::gc;
        use std::fmt::Write as _;

        let report = self.vacuum_report(source, keep, all, apply)?;

        let mut rows: Vec<SymbolMatch> = report
            .entries
            .iter()
            .map(|e| {
                let action = e.action.as_str();
                let class = match e.class {
                    gc::VersionClass::Current => "current",
                    gc::VersionClass::Newer => "newer",
                    gc::VersionClass::Older => "older",
                };
                let fields = std::collections::HashMap::from([
                    ("source".to_string(), e.source.clone()),
                    ("version".to_string(), e.version.to_string()),
                    ("class".to_string(), class.to_string()),
                    ("action".to_string(), action.to_string()),
                    ("size".to_string(), gc::human_bytes(e.size_bytes)),
                    ("bytes".to_string(), e.size_bytes.to_string()),
                ]);
                SymbolMatch {
                    name: e.name.clone(),
                    node_kind: Some("cache_version".to_string()),
                    fql_kind: Some(action.to_string()),
                    language: None,
                    path: Some(e.path.clone()),
                    line: None,
                    usages_count: Some(usize::try_from(e.size_bytes).unwrap_or(usize::MAX)),
                    fields,
                    count: None,
                    node_id: None,
                }
            })
            .collect();

        let total = rows.len();
        // Cap the per-directory rows like any FIND query: the actionable totals
        // (count + reclaimable bytes) ride in `hint`, and keeping `total` at the
        // full count makes `total > results.len()` signal that more rows exist.
        rows.truncate(crate::engine::DEFAULT_QUERY_LIMIT);

        let hint = Some(if apply {
            let mut msg = format!(
                "vacuum applied: removed {} version dir(s), reclaimed {} across {} source(s)",
                report.delete_count,
                gc::human_bytes(report.delete_bytes),
                report.source_count
            );
            if report.errors > 0 {
                let _ = write!(msg, "; {} deletion error(s) — see logs", report.errors);
            }
            msg
        } else {
            format!(
                "vacuum preview: {} version dir(s) / {} would be deleted across {} source(s). Add APPLY to execute.",
                report.delete_count,
                gc::human_bytes(report.delete_bytes),
                report.source_count
            )
        });

        Ok(ForgeQLResult::Query(QueryResult {
            op: "vacuum".to_string(),
            results: rows,
            total,
            metric_hint: Some("size_bytes".to_string()),
            group_by_field: None,
            hint,
        }))
    }

    /// `USE source.branch [AS 'custom-branch']` — create or resume a session.
    #[allow(clippy::too_many_lines)]
    pub(super) fn use_source(
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
        drop(worktree::create(
            &repo_path,
            &wt_name,
            branch,
            &wt_path,
            Some(&git_branch),
        )?);
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

        Ok(self.finalize_use_source(session, &coords, source_name, session_token, wt_existed))
    }

    /// Configure columnar shadow-write on `session` when a `.forgeql.yaml` is
    /// present. Wraps `git_blob_sha1` behind `HashFn` so `ShadowWriter` stays
    /// decoupled from the concrete provider type.
    fn configure_columnar_build(session: &mut Session, repo_path: &std::path::Path) {
        let segments_dir = repo_path.join("forgeql").join("segments");
        let hash_fn: crate::storage::HashFn = Arc::new(|b: &[u8]| git_blob_sha1(b).to_vec());
        let overlays_dir = repo_path.join("forgeql").join("overlays");
        session.set_columnar_build(crate::storage::ColumnarBuildContext::new(
            segments_dir,
            overlays_dir,
            "git-sha1",
            hash_fn,
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
        }
        // FT7: on reconnect, reindex any tracked files that were modified on
        // disk but not captured in a checkpoint commit.  Non-fatal — if the git
        // diff fails (e.g. detached HEAD), log a warning and continue with the
        // cached index (graceful degradation to pre-FT7 behaviour).
        if wt_existed {
            match git::diff_head_to_worktree(&session.worktree_path) {
                Ok(paths) if paths.is_empty() => {}
                Ok(paths) => {
                    tracing::info!(count = paths.len(), "reconnect: reindexing dirty file(s)",);
                    if let Err(e) = session.reindex_files(&paths) {
                        tracing::warn!("reconnect: reindex_files failed (non-fatal): {e}",);
                    }
                }
                Err(e) => {
                    tracing::warn!("reconnect: git diff HEAD failed (non-fatal): {e}");
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
    ) -> ForgeQLResult {
        // PhaseFT5: prefer columnar stats; fall back to legacy table.
        let symbols_indexed = session.engine().index_stats().map_or_else(
            || session.index().map_or(0, |idx| idx.rows.len()),
            |s| s.rows,
        );

        // Write the initial timestamp so background pruners see this worktree as active.
        session.touch();
        let map_key = session_token.clone();
        drop(self.sessions.insert(map_key.clone(), session));

        // If this session was previously registered as pending (from
        // restore_sessions_from_disk), remove it now that it is fully active.
        drop(self.pending_sessions.remove(&map_key));

        ForgeQLResult::SourceOp(SourceOpResult {
            op: "use_source".to_string(),
            source_name: Some(source_name.to_string()),
            session_id: Some(session_token),
            branches: Vec::new(),
            symbols_indexed: Some(symbols_indexed),
            resumed: wt_existed,
            message: if wt_existed {
                Some(format!(
                    "resumed existing worktree for {} — uncommitted changes preserved",
                    coords.git_branch()
                ))
            } else {
                Some(format!("created new worktree for {}", coords.git_branch()))
            },
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
        let resume_outcome: Option<(String, Option<usize>)> = {
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
                    Some((existing_id.clone(), Some(symbols_indexed)))
                }
            } else {
                None
            }
        };
        match resume_outcome {
            Some((id, Some(symbols_indexed))) => {
                Ok(Some(ForgeQLResult::SourceOp(SourceOpResult {
                    op: "use_source".to_string(),
                    source_name: Some(source_name.to_string()),
                    session_id: Some(id),
                    branches: Vec::new(),
                    symbols_indexed: Some(symbols_indexed),
                    resumed: true,
                    message: Some(format!(
                        "resumed in-memory session for {}",
                        coords.git_branch()
                    )),
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
        let columnar_warm = if let Some(ctx) = session.columnar_build() {
            let commit =
                crate::session::Session::get_head_oid(&session.worktree_path).unwrap_or_default();
            let path = ctx.overlay_path_for(&commit);
            path.exists() && crate::storage::columnar::overlay::Overlay::open(&path).is_ok()
        } else {
            false
        };

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
            }
            Err(e) => {
                tracing::warn!(%commit, "columnar warm_or_open failed (non-fatal): {e}");
                // Fall back to legacy if the warm path skipped resume_index.
                if columnar_warm && let Err(re) = session.resume_index() {
                    tracing::warn!("columnar fallback resume_index failed: {re}");
                }
            }
        }
        Ok(())
    }
    /// `SHOW SOURCES` — list all registered sources.
    #[allow(clippy::unnecessary_wraps)] // uniform Result return across all ops
    pub(super) fn show_sources(&self) -> Result<ForgeQLResult> {
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
                    node_id: None,
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
            group_by_field: None,
            hint: None,
        }))
    }

    /// `SHOW BRANCHES [OF 'source']` — list branches of a source.
    pub(super) fn show_branches(&self, session_id: Option<&str>) -> Result<ForgeQLResult> {
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

    /// `SHOW STATS [FOR 'session_id']` — emit internal diagnostics for one or
    /// all active sessions.
    ///
    /// When `for_session` is `Some(sid)`, only that session is included.
    /// When `None`, all sessions with a ready index are reported.
    #[allow(clippy::unnecessary_wraps)]
    pub(super) fn show_stats(&self, for_session: Option<&str>) -> Result<ForgeQLResult> {
        let sessions: Vec<SessionStats> = self
            .sessions
            .iter()
            .filter(|(id, _)| for_session.is_none_or(|s| *id == s))
            .filter_map(|(id, session)| {
                // PhaseFT5: two-arm path — columnar sessions have no legacy table.
                if session.has_columnar() {
                    let rows = session.engine().index_stats().map_or(0, |s| s.rows);
                    return Some(SessionStats {
                        session_id: id.clone(),
                        source: session.source_name.clone(),
                        branch: session.branch.clone(),
                        rows,
                        distinct_names: 0,
                        distinct_paths: 0,
                        usage_symbols: 0,
                        usage_sites: 0,
                        trigram_distinct: 0,
                        mem_total_bytes: 0,
                        mem_rows_bytes: 0,
                        mem_usages_bytes: 0,
                        mem_indexes_bytes: 0,
                        mem_trigram_bytes: 0,
                        mem_strings_bytes: 0,
                        by_language: std::collections::HashMap::new(),
                        by_fql_kind: std::collections::HashMap::new(),
                    });
                }
                // Legacy path.
                let index = session.index()?;
                let mem = index.mem_estimate();
                Some(SessionStats {
                    session_id: id.clone(),
                    source: session.source_name.clone(),
                    branch: session.branch.clone(),
                    rows: index.rows.len(),
                    distinct_names: mem.strings_names,
                    distinct_paths: mem.strings_paths,
                    usage_symbols: mem.usages_symbols,
                    usage_sites: mem.usages_sites,
                    trigram_distinct: mem.trigram_entries,
                    mem_total_bytes: mem.total_bytes(),
                    mem_rows_bytes: mem.rows_bytes,
                    mem_usages_bytes: mem.usages_bytes,
                    mem_indexes_bytes: mem.name_index_bytes
                        + mem.kind_index_bytes
                        + mem.fql_kind_index_bytes,
                    mem_trigram_bytes: mem.trigram_bytes,
                    mem_strings_bytes: mem.strings_bytes,
                    // Resolve interned u32 IDs to string keys for the output DTO.
                    by_language: index.stats.resolved_by_language(&index.strings),
                    by_fql_kind: index.stats.resolved_by_fql_kind(&index.strings),
                })
            })
            .collect();

        Ok(ForgeQLResult::Show(crate::result::ShowResult {
            op: "show_stats".to_string(),
            symbol: None,
            file: None,
            content: ShowContent::Stats { sessions },
            start_line: None,
            end_line: None,
            total_lines: None,
            hint: None,
            metadata: None,
        }))
    }
}
