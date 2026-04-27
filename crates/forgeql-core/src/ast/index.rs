/// AST index — flat row model with dynamic fields.
///
/// Every "interesting" tree-sitter node produces one [`IndexRow`].
/// A node is interesting if [`extract_name`] returns a name for it.
///
/// KEY RULE: Never store raw `tree_sitter::Node` references.
/// Always extract byte ranges and store `Range<usize>`.
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::ast::enrich::guard_utils::{
    GuardFrame, build_env_guard_frame, build_guard_frame, collect_attribute_guard_frames,
    inject_guard_fields,
};
use crate::ast::enrich::macro_table::MacroTable;
use crate::ast::enrich::{EnrichContext, NodeEnricher, default_enrichers};
use crate::ast::intern::ColumnarTable;
use crate::ast::lang::{LanguageRegistry, LanguageSupport};
use crate::ast::trigram::TrigramIndex;
use crate::error::ForgeError;
use crate::workspace::Workspace;
// -----------------------------------------------------------------------
// IndexRow — the universal row type
// -----------------------------------------------------------------------

/// A single indexed AST node — the universal row type.
///
/// Every named tree-sitter node produces one row.  The `fields` map contains
/// all grammar fields of the node, auto-extracted by name from the Language
/// API.
///
/// All five top-level string fields (name, node kind, FQL kind, language, path)
/// are stored only as interned IDs.  Resolve them at output time via the
/// `SymbolTable::name_of`, `node_kind_of`, `fql_kind_of`, `language_of`, and
/// `path_of` accessor methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexRow {
    /// Byte range of the full AST node in the source file.
    pub byte_range: Range<usize>,
    /// 1-based start line number of the node.
    pub line: usize,
    /// Number of times this symbol name appears as an identifier reference
    /// across the indexed workspace.  Precomputed at build time so queries
    /// can filter/sort by `usages` without a per-row `HashMap` lookup.
    #[serde(default)]
    pub usages_count: u32,
    /// Dynamic enrichment fields — interned from the raw `HashMap<String, String>`
    /// produced by enrichers.  Both keys and values are IDs into
    /// [`ColumnarTable::field_keys`] and [`ColumnarTable::field_values`].
    ///
    /// Resolve at output time via [`crate::ast::intern::ColumnarTable::field_str`]
    /// (single-field lookup) or [`crate::ast::intern::ColumnarTable::resolve_fields`]
    /// (full map for serialisation).
    pub fields: HashMap<u32, u32>,
    /// Interned symbol name — resolve via [`SymbolTable::name_of`].
    pub name_id: u32,
    /// Interned raw tree-sitter node kind — resolve via [`SymbolTable::node_kind_of`].
    pub node_kind_id: u32,
    /// Interned universal FQL kind — resolve via [`SymbolTable::fql_kind_of`].
    pub fql_kind_id: u32,
    /// Interned language identifier — resolve via [`SymbolTable::language_of`].
    pub language_id: u32,
    /// Interned source file path — resolve via [`SymbolTable::path_of`].
    pub path_id: u32,
}

// -----------------------------------------------------------------------
// UsageSite — cross-reference entry (unchanged from v1)
// -----------------------------------------------------------------------

/// A reference (usage) of a symbol — where an identifier token appears.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSite {
    /// Source file containing the reference.
    pub path: PathBuf,
    /// Byte range of the identifier token at this usage site.
    pub byte_range: Range<usize>,
    /// 1-based source line of the identifier token.
    ///
    /// Populated at index-build time from the tree-sitter node position.
    /// Used to make individual usage rows distinguishable in CSV output.
    pub line: usize,
}

// -----------------------------------------------------------------------
// IndexStats — pre-aggregated group counts
// -----------------------------------------------------------------------

/// Pre-aggregated per-group symbol counts, computed once at build time.
///
/// Enables O(1) `GROUP BY fql_kind` and `GROUP BY language` queries without
/// scanning the full row list.
///
/// Keys are **interned IDs** from [`ColumnarTable`], not raw strings.  Resolve
/// to human-readable strings at output time via [`IndexStats::resolved_by_fql_kind`]
/// and [`IndexStats::resolved_by_language`].
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    /// Symbol count per `fql_kind` value (key = `fql_kind_id` from the intern pool).
    pub by_fql_kind: HashMap<u32, usize>,
    /// Symbol count per `language` value (key = `language_id` from the intern pool).
    pub by_language: HashMap<u32, usize>,
}

impl IndexStats {
    /// Resolve `by_fql_kind` interned IDs back to string keys for output.
    ///
    /// Called by the output layer (`exec_find`, `exec_source`) to produce
    /// human-readable maps without touching the hot index-build path.
    #[must_use]
    pub fn resolved_by_fql_kind(
        &self,
        strings: &crate::ast::intern::ColumnarTable,
    ) -> HashMap<String, usize> {
        self.by_fql_kind
            .iter()
            .map(|(&id, &count)| (strings.fql_kinds.get(id).to_owned(), count))
            .collect()
    }

    /// Resolve `by_language` interned IDs back to string keys for output.
    ///
    /// Called by the output layer (`exec_find`, `exec_source`) to produce
    /// human-readable maps without touching the hot index-build path.
    #[must_use]
    pub fn resolved_by_language(
        &self,
        strings: &crate::ast::intern::ColumnarTable,
    ) -> HashMap<String, usize> {
        self.by_language
            .iter()
            .map(|(&id, &count)| (strings.languages.get(id).to_owned(), count))
            .collect()
    }
}

// -----------------------------------------------------------------------
// MemEstimate — output of SymbolTable::mem_estimate()
// -----------------------------------------------------------------------

/// Approximate heap-memory breakdown for a [`SymbolTable`].
///
/// All values are in bytes. Use [`SymbolTable::mem_estimate`] to obtain one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemEstimate {
    /// Heap bytes used by `rows: Vec<IndexRow>` including the per-row
    /// `fields: HashMap<String,String>` enrichment payloads.
    pub rows_bytes: usize,
    /// Total number of indexed rows.
    pub rows_count: usize,
    /// Heap bytes used by `usages: HashMap<String, Vec<UsageSite>>`.
    pub usages_bytes: usize,
    /// Number of distinct symbol names with usage sites.
    pub usages_symbols: usize,
    /// Total number of individual usage-site entries.
    pub usages_sites: usize,
    /// Heap bytes used by `name_index: HashMap<u32, Vec<u32>>`.
    pub name_index_bytes: usize,
    /// Heap bytes used by `kind_index: HashMap<u32, Vec<u32>>`.
    pub kind_index_bytes: usize,
    /// Heap bytes used by `fql_kind_index: HashMap<u32, Vec<u32>>`.
    pub fql_kind_index_bytes: usize,
    /// Heap bytes used by `trigram_index: TrigramIndex`.
    pub trigram_bytes: usize,
    /// Number of distinct trigrams in the trigram index.
    pub trigram_entries: usize,
    /// Heap bytes used by `strings: ColumnarTable` (all five intern pools).
    pub strings_bytes: usize,
    /// Number of distinct interned symbol names.
    pub strings_names: usize,
    /// Number of distinct interned paths.
    pub strings_paths: usize,
}

impl MemEstimate {
    /// Sum of all component estimates — approximate total heap bytes.
    #[must_use]
    pub const fn total_bytes(&self) -> usize {
        self.rows_bytes
            + self.usages_bytes
            + self.name_index_bytes
            + self.kind_index_bytes
            + self.fql_kind_index_bytes
            + self.trigram_bytes
            + self.strings_bytes
    }
}

// -----------------------------------------------------------------------
// SymbolTable
// -----------------------------------------------------------------------

/// The full index for one workspace.
///
/// `build()` parses every source file and fills:
/// - `rows`:            all named AST nodes (functions, types, macros, etc.)
/// - `usages`:          symbol name → all identifier occurrence sites
/// - `name_index`:      `name_id` → row indices for O(1) name lookup
/// - `kind_index`:      `node_kind_id` → row indices for fast kind filtering
/// - `stats`:           pre-aggregated group counts for O(1) GROUP BY
/// - `strings`:         intern pool for all five top-level string fields
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SymbolTable {
    /// All indexed AST nodes (definitions, declarations, macros, includes).
    pub rows: Vec<IndexRow>,
    /// Symbol name → all sites where the identifier text appears.
    pub usages: HashMap<String, Vec<UsageSite>>,
    /// Name ID → row indices lookup for O(1) access.
    name_index: HashMap<u32, Vec<u32>>,
    /// Node kind ID → row indices for fast kind filtering.
    kind_index: HashMap<u32, Vec<u32>>,
    /// FQL kind ID → row indices for fast universal-kind filtering.
    fql_kind_index: HashMap<u32, Vec<u32>>,
    /// Pre-aggregated group counts for O(1) GROUP BY on `fql_kind` / `language`.
    #[serde(default)]
    pub stats: IndexStats,
    /// Trigram inverted index over symbol names for fast substring / regex pre-filtering.
    ///
    /// Not persisted in the cache — rebuilt in O(N) during
    /// [`SymbolTable::rebuild_indexes_from_rows`] on cache load.
    #[serde(skip)]
    pub trigram_index: TrigramIndex,
    /// Interned copies of all five top-level string fields in `rows`.
    ///
    /// Not serialised in `SymbolTable` — saved separately in `CachedIndex.strings`
    /// and restored by `CachedIndex::into_table`.
    ///
    /// Use [`SymbolTable::name_of`], [`SymbolTable::fql_kind_of`], etc. to
    /// resolve IDs at output time.
    #[serde(skip)]
    pub(crate) strings: ColumnarTable,
}

/// A row reference pairing an [`IndexRow`] with its owning [`SymbolTable`].
///
/// This is needed wherever string fields of a row must be resolved (e.g. for
/// filter evaluation) without storing the strings directly in `IndexRow`.
pub struct RowRef<'t> {
    pub row: &'t IndexRow,
    pub table: &'t SymbolTable,
}

// -----------------------------------------------------------------------
// Private helpers
// -----------------------------------------------------------------------

/// Remap `row`'s five ID fields from `src` pool into `dst` pool.
///
/// Used in [`SymbolTable::merge`] where IDs from the incoming table
/// are valid only in `src.strings` and must be re-interned into `dst.strings`.
#[inline]
fn reassign_intern_ids(src: &ColumnarTable, dst: &mut ColumnarTable, row: &mut IndexRow) {
    let name = src.names.get(row.name_id);
    let node_kind = src.node_kinds.get(row.node_kind_id);
    let fql_kind = src.fql_kinds.get(row.fql_kind_id);
    let language = src.languages.get(row.language_id);
    let path = src.paths.get(row.path_id);
    let (name_id, node_kind_id, fql_kind_id, language_id, path_id) =
        dst.intern_row(name, node_kind, fql_kind, language, path);
    row.name_id = name_id;
    row.node_kind_id = node_kind_id;
    row.fql_kind_id = fql_kind_id;
    row.language_id = language_id;
    row.path_id = path_id;

    // Remap field key+value IDs: per-file pool IDs are invalid after merge.
    // Remap field key+value IDs from the per-file pool into the merged pool.
    // `.to_owned()` copies each string out of `src` before `dst` is borrowed
    // mutably — satisfying the borrow checker.
    row.fields = row
        .fields
        .iter()
        .map(|(&kid, &vid)| {
            let k = src.field_keys.get(kid).to_owned();
            let v = src.field_values.get(vid).to_owned();
            let remapped_key = dst.field_keys.intern(k.as_str());
            let remapped_val = dst.field_values.intern(v.as_str());
            (remapped_key, remapped_val)
        })
        .collect();
}

/// Update all secondary indexes and stats for one newly-interned row.
///
/// # Why a free function?
///
/// The three call sites (`push_row`, `merge`, `rebuild_indexes_from_rows`) all
/// need to read `strings` (immutable) while simultaneously mutating the index
/// maps, stats, and trigram fields.  A `&mut self` method would hold a mutable
/// borrow over the entire struct, preventing the read of `self.strings`.
/// A free function with explicit field borrows lets the borrow checker track
/// the disjoint accesses and allows zero-allocation access to the string pool.
///
/// # Option B — `IndexStats` uses `u32` keys
///
/// Because `stats.by_fql_kind` and `stats.by_language` now key by interned ID,
/// this function never calls `.to_owned()` on `fql_kind` or `language` strings.
/// The only remaining string read is `strings.names.get(row.name_id)` for the
/// trigram insert — a `&str` borrow from the pool, no allocation.
#[allow(clippy::too_many_arguments)]
fn index_row_into_secondaries(
    name_index: &mut HashMap<u32, Vec<u32>>,
    kind_index: &mut HashMap<u32, Vec<u32>>,
    fql_kind_index: &mut HashMap<u32, Vec<u32>>,
    stats: &mut IndexStats,
    trigram_index: &mut crate::ast::trigram::TrigramIndex,
    strings: &ColumnarTable,
    row: &IndexRow,
    idx: u32,
) {
    name_index.entry(row.name_id).or_default().push(idx);
    kind_index.entry(row.node_kind_id).or_default().push(idx);
    if !strings.fql_kinds.get(row.fql_kind_id).is_empty() {
        fql_kind_index.entry(row.fql_kind_id).or_default().push(idx);
        *stats.by_fql_kind.entry(row.fql_kind_id).or_insert(0) += 1;
    }
    if !strings.languages.get(row.language_id).is_empty() {
        *stats.by_language.entry(row.language_id).or_insert(0) += 1;
    }
    // `get` returns a `&str` borrowed from the pool — zero allocation.
    trigram_index.insert(idx as usize, strings.names.get(row.name_id));
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
    pub fn build(
        workspace: &Workspace,
        lang_registry: &LanguageRegistry,
    ) -> Result<(Self, MacroTable)> {
        // 1 — collect file paths that have a registered language.
        let paths: Vec<PathBuf> = workspace
            .files()
            .filter(|p| lang_registry.language_for_path(p).is_some())
            .collect();

        debug!(files = paths.len(), "indexing files in parallel");

        // Pass 1 — collect macro definitions (parallel, per-file, then merged).
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

        debug!(
            macro_defs = macro_table.def_count(),
            "first-pass macro collection complete"
        );

        // Pass 2 — parse + enrich each file in parallel, merging via tree
        // reduction so merges also happen across multiple cores.
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

                match index_file(
                    &mut parser,
                    path,
                    &mut file_table,
                    &enrichers,
                    lang.as_ref(),
                    Some(&macro_table),
                ) {
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
                Some(file_table)
            })
            .reduce(Self::default, |mut acc, file_table| {
                acc.merge(file_table);
                acc
            });

        debug!(
            rows = table.rows.len(),
            usages = table.usages.values().map(Vec::len).sum::<usize>(),
            names = table.name_index.len(),
            kinds = table.kind_index.len(),
            "index built"
        );

        // Post-pass — run post_pass for each enricher (aggregation, cross-row metrics).
        // `None` scope = process the entire table (full build).
        let enrichers = default_enrichers();
        for enricher in &enrichers {
            enricher.post_pass(&mut table, None);
        }

        // Precompute per-row usages_count from the completed usages map.
        table.populate_usage_counts();

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
            index_row_into_secondaries(
                &mut self.name_index,
                &mut self.kind_index,
                &mut self.fql_kind_index,
                &mut self.stats,
                &mut self.trigram_index,
                &self.strings,
                &row,
                abs_u32,
            );
            self.rows.push(row);
        }

        // Merge usage sites.
        for (name, sites) in other.usages {
            self.usages.entry(name).or_default().extend(sites);
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
        index_row_into_secondaries(
            &mut self.name_index,
            &mut self.kind_index,
            &mut self.fql_kind_index,
            &mut self.stats,
            &mut self.trigram_index,
            &self.strings,
            &row,
            index_u32,
        );
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
            index_row_into_secondaries(
                &mut self.name_index,
                &mut self.kind_index,
                &mut self.fql_kind_index,
                &mut self.stats,
                &mut self.trigram_index,
                &self.strings,
                row,
                index_u32,
            );
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
        self.usages.entry(name).or_default().push(UsageSite {
            path: path.to_path_buf(),
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
        let usage_site_fixed = std::mem::size_of::<UsageSite>();
        let usages_bytes: usize = self
            .usages
            .iter()
            .map(|(k, v)| {
                k.capacity()
                    + v.capacity() * (usage_site_fixed + 8) // UsageSite + PathBuf heap
                    + v.iter().map(|s| s.path.capacity()).sum::<usize>()
                    + 56 // bucket overhead
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
            + path_pool_bytes;

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

        for sites in self.usages.values_mut() {
            sites.retain(|usage| usage.path != path);
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
                    match index_file(&mut parser, path, self, &enrichers, lang.as_ref(), None) {
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

// -----------------------------------------------------------------------
// First-pass macro collector
// -----------------------------------------------------------------------

/// Walk the AST of a single file and collect all macro definitions.
///
/// Returns an empty `Vec` when the language has no `macro_expander()`.
///
/// # Errors
/// Returns an error if the file cannot be read or tree-sitter parsing fails.
fn collect_macro_defs_for_file(
    parser: &mut tree_sitter::Parser,
    path: &Path,
    language: &dyn LanguageSupport,
) -> Result<Vec<crate::ast::lang::MacroDef>> {
    let Some(expander) = language.macro_expander() else {
        return Ok(Vec::new());
    };
    let source = crate::workspace::file_io::read_bytes(path)?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| ForgeError::AstParse {
            path: path.to_path_buf(),
        })?;
    let config = language.config();
    let mut cursor = tree.root_node().walk();
    let mut defs = Vec::new();
    loop {
        let node = cursor.node();
        if config.macro_def_kinds().iter().any(|k| k == node.kind())
            && let Some(mut def) = expander.extract_def(node, &source, config)
        {
            def.file = path.to_path_buf();
            defs.push(def);
        }
        if !config.is_skip_kind(node.kind()) && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return Ok(defs);
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

// -----------------------------------------------------------------------
// Index one file (second pass)
// -----------------------------------------------------------------------

/// Index a single file, adding its rows to `table`.
///
/// `macro_table` — optional table of macro definitions built during the
/// first pass; passed through to [`EnrichContext`] for macro-aware enrichers.
///
/// # Errors
/// Returns an error if the file cannot be read or tree-sitter parsing fails.
pub fn index_file(
    parser: &mut tree_sitter::Parser,
    path: &Path,
    table: &mut SymbolTable,
    enrichers: &[Box<dyn NodeEnricher>],
    language: &dyn LanguageSupport,
    macro_table: Option<&MacroTable>,
) -> Result<usize> {
    let source = crate::workspace::file_io::read_bytes(path)?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| ForgeError::AstParse {
            path: path.to_path_buf(),
        })?;

    let ts_lang = language.tree_sitter_language();
    let before = table.rows.len();

    let mut cursor = tree.root_node().walk();
    collect_nodes(
        &source,
        path,
        &mut cursor,
        &ts_lang,
        language,
        table,
        enrichers,
        macro_table,
    );

    Ok(table.rows.len() - before)
}

// -----------------------------------------------------------------------
// Generic node collector
// -----------------------------------------------------------------------

/// Walk the AST and produce index rows for every named node.
///
/// A node is "interesting" if [`extract_name`] returns a name for it.
/// Identifier tokens are also indexed as usage sites regardless of kind.
///
/// `preproc_else` and `preproc_elif` subtrees are skipped entirely so that
/// only the primary (#if) branch is indexed.  Without this, tree-sitter's
/// full-source parse would create duplicate rows and usage sites for every
/// symbol that appears in both a `#if` branch and its `#else` counterpart.
///
/// Uses iterative depth-first traversal via `TreeCursor` navigation to
/// avoid stack overflow on large codebases (e.g. Zephyr RTOS).
#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn collect_nodes(
    source: &[u8],
    path: &Path,
    cursor: &mut tree_sitter::TreeCursor<'_>,
    ts_language: &tree_sitter::Language,
    language: &dyn LanguageSupport,
    table: &mut SymbolTable,
    enrichers: &[Box<dyn NodeEnricher>],
    macro_table: Option<&MacroTable>,
) {
    let config = language.config();
    let lang_name = language.name();
    let mut guard_stack: Vec<GuardFrame> = Vec::new();
    // Pre-compile env_guard_patterns once per file.
    let env_guard_regex: Option<regex::RegexSet> = if config.env_guard_patterns().is_empty() {
        None
    } else {
        regex::RegexSet::new(config.env_guard_patterns()).ok()
    };

    loop {
        let node = cursor.node();

        // --- Guard stack management ---
        // Pop frames whose byte scope we've left.
        while let Some(frame) = guard_stack.last() {
            if node.start_byte() >= frame.guard_byte_range.end {
                drop(guard_stack.pop());
            } else {
                break;
            }
        }
        // Push a new frame when entering a block-guard-opening node.
        if config.has_guard_support()
            && (config.is_block_guard_kind(node.kind())
                || config.is_elif_kind(node.kind())
                || config.is_else_kind(node.kind()))
        {
            let frame = build_guard_frame(node, source, config, &guard_stack);
            guard_stack.push(frame);
        }
        // Push a heuristic guard frame for env-guarded `if` nodes
        // (e.g. Python `if TYPE_CHECKING:` or `if sys.platform == "linux":`).
        if let Some(regex_set) = &env_guard_regex
            && language.map_kind(node.kind()) == Some("if")
            && let Some(frame) = build_env_guard_frame(node, source, config, regex_set)
        {
            guard_stack.push(frame);
        }
        // --- End guard stack management ---

        // Skip alternate conditional-compilation branches entirely.
        let skip = config.is_skip_kind(node.kind());

        if !skip {
            // Build the enrichment context once for this node.
            let ctx = EnrichContext {
                node,
                source,
                path,
                language_name: lang_name,
                language_config: config,
                language_support: language,
                guard_stack: &guard_stack,
                macro_table,
            };

            // Every named node becomes a row.
            if let Some(name) = language.extract_name(node, source) {
                let mut fields = extract_fields(node, source, ts_language);

                // Inject guard fields from the current block-guard stack.
                if !guard_stack.is_empty() {
                    inject_guard_fields(&guard_stack, &mut fields);
                }

                // Inject item-level attribute guards (e.g. Rust `#[cfg(...)]`).
                let attr_guard_name = config.item_guard_attribute();
                if !attr_guard_name.is_empty() {
                    let attr_frames = collect_attribute_guard_frames(node, source, attr_guard_name);
                    if !attr_frames.is_empty() {
                        inject_guard_fields(&attr_frames, &mut fields);
                    }
                }

                // Run all enrichers on this row.
                for enricher in enrichers {
                    enricher.enrich_row(&ctx, &name, &mut fields);
                }

                let fql_kind_val = language.map_kind(node.kind()).unwrap_or("");
                let (name_id, node_kind_id, fql_kind_id, language_id, path_id) = table
                    .strings
                    .intern_row(&name, node.kind(), fql_kind_val, lang_name, path);
                // Intern field keys+values before storing — converts the temporary
                // HashMap<String,String> enricher buffer into HashMap<u32,u32>.
                let fields = table.strings.intern_fields(fields);
                table.push_row(IndexRow {
                    name_id,
                    node_kind_id,
                    fql_kind_id,
                    language_id,
                    path_id,
                    byte_range: node.byte_range(),
                    line: node.start_position().row + 1,
                    usages_count: 0,
                    fields,
                });
            } else if let Some(mtable) = macro_table {
                // Re-tag: tree-sitter-cpp parses C macro calls as
                // call_expression, not macro_invocation.  When extract_name
                // returns None for a call_expression whose function name is
                // in the MacroTable, emit a macro_call row.
                let call_kind = config.call_expression_kind();
                if !call_kind.is_empty()
                    && node.kind() == call_kind
                    && let Some(func_node) = node.child_by_field_name("function")
                {
                    let func_name = node_text(source, func_node);
                    if !func_name.is_empty() && mtable.contains(&func_name) {
                        let mut fields = extract_fields(node, source, ts_language);

                        if !guard_stack.is_empty() {
                            inject_guard_fields(&guard_stack, &mut fields);
                        }
                        let attr_guard_name = config.item_guard_attribute();
                        if !attr_guard_name.is_empty() {
                            let attr_frames =
                                collect_attribute_guard_frames(node, source, attr_guard_name);
                            if !attr_frames.is_empty() {
                                inject_guard_fields(&attr_frames, &mut fields);
                            }
                        }

                        for enricher in enrichers {
                            enricher.enrich_row(&ctx, &func_name, &mut fields);
                        }

                        let (name_id, node_kind_id, fql_kind_id, language_id, path_id) = table
                            .strings
                            .intern_row(&func_name, node.kind(), "macro_call", lang_name, path);
                        let fields = table.strings.intern_fields(fields);
                        table.push_row(IndexRow {
                            name_id,
                            node_kind_id,
                            fql_kind_id,
                            language_id,
                            path_id,
                            byte_range: node.byte_range(),
                            line: node.start_position().row + 1,
                            usages_count: 0,
                            fields,
                        });
                    }
                }
            }
            // Run extra_rows() for every node (even if extract_name returned None).
            for enricher in enrichers {
                for extra in enricher.extra_rows(&ctx) {
                    let extra_path = extra.path_override.as_deref().unwrap_or(path);
                    let (eni, enk, enf, enl, enp) = table.strings.intern_row(
                        &extra.name,
                        &extra.node_kind,
                        &extra.fql_kind,
                        lang_name,
                        extra_path,
                    );
                    let fields = table.strings.intern_fields(extra.fields);
                    table.push_row(IndexRow {
                        name_id: eni,
                        node_kind_id: enk,
                        fql_kind_id: enf,
                        language_id: enl,
                        path_id: enp,
                        byte_range: extra.byte_range,
                        line: extra.line,
                        usages_count: 0,
                        fields,
                    });
                }
            }

            // All identifier tokens become usage sites.
            if config.is_usage_node_kind(node.kind()) {
                let name = node_text(source, node);
                if name.len() > 1 {
                    let line = node.start_position().row + 1;
                    table.add_usage(name, path, node.byte_range(), line);
                }
            }

            // Descend into children.
            if cursor.goto_first_child() {
                continue;
            }
        }
        // When `skip` is true we never call goto_first_child(), so the
        // entire subtree is skipped — matches the old early-return behaviour.

        // Move to next sibling, or walk up until we find one.
        if cursor.goto_next_sibling() {
            continue;
        }
        let mut found_sibling = false;
        while cursor.goto_parent() {
            if cursor.goto_next_sibling() {
                found_sibling = true;
                break;
            }
        }
        if !found_sibling {
            break;
        }
    }
}

/// Extract all grammar fields from a tree-sitter node into a string map.
fn extract_fields(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    language: &tree_sitter::Language,
) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let field_count = language.field_count();

    #[allow(clippy::cast_possible_truncation)]
    for field_id in 1..=(field_count as u16) {
        if let Some(child) = node.child_by_field_id(field_id)
            && let Some(field_name) = language.field_name_for_id(field_id)
        {
            let text = node_text(source, child);
            if !text.is_empty() {
                drop(fields.insert(field_name.to_string(), text));
            }
        }
    }

    fields
}

// -----------------------------------------------------------------------
// Shared utilities
// -----------------------------------------------------------------------

/// Return the source text of `node` as a `String`.
pub(crate) fn node_text(source: &[u8], node: tree_sitter::Node<'_>) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::ast::lang::CppLanguageInline;

    fn two_row_table() -> SymbolTable {
        let mut table = SymbolTable::default();
        table.push_row_strings(
            "foo",
            "function_definition",
            "",
            "",
            Path::new("a.cpp"),
            0..30,
            1,
            HashMap::new(),
        );
        table.push_row_strings(
            "bar",
            "function_definition",
            "",
            "",
            Path::new("b.cpp"),
            0..30,
            1,
            HashMap::new(),
        );
        table.add_usage("foo".to_string(), Path::new("a.cpp"), 0..3, 1);
        table.add_usage("foo".to_string(), Path::new("b.cpp"), 10..13, 1);
        table
    }

    #[test]
    fn push_row_updates_secondary_indexes() {
        let mut table = SymbolTable::default();
        table.push_row_strings(
            "alpha",
            "function_definition",
            "",
            "",
            Path::new("src/alpha.cpp"),
            0..10,
            1,
            HashMap::new(),
        );
        assert_eq!(table.rows.len(), 1);
        let name_id = table.strings.names.get_id("alpha").unwrap();
        let kind_id = table
            .strings
            .node_kinds
            .get_id("function_definition")
            .unwrap();
        assert_eq!(table.name_index[&name_id], vec![0u32]);
        assert_eq!(table.kind_index[&kind_id], vec![0u32]);
    }

    #[test]
    fn find_def_returns_last_row_for_name() {
        let mut table = SymbolTable::default();
        table.push_row_strings(
            "foo",
            "declaration",
            "",
            "",
            Path::new("inc/foo.h"),
            0..10,
            1,
            HashMap::new(),
        );
        table.push_row_strings(
            "foo",
            "function_definition",
            "",
            "",
            Path::new("src/foo.cpp"),
            0..50,
            1,
            HashMap::new(),
        );
        let def = table.find_def("foo").expect("should find foo");
        assert_eq!(table.node_kind_of(def), "function_definition");
    }

    #[test]
    fn rows_by_kind_returns_correct_subset() {
        let table = two_row_table();
        let fns: Vec<&IndexRow> = table.rows_by_kind("function_definition").collect();
        assert_eq!(fns.len(), 2);
        assert!(fns.iter().any(|r| table.name_of(r) == "foo"));
        assert!(fns.iter().any(|r| table.name_of(r) == "bar"));
    }

    #[test]
    fn purge_file_removes_rows_and_usage_sites() {
        let mut table = two_row_table();
        table.purge_file(Path::new("a.cpp"));

        assert!(table.find_def("foo").is_none());
        assert!(table.find_def("bar").is_some());

        let foo_sites = table.find_usages("foo");
        assert_eq!(foo_sites.len(), 1);
        assert_eq!(foo_sites[0].path, PathBuf::from("b.cpp"));
    }

    #[test]
    fn purge_file_removes_empty_usage_keys() {
        let mut table = SymbolTable::default();
        table.add_usage("only_here".to_string(), Path::new("x.cpp"), 0..5, 1);
        table.purge_file(Path::new("x.cpp"));
        assert!(!table.usages.contains_key("only_here"));
    }

    #[test]
    fn purge_file_rebuilds_index_stats() {
        // Two rows in two files, both contributing to fql_kind / language stats.
        let mut table = SymbolTable::default();
        table.push_row_strings(
            "f1",
            "function_definition",
            "function",
            "cpp",
            Path::new("a.cpp"),
            0..10,
            1,
            HashMap::new(),
        );
        table.push_row_strings(
            "f2",
            "function_definition",
            "function",
            "cpp",
            Path::new("b.cpp"),
            0..10,
            1,
            HashMap::new(),
        );
        // IndexStats now keys by interned u32 — resolve to strings for assertion.
        assert_eq!(
            table
                .stats
                .resolved_by_fql_kind(&table.strings)
                .get("function"),
            Some(&2)
        );
        assert_eq!(
            table.stats.resolved_by_language(&table.strings).get("cpp"),
            Some(&2)
        );

        // Purge one file — stats must reflect only the surviving row.
        table.purge_file(Path::new("a.cpp"));
        assert_eq!(
            table
                .stats
                .resolved_by_fql_kind(&table.strings)
                .get("function"),
            Some(&1)
        );
        assert_eq!(
            table.stats.resolved_by_language(&table.strings).get("cpp"),
            Some(&1)
        );

        // Purge the other — stats must be empty (key removed entirely).
        table.purge_file(Path::new("b.cpp"));
        assert!(table.stats.by_fql_kind.is_empty());
        assert!(table.stats.by_language.is_empty());
    }

    #[test]
    fn reindex_files_refreshes_content() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.cpp");
        std::fs::write(&file, "void alpha() {}").unwrap();

        let mut table = SymbolTable::default();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .unwrap();
        index_file(
            &mut parser,
            &file,
            &mut table,
            &default_enrichers(),
            &CppLanguageInline,
            None,
        )
        .unwrap();
        assert!(table.find_def("alpha").is_some());

        std::fs::write(&file, "void beta() {}").unwrap();
        let registry = LanguageRegistry::new(vec![std::sync::Arc::new(CppLanguageInline)]);
        table.reindex_files(&[file], &registry).unwrap();

        assert!(
            table.find_def("alpha").is_none(),
            "stale entry should be purged"
        );
        assert!(table.find_def("beta").is_some(), "new entry should exist");
    }

    fn index_snippet(code: &str) -> SymbolTable {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("snippet.cpp");
        std::fs::write(&file, code).unwrap();
        let mut table = SymbolTable::default();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .unwrap();
        index_file(
            &mut parser,
            &file,
            &mut table,
            &default_enrichers(),
            &CppLanguageInline,
            None,
        )
        .unwrap();
        table
    }

    #[test]
    fn indexes_function_definition() {
        let table = index_snippet("void processSignal(int speed) { return; }");
        let row = table.find_def("processSignal").expect("indexed");
        assert_eq!(table.node_kind_of(row), "function_definition");
        assert_eq!(row.line, 1);
    }

    #[test]
    fn indexes_qualified_function_definition() {
        let table =
            index_snippet("class Motor { void setup(); };\nvoid Motor::setup() { return; }");
        assert!(
            table.find_def("Motor::setup").is_some(),
            "qualified method should be indexed under its full name"
        );
        // The member declaration inside the class body is also indexed
        // under the bare name as a field_declaration.
        let decl = table
            .find_def("setup")
            .expect("member declaration should be indexed");
        assert_eq!(table.node_kind_of(decl), "field_declaration");
    }

    #[test]
    fn bare_and_qualified_functions_coexist() {
        let table = index_snippet(
            "void setup() {}\nclass Motor { void setup(); };\nvoid Motor::setup() {}",
        );
        // find_def returns the last row for a name — the field_declaration
        // from the class body comes after the bare function_definition.
        let last = table.find_def("setup").expect("setup");
        assert_eq!(table.node_kind_of(last), "field_declaration");
        // The bare function_definition is still in the table.
        let has_bare_def = table
            .rows
            .iter()
            .any(|r| table.name_of(r) == "setup" && table.node_kind_of(r) == "function_definition");
        assert!(has_bare_def, "bare function_definition should exist");
        let qualified = table.find_def("Motor::setup").expect("qualified setup");
        assert_eq!(table.node_kind_of(qualified), "function_definition");
    }

    #[test]
    fn indexes_member_function_declaration() {
        let table = index_snippet(
            "class SignalSequencer {\n  void loadSignalCode(int code);\n  int getValue() const;\n};",
        );
        let load = table
            .find_def("loadSignalCode")
            .expect("member declaration indexed");
        assert_eq!(table.node_kind_of(load), "field_declaration");
        let get = table
            .find_def("getValue")
            .expect("member declaration indexed");
        assert_eq!(table.node_kind_of(get), "field_declaration");
    }

    #[test]
    fn indexes_member_data_field() {
        let table = index_snippet("struct Point { int x; double y; };");
        let x = table.find_def("x").expect("data member indexed");
        assert_eq!(table.node_kind_of(x), "field_declaration");
        let y = table.find_def("y").expect("data member indexed");
        assert_eq!(table.node_kind_of(y), "field_declaration");
    }

    #[test]
    fn indexes_struct_specifier() {
        let table = index_snippet("struct Motor { int speed; };");
        let row = table.find_def("Motor").expect("indexed");
        assert_eq!(table.node_kind_of(row), "struct_specifier");
    }

    #[test]
    fn indexes_preproc_def() {
        let table = index_snippet("#define BAUD_RATE 9600");
        let row = table.find_def("BAUD_RATE").expect("indexed");
        assert_eq!(table.node_kind_of(row), "preproc_def");
    }

    #[test]
    fn indexes_enum_specifier() {
        let table = index_snippet("enum class State { Idle, Running };");
        let row = table.find_def("State").expect("indexed");
        assert_eq!(table.node_kind_of(row), "enum_specifier");
    }

    #[test]
    fn indexes_preproc_include() {
        let table = index_snippet("#include <stdint.h>");
        let row = table.find_def("stdint.h").expect("indexed");
        assert_eq!(table.node_kind_of(row), "preproc_include");
    }

    #[test]
    fn usage_sites_indexed_for_identifier_tokens() {
        let table = index_snippet("void foo() { foo(); }");
        let sites = table.find_usages("foo");
        assert!(!sites.is_empty(), "foo should have usage sites");
    }

    #[test]
    fn member_method_declaration_carries_body_symbol() {
        let table = index_snippet(
            "class Motor { void setup(int speed); };\nvoid Motor::setup(int speed) {}",
        );
        let decl = table.find_def("setup").expect("member declaration indexed");
        assert_eq!(table.node_kind_of(decl), "field_declaration");
        assert_eq!(
            table.strings.field_str(&decl.fields, "body_symbol"),
            Some("Motor::setup"),
            "body_symbol must point to the qualified name"
        );
    }

    #[test]
    fn data_member_has_no_body_symbol() {
        let table = index_snippet("struct Point { int x; double y; };");
        let x = table.find_def("x").expect("data member indexed");
        assert!(
            table.strings.field_str(&x.fields, "body_symbol").is_none(),
            "data members should not have body_symbol"
        );
    }
    // -- find_all_defs ---------------------------------------------------
    #[test]
    fn find_all_defs_empty_for_unknown_name() {
        let table = two_row_table();
        let defs = table.find_all_defs("nonexistent");
        assert!(defs.is_empty(), "unknown symbol must return empty vec");
    }

    #[test]
    fn find_all_defs_returns_all_matching_rows() {
        // Push the same name into two files to simulate a multi-file workspace.
        let mut table = SymbolTable::default();
        table.push_row_strings(
            "shared",
            "function_definition",
            "",
            "",
            Path::new("src/a.cpp"),
            0..10,
            1,
            HashMap::new(),
        );
        table.push_row_strings(
            "shared",
            "function_definition",
            "",
            "",
            Path::new("src/b.cpp"),
            0..10,
            1,
            HashMap::new(),
        );

        let defs = table.find_all_defs("shared");
        assert_eq!(defs.len(), 2, "both rows must be returned");
    }

    #[test]
    fn find_all_defs_single_result() {
        let table = two_row_table();
        let defs = table.find_all_defs("foo");
        assert_eq!(defs.len(), 1);
        assert_eq!(table.name_of(defs[0]), "foo");
    }

    // -- suggest_similar -------------------------------------------------

    // -- suggest_similar -------------------------------------------------

    #[test]
    fn suggest_similar_prefix_match() {
        let table = two_row_table(); // has "foo" and "bar"
        let suggestions = table.suggest_similar("fo", 10);
        assert!(suggestions.contains(&"foo"), "prefix 'fo' must match 'foo'");
    }

    #[test]
    fn suggest_similar_substring_match() {
        let table = two_row_table(); // has "foo" and "bar"
        let suggestions = table.suggest_similar("oo", 10);
        assert!(
            suggestions.contains(&"foo"),
            "substring 'oo' must match 'foo'"
        );
    }

    #[test]
    fn suggest_similar_case_insensitive() {
        let table = two_row_table(); // has "foo"
        let suggestions = table.suggest_similar("FOO", 10);
        assert!(
            suggestions.contains(&"foo"),
            "uppercase query must match lowercase name"
        );
    }

    #[test]
    fn suggest_similar_no_match_returns_empty() {
        let table = two_row_table();
        let suggestions = table.suggest_similar("zzz_nonexistent", 10);
        assert!(suggestions.is_empty(), "no match must return empty vec");
    }

    #[test]
    fn suggest_similar_respects_max_limit() {
        // Build a table with 5 symbols that all start with "sym".
        let mut table = SymbolTable::default();
        for i in 0..5_usize {
            table.push_row_strings(
                &format!("sym_{i}"),
                "function_definition",
                "",
                "",
                Path::new("src/lib.cpp"),
                0..10,
                1,
                HashMap::new(),
            );
        }
        let suggestions = table.suggest_similar("sym", 3);
        assert!(
            suggestions.len() <= 3,
            "result must not exceed max limit of 3"
        );
    }

    // -- intern-pool correctness -----------------------------------------

    /// Verify that the five accessor methods return the expected strings
    /// after rows are pushed via `push_row_strings`.
    #[test]
    fn accessors_match_string_fields() {
        let mut table = SymbolTable::default();
        let data = [
            (
                "alpha",
                "function_definition",
                "function",
                "cpp",
                "src/a.cpp",
                0..10usize,
                1usize,
            ),
            (
                "beta",
                "struct_specifier",
                "struct",
                "cpp",
                "src/a.cpp",
                10..20,
                5,
            ),
            (
                "gamma",
                "function_definition",
                "function",
                "rust",
                "src/b.rs",
                0..15,
                1,
            ),
        ];
        for &(name, nk, fql, lang, path, ref br, line) in &data {
            table.push_row_strings(
                name,
                nk,
                fql,
                lang,
                Path::new(path),
                br.clone(),
                line,
                HashMap::new(),
            );
        }
        for (row, &(exp_name, exp_nk, exp_fql, exp_lang, exp_path, _, _)) in
            table.rows.iter().zip(data.iter())
        {
            assert_eq!(table.name_of(row), exp_name, "name_of");
            assert_eq!(table.node_kind_of(row), exp_nk, "node_kind_of");
            assert_eq!(table.fql_kind_of(row), exp_fql, "fql_kind_of");
            assert_eq!(table.language_of(row), exp_lang, "language_of");
            assert_eq!(table.path_of(row), Path::new(exp_path), "path_of");
        }
    }

    /// Rows with the same low-cardinality fields must share pool slots, keeping
    /// pool sizes bounded by unique-value cardinality rather than row count.
    #[test]
    fn intern_pool_sizes_reflect_unique_values() {
        let mut table = SymbolTable::default();
        // 100 rows: unique names, shared node_kind/fql_kind/language/path.
        for i in 0..100_usize {
            table.push_row_strings(
                &format!("fn_{i}"),
                "function_definition",
                "function",
                "cpp",
                Path::new("src/big.cpp"),
                0..10,
                i + 1,
                HashMap::new(),
            );
        }
        assert_eq!(table.strings.names.len(), 100, "100 unique names");
        assert_eq!(table.strings.node_kinds.len(), 1, "one node_kind");
        assert_eq!(table.strings.fql_kinds.len(), 1, "one fql_kind");
        assert_eq!(table.strings.languages.len(), 1, "one language");
        assert_eq!(table.strings.paths.len(), 1, "one path");
    }
}
