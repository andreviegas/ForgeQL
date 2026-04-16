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
///       with `expanded_reads`, `expansion_failed`, `expansion_failure_reason` fields.
pub const CURRENT_VERSION: u32 = 22;

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
}

impl CachedIndex {
    /// Create a `CachedIndex` by taking ownership of a `SymbolTable`.
    ///
    /// This avoids cloning millions of rows.  Use `into_table()` after
    /// `save()` to recover the table.
    #[must_use]
    pub fn from_table(table: SymbolTable, commit_hash: impl Into<String>) -> Self {
        Self {
            version: CURRENT_VERSION,
            commit_hash: commit_hash.into(),
            rows: table.rows,
            usages: table.usages,
            file_hashes: HashMap::new(),
            macro_defs: Vec::new(),
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
    ) -> Self {
        Self {
            version: CURRENT_VERSION,
            commit_hash: commit_hash.into(),
            rows: table.rows,
            usages: table.usages,
            file_hashes: HashMap::new(),
            macro_defs: macro_table.into_defs(),
        }
    }

    /// Reconstruct a `SymbolTable` from this cache.
    ///
    /// Secondary indexes (`name_index`, `kind_index`) are rebuilt from `rows`
    /// via `push_row`, matching `SymbolTable::build()` behaviour.
    /// Macro definitions are discarded; use `into_table_and_macros` if needed.
    #[must_use]
    pub fn into_table(self) -> SymbolTable {
        let mut table = SymbolTable::default();
        for row in self.rows {
            table.push_row(row);
        }
        table.usages = self.usages;
        table
    }

    /// Reconstruct a `SymbolTable` and a `MacroTable` from this cache.
    ///
    /// This is the counterpart to `from_table_and_macros` — prefer it over
    /// `into_table` whenever the macro table is also needed.
    #[must_use]
    pub fn into_table_and_macros(self) -> (SymbolTable, MacroTable) {
        let mut symbol_table = SymbolTable::default();
        for row in self.rows {
            symbol_table.push_row(row);
        }
        symbol_table.usages = self.usages;

        let mut macro_table = MacroTable::new();
        for def in self.macro_defs {
            macro_table.insert(def);
        }
        (symbol_table, macro_table)
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
    use std::path::PathBuf;

    use super::*;
    use crate::ast::index::{IndexRow, SymbolTable};
    use tempfile::tempdir;

    fn sample_table() -> SymbolTable {
        let mut t = SymbolTable::default();
        t.push_row(IndexRow {
            name: "foo".to_string(),
            node_kind: "function_definition".to_string(),
            fql_kind: String::new(),
            language: String::new(),
            path: PathBuf::from("src/foo.cpp"),
            byte_range: 10..20,
            line: 1,
            fields: HashMap::new(),
        });
        let _ = t.usages.insert(
            "foo".to_string(),
            vec![
                UsageSite {
                    path: PathBuf::from("src/foo.cpp"),
                    byte_range: 10..13,
                    line: 1,
                },
                UsageSite {
                    path: PathBuf::from("src/bar.cpp"),
                    byte_range: 55..58,
                    line: 3,
                },
            ],
        );
        t
    }

    #[test]
    fn roundtrip_serialize_deserialize() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join(".forgeql-index");

        let original = sample_table();
        let cached = CachedIndex::from_table(original, "abc123");

        cached.save(&path).expect("save");
        let loaded = CachedIndex::load(&path).expect("load");

        assert_eq!(loaded.version, CURRENT_VERSION);
        assert_eq!(loaded.commit_hash, "abc123");
        assert!(loaded.rows.iter().any(|r| r.name == "foo"));
        assert_eq!(loaded.usages["foo"].len(), 2);
    }

    #[test]
    fn into_table_roundtrip() {
        let original = sample_table();
        let cached = CachedIndex::from_table(original, "");
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
            rows: Vec::new(),
            usages: HashMap::new(),
            file_hashes: HashMap::new(),
            macro_defs: Vec::new(),
        };
        wrong.save(&path).expect("save");

        let result = CachedIndex::load(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("version mismatch"));
    }
}
