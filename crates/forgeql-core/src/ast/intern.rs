//! Generic interning pool for compact [`IndexRow`] field storage.
//!
//! Instead of storing one `String` per row per field, [`IndexRow`] stores a
//! compact `u32` ID into the matching pool inside [`ColumnarTable`].  The
//! actual string data lives in the pool, shared across all rows.
//!
//! # Design
//!
//! A single generic type [`InternPool<O>`] handles any owned type `O`.
//! The two concrete pools used by [`ColumnarTable`] are type aliases:
//!
//! | Alias        | Owned type | Borrowed type | Cardinality       |
//! |--------------|------------|---------------|-------------------|
//! | `StringPool` | `String`   | `&str`        | varies            |
//! | `PathPool`   | `PathBuf`  | `&Path`       | varies            |
//!
//! Lookup by the borrowed form (`&str`, `&Path`) uses the standard library's
//! [`Borrow`] trait — **no allocation on cache hits**.  On a miss, one
//! `to_owned()` converts the borrowed value to owned; a single `clone()` fills
//! both the `Vec` slot and the `HashMap` key, replacing the previous
//! double-`to_owned()` pattern.
//!
//! # Adding a new pool type
//!
//! Any type pair `(Owned, Borrowed)` where `Owned: Borrow<Borrowed>` and
//! `Borrowed: ToOwned<Owned = Owned>` works out of the box:
//!
//! ```rust,ignore
//! // e.g. intern raw tree-sitter byte slices
//! pub type BytesPool = InternPool<Vec<u8>>;
//! ```
//!
//! You only need to add a `get` / `iter` impl block if you want the
//! type-specific ergonomic accessor (see [`InternPool<String>`] and
//! [`InternPool<PathBuf>`] below).
//!
//! [`Borrow`]: std::borrow::Borrow
//! [`IndexRow`]: crate::ast::index::IndexRow

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------
// InternPool<O> — generic append-only interning store
// -----------------------------------------------------------------------

/// Generic append-only interning pool.
///
/// `O` is the **owned** stored type (e.g. [`String`], [`PathBuf`]).
/// Intern by borrowing (`&str`, `&Path`) via [`InternPool::intern`]; look up
/// by ID via the type-specific `get` impl.
///
/// IDs are stable `u32` values — the pool only ever grows, never reorders.
/// Lookup on a hit is allocation-free thanks to `HashMap::get`'s [`Borrow`]
/// blanket (e.g. `HashMap<String, u32>::get(&str)` works without cloning).
///
/// # Type aliases
///
/// - [`StringPool`] = `InternPool<String>`
/// - [`PathPool`]   = `InternPool<PathBuf>`
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct InternPool<O: Eq + Hash> {
    items: Vec<O>,
    lookup: HashMap<O, u32>,
}

impl<O: Eq + Hash> InternPool<O> {
    /// Intern `value` by its borrowed form and return a stable `u32` ID.
    ///
    /// - **Hit**: returns the existing ID with zero allocations.
    /// - **Miss**: converts via `to_owned()` once, then `clone()`s into the
    ///   `Vec` slot; the original owned value becomes the `HashMap` key.
    ///   Two allocations total — one fewer than the previous double-`to_owned()`
    ///   pattern.
    ///
    /// # Panics
    /// Panics if the pool would exceed `u32::MAX` unique entries.
    ///
    /// # Example
    /// ```rust,ignore
    /// let mut pool = StringPool::default();
    /// let id = pool.intern("hello");
    /// assert_eq!(pool.get(id), "hello");
    /// ```
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn intern<B>(&mut self, value: &B) -> u32
    where
        B: ToOwned<Owned = O> + Hash + Eq + ?Sized,
        O: Borrow<B> + Clone,
    {
        // Hit path — Borrow<B> lets HashMap accept &B without cloning the key.
        if let Some(&id) = self.lookup.get(value) {
            return id;
        }
        // Miss path — one to_owned() + one clone, not two to_owned() calls.
        let id = u32::try_from(self.items.len())
            .expect("InternPool overflow: more than u32::MAX unique entries");
        let owned = value.to_owned();
        self.items.push(owned.clone());
        let _ = self.lookup.insert(owned, id);
        id
    }

    /// Return the ID for `key` if it has been interned, without inserting.
    ///
    /// Uses [`Borrow`] so you can pass `&str` into a `StringPool` or
    /// `&Path` into a `PathPool` without allocating.
    #[must_use]
    #[inline]
    pub fn get_id<B>(&self, key: &B) -> Option<u32>
    where
        O: Borrow<B>,
        B: Hash + Eq + ?Sized,
    {
        self.lookup.get(key).copied()
    }

    /// Iterate all interned values in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &O> {
        self.items.iter()
    }

    /// Number of unique entries stored.
    #[must_use]
    #[inline]
    pub const fn len(&self) -> usize {
        self.items.len()
    }

    /// `true` if no entries have been interned yet.
    #[must_use]
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

// -----------------------------------------------------------------------
// Concrete get() + iter() impls
//
// The generic impl cannot define get() because the empty-value fallback
// differs by type ("" for strings, Path::new("") for paths).  Each
// concrete impl below adds the idiomatic ergonomic accessor.
// -----------------------------------------------------------------------

impl InternPool<String> {
    /// Resolve `id` back to a `&str`.
    ///
    /// Returns `""` for any out-of-range ID (defensive; should never occur
    /// for IDs produced by this pool).
    #[must_use]
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub fn get(&self, id: u32) -> &str {
        self.items.get(id as usize).map_or("", String::as_str)
    }

    /// Iterate all interned strings in insertion order as `&str` slices.
    pub fn iter_str(&self) -> impl Iterator<Item = &str> {
        self.items.iter().map(String::as_str)
    }
}

impl InternPool<PathBuf> {
    /// Resolve `id` back to a `&Path`.
    ///
    /// Returns `Path::new("")` for any out-of-range ID (defensive).
    #[must_use]
    #[inline]
    #[allow(clippy::cast_possible_truncation)]
    pub fn get(&self, id: u32) -> &Path {
        self.items
            .get(id as usize)
            .map_or_else(|| Path::new(""), PathBuf::as_path)
    }

    /// Iterate all interned paths in insertion order as `&Path` slices.
    pub fn iter_paths(&self) -> impl Iterator<Item = &Path> {
        self.items.iter().map(PathBuf::as_path)
    }
}

// -----------------------------------------------------------------------
// Type aliases — backward-compatible names for the two pools used
// throughout the codebase.  All call sites continue to compile unchanged.
// -----------------------------------------------------------------------

/// Intern pool for string values.  Alias for `InternPool<String>`.
///
/// Intern with `&str`; resolve with [`get`][InternPool<String>::get].
pub type StringPool = InternPool<String>;

/// Intern pool for path values.  Alias for `InternPool<PathBuf>`.
///
/// Intern with `&Path`; resolve with [`get`][InternPool<PathBuf>::get].
pub type PathPool = InternPool<PathBuf>;

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
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
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

    // --- Generic InternPool<String> (via StringPool alias) ---------------

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
    fn string_pool_hit_no_alloc_semantic() {
        // Verify the hit path: get_id must return Some after intern.
        let mut pool = StringPool::default();
        let id = pool.intern("zephyr");
        assert_eq!(pool.get_id("zephyr"), Some(id));
        assert_eq!(pool.get_id("other"), None);
    }

    #[test]
    fn string_pool_out_of_range_returns_empty() {
        let pool = StringPool::default();
        assert_eq!(pool.get(0), "");
        assert_eq!(pool.get(u32::MAX), "");
    }

    // --- Generic InternPool<PathBuf> (via PathPool alias) ----------------

    #[test]
    fn path_pool_deduplicates() {
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
    fn path_pool_get_id() {
        let mut pool = PathPool::default();
        let id = pool.intern(Path::new("include/zephyr/kernel.h"));
        assert_eq!(pool.get_id(Path::new("include/zephyr/kernel.h")), Some(id));
        assert_eq!(pool.get_id(Path::new("other.h")), None);
    }

    // --- ColumnarTable ---------------------------------------------------

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

    /// Push many rows sharing the same low-cardinality fields.  The pool sizes
    /// must reflect unique values only, not row count.
    #[test]
    fn pool_dedupes_at_scale() {
        let mut col = ColumnarTable::default();
        // 1 000 rows: each has a unique name but shares everything else.
        for i in 0..1_000_u32 {
            let _ = col.intern_row(
                &format!("sym_{i}"),
                "function_definition",
                "function",
                "cpp",
                Path::new("src/large.cpp"),
            );
        }
        assert_eq!(col.names.len(), 1_000, "every name is unique");
        assert_eq!(col.node_kinds.len(), 1, "only one node_kind variant");
        assert_eq!(col.fql_kinds.len(), 1, "only one fql_kind variant");
        assert_eq!(col.languages.len(), 1, "only one language variant");
        assert_eq!(col.paths.len(), 1, "all rows share one path");
    }

    /// IDs returned from `intern` must resolve back to the original string.
    #[test]
    fn pool_roundtrip() {
        let mut pool = StringPool::default();
        let words = ["alpha", "beta", "gamma", "alpha", "delta", "beta"];
        let ids: Vec<u32> = words.iter().map(|w| pool.intern(*w)).collect();
        for (w, id) in words.iter().zip(ids.iter()) {
            assert_eq!(pool.get(*id), *w, "round-trip must be lossless");
        }
        // Only 4 unique words.
        assert_eq!(pool.len(), 4);
    }

    /// Demonstrate that any (Owned, Borrowed) pair satisfying `Borrow` + `ToOwned`
    /// works without writing a new pool type.
    #[test]
    fn generic_pool_works_for_vec_u8() {
        // InternPool<Vec<u8>> — intern byte slices, resolve by &[u8].
        let mut pool: InternPool<Vec<u8>> = InternPool::default();
        let id0 = pool.intern(b"hello".as_slice());
        let id1 = pool.intern(b"world".as_slice());
        let id2 = pool.intern(b"hello".as_slice());
        assert_eq!(id0, id2);
        assert_ne!(id0, id1);
        assert_eq!(pool.get_id(b"hello".as_slice()), Some(id0));
        assert_eq!(pool.len(), 2);
    }
}
