#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
//! `LegacyMemoryStorage` — `SymbolTable`-backed [`StorageEngine`] implementation.
//!
//! This is the default backend for Phase 01. It wraps the existing in-RAM
//! `SymbolTable` behind the [`StorageEngine`] trait, enabling all `exec_*`
//! paths to be written against the trait instead of the concrete type.
//!
//! The implementation is intentionally a near-verbatim lift of the hot loops
//! that previously lived in `exec_find.rs`. No algorithmic changes.

mod helpers;
mod prefilter;

/// Whether `field` is a known enrichment field name for ANY language.
///
/// Used by the engine to tell a misspelled WHERE field (matches nothing,
/// worth a hint) apart from a valid enrichment field with no matching rows.
#[must_use]
pub fn is_known_enrichment_field(field: &str) -> bool {
    prefilter::is_known_enrichment_field(field)
}

mod resolve;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use tracing::debug;

use crate::{
    ast::{
        cache::CachedIndex,
        enrich::macro_table::MacroTable,
        index::{IndexStats, SegmentBuildCtx, SymbolTable},
        lang::LanguageRegistry,
    },
    ir::{Clauses, GroupBy},
    result::SymbolMatch,
    workspace::Workspace,
};

use super::{FindPage, StorageEngine, SymbolLocation, row_to_location};

// -----------------------------------------------------------------------
// LegacyMemoryStorage
// -----------------------------------------------------------------------

/// In-RAM `StorageEngine` backed by the existing `SymbolTable`.
///
/// Holds the optional index and macro table.  All lifecycle operations
/// (build, persist, load) delegate to `SymbolTable::build` and
/// `CachedIndex`.
pub struct LegacyMemoryStorage {
    /// The full symbol index, populated after `build` or `load_from_cache`.
    table: Option<SymbolTable>,
    /// Macro definitions collected during the two-pass pipeline.
    macro_table: Option<MacroTable>,
    /// Language support registry — used by `build` and `reindex_files`.
    lang_registry: Arc<LanguageRegistry>,
    /// Worktree root the indexed paths sit under.
    ///
    /// Needed so an incremental re-index derives guard group IDs from the same
    /// repo-relative path the full build used. Without it the two disagree and
    /// the re-index ordinal key silently stops matching.
    worktree_root: Option<PathBuf>,
}

impl LegacyMemoryStorage {
    /// Create an empty storage instance.
    ///
    /// The index is `None` until `build` or `load_from_cache` is called.
    #[must_use]
    pub const fn new(lang_registry: Arc<LanguageRegistry>) -> Self {
        Self {
            table: None,
            macro_table: None,
            lang_registry,
            worktree_root: None,
        }
    }

    /// Return a reference to the symbol table, if the index has been built.
    #[must_use]
    pub const fn table(&self) -> Option<&SymbolTable> {
        self.table.as_ref()
    }

    /// Return a mutable reference to the symbol table, if the index has been built.
    #[must_use]
    pub const fn table_mut(&mut self) -> Option<&mut SymbolTable> {
        self.table.as_mut()
    }

    /// Build the index, optionally firing the inline columnar segment hook.
    ///
    /// `seg_ctx` is `Some` when shadow-write is enabled (passed by
    /// `Session::build_index`); `SymbolTable::build` then emits a segment per file.
    pub fn build_with_seg_ctx(
        &mut self,
        workspace: &Workspace,
        seg_ctx: Option<&SegmentBuildCtx>,
    ) -> Result<()> {
        self.worktree_root = Some(workspace.root().to_path_buf());
        let (table, macro_table) = SymbolTable::build(workspace, &self.lang_registry, seg_ctx)?;
        debug!(
            symbols = table.rows.len(),
            "LegacyMemoryStorage: index built"
        );
        self.table = Some(table);
        // Keep the macro table only where something will read it again.
        //
        // By the time `build` returns, the table has already done its whole
        // job: it fed macro expansion while each file was being indexed. The
        // one later reader is `persist_to_cache`, and the caller runs that
        // exactly when it passed no `seg_ctx` — the two conditions are the
        // same condition. So on the columnar path this used to hold one heap
        // allocation per macro definition, unread, until the session dropped
        // its legacy index at the very end.
        //
        // That is not a small residue on a large corpus. On the Linux kernel
        // it is 6,119,906 definitions and 6.5 GiB, held across the whole
        // overlay build — five minutes of work that never looks at it, and
        // five minutes during which every other phase's allocations sit on
        // top of it. Incremental reindex does not need it either:
        // `SymbolTable::reindex_files` takes no macro table and never has.
        self.macro_table = seg_ctx.is_none().then_some(macro_table);
        Ok(())
    }
}
// Fast-path GROUP BY helper (moved from exec_find.rs)
// -----------------------------------------------------------------------

/// Try to answer a `FIND symbols GROUP BY <field>` query entirely from
/// pre-aggregated `IndexStats` without scanning individual rows.
///
/// Returns `(pre-filtered results, remaining clauses)` when the fast path
/// applies, or `None` to fall through to the normal scan.
fn try_group_by_stats_fast_path(
    index: &SymbolTable,
    clauses: &Clauses,
) -> Option<(Vec<SymbolMatch>, Clauses)> {
    // Must have a GROUP BY on a supported field, no WHERE filters, no globs.
    if !clauses.where_predicates.is_empty()
        || clauses.in_glob.is_some()
        || !clauses.exclude_globs.is_empty()
    {
        return None;
    }

    let group_field = match &clauses.group_by {
        Some(GroupBy::Field(f)) => f.clone(),
        _ => return None,
    };

    // IndexStats keys are interned u32 IDs — resolve to strings at output time.
    let map: Vec<(String, usize)> = match group_field.as_str() {
        "fql_kind" => index
            .stats
            .resolved_by_fql_kind(&index.strings)
            .into_iter()
            .collect(),
        "language" | "lang" => index
            .stats
            .resolved_by_language(&index.strings)
            .into_iter()
            .collect(),
        _ => return None,
    };

    let results: Vec<SymbolMatch> = map
        .into_iter()
        .map(|(key, count)| {
            let fql_kind = if group_field == "fql_kind" {
                Some(key.clone())
            } else {
                None
            };
            let language = if group_field == "language" || group_field == "lang" {
                Some(key.clone())
            } else {
                None
            };
            SymbolMatch {
                name: key,
                node_kind: None,
                fql_kind,
                language,
                path: None,
                line: None,
                usages_count: None,
                fields: HashMap::new(),
                count: Some(count),
                node_id: None,
                rev: None,
            }
        })
        .collect();

    // Remaining clauses: HAVING, ORDER BY, OFFSET, LIMIT — group_by already consumed.
    let remaining = Clauses {
        where_predicates: Vec::new(),
        having_predicates: clauses.having_predicates.clone(),
        order_by: clauses.order_by.clone(),
        group_by: None,
        limit: clauses.limit,
        offset: clauses.offset,
        in_glob: None,
        exclude_globs: Vec::new(),
        depth: None,
    };

    Some((results, remaining))
}

// -----------------------------------------------------------------------
// StorageEngine impl
// -----------------------------------------------------------------------

impl StorageEngine for LegacyMemoryStorage {
    fn backend_name(&self) -> &'static str {
        "legacy"
    }

    // ---- read-only queries ---------------------------------------------

    fn find_symbols(&self, clauses: &Clauses, root: &Path) -> Result<FindPage> {
        let index = self
            .table
            .as_ref()
            .ok_or_else(|| anyhow!("session index not ready — retry USE"))?;
        let configs = self.lang_registry.configs();

        // Fast path: GROUP BY fql_kind / language with no WHERE/IN/EXCLUDE
        if let Some((mut results, remaining)) = try_group_by_stats_fast_path(index, clauses) {
            let total = crate::filter::apply_clauses_counted(&mut results, &remaining);
            return Ok(FindPage::of(results, total));
        }

        let (mut results, remaining, capped_total) =
            prefilter::find_symbols_prefilter(index, clauses, root, &configs)?;
        prefilter::validate_order_by_field(&remaining, &results, &configs)?;
        let total = crate::filter::apply_clauses_counted(&mut results, &remaining);
        Ok(FindPage::of(results, capped_total.unwrap_or(total)))
    }

    fn find_usages(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
        bound: Option<crate::storage::UsageBound>,
    ) -> Result<crate::storage::UsagePage> {
        let index = self
            .table
            .as_ref()
            .ok_or_else(|| anyhow!("session index not ready — retry USE"))?;
        let configs = self.lang_registry.configs();

        let sites = crate::ast::query::find_usages(index, name);
        // The row budget, mirroring the on-disk backend's usages check — one
        // site becomes exactly one result row.
        let max_rows = crate::storage::columnar::columnar_storage::find_max_rows();
        if sites.len() > max_rows {
            return Err(
                crate::storage::columnar::columnar_storage::usages_budget_exceeded(
                    sites.len(),
                    max_rows,
                ),
            );
        }
        // The page is cut from the sites, so a site outside it is never built.
        let views: Vec<crate::storage::SiteView<'_>> = sites
            .iter()
            .filter(|site| {
                helpers::passes_glob_filter(index.strings.paths.get(site.path_id), clauses, root)
            })
            .map(|site| crate::storage::SiteView {
                name,
                path: index.strings.paths.get(site.path_id),
                line: site.line,
                // This backend tags no role, so the row it builds carries an
                // empty map and both readers miss on `role` alike.
                role: None,
            })
            .collect();

        // Strip IN/EXCLUDE from clauses — already applied above.
        let remaining = Clauses {
            in_glob: None,
            exclude_globs: Vec::new(),
            ..clauses.clone()
        };

        prefilter::validate_order_by_field(&remaining, &views, &configs)?;
        Ok(crate::storage::usage_page_from_sites(
            views, &remaining, bound, None,
        ))
    }

    // ---- symbol resolution (used by SHOW paths) ------------------------

    fn resolve_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        let index = self
            .table
            .as_ref()
            .ok_or_else(|| anyhow!("session index not ready — retry USE"))?;
        resolve::resolve_symbol(index, name, clauses, root)
            .map(|row| Some(row_to_location(row, index)))
    }

    fn resolve_type_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        let index = self
            .table
            .as_ref()
            .ok_or_else(|| anyhow!("session index not ready — retry USE"))?;
        resolve::resolve_type_symbol(index, name, clauses, root)
            .map(|row| Some(row_to_location(row, index)))
    }

    fn resolve_body_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        let index = self
            .table
            .as_ref()
            .ok_or_else(|| anyhow!("session index not ready — retry USE"))?;
        resolve::resolve_body_symbol(index, name, clauses, root)
            .map(|row| Some(row_to_location(row, index)))
    }

    // ---- aggregates ----------------------------------------------------

    fn index_stats(&self) -> Option<&IndexStats> {
        self.table.as_ref().map(|t| &t.stats)
    }

    // ---- lifecycle -----------------------------------------------------

    fn build(&mut self, workspace: &Workspace) -> Result<()> {
        self.build_with_seg_ctx(workspace, None)
    }

    fn reindex_files(&mut self, paths: &[PathBuf]) -> Result<()> {
        let lang_registry = Arc::clone(&self.lang_registry);
        let root = self.worktree_root.clone();
        let table = self
            .table
            .as_mut()
            .ok_or_else(|| anyhow!("cannot reindex: no index built yet"))?;
        table.reindex_files(paths, &lang_registry, root.as_deref())
    }

    fn purge_file(&mut self, path: &Path) -> Result<()> {
        if let Some(ref mut table) = self.table {
            table.purge_file(path);
        }
        Ok(())
    }

    fn persist_to_cache(
        &mut self,
        worktree_path: &Path,
        commit_hash: &str,
        source_name: &str,
    ) -> Result<()> {
        // Take ownership for the round-trip through CachedIndex, then put back.
        let table = self
            .table
            .take()
            .ok_or_else(|| anyhow!("cannot save: no index built yet"))?;
        let macro_table = self.macro_table.take().unwrap_or_default();
        let cached =
            CachedIndex::from_table_and_macros(table, macro_table, commit_hash, source_name);
        let cache_path = worktree_path.join(".forgeql-index");
        cached.save(&cache_path)?;
        // Restore ownership.
        let (table, macro_table) = cached.into_table_and_macros();
        self.table = Some(table);
        self.macro_table = Some(macro_table);
        Ok(())
    }

    fn load_from_cache(
        &mut self,
        worktree_path: &Path,
        head_oid: &str,
        source_name: &str,
    ) -> Result<bool> {
        self.worktree_root = Some(worktree_path.to_path_buf());
        let cache_path = worktree_path.join(".forgeql-index");
        match CachedIndex::load(&cache_path) {
            Ok(cached)
                if cached.commit_hash == head_oid
                    && (cached.source_name.is_empty() || cached.source_name == source_name) =>
            {
                debug!(
                    commit = %head_oid,
                    "LegacyMemoryStorage: cache hit — restoring index from disk"
                );
                let (table, macro_table) = cached.into_table_and_macros();
                self.table = Some(table);
                self.macro_table = Some(macro_table);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn drop_stored_index(&mut self) {
        self.table = None;
        self.macro_table = None;
    }

    fn has_index(&self) -> bool {
        self.table.is_some()
    }

    // ---- SHOW helpers --------------------------------------------------

    fn locate_definition(&self, name: &str) -> Option<(std::path::PathBuf, usize)> {
        let table = self.table.as_ref()?;
        table
            .find_def(name)
            .map(|row| (table.path_of(row).to_path_buf(), row.line))
    }

    fn show_outline_for_file(
        &self,
        workspace: &crate::workspace::Workspace,
        file: &str,
        _all: bool,
    ) -> Result<serde_json::Value> {
        let table = self
            .table
            .as_ref()
            .ok_or_else(|| anyhow!("session index not ready — retry USE"))?;
        crate::ast::show::show_outline(table, workspace, file)
    }
}
