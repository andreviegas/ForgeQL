/// AST index — flat row model with dynamic fields.
///
/// Every "interesting" tree-sitter node produces one [`IndexRow`].
/// A node is interesting if [`extract_name`] returns a name for it.
///
/// KEY RULE: Never store raw `tree_sitter::Node` references.
/// Always extract byte ranges and store `Range<usize>`.
use std::collections::HashMap;
use std::ops::Range;
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

pub use file_indexer::{IndexContext, OrdinalHint, OrdinalRemapper, index_file};

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
mod tests {
    #![expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]

    use std::path::Path;

    use super::*;
    use crate::ast::enrich::default_enrichers;
    use crate::ast::lang::{CppLanguageInline, LanguageRegistry};

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
        assert_eq!(
            table.strings.paths.get(foo_sites[0].path_id),
            Path::new("b.cpp")
        );
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
        let enrichers = default_enrichers();
        {
            let mut ctx = IndexContext {
                path: &file,
                language: &CppLanguageInline,
                enrichers: &enrichers,
                macro_table: None,
                ordinal_remapper: None,
                table: &mut table,
            };
            index_file(&mut parser, &mut ctx, None).unwrap();
        }
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
        let enrichers = default_enrichers();
        {
            let mut ctx = IndexContext {
                path: &file,
                language: &CppLanguageInline,
                enrichers: &enrichers,
                macro_table: None,
                ordinal_remapper: None,
                table: &mut table,
            };
            index_file(&mut parser, &mut ctx, None).unwrap();
        }
        table
    }

    #[test]
    fn leading_attribute_folds_into_node_span() {
        // A Rust item's span (line / byte_range / rev) should start at its
        // leading `#[...]` attribute, not at the `fn`/`struct` keyword.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("snippet.rs");
        std::fs::write(&file, "#[derive(Clone)]\nstruct Widget;\n").unwrap();

        let mut table = SymbolTable::default();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let enrichers = default_enrichers();
        {
            let mut ctx = IndexContext {
                path: &file,
                language: &crate::ast::lang::RustLanguageInline,
                enrichers: &enrichers,
                macro_table: None,
                ordinal_remapper: None,
                table: &mut table,
            };
            index_file(&mut parser, &mut ctx, None).unwrap();
        }

        let widget = table
            .rows
            .iter()
            .find(|r| table.name_of(r) == "Widget")
            .expect("struct Widget should be indexed");
        // Line 1 is `#[derive(Clone)]`; line 2 is `struct Widget;`. The attribute
        // folds into the span, so the node reports line 1 and starts at byte 0.
        assert_eq!(widget.line, 1, "span should start at the #[derive] line");
        assert_eq!(
            widget.byte_range.start, 0,
            "byte_range should include the leading attribute"
        );
    }

    #[test]
    fn control_flow_node_parents_its_body() {
        // A statement inside an `if` must parent to the if-node, not jump up to the
        // enclosing function (plan §4.1 branches-as-parents). Engine-level, keyed on
        // config.is_control_flow_kind, so it holds for every language.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("snippet.rs");
        let src = "fn f(x: i32) {\n    if x > 0 {\n        let y = x;\n    }\n}\n";
        std::fs::write(&file, src).unwrap();

        let mut table = SymbolTable::default();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let enrichers = default_enrichers();
        {
            let mut ctx = IndexContext {
                path: &file,
                language: &crate::ast::lang::RustLanguageInline,
                enrichers: &enrichers,
                macro_table: None,
                ordinal_remapper: None,
                table: &mut table,
            };
            index_file(&mut parser, &mut ctx, None).unwrap();
        }

        let if_ord = table
            .rows
            .iter()
            .find(|r| table.fql_kind_of(r) == "if")
            .and_then(|r| r.ordinal)
            .expect("if node should be indexed with an ordinal");
        let let_row = table
            .rows
            .iter()
            .find(|r| table.name_of(r) == "y")
            .expect("`let y` should be indexed");
        assert_eq!(
            let_row.parent_ordinal, if_ord,
            "statement inside the `if` should parent to the if-node, not the function"
        );
    }

    fn index_rust_snippet(src: &str) -> SymbolTable {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("snippet.rs");
        std::fs::write(&file, src).unwrap();
        let mut table = SymbolTable::default();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let enrichers = default_enrichers();
        {
            let mut ctx = IndexContext {
                path: &file,
                language: &crate::ast::lang::RustLanguageInline,
                enrichers: &enrichers,
                macro_table: None,
                ordinal_remapper: None,
                table: &mut table,
            };
            index_file(&mut parser, &mut ctx, None).unwrap();
        }
        table
    }

    #[test]
    fn comment_run_births_a_childless_block() {
        // Three consecutive `//` comments coalesce into one synthetic
        // `comment_block` that spans the whole run, shares the comments' parent,
        // and has no children of its own.
        let src = "// first line\n// second line\n// third line\nfn f() {}\n";
        let table = index_rust_snippet(src);

        let block = table
            .rows
            .iter()
            .find(|r| table.fql_kind_of(r) == "comment_block")
            .expect("a comment_block should be born for a run of 3 comments");

        let block_text = &src[block.byte_range.clone()];
        assert!(
            block_text.contains("first line"),
            "block should cover the first comment"
        );
        assert!(
            block_text.contains("third line"),
            "block should cover the last comment"
        );

        let block_ord = block.ordinal.expect("block must have an ordinal");
        assert!(
            !table.rows.iter().any(|r| r.parent_ordinal == block_ord),
            "the comment_block must be childless"
        );

        let comment = table
            .rows
            .iter()
            .find(|r| table.fql_kind_of(r) == "comment")
            .expect("individual comment rows should still exist");
        assert_eq!(
            block.parent_ordinal, comment.parent_ordinal,
            "the block is a sibling of its members, sharing their parent"
        );

        let comment_count = table
            .rows
            .iter()
            .filter(|r| table.fql_kind_of(r) == "comment")
            .count();
        assert_eq!(comment_count, 3, "individual comment rows are preserved");
    }

    #[test]
    fn comment_block_bridges_blank_lines() {
        // A blank line between same-style comments does not split the block:
        // blank lines are not tree nodes, so the comments stay adjacent siblings.
        let src = "// a\n// b\n\n// c\n// d\nfn f() {}\n";
        let table = index_rust_snippet(src);

        let blocks: Vec<_> = table
            .rows
            .iter()
            .filter(|r| table.fql_kind_of(r) == "comment_block")
            .collect();
        assert_eq!(blocks.len(), 1, "the blank line must not split the run");
        let block_text = &src[blocks[0].byte_range.clone()];
        assert!(block_text.contains("// a") && block_text.contains("// d"));
    }

    #[test]
    fn comment_block_splits_on_style() {
        // `///` (doc) and `//` (line) runs form separate blocks via split_on_attr.
        let src = "/// doc one\n/// doc two\n// line one\n// line two\nfn f() {}\n";
        let table = index_rust_snippet(src);

        let blocks = table
            .rows
            .iter()
            .filter(|r| table.fql_kind_of(r) == "comment_block")
            .count();
        assert_eq!(blocks, 2, "doc and line comment runs are separate blocks");
    }

    #[test]
    fn single_comment_gets_no_block() {
        // A lone comment (run shorter than min_run = 2) stays a plain comment.
        let src = "// lonely\nfn f() {}\n";
        let table = index_rust_snippet(src);
        assert!(
            !table
                .rows
                .iter()
                .any(|r| table.fql_kind_of(r) == "comment_block"),
            "a single comment must not create a block"
        );
    }

    #[test]
    fn block_members_carry_block_alias_fields() {
        // Each member of a block is tagged with the block's 4-digit ordinal
        // (`block_ord`) and its 1-based offset within the block (`block_off`),
        // which powers the FIND/SHOW `block_id(offset)` surfacing.
        let src = "// first\n// second\n// third\nfn f() {}\n";
        let table = index_rust_snippet(src);

        let block_ord = table
            .rows
            .iter()
            .find(|r| table.fql_kind_of(r) == "comment_block")
            .and_then(|r| r.ordinal)
            .expect("comment_block exists");
        let expected_ord = format!("{block_ord:04}");

        let offs: Vec<&str> = table
            .rows
            .iter()
            .filter(|r| table.fql_kind_of(r) == "comment")
            .map(|r| {
                assert_eq!(
                    table.field_str(&r.fields, "block_ord"),
                    Some(expected_ord.as_str()),
                    "each member points at the owning block ordinal"
                );
                table.field_str(&r.fields, "block_off").unwrap_or_default()
            })
            .collect();
        assert_eq!(
            offs,
            ["1", "2", "3"],
            "members are tagged with 1-based offsets within the block"
        );
    }

    #[test]
    fn control_flow_body_preserves_sibling_node_ids_across_unrelated_edit() {
        // §4.1 must not break node-id survival across an unrelated edit (the NID08
        // "if node-ids survive line drift" property, at unit scope).
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("snippet.rs");
        let src_a = "fn f(x: i32) {\n    if x > 0 {\n        g();\n    }\n    if x < 0 {\n        h();\n    }\n}\n";
        std::fs::write(&file, src_a).unwrap();

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let enrichers = default_enrichers();

        let mut table_a = SymbolTable::default();
        {
            let mut ctx = IndexContext {
                path: &file,
                language: &crate::ast::lang::RustLanguageInline,
                enrichers: &enrichers,
                macro_table: None,
                ordinal_remapper: None,
                table: &mut table_a,
            };
            index_file(&mut parser, &mut ctx, None).unwrap();
        }

        let lt_if_ordinal = |t: &SymbolTable| -> u32 {
            t.rows
                .iter()
                .find(|r| t.fql_kind_of(r) == "if" && t.name_of(r).contains('<'))
                .and_then(|r| r.ordinal)
                .expect("the `x < 0` if should be indexed with an ordinal")
        };
        let before = lt_if_ordinal(&table_a);

        let mut hints = Vec::new();
        for row in &table_a.rows {
            let Some(ordinal) = row.ordinal else {
                continue;
            };
            let fields = table_a.resolve_fields(&row.fields);
            hints.push(OrdinalHint {
                name: table_a.name_of(row).to_string(),
                fql_kind: table_a.fql_kind_of(row).to_string(),
                parent_ordinal: row.parent_ordinal,
                guard_group_id: fields.get("guard_group_id").cloned(),
                guard_branch: fields.get("guard_branch").cloned(),
                first_body_statement_fingerprint: fields
                    .get("first_body_statement_fingerprint")
                    .cloned(),
                content_hash: fields.get("content_hash").cloned(),
                ordinal,
            });
        }

        let src_b = format!("// drift marker\n{src_a}");
        std::fs::write(&file, &src_b).unwrap();
        let mut table_b = SymbolTable::default();
        {
            let mut ctx = IndexContext {
                path: &file,
                language: &crate::ast::lang::RustLanguageInline,
                enrichers: &enrichers,
                macro_table: None,
                ordinal_remapper: Some(OrdinalRemapper::from_previous(hints)),
                table: &mut table_b,
            };
            index_file(&mut parser, &mut ctx, None).unwrap();
        }
        let after = lt_if_ordinal(&table_b);

        assert_eq!(
            before, after,
            "the `x < 0` if node-id must survive an unrelated edit above the function"
        );
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
