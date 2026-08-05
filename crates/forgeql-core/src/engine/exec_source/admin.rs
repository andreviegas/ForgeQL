//! The source admin verbs: `CREATE SOURCE`, `REFRESH SOURCE` and `VACUUM`.
//!
//! These act on the registry and on stored index data rather than on a
//! session — registering a repository, re-reading its refs, and reclaiming
//! disk from superseded index versions.
//!
//! `vacuum_report` is `pub` rather than crate-internal: it is `VACUUM`'s
//! shared implementation and the CLI calls it directly for the dry-run path.

use std::sync::Arc;

use anyhow::Result;
use tracing::info;

use crate::{
    git::source::Source,
    result::{ForgeQLResult, QueryResult, SourceOpResult, SymbolMatch},
};

use crate::engine::{ForgeQLEngine, load_verify_config, warm};

impl ForgeQLEngine {
    pub(in crate::engine) fn create_source(
        &mut self,
        name: &str,
        url: &str,
    ) -> Result<ForgeQLResult> {
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
                base_commit: None,
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
                match warm::pick_warm_targets(registered, &policy) {
                    Ok(targets) => warm::spawn_warmer(
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
            base_commit: None,
            message: template_msg,
        }))
    }

    /// `REFRESH SOURCE 'name'` — fetch all remotes on an existing bare repo.
    pub(in crate::engine) fn refresh_source(&self, name: &str) -> Result<ForgeQLResult> {
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
                let moved: Vec<warm::WarmTarget> = after
                    .iter()
                    .filter(|(b, sha)| before.get(*b) != Some(*sha))
                    .map(|(b, sha)| warm::WarmTarget {
                        branch: b.clone(),
                        commit_sha: sha.clone(),
                    })
                    .collect();
                if !moved.is_empty() {
                    warm::spawn_warmer(
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
            base_commit: None,
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
    pub(in crate::engine) fn vacuum(
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
                    rev: None,
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
            found_rev: None,
        }))
    }
}
