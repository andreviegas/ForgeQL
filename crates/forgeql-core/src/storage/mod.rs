#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
//! Storage engine abstraction for ForgeQL.
//!
//! This module defines the [`StorageEngine`] trait — a MySQL-handler–style
//! abstraction that decouples all `exec_*` query paths from the concrete
//! `SymbolTable` type. Every backend (legacy in-RAM, future columnar disk
//! store) implements this trait.
//!
//! Also contains the [`SourceProvider`] trait (see [`source_provider`]) that
//! decouples the storage engine from git internals.
//!
//! # Phase 05.4 scope
//!
//! In this phase:
//! - [`LegacyMemoryStorage`] wraps the existing `SymbolTable` and is the only
//!   active backend. All live queries are served by it.
//! - [`StubColumnarStorage`] is a throwaway empty implementation used to validate
//!   the trait shape compiles for a non-legacy backend.
//! - `SHOW` paths reach the legacy table via `Session::index()` which calls
//!   `BackendSet::legacy_storage()`. The `StorageEngine` trait contains no
//!   legacy-specific escape hatches as of Phase 05.4.

pub mod backend_set;
pub mod columnar;
pub mod git_sha1_provider;
pub mod legacy;
pub mod mock_provider;
pub mod source_provider;
pub mod stub;

pub use backend_set::BackendSet;
pub use columnar::overlay::Overlay;
pub use columnar::shadow_writer::ShadowWriteResult;
pub use columnar::{
    ColumnarBuildContext, ColumnarStorage, HashFn, OverlayBuilder, SegmentReader, ShadowWriter,
};
pub use legacy::LegacyMemoryStorage;
pub use source_provider::SourceProvider;
pub use stub::StubColumnarStorage;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::ast::index::{IndexRow, IndexStats, SymbolTable};
use crate::ir::Clauses;
use crate::result::SymbolMatch;
use crate::workspace::Workspace;

// -----------------------------------------------------------------------
// SymbolLocation — lightweight symbol reference for SHOW resolution
// -----------------------------------------------------------------------

/// Identifies the on-disk location of a single symbol definition.
///
/// Returned by [`StorageEngine::resolve_symbol`] and its variants.
/// Contains enough information to re-read and re-parse the source file
/// without retaining any reference into the storage backend.
///
/// The `exec_show` path obtains a `SymbolLocation`, reads the source bytes
/// from disk, and feeds the row info into the tree-sitter re-parser.
#[derive(Debug, Clone)]
pub struct SymbolLocation {
    /// Absolute path to the source file containing the symbol.
    pub path: PathBuf,
    /// Byte range of the symbol node within the source file.
    pub byte_range: std::ops::Range<usize>,
    /// 1-based start line number of the symbol.
    pub line: usize,
    /// Interned language ID (backend-specific).
    pub language_id: u32,
    /// Raw tree-sitter node kind (e.g. `"function_definition"`).
    /// Used by `show_signature` to determine whether to look for a body node.
    pub node_kind: String,
    /// Pre-resolved enrichment fields for this symbol.
    /// Populated by `row_to_location`; empty for non-legacy backends.
    pub enrichment: HashMap<String, String>,
    /// Content SHA-1 of the source file at resolve time, when known.
    ///
    /// Populated by the columnar backend from `SegmentMeta::content_id`.
    /// The legacy backend always leaves this as `None`.
    /// When `Some`, `get_or_parse_for_show` can skip `read_bytes` on a cache
    /// hit and skip `sha1_of_bytes` on a miss.
    pub blob_sha: Option<[u8; 20]>,
}

// -----------------------------------------------------------------------
// -----------------------------------------------------------------------
// StorageEngine trait
// -----------------------------------------------------------------------

/// The central abstraction over all ForgeQL storage backends.
///
/// All `exec_*` query paths go through this trait. The concrete
/// [`LegacyMemoryStorage`] is the default implementation for Phase 01;
/// a columnar disk-backed engine will be added in later phases.
///
/// Implementors must be `Send + Sync` so sessions can be held in a
/// `HashMap` inside `Arc<Mutex<ForgeQLEngine>>`.
pub trait StorageEngine: Send + Sync {
    /// Short identifier for the backend, e.g. `"legacy"`, `"columnar"`.
    fn backend_name(&self) -> &'static str;

    // -------- read-only queries ----------------------------------------

    /// Execute a `FIND symbols` query.
    ///
    /// Applies all fast-path index shortcuts, predicate evaluation, ORDER BY,
    /// and GROUP BY internally. The caller is responsible for DEFAULT_QUERY_LIMIT
    /// truncation and result formatting.
    ///
    /// Returns the full result set (no truncation).
    fn find_symbols(&self, clauses: &Clauses, root: &Path) -> Result<Vec<SymbolMatch>>;

    /// Execute a `FIND usages OF 'name'` query.
    ///
    /// Applies glob filtering and the remaining clause pipeline internally.
    /// Returns the full result set (no truncation).
    fn find_usages(&self, name: &str, clauses: &Clauses, root: &Path) -> Result<Vec<SymbolMatch>>;

    // -------- symbol resolution (for SHOW) --------------------------------

    /// Resolve a symbol name to its on-disk location.
    ///
    /// Applies `IN`/`EXCLUDE` and `WHERE` clauses to disambiguate when
    /// multiple candidates exist. Returns `Ok(None)` when the symbol is not
    /// found (the caller may emit a friendly error).
    fn resolve_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>>;

    /// Like [`resolve_symbol`] but prefers type definitions with members.
    fn resolve_type_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>>;

    /// Like [`resolve_symbol`] but follows the `body_symbol` redirect field.
    fn resolve_body_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>>;

    // -------- aggregates --------------------------------------------------

    /// Return a reference to the pre-aggregated [`IndexStats`], if the index
    /// has been built.
    fn index_stats(&self) -> Option<&IndexStats>;

    // -------- lifecycle ---------------------------------------------------

    /// Build a fresh index from all files in `workspace`.
    ///
    /// After a successful call `has_index()` returns `true`.
    fn build(&mut self, workspace: &Workspace) -> Result<()>;

    /// Incrementally re-index the given paths after a mutation.
    ///
    /// Deleted files are purged; modified files are re-parsed.
    fn reindex_files(&mut self, paths: &[PathBuf]) -> Result<()>;

    /// Remove all rows originating from a single source file.
    fn purge_file(&mut self, path: &Path) -> Result<()>;

    /// Persist the in-memory index to `<worktree_path>/.forgeql-index`.
    ///
    /// `commit_hash` and `source_name` are stored in the cache header so
    /// that `load_from_cache` can validate freshness on the next resume.
    fn persist_to_cache(
        &mut self,
        worktree_path: &Path,
        commit_hash: &str,
        source_name: &str,
    ) -> Result<()>;

    /// Attempt to load the index from `<worktree_path>/.forgeql-index`.
    ///
    /// Returns `true` on a cache hit (the cached commit matches `head_oid`
    /// and the source name matches), `false` when the cache is absent or
    /// stale (caller should call `build` instead).
    fn load_from_cache(
        &mut self,
        worktree_path: &Path,
        head_oid: &str,
        source_name: &str,
    ) -> Result<bool>;

    /// Drop the in-memory index without saving.
    ///
    /// Used by `ROLLBACK` so the next `resume_index` reads the freshly
    /// restored `.forgeql-index` from disk.
    fn drop_stored_index(&mut self);

    /// Return `true` when an index has been built or loaded from cache.
    /// Return `true` when an index has been built or loaded from cache.
    fn has_index(&self) -> bool;

    // -------- SHOW helpers ------------------------------------------------

    /// Locate a symbol definition by name, returning its file path and line.
    ///
    /// Used by `show_callees` to annotate each callee with its definition
    /// location. Returns `None` when the name is not found or the backend
    /// does not support definition lookup.
    fn locate_definition(&self, _name: &str) -> Option<(PathBuf, usize)> {
        None
    }

    /// Render `SHOW outline OF 'file'` as a JSON value.
    ///
    /// Delegates to the backend's symbol rows so `exec_show` does not need to
    /// hold a `&SymbolTable` reference for outline queries.
    fn show_outline_for_file(&self, workspace: &Workspace, file: &str)
    -> Result<serde_json::Value>;
}
pub(crate) fn row_to_location(row: &IndexRow, table: &SymbolTable) -> SymbolLocation {
    SymbolLocation {
        path: table.path_of(row).to_path_buf(),
        byte_range: row.byte_range.clone(),
        line: row.line,
        language_id: row.language_id,
        node_kind: table.node_kind_of(row).to_string(),
        enrichment: table.strings.resolve_fields(&row.fields),
        blob_sha: None,
    }
}
// -----------------------------------------------------------------------
// Phase 01 integration tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{StorageEngine, SymbolLocation};
    use crate::{
        ir::Clauses,
        storage::{
            mock_provider::MockProvider,
            source_provider::{ContentId, SourceProvider},
            stub::StubColumnarStorage,
        },
    };

    // --- StubColumnarStorage (trait shape) ---

    #[test]
    fn stub_backend_name() {
        let s = StubColumnarStorage;
        assert_eq!(s.backend_name(), "stub");
    }

    #[test]
    fn stub_has_no_index() {
        let s = StubColumnarStorage;
        assert!(!s.has_index());
    }

    #[test]
    fn stub_find_symbols_returns_empty() {
        let s = StubColumnarStorage;
        let clauses = Clauses::default();
        let root = Path::new("/tmp");
        let results = s.find_symbols(&clauses, root).expect("should not error");
        assert!(results.is_empty());
    }

    #[test]
    fn stub_find_usages_returns_empty() {
        let s = StubColumnarStorage;
        let clauses = Clauses::default();
        let root = Path::new("/tmp");
        let results = s
            .find_usages("foo", &clauses, root)
            .expect("should not error");
        assert!(results.is_empty());
    }

    #[test]
    fn stub_resolve_symbol_returns_none() {
        let s = StubColumnarStorage;
        let clauses = Clauses::default();
        let root = Path::new("/tmp");
        let loc: Option<SymbolLocation> = s
            .resolve_symbol("foo", &clauses, root)
            .expect("should not error");
        assert!(loc.is_none());
    }

    #[test]
    fn stub_persist_and_load_are_noops() {
        let mut s = StubColumnarStorage;
        s.persist_to_cache(Path::new("/tmp"), "abc123", "test")
            .expect("persist noop");
        let loaded = s
            .load_from_cache(Path::new("/tmp"), "abc123", "test")
            .expect("load noop");
        assert!(!loaded, "stub always returns false for load");
    }

    // --- MockProvider (SourceProvider shape) ---

    #[test]
    fn mock_provider_insert_and_read() {
        let mut p = MockProvider::default();
        let id = p.insert(b"hello");
        let bytes = p.read_content(&id).expect("blob must exist");
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn mock_provider_hash_is_deterministic() {
        let p = MockProvider::default();
        let id1 = p.hash_content(b"world");
        let id2 = p.hash_content(b"world");
        assert_eq!(id1.hex(), id2.hex(), "same bytes must produce same id");
    }

    #[test]
    fn mock_provider_walk_snapshot() {
        let mut p = MockProvider::default();
        let id = p.insert(b"fn foo() {}");
        p.add_snapshot("snap-a", vec![(PathBuf::from("src/foo.rs"), id.clone())]);
        p.set_current("snap-a");

        let snap = p
            .current_snapshot(Path::new("/repo"))
            .expect("current snap");
        let entries: Vec<_> = p
            .walk_snapshot(&snap)
            .expect("walk ok")
            .map(|r| r.expect("entry ok"))
            .collect();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, PathBuf::from("src/foo.rs"));
        assert_eq!(entries[0].1.hex(), id.hex());
    }

    #[test]
    fn mock_provider_changed_paths() {
        let mut p = MockProvider::default();
        let id_a = p.insert(b"v1");
        let id_b = p.insert(b"v2");
        let id_c = p.insert(b"new");

        p.add_snapshot(
            "snap-1",
            vec![
                (PathBuf::from("a.rs"), id_a.clone()),
                (PathBuf::from("b.rs"), id_b),
            ],
        );
        p.add_snapshot(
            "snap-2",
            vec![
                (PathBuf::from("a.rs"), id_a),
                (PathBuf::from("b.rs"), id_c.clone()),
                (PathBuf::from("c.rs"), id_c),
            ],
        );

        let from = MockProvider::mock_snapshot("snap-1");
        let to = MockProvider::mock_snapshot("snap-2");

        let changed = p.changed_paths(&from, &to).expect("changed paths ok");
        assert!(changed.contains(&PathBuf::from("b.rs")));
        assert!(changed.contains(&PathBuf::from("c.rs")));
        assert!(!changed.contains(&PathBuf::from("a.rs")));
    }

    #[test]
    fn stub_satisfies_dyn_trait_bound() {
        let _: Box<dyn StorageEngine> = Box::new(StubColumnarStorage);
    }
}
