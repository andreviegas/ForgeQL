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
//! # Phase 01 scope
//!
//! In this phase:
//! - [`LegacyMemoryStorage`] wraps the existing `SymbolTable` and is the only
//!   active backend. All live queries are served by it.
//! - [`StubColumnarStorage`] is a throwaway empty implementation used to validate
//!   the trait shape compiles for a non-legacy backend.
//! - `SHOW` paths still use [`StorageEngine::as_legacy_table`] in `exec_show`
//!   because the show functions need the concrete `SymbolTable` for ID resolution.
//!   This will change once the columnar backend lands in Phase 05/06.

pub mod git_sha1_provider;
pub mod legacy;
pub mod mock_provider;
pub mod source_provider;
pub mod stub;

pub use legacy::LegacyMemoryStorage;
pub use source_provider::SourceProvider;
pub use stub::StubColumnarStorage;

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
/// The `SymbolTable`-specific `IndexRow` is not needed at this level.
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
}

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
    fn has_index(&self) -> bool;

    // -------- legacy escape hatch -----------------------------------------

    /// Return a reference to the concrete `SymbolTable` for code that
    /// legitimately needs the legacy type (SHOW functions, `exec_change`, tests).
    ///
    /// Returns `None` for non-legacy backends.
    ///
    /// **Phase 01 only.** Will be removed once the SHOW and transform paths
    /// are refactored to work through this trait directly.
    fn as_legacy_table(&self) -> Option<&SymbolTable> {
        None
    }

    /// Return a mutable reference to the concrete `SymbolTable`.
    ///
    /// See [`as_legacy_table`](Self::as_legacy_table) for notes.
    fn as_legacy_table_mut(&mut self) -> Option<&mut SymbolTable> {
        None
    }
}

// -----------------------------------------------------------------------
// Helper: IndexRow → SymbolLocation conversion
// -----------------------------------------------------------------------

/// Convert an `IndexRow` reference + its owning `SymbolTable` into a
/// [`SymbolLocation`].  Used by legacy resolve methods.
pub(crate) fn row_to_location(row: &IndexRow, table: &SymbolTable) -> SymbolLocation {
    SymbolLocation {
        path: table.path_of(row).to_path_buf(),
        byte_range: row.byte_range.clone(),
        line: row.line,
        language_id: row.language_id,
    }
}
