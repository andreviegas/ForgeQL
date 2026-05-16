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

    /// `USE source.branch [AS 'custom-branch']` — create or resume a session.
    #[allow(clippy::too_many_lines)]
    pub(super) fn use_source(
        &mut self,
        source_name: &str,
        branch: &str,
        as_branch: &str,
    ) -> Result<ForgeQLResult> {
        // Construct session identity — single source of truth for map key,
        // git branch name, and worktree path derivations.
        let coords = SessionCoords::anonymous(source_name, branch, as_branch);
        if let Err(msg) = coords.validate() {
            return Err(crate::error::ForgeError::InvalidInput(msg).into());
        }

        let budget_branch = coords.budget_branch();

        info!(%source_name, %branch, ?as_branch, %budget_branch, "starting session");

        // Session resume: if an in-memory session already exists for this
        // source + branch + as_branch combination, reuse it — unless the
        // branch HEAD in the bare repo has moved (e.g. after REFRESH SOURCE),
        // in which case evict the stale session and fall through to create a
        // fresh one.
        //
        // We collect the decision into `resume_outcome` before mutating
        // `self.sessions` to avoid holding a shared borrow across a mutable one.
        // Because the alias is the session key (see below), an O(1) lookup suffices.
        let resume_outcome: Option<(String, Option<usize>)> = {
            if let Some((existing_id, existing_session)) = self.sessions.get_key_value(as_branch) {
                // Guard: if the alias was previously bound to a *different* source
                // or a different user, evict it rather than returning the wrong
                // repo's data or leaking one user's session to another.
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
                    // PhaseFT5: prefer columnar stats; fall back to legacy table.
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
                return Ok(ForgeQLResult::SourceOp(SourceOpResult {
                    op: "use_source".to_string(),
                    source_name: Some(source_name.to_string()),
                    session_id: Some(id),
                    branches: Vec::new(),
                    symbols_indexed: Some(symbols_indexed),
                    resumed: true,
                    message: Some(format!(
                        "resumed in-memory session for fql/{branch}/{as_branch}"
                    )),
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

        // The alias is the session key — deterministic, memorable, and
        // reconstructable from the USE command the model already knows.
        // No opaque generated ID needed.
        let session_id = as_branch.to_string();
        // All path and branch name derivations go through `SessionCoords`
        // so the layout can be changed in one place — see session/coords.rs.
        let wt_name = coords.worktree_dir();
        let git_branch = coords.git_branch();
        let wt_path = SessionCoords::worktrees_root(&self.data_dir).join(&wt_name);

        let wt_existed = wt_path.exists();
        drop(worktree::create(
            &repo_path,
            &wt_name,
            branch,
            &wt_path,
            Some(&git_branch),
        )?);

        let mut session = Session::new(
            &session_id,
            &coords.user,
            wt_path,
            source_name,
            branch,
            &Arc::clone(&self.lang_registry),
        );
        session.custom_branch = Some(git_branch);
        session.worktree_name = wt_name;

        // Load config once — before resume_index so shadow-write is configured
        // before the first build.  The same config is then used to freeze the
        // verify steps and initialise the budget below.
        let maybe_config = load_verify_config(&repo_path, source_name, &session.worktree_path);

        // Configure columnar when a `.forgeql.yaml` is present (always-on).
        if maybe_config.is_some() {
            let segments_dir = repo_path.join("forgeql").join("segments");
            // Wrap git_blob_sha1 behind HashFn so ShadowWriter stays decoupled
            // from the concrete provider type (Issue 1).
            let hash_fn: crate::storage::HashFn = Arc::new(|b: &[u8]| git_blob_sha1(b).to_vec());
            let overlays_dir = repo_path.join("forgeql").join("overlays");
            session.set_columnar_build(crate::storage::ColumnarBuildContext::new(
                segments_dir,
                overlays_dir,
                "git-sha1",
                hash_fn,
            ));
        }

        // Warm-path optimisation: if the columnar overlay already exists for
        // the current HEAD commit, skip resume_index() entirely — loading the
        // 2-3 GB legacy SymbolTable only to immediately discard it wastes RAM
        // and time.  We go straight to warm_or_open(ctx, None) which reads
        // the overlay from disk in seconds.
        //
        // Cold path (no overlay yet): fall through to resume_index() so the
        // legacy SymbolTable is available for the shadow-writer to build
        // segments and create the overlay for the first time.
        let columnar_warm = if let Some(ctx) = session.columnar_build() {
            let commit =
                crate::session::Session::get_head_oid(&session.worktree_path).unwrap_or_default();
            ctx.overlay_path_for(&commit).exists()
        } else {
            false
        };

        if !columnar_warm {
            // Cold path: load legacy index so shadow-writer can build the
            // overlay.  Use resume_index() so an existing disk cache at
            // <worktree>/.forgeql-index is reused when HEAD matches.
            session.resume_index()?;
        }

        // If the columnar backend is enabled, ensure the overlay exists for this
        // commit and install a ready-to-query ColumnarStorage on the session.
        if let Some(ctx) = session.columnar_build().cloned() {
            let commit =
                crate::session::Session::get_head_oid(&session.worktree_path).unwrap_or_default();
            // Warm path passes None for legacy — overlay is loaded from disk.
            // Cold path passes the loaded legacy storage for shadow-write.
            let legacy = if columnar_warm {
                None
            } else {
                session.legacy_storage()
            };
            match crate::storage::columnar::ColumnarStorage::warm_or_open(
                &ctx,
                legacy,
                session.worktree_path.clone(),
                &commit,
                Arc::clone(&self.lang_registry),
            ) {
                Ok(storage) => {
                    // Delta is loaded inside warm_or_open — just install.
                    session.install_columnar(Box::new(storage));
                    // PhaseFT5: free legacy RAM now that columnar is default.
                    session.drop_legacy_index();
                }
                Err(e) => {
                    tracing::warn!(%commit, "columnar warm_or_open failed (non-fatal): {e}");
                    // warm_or_open failed; fall back to legacy if it wasn't
                    // loaded (warm path skipped resume_index).
                    if columnar_warm && let Err(re) = session.resume_index() {
                        tracing::warn!("columnar fallback resume_index failed: {re}");
                    }
                }
            }
        } else {
            // Columnar not configured — legacy already loaded by resume_index()
            // above (columnar_warm is always false when ctx is None).
        }

        // FT6: restore checkpoint stack from disk if the file is present and
        // the stored HEAD matches the current worktree HEAD.  Both conditions
        // must hold to guarantee the stack is consistent with git state.
        // On mismatch or any error, the session starts with an empty stack
        // (graceful degradation — same behaviour as before FT6).
        {
            // Clone the path first to avoid holding an immutable borrow on
            // `session` while also passing `&mut session` to `try_restore`.
            let worktree = session.worktree_path.clone();
            let current_head = crate::session::Session::get_head_oid(&worktree).unwrap_or_default();
            crate::session::checkpoint_file::try_restore(&mut session, &worktree, &current_head);
        }
        // FT7: on reconnect, reindex any tracked files that were modified on
        // disk but not captured in a checkpoint commit.  Skipped for fresh
        // sessions (no dirty files possible).  Non-fatal — if the git diff
        // fails (e.g. detached HEAD), log a warning and continue with the
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
            session.frozen_verify_steps = Some(config.verify_steps);
        }

        // PhaseFT5: prefer columnar stats; fall back to legacy table.
        let symbols_indexed = session.engine().index_stats().map_or_else(
            || session.index().map_or(0, |idx| idx.rows.len()),
            |s| s.rows,
        );
        let sid = session_id.clone();

        // Write the initial timestamp so background pruners see this worktree as active.
        session.touch();
        drop(self.sessions.insert(session_id, session));

        Ok(ForgeQLResult::SourceOp(SourceOpResult {
            op: "use_source".to_string(),
            source_name: Some(source_name.to_string()),
            session_id: Some(sid),
            branches: Vec::new(),
            symbols_indexed: Some(symbols_indexed),
            resumed: wt_existed,
            message: if wt_existed {
                Some(format!(
                    "resumed existing worktree for fql/{branch}/{as_branch} — uncommitted changes preserved"
                ))
            } else {
                Some(format!("created new worktree for fql/{branch}/{as_branch}"))
            },
        }))
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
