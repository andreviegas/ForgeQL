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

use crate::ast::index::{IndexRow, SymbolTable, UsageSite};

// -----------------------------------------------------------------------
// Current format version
// -----------------------------------------------------------------------

/// Increment this when `CachedIndex` fields change incompatibly.
///
/// `load()` returns `Err` if the on-disk version does not match.
/// v8: `UsageSite` gains `line: usize` — 1-based line number of the identifier token.
pub const CURRENT_VERSION: u32 = 8;

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
        }
    }

    /// Reconstruct a `SymbolTable` from this cache.
    ///
    /// Secondary indexes (`name_index`, `kind_index`) are rebuilt from `rows`
    /// via `push_row`, matching `SymbolTable::build()` behaviour.
    #[must_use]
    pub fn into_table(self) -> SymbolTable {
        let mut table = SymbolTable::default();
        for row in self.rows {
            table.push_row(row);
        }
        table.usages = self.usages;
        table
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
        };
        wrong.save(&path).expect("save");

        let result = CachedIndex::load(&path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("version mismatch"));
    }
}
