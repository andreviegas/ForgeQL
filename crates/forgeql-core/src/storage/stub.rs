#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
//! `StubColumnarStorage` — throwaway empty [`StorageEngine`] for trait-shape validation.
//!
//! Returns empty results / errors for everything. Used in Phase 01 integration
//! tests to confirm the trait shape works for a non-legacy implementation.
//! Removed in Phase 03 when the real columnar backend lands.
//!
//! # Why this exists
//!
//! A trait designed against a single implementation tends to bake in that
//! implementation's assumptions. Implementing the stub reveals any methods
//! that are impossible to satisfy without `SymbolTable` internals — those
//! methods need to be redesigned before the trait is stable.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::{
    ast::index::{IndexStats, SymbolTable},
    ir::Clauses,
    result::SymbolMatch,
    workspace::Workspace,
};

use super::{StorageEngine, SymbolLocation};

/// Throwaway stub for trait-shape validation in Phase 01.
///
/// Every method returns `Ok(empty)` or `Ok(false)`. Panics are forbidden
/// so the test suite can swap this in without fear of unexpected aborts.
pub struct StubColumnarStorage;

impl StorageEngine for StubColumnarStorage {
    fn backend_name(&self) -> &'static str {
        "stub"
    }

    fn find_symbols(&self, _clauses: &Clauses, _root: &Path) -> Result<Vec<SymbolMatch>> {
        Ok(vec![])
    }

    fn find_usages(
        &self,
        _name: &str,
        _clauses: &Clauses,
        _root: &Path,
    ) -> Result<Vec<SymbolMatch>> {
        Ok(vec![])
    }

    fn resolve_symbol(
        &self,
        _name: &str,
        _clauses: &Clauses,
        _root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        Ok(None)
    }

    fn resolve_type_symbol(
        &self,
        _name: &str,
        _clauses: &Clauses,
        _root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        Ok(None)
    }

    fn resolve_body_symbol(
        &self,
        _name: &str,
        _clauses: &Clauses,
        _root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        Ok(None)
    }

    fn index_stats(&self) -> Option<&IndexStats> {
        None
    }

    fn build(&mut self, _workspace: &Workspace) -> Result<()> {
        Ok(())
    }

    fn reindex_files(&mut self, _paths: &[PathBuf]) -> Result<()> {
        Ok(())
    }

    fn purge_file(&mut self, _path: &Path) -> Result<()> {
        Ok(())
    }

    fn persist_to_cache(
        &mut self,
        _worktree_path: &Path,
        _commit_hash: &str,
        _source_name: &str,
    ) -> Result<()> {
        Ok(())
    }

    fn load_from_cache(
        &mut self,
        _worktree_path: &Path,
        _head_oid: &str,
        _source_name: &str,
    ) -> Result<bool> {
        Ok(false)
    }

    fn drop_stored_index(&mut self) {}

    fn has_index(&self) -> bool {
        false
    }

    fn as_legacy_table(&self) -> Option<&SymbolTable> {
        None
    }

    fn as_legacy_table_mut(&mut self) -> Option<&mut SymbolTable> {
        None
    }
}
