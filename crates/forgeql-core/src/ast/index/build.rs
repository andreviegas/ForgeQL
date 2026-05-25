//! Build, query and incremental-update methods for [`super::SymbolTable`].
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rayon::prelude::*;
use tracing::{debug, info, warn};

use crate::ast::enrich::default_enrichers;
use crate::ast::enrich::macro_table::MacroTable;
use crate::ast::intern::ColumnarTable;
use crate::ast::lang::LanguageRegistry;
use crate::ast::trigram::TrigramIndex;
use crate::error::ForgeError;
use crate::workspace::Workspace;

use super::file_indexer::{IndexContext, collect_macro_defs_for_file, index_file};
use super::{
    IndexRow, IndexStats, MemEstimate, SegmentBuildCtx, SymbolTable, UsageSite, reassign_intern_ids,
};
/// Builder that holds disjoint borrows of all secondary-index fields in
/// `SymbolTable`, enabling `insert` to read `strings` (immutable borrow) while
/// simultaneously mutating the index maps, stats, and trigram index.
///
/// Constructing the builder from individual field borrows (rather than `&mut self`)
/// lets the borrow checker track the accesses as disjoint, which a `&mut self`
/// method cannot do.
struct SecondaryIndexBuilder<'a> {
    name_index: &'a mut HashMap<u32, Vec<u32>>,
    kind_index: &'a mut HashMap<u32, Vec<u32>>,
    fql_kind_index: &'a mut HashMap<u32, Vec<u32>>,
    stats: &'a mut IndexStats,
    trigram_index: &'a mut TrigramIndex,
    strings: &'a ColumnarTable,
}

impl SecondaryIndexBuilder<'_> {
    fn insert(&mut self, row: &IndexRow, idx: u32) {
        self.name_index.entry(row.name_id).or_default().push(idx);
        self.kind_index
            .entry(row.node_kind_id)
            .or_default()
            .push(idx);
        if !self.strings.fql_kinds.get(row.fql_kind_id).is_empty() {
            self.fql_kind_index
                .entry(row.fql_kind_id)
                .or_default()
                .push(idx);
            *self.stats.by_fql_kind.entry(row.fql_kind_id).or_insert(0) += 1;
        }
        if !self.strings.languages.get(row.language_id).is_empty() {
            *self.stats.by_language.entry(row.language_id).or_insert(0) += 1;
        }
        // `get` returns a `&str` borrowed from the pool — zero allocation.
        self.trigram_index
            .insert(idx as usize, self.strings.names.get(row.name_id));
    }
}

impl SymbolTable {
    /// Build a `SymbolTable` by parsing all supported files in the workspace.
    ///
    /// Files are parsed and enriched **in parallel** using rayon.  Each thread
    /// creates its own `Parser` and enricher set, producing a per-file table.
    /// Results are merged sequentially, then post-pass enrichment runs.
    ///
    /// # Errors
    /// Returns `Err` if the tree-sitter language cannot be set.
    #[allow(clippy::too_many_lines)]
    pub fn build(
        workspace: &Workspace,
        lang_registry: &LanguageRegistry,
        seg_ctx: Option<&SegmentBuildCtx>,
    ) -> Result<(Self, MacroTable)> {
        // 1 — collect file paths that have a registered language.
        let paths: Vec<PathBuf> = workspace
            .files()
            .filter(|p| lang_registry.language_for_path(p).is_some())
            .collect();

        debug!(files = paths.len(), "indexing files in parallel");

        // Pass 1 — collect macro definitions (parallel, per-file, then merged).
        let t_build = std::time::Instant::now();
        let t_step = std::time::Instant::now();
        let macro_table: MacroTable = paths
            .par_iter()
            .filter_map(|path| {
                let lang = lang_registry.language_for_path(path)?;
                let _ = lang.macro_expander()?;
                let mut parser = tree_sitter::Parser::new();
                if parser.set_language(&lang.tree_sitter_language()).is_err() {
                    return None;
                }
                match collect_macro_defs_for_file(&mut parser, path, lang.as_ref()) {
                    Ok(defs) if !defs.is_empty() => {
                        let mut local = MacroTable::new();
                        for def in defs {
                            local.insert(def);
                        }
                        Some(local)
                    }
                    _ => None,
                }
            })
            .reduce(MacroTable::new, |mut acc, local| {
                acc.merge_from(local);
                acc
            });

        info!(
            ms = t_step.elapsed().as_millis(),
            macro_defs = macro_table.def_count(),
            "TIMING build pass1: macro collection"
        );

        // ── Columnar fast-path ─────────────────────────────────────────────
        // When a SegmentBuildCtx is provided, segments are written inline
        // per-file during index_file() (including per-file post_pass).
        // No merge, full-table post_pass, or populate_usage_counts is needed —
        // the columnar engine never queries the SymbolTable after build.
        // This eliminates the ~2-minute sequential bottleneck on large repos.
        if seg_ctx.is_some() {
            let t_fast = std::time::Instant::now();
            paths.par_iter().for_each(|path| {
                let Some(lang) = lang_registry.language_for_path(path) else {
                    return;
                };
                let mut parser = tree_sitter::Parser::new();
                if parser.set_language(&lang.tree_sitter_language()).is_err() {
                    warn!(path = %path.display(), "columnar fast-path: failed to set language");
                    return;
                }
                let enrichers = default_enrichers();
                let mut file_table = Self::default();
                {
                    let mut ctx = IndexContext {
                        path,
                        language: lang.as_ref(),
                        enrichers: &enrichers,
                        macro_table: Some(&macro_table),
                        table: &mut file_table,
                    };
                    match index_file(&mut parser, &mut ctx, seg_ctx) {
                        Ok(count) => {
                            debug!(path = %path.display(), rows = count, "indexed (columnar fast-path)");
                        }
                        Err(e) => warn!(path = %path.display(), "skipping file: {e}"),
                    }
                }
                // file_table dropped here — no merge needed for columnar.
            });
            info!(
                ms = t_fast.elapsed().as_millis(),
                files = paths.len(),
                "TIMING build total: SymbolTable::build (columnar fast-path, no merge)"
            );
            return Ok((Self::default(), macro_table));
        }

        // Pass 2 — parse + enrich each file in parallel, merging via tree
        // reduction so merges also happen across multiple cores.
        let t_step = std::time::Instant::now();
        let mut table: Self = paths
            .par_iter()
            .filter_map(|path| {
                let lang = lang_registry.language_for_path(path)?;
                let mut parser = tree_sitter::Parser::new();
                if parser.set_language(&lang.tree_sitter_language()).is_err() {
                    warn!(path = %path.display(), "failed to set tree-sitter language");
                    return None;
                }
                let enrichers = default_enrichers();
                let mut file_table = Self::default();

                {
                    let mut ctx = IndexContext {
                        path,
                        language: lang.as_ref(),
                        enrichers: &enrichers,
                        macro_table: Some(&macro_table),
                        table: &mut file_table,
                    };
                    match index_file(&mut parser, &mut ctx, seg_ctx) {
                        Ok(count) => {
                            debug!(
                                path = %workspace.relative(path).display(),
                                rows = count,
                                "indexed"
                            );
                        }
                        Err(err) => {
                            warn!(path = %path.display(), error = %err, "skipping file");
                            return None;
                        }
                    }
                }
                Some(file_table)
            })
            .reduce(Self::default, |mut acc, file_table| {
                acc.merge(file_table);
                acc
            });

        info!(
            ms = t_step.elapsed().as_millis(),
            rows = table.rows.len(),
            "TIMING build pass2: parse + reduce"
        );

        // Post-pass — run post_pass for each enricher (aggregation, cross-row metrics).
        // `None` scope = process the entire table (full build).
        let t_step = std::time::Instant::now();
        let enrichers = default_enrichers();
        for enricher in &enrichers {
            enricher.post_pass(&mut table, None);
        }
        info!(ms = t_step.elapsed().as_millis(), "TIMING build post_pass");

        // Precompute per-row usages_count from the completed usages map.
        let t_step = std::time::Instant::now();
        table.populate_usage_counts();
        info!(
            ms = t_step.elapsed().as_millis(),
            rows = table.rows.len(),
            usages = table.usages.values().map(Vec::len).sum::<usize>(),
            "TIMING build populate_usage_counts"
        );

        info!(
            ms = t_build.elapsed().as_millis(),
            "TIMING build total: SymbolTable::build"
        );
        Ok((table, macro_table))
    }

    /// Merge another `SymbolTable` into this one.
    ///
    /// Row indices in `name_index` and `kind_index` are offset by the
    /// current row count so they remain correct after the merge.
    fn merge(&mut self, other: Self) {
        let offset = self.rows.len();

        // Merge rows and fix secondary indexes.
        for (i, mut row) in other.rows.into_iter().enumerate() {
            let abs = offset + i;
            debug_assert!(
                u32::try_from(abs).is_ok(),
                "row index exceeds u32::MAX during merge"
            );
            let abs_u32 = u32::try_from(abs).unwrap_or(u32::MAX);
            // Remap IDs: values from `other.strings` are not valid in `self.strings`.
            reassign_intern_ids(&other.strings, &mut self.strings, &mut row);
            SecondaryIndexBuilder {
                name_index: &mut self.name_index,
                kind_index: &mut self.kind_index,
                fql_kind_index: &mut self.fql_kind_index,
                stats: &mut self.stats,
                trigram_index: &mut self.trigram_index,
                strings: &self.strings,
            }
            .insert(&row, abs_u32);
            self.rows.push(row);
        }

        // Merge usage sites — remap path_id from other.strings.paths into self.strings.paths.
        for (name, sites) in other.usages {
            let remapped: Vec<UsageSite> = sites
                .into_iter()
                .map(|s| {
                    let path = other.strings.paths.get(s.path_id);
                    let path_id = self.strings.paths.intern(path);
                    UsageSite { path_id, ..s }
                })
                .collect();
            self.usages.entry(name).or_default().extend(remapped);
        }
    }

    /// Append a row and update the secondary indexes.
    ///
    /// The row must have pre-filled `name_id`, `node_kind_id`, `fql_kind_id`,
    /// `language_id`, and `path_id` — set by `table.strings.intern_row()` in
    /// `collect_nodes` before calling this method.
    pub fn push_row(&mut self, row: IndexRow) {
        let index = self.rows.len();
        debug_assert!(
            u32::try_from(index).is_ok(),
            "row index exceeds u32::MAX in push_row"
        );
        let index_u32 = u32::try_from(index).unwrap_or(u32::MAX);
        SecondaryIndexBuilder {
            name_index: &mut self.name_index,
            kind_index: &mut self.kind_index,
            fql_kind_index: &mut self.fql_kind_index,
            stats: &mut self.stats,
            trigram_index: &mut self.trigram_index,
            strings: &self.strings,
        }
        .insert(&row, index_u32);
        self.rows.push(row);
    }

    /// Rebuild all secondary indexes and stats from `self.rows` in O(N).
    ///
    /// Used after cache load (when the pool is restored from `CachedIndex.strings`)
    /// and after [`purge_file`].  Clears all secondary indexes before rebuilding.
    pub fn rebuild_indexes_from_rows(&mut self) {
        self.name_index.clear();
        self.kind_index.clear();
        self.fql_kind_index.clear();
        self.trigram_index.clear();
        self.stats.by_fql_kind.clear();
        self.stats.by_language.clear();
        for (index, row) in self.rows.iter().enumerate() {
            let index_u32 = u32::try_from(index).unwrap_or(u32::MAX);
            SecondaryIndexBuilder {
                name_index: &mut self.name_index,
                kind_index: &mut self.kind_index,
                fql_kind_index: &mut self.fql_kind_index,
                stats: &mut self.stats,
                trigram_index: &mut self.trigram_index,
                strings: &self.strings,
            }
            .insert(row, index_u32);
        }
    }

    // ------------------------------------------------------------------
    // ------------------------------------------------------------------
    // Intern-pool accessors — resolve row IDs to string/path slices.
    // These are zero-copy; the returned references borrow from `self.strings`.
    // ------------------------------------------------------------------

    /// Resolve `row.name_id` to its symbol name.
    #[must_use]
    #[inline]
    pub fn name_of(&self, row: &IndexRow) -> &str {
        self.strings.names.get(row.name_id)
    }

    /// Look up a field value (by string key) in an interned `HashMap<u32, u32>`.
    #[must_use]
    #[inline]
    pub fn field_str<'a>(&'a self, fields: &HashMap<u32, u32>, key: &str) -> Option<&'a str> {
        self.strings.field_str(fields, key)
    }

    /// Convert an interned `HashMap<u32, u32>` back to a human-readable `HashMap<String, String>`.
    #[must_use]
    #[inline]
    pub fn resolve_fields(&self, fields: &HashMap<u32, u32>) -> HashMap<String, String> {
        self.strings.resolve_fields(fields)
    }

    /// Resolve `row.node_kind_id` to its raw tree-sitter node kind.
    #[must_use]
    #[inline]
    pub fn node_kind_of(&self, row: &IndexRow) -> &str {
        self.strings.node_kinds.get(row.node_kind_id)
    }

    /// Resolve `row.fql_kind_id` to its universal FQL kind string.
    #[must_use]
    #[inline]
    pub fn fql_kind_of(&self, row: &IndexRow) -> &str {
        self.strings.fql_kinds.get(row.fql_kind_id)
    }

    /// Resolve `row.language_id` to its language identifier string.
    #[must_use]
    #[inline]
    pub fn language_of(&self, row: &IndexRow) -> &str {
        self.strings.languages.get(row.language_id)
    }

    /// Resolve `row.path_id` to its source file path.
    #[must_use]
    #[inline]
    pub fn path_of(&self, row: &IndexRow) -> &std::path::Path {
        self.strings.paths.get(row.path_id)
    }

    /// Fill `IndexRow::usages_count` for every row from the `usages` map.
    ///
    /// Must be called after both `rows` and `usages` are fully populated.
    /// Skips rows where `usages_count` is already non-zero (idempotent on
    /// indexes built with a version that persists the field).
    pub fn populate_usage_counts(&mut self) {
        for i in 0..self.rows.len() {
            // Extract the bare name suffix (after last `::`) as an owned
            // String to release the immutable borrow on `self.strings`
            // before we look up `self.usages`.
            let usages_key = {
                let n = self.strings.names.get(self.rows[i].name_id);
                n.rsplit("::").next().unwrap_or(n).to_owned()
            };
            let count = self
                .usages
                .get(&usages_key)
                .map_or(0, |v| u32::try_from(v.len()).unwrap_or(u32::MAX));
            self.rows[i].usages_count = count;
        }
    }
    pub fn add_usage(&mut self, name: String, path: &Path, byte_range: Range<usize>, line: usize) {
        let path_id = self.strings.paths.intern(path);
        self.usages.entry(name).or_default().push(UsageSite {
            path_id,
            byte_range,
            line,
        });
    }

    /// Look up all usage sites for a symbol name.
    #[must_use]
    pub fn find_usages(&self, name: &str) -> &[UsageSite] {
        self.usages.get(name).map_or(&[], Vec::as_slice)
    }

    /// Look up the primary definition row for a symbol by name.
    ///
    /// When multiple rows share a name, returns the last-indexed row
    /// (last-write-wins, matching v1 behaviour).
    #[must_use]
    pub fn find_def(&self, name: &str) -> Option<&IndexRow> {
        let id = self.strings.names.get_id(name)?;
        self.name_index
            .get(&id)?
            .last()
            .map(|&idx| &self.rows[idx as usize])
    }

    /// Return all definition rows for a given symbol name.
    ///
    /// Unlike [`find_def`] which returns only the last-indexed row,
    /// this returns every row matching the name — essential for
    /// multi-language workspaces where the same name may exist in
    /// different files/languages.
    #[must_use]
    pub fn find_all_defs(&self, name: &str) -> Vec<&IndexRow> {
        let Some(id) = self.strings.names.get_id(name) else {
            return Vec::new();
        };
        self.name_index.get(&id).map_or_else(Vec::new, |indices| {
            indices
                .iter()
                .map(|&idx| &self.rows[idx as usize])
                .collect()
        })
    }

    /// Return up to `max` symbol names that are similar to `query`.
    ///
    /// Uses case-insensitive prefix matching and substring matching to
    /// find plausible alternatives when a symbol lookup fails.
    #[must_use]
    pub fn suggest_similar(&self, query: &str, max: usize) -> Vec<&str> {
        let lower = query.to_ascii_lowercase();
        let mut results: Vec<&str> = self
            .strings
            .names
            .iter_str()
            .filter(|name| {
                let nl = name.to_ascii_lowercase();
                nl.starts_with(&lower) || lower.starts_with(&nl) || nl.contains(&lower)
            })
            .take(max)
            .collect();
        results.sort_unstable();
        results.truncate(max);
        results
    }

    /// Return an iterator over all rows matching a tree-sitter node kind.
    pub fn rows_by_kind(&self, kind: &str) -> impl Iterator<Item = &IndexRow> {
        self.strings
            .node_kinds
            .get_id(kind)
            .and_then(|id| self.kind_index.get(&id))
            .into_iter()
            .flat_map(|v| v.iter().map(|&i| &self.rows[i as usize]))
    }

    /// Return an iterator over all rows matching a universal FQL kind.
    pub fn rows_by_fql_kind(&self, fql_kind: &str) -> impl Iterator<Item = &IndexRow> {
        self.strings
            .fql_kinds
            .get_id(fql_kind)
            .and_then(|id| self.fql_kind_index.get(&id))
            .into_iter()
            .flat_map(|v| v.iter().map(|&i| &self.rows[i as usize]))
    }

    /// Return an iterator over all rows with an exact name match.
    ///
    /// O(1) lookup via `name_index`; suitable for wildcard-free `LIKE` and
    /// fully-anchored `MATCHES` predicates.
    pub fn rows_by_name(&self, name: &str) -> impl Iterator<Item = &IndexRow> {
        self.strings
            .names
            .get_id(name)
            .and_then(|id| self.name_index.get(&id))
            .into_iter()
            .flat_map(|v| v.iter().map(|&i| &self.rows[i as usize]))
    }

    /// Return candidate rows whose names contain `substr` according to the
    /// trigram index.  The result is a **superset** — callers must still
    /// verify the full predicate.  Returns `None` when `substr` is too short
    /// (< 3 bytes) to use trigrams.
    #[must_use]
    pub fn trigram_candidates(&self, substr: &str) -> Option<Vec<&IndexRow>> {
        let ids = self.trigram_index.candidates(substr)?;
        Some(ids.into_iter().map(|i| &self.rows[i]).collect())
    }

    // -------------------------------------------------------------------
    // Memory diagnostics
    // -------------------------------------------------------------------

    /// Compute a breakdown of approximate heap consumption (in bytes) for
    /// all major components of this `SymbolTable`.
    ///
    /// All figures are **estimates** using `std::mem::size_of` for fixed-size
    /// parts plus per-element heap allocations for `String`, `Vec`, and
    /// `HashMap`.  `HashMap` overhead uses 56 B/bucket as a conservative
    /// approximation for `std::collections::HashMap` on 64-bit platforms.
    #[must_use]
    pub fn mem_estimate(&self) -> MemEstimate {
        // --- rows: Vec<IndexRow> ---
        // Each IndexRow has fixed fields + one HashMap<u32,u32> (fields).
        // After the u32-key/value interning: each entry is 8 bytes + bucket overhead.
        let row_fixed = std::mem::size_of::<IndexRow>(); // byte_range, line, usages_count, ids
        let row_fields_heap: usize = self
            .rows
            .iter()
            .map(|r| {
                // 8 bytes per (u32,u32) entry + ~56 bytes/bucket overhead.
                r.fields.len() * 8 + r.fields.capacity() * 56
            })
            .sum();
        let rows_bytes = self.rows.capacity() * row_fixed + row_fields_heap;

        // --- usages: HashMap<String, Vec<UsageSite>> ---
        // UsageSite is now fully fixed-size (path_id: u32, byte_range, line) — no heap per site.
        let usage_site_fixed = std::mem::size_of::<UsageSite>();
        let usages_bytes: usize = self
            .usages
            .iter()
            .map(|(k, v)| {
                k.capacity() + v.capacity() * usage_site_fixed + 56 // bucket overhead
            })
            .sum::<usize>()
            + self.usages.capacity() * 56;

        // --- name_index: HashMap<u32, Vec<u32>> ---
        let name_index_bytes: usize = self
            .name_index
            .values()
            .map(|v| v.capacity() * 4 + 24 + 56)
            .sum::<usize>()
            + self.name_index.capacity() * 56;

        // --- kind_index ---
        let kind_index_bytes: usize = self
            .kind_index
            .values()
            .map(|v| v.capacity() * 4 + 24 + 56)
            .sum::<usize>()
            + self.kind_index.capacity() * 56;

        // --- fql_kind_index ---
        let fql_kind_index_bytes: usize = self
            .fql_kind_index
            .values()
            .map(|v| v.capacity() * 4 + 24 + 56)
            .sum::<usize>()
            + self.fql_kind_index.capacity() * 56;

        // --- trigram_index: HashMap<[u8;3], Vec<usize>> ---
        let trigram_bytes: usize = self
            .trigram_index
            .posting_iter()
            .map(|v| v.capacity() * 8 + 24 + 56)
            .sum::<usize>()
            + self.trigram_index.posting_len() * 56;

        // --- strings: ColumnarTable ---
        // StringPool: Vec<String> + HashMap<String,u32>
        let string_pool_bytes = |pool: &crate::ast::intern::StringPool| -> usize {
            pool.iter().map(|s| s.len() + 24).sum::<usize>() // Vec<String> heap
                + pool.len() * 56 // lookup HashMap buckets (key cloned)
                + pool.iter().map(String::len).sum::<usize>() // key copies in lookup
        };
        let path_pool_bytes: usize = {
            let p = &self.strings.paths;
            p.iter().map(|p| p.as_os_str().len() + 24).sum::<usize>()
                + p.len() * 56
                + p.iter().map(|p| p.as_os_str().len()).sum::<usize>()
        };
        let strings_bytes = string_pool_bytes(&self.strings.names)
            + string_pool_bytes(&self.strings.node_kinds)
            + string_pool_bytes(&self.strings.fql_kinds)
            + string_pool_bytes(&self.strings.languages)
            + path_pool_bytes
            + string_pool_bytes(&self.strings.field_keys)
            + string_pool_bytes(&self.strings.field_values);

        MemEstimate {
            rows_bytes,
            rows_count: self.rows.len(),
            usages_bytes,
            usages_symbols: self.usages.len(),
            usages_sites: self.usages.values().map(Vec::len).sum(),
            name_index_bytes,
            kind_index_bytes,
            fql_kind_index_bytes,
            trigram_bytes,
            trigram_entries: self.trigram_index.posting_len(),
            strings_bytes,
            strings_names: self.strings.names.len(),
            strings_paths: self.strings.paths.len(),
        }
    }

    // -------------------------------------------------------------------
    // Incremental update
    // -------------------------------------------------------------------

    /// Remove all entries associated with `path` and rebuild secondary indexes.
    pub fn purge_file(&mut self, path: &Path) {
        let path_id = self.strings.paths.get_id(path);
        if let Some(pid) = path_id {
            self.rows.retain(|row| row.path_id != pid);
        }

        // Rebuild secondary indexes from scratch.
        self.rebuild_indexes_from_rows();

        if let Some(pid) = path_id {
            for sites in self.usages.values_mut() {
                sites.retain(|usage| usage.path_id != pid);
            }
        }
        self.usages.retain(|_, sites| !sites.is_empty());
    }
    /// # Errors
    /// Returns an error if parsing fails for any of the provided paths.
    pub fn reindex_files(
        &mut self,
        paths: &[PathBuf],
        lang_registry: &LanguageRegistry,
    ) -> Result<()> {
        let mut parser = tree_sitter::Parser::new();
        let enrichers = default_enrichers();

        for path in paths {
            self.purge_file(path);
            if path.exists() {
                if let Some(lang) = lang_registry.language_for_path(path) {
                    parser
                        .set_language(&lang.tree_sitter_language())
                        .map_err(|e| ForgeError::TreeSitterLanguage(e.to_string()))?;
                    let mut ctx = IndexContext {
                        path,
                        language: lang.as_ref(),
                        enrichers: &enrichers,
                        macro_table: None,
                        table: &mut *self,
                    };
                    match index_file(&mut parser, &mut ctx, None) {
                        Ok(count) => {
                            debug!(path = %path.display(), rows = count, "reindexed");
                        }
                        Err(err) => {
                            warn!(path = %path.display(), error = %err, "reindex failed");
                        }
                    }
                } else {
                    debug!(path = %path.display(), "purged (unsupported language)");
                }
            } else {
                debug!(path = %path.display(), "purged (file deleted)");
            }
        }
        // Run post_pass for each enricher, scoped to the changed paths.
        // This makes incremental re-indexing O(P) instead of O(N) — on
        // Zephyr (2.7M symbols) it turns ~17s of CHANGE-time post_pass
        // overhead into milliseconds.
        let scope: std::collections::HashSet<std::path::PathBuf> = paths.iter().cloned().collect();
        for enricher in &enrichers {
            enricher.post_pass(self, Some(&scope));
        }
        Ok(())
    }
}
#[cfg(test)]
impl SymbolTable {
    /// Test helper: intern string fields and append a row.
    #[allow(clippy::too_many_arguments)]
    pub fn push_row_strings(
        &mut self,
        name: &str,
        node_kind: &str,
        fql_kind: &str,
        language: &str,
        path: &std::path::Path,
        byte_range: std::ops::Range<usize>,
        line: usize,
        fields: HashMap<String, String>,
    ) {
        let (name_id, node_kind_id, fql_kind_id, language_id, path_id) = self
            .strings
            .intern_row(name, node_kind, fql_kind, language, path);
        let fields = self.strings.intern_fields(fields);
        self.push_row(IndexRow {
            name_id,
            node_kind_id,
            fql_kind_id,
            language_id,
            path_id,
            byte_range,
            line,
            usages_count: 0,
            fields,
        });
    }
}
