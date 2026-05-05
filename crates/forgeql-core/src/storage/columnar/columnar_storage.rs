#![allow(clippy::redundant_pub_crate)]
//! [`ColumnarStorage`] — the columnar disk-backed [`StorageEngine`].
//!
//! Implements the `StorageEngine` trait using the workspace overlay index
//! and per-file segment readers introduced in Phase 04/05.
//!
//! Query execution for `FIND symbols USING 'columnar'`:
//! 1. **Prefilter** — use the overlay's merged Roaring bitmaps to narrow
//!    the candidate global row IDs via indexed predicates (`fql_kind`, `name`).
//! 2. **Group** — partition candidate global row IDs by segment index.
//! 3. **Materialize** — for each segment, call `SegmentReader::materialize_rows`
//!    with the per-segment local row bitmap and the absolute source path.
//! 4. **Residual filter** — apply remaining clauses (other `WHERE`, `IN`,
//!    `EXCLUDE`, `ORDER BY`, `GROUP BY`, `LIMIT`, `OFFSET`) via
//!    `filter::apply_clauses`.
//!
//! `SHOW` commands (`resolve_symbol`, etc.) are out of scope for Phase 05 and
//! return a "Phase 06" error so callers can fall back to the legacy backend.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use roaring::RoaringBitmap;

use crate::ast::index::{IndexStats, SymbolTable};
use crate::filter::apply_clauses;
use crate::ir::{Clauses, CompareOp, PredicateValue};
use crate::result::SymbolMatch;
use crate::workspace::Workspace;

use super::overlay::{Overlay, RowPtr};
use super::segment_reader::SegmentReader;
use crate::storage::{StorageEngine, SymbolLocation};

// ─────────────────────────────────────────────────────────────────────────────
// ColumnarStorage
// ─────────────────────────────────────────────────────────────────────────────

/// Disk-backed columnar [`StorageEngine`] backed by per-file segment readers
/// and a workspace-level overlay index.
///
/// Constructed by `exec_source::use_source` after the overlay is built/opened.
pub struct ColumnarStorage {
    /// Worktree root; used to compute absolute source paths for materialization.
    worktree_root: PathBuf,
    /// Per-segment readers in the same order as `overlay.segments()`.
    segments: Vec<Arc<SegmentReader>>,
    /// Workspace overlay shared across sessions on the same commit SHA.
    overlay: Arc<Overlay>,
}

impl ColumnarStorage {
    /// Create a new `ColumnarStorage` from an open overlay and its segments.
    ///
    /// `segments` **must** be in the same order as `overlay.segments()`.
    #[must_use]
    pub const fn new(
        worktree_root: PathBuf,
        segments: Vec<Arc<SegmentReader>>,
        overlay: Arc<Overlay>,
    ) -> Self {
        Self {
            worktree_root,
            segments,
            overlay,
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Query helpers
    // ─────────────────────────────────────────────────────────────────────

    /// Stage 1 — build a candidate global-row-id bitmap using indexed predicates.
    ///
    /// Handles `WHERE fql_kind = 'X'` and `WHERE name = 'Y'` (exact match).
    /// Other predicates are handled later by `apply_clauses`.
    fn prefilter_global(&self, clauses: &Clauses) -> RoaringBitmap {
        let mut result: Option<RoaringBitmap> = None;

        for pred in &clauses.where_predicates {
            let Some(kind_bm) = (match (pred.field.as_str(), &pred.op, &pred.value) {
                ("fql_kind", CompareOp::Eq, PredicateValue::String(val)) => {
                    self.overlay.prefilter_kind(val).cloned()
                }
                ("name", CompareOp::Eq, PredicateValue::String(val)) => {
                    let bm = self.overlay.lookup_name_bitmap(val);
                    if bm.is_empty() {
                        Some(RoaringBitmap::new())
                    } else {
                        Some(bm)
                    }
                }
                _ => None,
            }) else {
                continue;
            };
            result = Some(match result {
                Some(prev) => prev & kind_bm,
                None => kind_bm,
            });
        }

        result.unwrap_or_else(|| (0..self.overlay.row_count()).collect())
    }

    /// Stage 2 — partition global row IDs by segment index.
    fn group_by_segment(&self, global_ids: &RoaringBitmap) -> HashMap<u32, RoaringBitmap> {
        let mut by_segment: HashMap<u32, RoaringBitmap> = HashMap::new();
        for global_id in global_ids {
            if let Some(RowPtr {
                segment_idx,
                local_row_idx,
            }) = self.overlay.resolve_global(global_id)
            {
                let _ = by_segment
                    .entry(segment_idx)
                    .or_default()
                    .insert(local_row_idx);
            }
        }
        by_segment
    }

    /// Stage 3 — materialize rows from each segment.
    fn materialize_all(&self, by_segment: &HashMap<u32, RoaringBitmap>) -> Vec<SymbolMatch> {
        let mut results = Vec::new();
        for (&seg_idx, local_rows) in by_segment {
            let Some(seg) = self.segments.get(seg_idx as usize) else {
                continue;
            };
            let Some(seg_meta) = self.overlay.segments().get(seg_idx as usize) else {
                continue;
            };
            let source_path = self.worktree_root.join(&seg_meta.source_path);
            let mut seg_results = seg.materialize_rows(local_rows, Some(&source_path));
            results.append(&mut seg_results);
        }
        results
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// StorageEngine implementation
// ─────────────────────────────────────────────────────────────────────────────

impl StorageEngine for ColumnarStorage {
    fn backend_name(&self) -> &'static str {
        "columnar"
    }

    fn find_symbols(&self, clauses: &Clauses, _root: &Path) -> Result<Vec<SymbolMatch>> {
        let candidates = self.prefilter_global(clauses);
        let by_segment = self.group_by_segment(&candidates);
        let mut results = self.materialize_all(&by_segment);
        apply_clauses(&mut results, clauses);
        Ok(results)
    }

    fn find_usages(&self, name: &str, clauses: &Clauses, _root: &Path) -> Result<Vec<SymbolMatch>> {
        // Phase 05 scope: exact-name FST lookup.
        let candidates = self.overlay.lookup_name_bitmap(name);
        let by_segment = self.group_by_segment(&candidates);
        let mut results = self.materialize_all(&by_segment);
        apply_clauses(&mut results, clauses);
        Ok(results)
    }

    fn resolve_symbol(
        &self,
        _name: &str,
        _clauses: &Clauses,
        _root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        // Phase 06: SHOW commands on the columnar backend.
        Err(anyhow::anyhow!(
            "SHOW commands on the columnar backend are not available until Phase 06; \
             use the default (legacy) backend for SHOW operations"
        ))
    }

    fn resolve_type_symbol(
        &self,
        _name: &str,
        _clauses: &Clauses,
        _root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        Err(anyhow::anyhow!(
            "SHOW commands on the columnar backend require Phase 06"
        ))
    }

    fn resolve_body_symbol(
        &self,
        _name: &str,
        _clauses: &Clauses,
        _root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        Err(anyhow::anyhow!(
            "SHOW commands on the columnar backend require Phase 06"
        ))
    }

    fn index_stats(&self) -> Option<&IndexStats> {
        None
    }

    fn build(&mut self, _workspace: &Workspace) -> Result<()> {
        // The columnar backend is populated by ShadowWriter during the legacy
        // build; it does not expose its own independent build path.
        Err(anyhow::anyhow!(
            "ColumnarStorage::build is not callable directly; \
             use shadow_write via LegacyMemoryStorage"
        ))
    }

    fn reindex_files(&mut self, _paths: &[PathBuf]) -> Result<()> {
        // Phase 07: incremental reindex.
        Err(anyhow::anyhow!(
            "ColumnarStorage incremental reindex is not available until Phase 07"
        ))
    }

    fn purge_file(&mut self, _path: &Path) -> Result<()> {
        // Phase 07.
        Err(anyhow::anyhow!(
            "ColumnarStorage::purge_file requires Phase 07"
        ))
    }

    fn persist_to_cache(
        &mut self,
        _worktree_path: &Path,
        _commit_hash: &str,
        _source_name: &str,
    ) -> Result<()> {
        // Overlays are already on disk — no separate cache step needed.
        Ok(())
    }

    fn load_from_cache(
        &mut self,
        _worktree_path: &Path,
        _head_oid: &str,
        _source_name: &str,
    ) -> Result<bool> {
        // The overlay is opened by `use_source`; not loaded here.
        Ok(false)
    }

    fn drop_stored_index(&mut self) {
        // Nothing to drop — the overlay is on disk.
    }

    fn has_index(&self) -> bool {
        true
    }

    fn show_outline_for_file(
        &self,
        _workspace: &Workspace,
        file: &str,
    ) -> Result<serde_json::Value> {
        // Phase 06.
        Ok(serde_json::json!({
            "op": "show_outline",
            "file": file,
            "results": [],
            "note": "columnar SHOW requires Phase 06"
        }))
    }

    fn as_legacy_table(&self) -> Option<&SymbolTable> {
        None
    }

    fn as_legacy_table_mut(&mut self) -> Option<&mut SymbolTable> {
        None
    }
}
