use std::sync::Arc;

use anyhow::Result;
use tracing::info;

use crate::{
    git::{self as git, source::Source, worktree},
    result::{ForgeQLResult, QueryResult, SourceOpResult, SymbolMatch},
    session::Session,
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
    pub(super) fn refresh_source(&self, name: &str) -> Result<ForgeQLResult> {
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
    #[allow(clippy::too_many_lines)]
    pub(super) fn use_source(
        &mut self,
        source_name: &str,
        branch: &str,
        as_branch: &str,
    ) -> Result<ForgeQLResult> {
        // Validate: the alias must differ from the source branch name.
        // Equal names (e.g. USE src.main AS 'main') are meaningless —
        // the worktree would be named fql/main/main and the budget key
        // would be ambiguous.
        if as_branch == branch {
            return Err(crate::error::ForgeError::InvalidInput(format!(
                "alias '{as_branch}' must differ from the source branch '{branch}'"
            ))
            .into());
        }

        // Compute the budget branch key:
        //   - trunk branches (main/master) → use the alias (the feature name)
        //   - feature branches → use the branch itself (alias is just local)
        let budget_branch = if matches!(branch, "main" | "master") {
            as_branch
        } else {
            branch
        };

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
        // Composite key: base-branch.alias for filesystem (flat, no nesting)
        // and fql/base-branch/alias for the git branch name.
        // Using the fql/ namespace prefix avoids the git loose-ref collision
        // where refs/heads/<branch> already exists as a file when we try to
        // create refs/heads/<branch>/<alias>.
        // Slashes in branch or alias would create nested directories when used
        // in a filesystem path, so replace them with dashes for the worktree
        // directory name.
        let safe_branch = branch.replace('/', "-");
        let safe_alias = as_branch.replace('/', "-");
        let wt_name = format!("{safe_branch}.{safe_alias}");
        let git_branch = format!("fql/{branch}/{as_branch}");
        let wt_path = self.data_dir.join("worktrees").join(&wt_name);

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
            "anonymous",
            wt_path,
            source_name,
            branch,
            Arc::clone(&self.lang_registry),
        );
        session.custom_branch = Some(git_branch);
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
            if let Some(ref budget_cfg) = config.line_budget {
                // Sweep expired budget files before initialising the budget
                // for this session — clean up abandoned branches for free.
                crate::budget::sweep_expired(&self.data_dir);
                session.init_budget(budget_cfg, wt_existed, &self.data_dir, budget_branch);
            }
            session.frozen_verify_steps = Some(config.verify_steps);
        }

        let symbols_indexed = session.index().map_or(0, |idx| idx.rows.len());
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
}
