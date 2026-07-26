/// AST index — flat row model with dynamic fields.
///
/// Every "interesting" tree-sitter node produces one [`IndexRow`].
/// A node is interesting if [`extract_name`] returns a name for it.
///
/// KEY RULE: Never store raw `tree_sitter::Node` references.
/// Always extract byte ranges and store `Range<usize>`.
use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::ast::intern::ColumnarTable;
use crate::ast::trigram::TrigramIndex;
// -----------------------------------------------------------------------
// SegmentBuildCtx — per-file columnar write context
// -----------------------------------------------------------------------

/// Type alias for the content-hash function used in [`SegmentBuildCtx`].
pub type SegHashFn = Arc<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync>;

/// Type alias for the per-file emit callback in [`SegmentBuildCtx`].
pub type SegEmitFn = Arc<dyn Fn(&[u8], &SymbolTable, usize) + Send + Sync>;

/// Type alias for the pre-parse segment-reuse hook in [`SegmentBuildCtx`].
///
/// Arguments: `(abs_path: &Path, content_id: &[u8])`. Returns `true` when a
/// valid segment for this exact content already exists on disk and has been
/// registered for the overlay build — the caller may skip parsing entirely.
pub type SegReuseFn = Arc<dyn Fn(&Path, &[u8]) -> bool + Send + Sync>;

/// Context threaded into [`index_file`] for per-file columnar shadow-write.
///
/// Defined here (alongside [`SymbolTable`]) to avoid a circular dependency
/// between `ast/index.rs` and `storage/columnar/`.  All function pointers are
/// type-erased so this module does not know about any concrete storage backend.
///
/// `SegmentBuildCtx` must be `Sync` so a single instance can be shared across
/// rayon threads inside [`SymbolTable::build`].
pub struct SegmentBuildCtx {
    /// Provider identifier embedded in segment paths (e.g. `"git-sha1"`).
    pub provider_id: String,
    /// Type-erased content-hash function.
    ///
    /// Maps raw file bytes to raw content-ID bytes.  For `GitSha1Provider`
    /// this returns a 20-byte SHA-1 blob hash.
    pub hash_fn: SegHashFn,
    /// Callback invoked after each file's rows have been committed to the
    /// per-file `SymbolTable`.
    ///
    /// Arguments: `(content_id: &[u8], table: &SymbolTable, rows_start: usize)`
    ///
    /// `rows_start` is always `0` for a fresh per-file table (the common path
    /// in `build()`), but may be `> 0` for future incremental re-index paths.
    pub emit_fn: SegEmitFn,
    /// Pre-parse reuse hook: when it returns `true` the segment for this
    /// exact content already exists and is registered — skip the parse.
    ///
    /// Only sound for build paths that discard the per-file `SymbolTable`
    /// (the columnar fast-path); paths that need the parsed rows must not
    /// consult it.
    pub reuse_fn: SegReuseFn,
}
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
    /// Stable per-file node ordinal used to build `node_id` handles.
    ///
    /// `None` means this row has no addressable ordinal (for example, synthetic
    /// enrichment rows).
    #[serde(default)]
    pub ordinal: Option<u32>,
    /// Ordinal of the nearest indexed ancestor; `u32::MAX` for top-level nodes.
    /// Typed field — replaces the `"parent_ordinal"` enrichment string.
    pub parent_ordinal: u32,
    /// First 8 bytes of SHA-256 of `source[byte_range]`, packed as a LE u64.
    /// `0` sentinel for analysis-only (non-addressable) rows.
    pub rev: u64,
    /// Dynamic enrichment fields — interned from the raw `HashMap<String, String>`
    /// produced by enrichers.  Both keys and values are IDs into
    /// [`ColumnarTable::field_keys`] and [`ColumnarTable::field_values`].
    ///
    /// Resolve at output time via [`crate::ast::intern::ColumnarTable::field_str`]
    /// (single-field lookup) or [`crate::ast::intern::ColumnarTable::resolve_fields`]
    /// (full map for serialisation).
    pub fields: HashMap<u32, u32>,
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
    /// Interned source file path — resolve via [`ColumnarTable::paths`].
    ///
    /// Stored as a `u32` ID into the shared [`PathPool`] so that 4.4 M usage
    /// sites across 14 K distinct files share a single allocation per path
    /// instead of one [`PathBuf`] heap allocation per site (~280 MB saved on
    /// zephyr-scale sessions).
    pub path_id: u32,
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
    /// Total number of indexed rows.  Populated by `ColumnarStorage` from
    /// `overlay.row_count()` so that columnar sessions appear in `SHOW SOURCES`.
    pub rows: usize,
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
mod build;
mod file_indexer;

pub use file_indexer::{IndexContext, OrdinalHint, OrdinalRemapper, OrdinalTombstones, index_file};

// -----------------------------------------------------------------------
// Shared utilities
// -----------------------------------------------------------------------

/// Return the source text of `node` as a `String`.
pub(crate) fn node_text(source: &[u8], node: tree_sitter::Node<'_>) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests;
