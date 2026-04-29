/// Index serialization and caching.
///
/// The `SymbolTable` is expensive to build (full tree-sitter parse of all files).
/// `CachedIndex` persists the index to disk between sessions so that only changed
/// files need to be re-parsed on resume.
///
/// Storage format: `bincode` (fast binary, 10-100x smaller and faster than JSON).
/// Cache file: `<worktree>/.forgeql-index`
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::ast::enrich::macro_table::MacroTable;
use crate::ast::index::{IndexRow, SymbolTable, UsageSite};
use crate::ast::intern::ColumnarTable;
use crate::ast::lang::MacroDef;

// -----------------------------------------------------------------------
// Current format version
// -----------------------------------------------------------------------

/// Increment this when `CachedIndex` fields change incompatibly.
///
/// v9: `parameter_declaration` gains `fql_kind = "variable"` — stale caches lack this.
/// v10: `alias_declaration` → `type_alias`, `preproc_function_def` → `macro` `fql_kind` mappings.
/// v11: `dead_store_conditional`, `decl_far_conditional`, `branch_depth` enrichment fields added.
/// v12: `decl_far_conditional` corrected from a count to a boolean `"true"`/`"false"`.
/// v13: `has_unused_reassign` false-positive fix — uninitialized declarations no longer seeded
///      as "written not read"; only declarations with explicit initializers are seeded.
/// v14: `ShadowEnricher` false-positive fix — `preproc_else`/`preproc_elif` branches are now
///      skipped during the shadow walk (honouring `skip_node_kinds`), eliminating false positives
///      where a variable in one `#ifdef`/`#else` arm appeared to shadow the same variable in the
///      sibling arm (mutually exclusive at runtime).
/// v15: Guard config section added (`block_guard_kinds`, etc.) — no index schema change.
/// v16: Guard stack in `collect_nodes()`: all `#ifdef`/`#else`/`#elif` branches are now indexed;
///      symbols inside guarded blocks carry `guard`, `guard_defines`, `guard_negates`,
///      `guard_mentions`, `guard_group_id`, `guard_branch`, `guard_kind` enrichment fields.
///   17. Rust `#[cfg(...)]` attributes inject guard enrichment fields (`guard_kind = "attribute"`).
///   18. Macro infrastructure: `MacroDef`, `MacroTable`, `MacroExpander`, `resolve_macro` scaffolding.
///   19. `MacroExpandEnricher` adds `macro_def_file`, `macro_def_line`, `macro_arity`, `macro_expansion` to `macro_call` rows.
///   20. Bug fix: C++ `macro_invocation` nodes now indexed as `macro_call` rows (`extract_name`
///       was missing the `"macro_invocation"` arm). **Policy**: bump this version for ANY
///       behavioral change that alters which rows are indexed or what enrichment fields are
///       populated — not only for structural `CachedIndex` field changes.
///   21. Bug fixes: synced `RustLanguageInline` and `CppLanguageInline` with production
///       implementations (added `macro_invocation` arm, `scoped_identifier` guard).
///       Added test coverage for Rust `#[cfg(...)]` attribute guard enrichment.
///   22. Complete macro expansion pipeline: C++ `call_expression` → `macro_call`
///       re-tagging via `MacroTable` lookup in `collect_nodes`; macro expansion
///       integration in `DeclDistanceEnricher` (dead-store suppression) and
///       `EscapeEnricher` (address-of detection); extended `MacroExpandEnricher`
///   23. `source_name` stored in `CachedIndex` via `from_table_and_macros`; stale-worktree
///       validation on cache resume now activates for macro-enabled sessions.
///   25. String fields removed from `IndexRow`; only ID fields remain. The `ColumnarTable`
///       string pool is now serialised as the `strings` field of `CachedIndex`.
///   26. `UsageSite.path: PathBuf` replaced by `path_id: u32` (interned into `ColumnarTable.paths`).
///      Eliminates ~280 MB of duplicated `PathBuf` heap on zephyr-scale sessions.
///   27. BUG-05/BUG-NEW-01/BUG-NEW-03: `param_count`, `return_count`, `goto_count`,
///       `string_count`, and `throw_count` are now bounded at C++ lambda boundaries —
///       values are lower for functions containing lambdas. BUG-06: `is_magic` is now
///       `false` for numbers whose direct parent is `enumerator` or `init_declarator`
///       (C++ named-constant contexts). Driven by new `constant_def_parent_kinds` and
///       `nested_function_body_kinds` config arrays in `cpp.json`.
pub const CURRENT_VERSION: u32 = 27;

// -----------------------------------------------------------------------
// CachedIndex
// -----------------------------------------------------------------------

/// A serializable snapshot of a `SymbolTable` with cache metadata.
#[derive(Debug, Serialize, Deserialize)]
pub struct CachedIndex {
    /// Format version — must equal `CURRENT_VERSION` to load.
    pub version: u32,
    /// The git commit hash this index was built from.
    /// Empty string when git is unavailable.
    pub commit_hash: String,
    /// Source name this index was built from (e.g. "forgeql-pub").
    /// Used to detect stale worktree reuse across different sources.
    #[serde(default)]
    pub source_name: String,
    /// All indexed AST rows (flat list — replaces symbols/defines/enums).
    pub rows: Vec<IndexRow>,
    /// Usage sites: name → all identifier occurrences.
    pub usages: HashMap<String, Vec<UsageSite>>,
    /// Git blob hash per file at index-build time (for incremental update).
    /// Empty until Phase D.
    pub file_hashes: HashMap<PathBuf, String>,
    /// Macro definitions collected during the first indexing pass.
    /// Empty when the source language has no macro-expansion support.
    #[serde(default)]
    pub macro_defs: Vec<MacroDef>,
    /// String intern pool — all unique names, kinds, languages, and paths.
    /// Must be restored to `SymbolTable::strings` when calling `into_table`.
    #[serde(default)]
    pub strings: ColumnarTable,
}

impl CachedIndex {
    /// Create a `CachedIndex` by taking ownership of a `SymbolTable`.
    pub fn from_table(
        table: SymbolTable,
        commit_hash: impl Into<String>,
        source_name: impl Into<String>,
    ) -> Self {
        Self {
            version: CURRENT_VERSION,
            commit_hash: commit_hash.into(),
            source_name: source_name.into(),
            rows: table.rows,
            usages: table.usages,
            file_hashes: HashMap::new(),
            macro_defs: Vec::new(),
            strings: table.strings,
        }
    }

    /// Create a `CachedIndex` from a `SymbolTable` and a `MacroTable`.
    ///
    /// The macro definitions are flattened into `macro_defs` for serialisation.
    /// Use `into_table_and_macros()` to recover both tables after loading.
    #[must_use]
    pub fn from_table_and_macros(
        table: SymbolTable,
        macro_table: MacroTable,
        commit_hash: impl Into<String>,
        source_name: impl Into<String>,
    ) -> Self {
        Self {
            version: CURRENT_VERSION,
            commit_hash: commit_hash.into(),
            source_name: source_name.into(),
            rows: table.rows,
            usages: table.usages,
            file_hashes: HashMap::new(),
            macro_defs: macro_table.into_defs(),
            strings: table.strings,
        }
    }

    /// Reconstruct a `SymbolTable` from this cache.
    ///
    /// The string pool is restored and secondary indexes are rebuilt from `rows`.
    /// Macro definitions are discarded; use `into_table_and_macros` if needed.
    #[must_use]
    pub fn into_table(self) -> SymbolTable {
        let mut table = SymbolTable::default();
        table.strings = self.strings;
        table.rows = self.rows;
        table.usages = self.usages;
        table.rebuild_indexes_from_rows();
        table
    }

    /// Reconstruct a `SymbolTable` and a `MacroTable` from this cache.
    ///
    /// This is the counterpart to `from_table_and_macros` — prefer it over
    /// `into_table` whenever the macro table is also needed.
    #[must_use]
    pub fn into_table_and_macros(self) -> (SymbolTable, MacroTable) {
        let mut symbol_table = SymbolTable::default();
        symbol_table.strings = self.strings;
        symbol_table.rows = self.rows;
        symbol_table.usages = self.usages;
        symbol_table.rebuild_indexes_from_rows();
        symbol_table.populate_usage_counts();

        let mut macro_table = MacroTable::new();
        for def in self.macro_defs {
            macro_table.insert(def);
        }
        (symbol_table, macro_table)
    }

    /// Serialize `table` and `macro_table` to `path` without taking ownership.
    ///
    /// Unlike `from_table_and_macros` + `save`, this keeps the caller's
    /// `SymbolTable` (with its already-built secondary indexes) alive, so
    /// `build_index` can assign it directly without a second O(N) rebuild.
    ///
    /// # Errors
    /// Returns `Err` if serialization or the atomic write fails.
    pub fn save_from_parts(
        table: &SymbolTable,
        macro_table: &MacroTable,
        commit_hash: &str,
        source_name: &str,
        path: &Path,
    ) -> Result<()> {
        let snapshot = Self {
            version: CURRENT_VERSION,
            commit_hash: commit_hash.to_owned(),
            source_name: source_name.to_owned(),
            rows: table.rows.clone(),
            usages: table.usages.clone(),
            file_hashes: HashMap::new(),
            macro_defs: macro_table.to_defs(),
            strings: table.strings.clone(),
        };
        let bytes = bincode::serialize(&snapshot)?;
        crate::workspace::file_io::write_atomic(path, &bytes)?;
        Ok(())
    }

    /// Serialize and write to `path` atomically.
    ///
    /// # Errors
    /// Returns `Err` if serialization fails or the atomic write fails.
    pub fn save(&self, path: &Path) -> Result<()> {
        let bytes = bincode::serialize(self)?;
        crate::workspace::file_io::write_atomic(path, &bytes)?;
        Ok(())
    }

    /// Load and deserialize from `path`.
    ///
    /// # Errors
    /// Returns `Err` if the file does not exist, is corrupt, or has an
    /// incompatible version number.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = crate::workspace::file_io::read_bytes(path)?;
        let index: Self = bincode::deserialize(&bytes)?;

        if index.version != CURRENT_VERSION {
            bail!(
                "cached index version mismatch: file has v{}, expected v{}",
                index.version,
                CURRENT_VERSION
            );
        }

        Ok(index)
    }
}
// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::*;
    use crate::ast::index::SymbolTable;
    use crate::ast::intern::ColumnarTable;
    use tempfile::tempdir;

    fn sample_table() -> SymbolTable {
        let mut t = SymbolTable::default();
        t.push_row_strings(
            "foo",
            "function_definition",
            "",
            "",
            Path::new("src/foo.cpp"),
            10..20,
            1,
            HashMap::new(),
        );
        t.add_usage("foo".to_string(), Path::new("src/foo.cpp"), 10..13, 1);
        t.add_usage("foo".to_string(), Path::new("src/bar.cpp"), 55..58, 3);
        t
    }

    #[test]
    fn roundtrip_serialize_deserialize() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(".forgeql-index");

        let original = sample_table();
        let cached = CachedIndex::from_table(original, "abc123", "test-source");

        cached.save(&path).expect("save");
        let loaded = CachedIndex::load(&path).expect("load");
        let recovered = loaded.into_table();

        assert_eq!(recovered.strings.names.len(), 1);
        assert!(recovered.find_def("foo").is_some());
        assert_eq!(recovered.find_usages("foo").len(), 2);
    }

    #[test]
    fn into_table_roundtrip() {
        let original = sample_table();
        let cached = CachedIndex::from_table(original, "", "");
        let recovered = cached.into_table();

        assert!(recovered.find_def("foo").is_some());
        assert_eq!(recovered.find_usages("foo").len(), 2);
    }

    #[test]
    fn load_nonexistent_file_returns_error() {
        let result = CachedIndex::load(Path::new("/tmp/forgeql-no-such-file.idx"));
        assert!(result.is_err());
    }

    #[test]
    fn load_corrupt_data_returns_error() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(".forgeql-index");
        crate::workspace::file_io::write_atomic(&path, b"not valid bincode data here")
            .expect("write");
        let result = CachedIndex::load(&path);
        assert!(result.is_err());
    }

    #[test]
    fn version_mismatch_returns_error() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(".forgeql-index");

        let wrong = CachedIndex {
            version: 999,
            commit_hash: String::new(),
            source_name: String::new(),
            rows: Vec::new(),
            usages: HashMap::new(),
            file_hashes: HashMap::new(),
            macro_defs: Vec::new(),
            strings: ColumnarTable::default(),
        };
        wrong.save(&path).expect("save");

        let result = CachedIndex::load(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("version mismatch"));
    }
}
