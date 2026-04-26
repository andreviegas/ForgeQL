//! Interned string pools for compact [`IndexRow`] field storage.
//!
//! Instead of storing one `String` per row per field, [`IndexRow`] stores a
//! compact integer ID into the matching pool inside [`ColumnarTable`].  The
//! actual string data lives in the pool, shared across all rows.
//!
//! # Memory model
//!
//! | Field       | Cardinality       | Before (per row) | After (per row) |
//! |-------------|-------------------|------------------|-----------------|
//! | `name`      | ~unique           | 24 B + heap      | 4 B (`u32`)     |
//! | `node_kind` | ~50 distinct      | 24 B + heap      | 4 B (`u32`)     |
//! | `fql_kind`  | ≤21 distinct      | 24 B + heap      | 4 B (`u32`)     |
//! | `language`  | ≤5 distinct       | 24 B + heap      | 4 B (`u32`)     |
//! | `path`      | ~100 K distinct   | 24 B + heap      | 4 B (`u32`)     |
//!
//! The pools are **not** serialised as part of `CachedIndex` in this phase —
//! they are rebuilt in O(N) from the row `String` fields during cache load
//! (via [`SymbolTable::push_row`]).  A future cache-version bump may serialise
//! them directly to trade load-time CPU for I/O reduction.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

// -----------------------------------------------------------------------
// StringPool — append-only interning store
// -----------------------------------------------------------------------

/// Append-only string interning pool.
///
/// Each unique string is stored once; subsequent calls to [`intern`] for the
/// same string return the same `u32` ID.  IDs are stable — the pool only
/// ever grows, never reorders.
///
/// [`intern`]: StringPool::intern
#[derive(Debug, Default, Clone)]
pub struct StringPool {
    strings: Vec<String>,
    lookup: HashMap<String, u32>,
}

impl StringPool {
    /// Intern `s` and return its stable `u32` ID.  Amortised O(1).
    ///
    /// # Panics
    /// Panics if the pool would exceed `u32::MAX` unique entries.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.lookup.get(s) {
            return id;
        }
        let id = u32::try_from(self.strings.len())
            .expect("StringPool overflow: more than u32::MAX unique strings");
        self.strings.push(s.to_owned());
        let _ = self.lookup.insert(s.to_owned(), id);
        id
    }

    /// Resolve `id` back to its string slice.
    ///
    /// Returns `""` for any out-of-range ID (defensive; should never occur for
    /// IDs produced by this pool).
    #[must_use]
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub fn get(&self, id: u32) -> &str {
        self.strings.get(id as usize).map_or("", String::as_str)
    }

    /// Number of unique strings stored.
    #[must_use]
    #[inline]
    pub const fn len(&self) -> usize {
        self.strings.len()
    }

    /// `true` if no strings have been interned yet.
    #[must_use]
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.strings.is_empty()
    }
}

// -----------------------------------------------------------------------
// PathPool — same pattern, typed for PathBuf
// -----------------------------------------------------------------------

/// Append-only path interning pool.
///
/// Operates identically to [`StringPool`] but stores [`PathBuf`] values.
/// Paths deduplicate aggressively: at 8 M symbols over ~100 K files the
/// average deduplication ratio is ~80×, reducing the effective per-row cost
/// from ~59 B to 4 B.
#[derive(Debug, Default, Clone)]
pub struct PathPool {
    paths: Vec<PathBuf>,
    lookup: HashMap<PathBuf, u32>,
}

impl PathPool {
    /// Intern `p` and return its stable `u32` ID.  Amortised O(1).
    ///
    /// # Panics
    /// Panics if the pool would exceed `u32::MAX` unique entries.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn intern(&mut self, p: &Path) -> u32 {
        if let Some(&id) = self.lookup.get(p) {
            return id;
        }
        let id = u32::try_from(self.paths.len())
            .expect("PathPool overflow: more than u32::MAX unique paths");
        self.paths.push(p.to_owned());
        let _ = self.lookup.insert(p.to_owned(), id);
        id
    }

    /// Resolve `id` back to a `&Path`.
    ///
    /// Returns `Path::new("")` for any out-of-range ID (defensive).
    #[must_use]
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub fn get(&self, id: u32) -> &Path {
        self.paths
            .get(id as usize)
            .map_or_else(|| Path::new(""), PathBuf::as_path)
    }

    /// Number of unique paths stored.
    #[must_use]
    #[inline]
    pub const fn len(&self) -> usize {
        self.paths.len()
    }

    /// `true` if no paths have been interned yet.
    #[must_use]
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

// -----------------------------------------------------------------------
// ColumnarTable — composite pool for all top-level IndexRow string fields
// -----------------------------------------------------------------------

/// Composite intern pool holding deduplicated copies of every top-level string
/// field in [`IndexRow`].
///
/// [`IndexRow`] stores compact `u32` IDs (`name_id`, `node_kind_id`, …)
/// instead of per-row `String` allocations.  The actual string data lives
/// here, shared across all rows of a [`SymbolTable`].
///
/// **Population**: IDs are assigned by [`ColumnarTable::intern_row`], which is
/// called from [`SymbolTable::push_row`] and the parallel-build merge path.
///
/// **Access**: use [`SymbolTable::name_of`], [`SymbolTable::fql_kind_of`], etc.
/// to resolve IDs back to `&str` / `&Path` slices at output time.
///
/// **Persistence**: this struct is `#[serde(skip)]` on [`SymbolTable`] in the
/// current phase — the pools are reconstructed in O(N) from the row `String`
/// fields on every cache load.
///
/// [`IndexRow`]: crate::ast::index::IndexRow
/// [`SymbolTable`]: crate::ast::index::SymbolTable
#[derive(Debug, Default, Clone)]
pub struct ColumnarTable {
    /// Symbol name pool.  High cardinality (~unique per codebase); deduplicates
    /// overloaded / identically-named symbols across files.
    pub names: StringPool,
    /// Raw tree-sitter `node_kind` pool (~50 distinct values in C/C++).
    pub node_kinds: StringPool,
    /// Universal FQL kind pool (≤ 21 distinct values across all languages).
    pub fql_kinds: StringPool,
    /// Language identifier pool (≤ 5 distinct values currently).
    pub languages: StringPool,
    /// Source file path pool.  Deduplication ratio ~80× at 8 M symbols.
    pub paths: PathPool,
}

impl ColumnarTable {
    /// Intern the five top-level string fields that make up an index row and
    /// return their IDs as `(name_id, node_kind_id, fql_kind_id, language_id, path_id)`.
    ///
    /// Called from [`SymbolTable::push_row`] and the merge path so that every
    /// row in a finalised table has valid IDs.
    #[must_use]
    pub fn intern_row(
        &mut self,
        name: &str,
        node_kind: &str,
        fql_kind: &str,
        language: &str,
        path: &Path,
    ) -> (u32, u32, u32, u32, u32) {
        let name_id = self.names.intern(name);
        let node_kind_id = self.node_kinds.intern(node_kind);
        let fql_kind_id = self.fql_kinds.intern(fql_kind);
        let language_id = self.languages.intern(language);
        let path_id = self.paths.intern(path);
        (name_id, node_kind_id, fql_kind_id, language_id, path_id)
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_pool_deduplicates() {
        let mut pool = StringPool::default();
        let id0 = pool.intern("hello");
        let id1 = pool.intern("world");
        let id2 = pool.intern("hello");
        assert_eq!(id0, id2, "same string must yield same ID");
        assert_ne!(id0, id1);
        assert_eq!(pool.get(id0), "hello");
        assert_eq!(pool.get(id1), "world");
        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn path_pool_deduplicates() {
        use std::path::PathBuf;
        let mut pool = PathPool::default();
        let p = PathBuf::from("src/main.rs");
        let id0 = pool.intern(&p);
        let id1 = pool.intern(&p);
        let id2 = pool.intern(Path::new("src/lib.rs"));
        assert_eq!(id0, id1);
        assert_ne!(id0, id2);
        assert_eq!(pool.get(id0), p.as_path());
    }

    #[test]
    fn columnar_table_intern_row() {
        let mut col = ColumnarTable::default();
        let (n0, nk0, fk0, l0, p0) = col.intern_row(
            "my_func",
            "function_definition",
            "function",
            "cpp",
            Path::new("src/foo.cpp"),
        );
        // Second call with same values — IDs must be identical.
        let (n1, nk1, fk1, l1, p1) = col.intern_row(
            "my_func",
            "function_definition",
            "function",
            "cpp",
            Path::new("src/foo.cpp"),
        );
        assert_eq!(n0, n1);
        assert_eq!(nk0, nk1);
        assert_eq!(fk0, fk1);
        assert_eq!(l0, l1);
        assert_eq!(p0, p1);
        // Different function in the same file shares node_kind/fql_kind/language/path IDs.
        let (n2, nk2, fk2, l2, p2) = col.intern_row(
            "other_func",
            "function_definition",
            "function",
            "cpp",
            Path::new("src/foo.cpp"),
        );
        assert_ne!(n0, n2, "different names must get different IDs");
        assert_eq!(nk0, nk2, "same node_kind must share ID");
        assert_eq!(fk0, fk2);
        assert_eq!(l0, l2);
        assert_eq!(p0, p2, "same path must share ID");
    }
}
