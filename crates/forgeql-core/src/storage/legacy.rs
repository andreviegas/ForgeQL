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

use super::{StorageEngine, SymbolLocation, row_to_location};

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
        let (table, macro_table) = SymbolTable::build(workspace, &self.lang_registry, seg_ctx)?;
        debug!(
            symbols = table.rows.len(),
            "LegacyMemoryStorage: index built"
        );
        self.table = Some(table);
        self.macro_table = Some(macro_table);
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

    fn find_symbols(&self, clauses: &Clauses, root: &Path) -> Result<Vec<SymbolMatch>> {
        let index = self
            .table
            .as_ref()
            .ok_or_else(|| anyhow!("session index not ready — retry USE"))?;
        let configs = self.lang_registry.configs();

        // Fast path: GROUP BY fql_kind / language with no WHERE/IN/EXCLUDE
        if let Some((mut results, remaining)) = try_group_by_stats_fast_path(index, clauses) {
            crate::filter::apply_clauses(&mut results, &remaining);
            return Ok(results);
        }

        let (mut results, remaining) =
            prefilter::find_symbols_prefilter(index, clauses, root, &configs);
        prefilter::validate_order_by_field(&remaining, &results, &configs)?;
        crate::filter::apply_clauses(&mut results, &remaining);
        Ok(results)
    }

    fn find_usages(&self, name: &str, clauses: &Clauses, root: &Path) -> Result<Vec<SymbolMatch>> {
        let index = self
            .table
            .as_ref()
            .ok_or_else(|| anyhow!("session index not ready — retry USE"))?;
        let configs = self.lang_registry.configs();

        let sites = crate::ast::query::find_usages(index, name);
        let mut results: Vec<SymbolMatch> = sites
            .iter()
            .filter(|site| {
                helpers::passes_glob_filter(index.strings.paths.get(site.path_id), clauses, root)
            })
            .map(|site| SymbolMatch {
                name: name.to_string(),
                node_kind: None,
                fql_kind: None,
                language: None,
                path: Some(index.strings.paths.get(site.path_id).to_path_buf()),
                line: Some(site.line),
                usages_count: None,
                fields: std::collections::HashMap::new(),
                count: None,
                node_id: None,
            })
            .collect();

        // Strip IN/EXCLUDE from clauses — already applied above.
        let remaining = Clauses {
            in_glob: None,
            exclude_globs: Vec::new(),
            ..clauses.clone()
        };

        prefilter::validate_order_by_field(&remaining, &results, &configs)?;
        crate::filter::apply_clauses(&mut results, &remaining);
        Ok(results)
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
        let table = self
            .table
            .as_mut()
            .ok_or_else(|| anyhow!("cannot reindex: no index built yet"))?;
        table.reindex_files(paths, &self.lang_registry)
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
