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

use anyhow::{Result, anyhow};
use roaring::RoaringBitmap;
use tracing::debug;

use crate::ast::index::IndexStats;
use crate::filter::apply_clauses;
use crate::ir::{Clauses, CompareOp, PredicateValue};
use crate::result::SymbolMatch;
use crate::workspace::Workspace;

use super::overlay::{Overlay, RowPtr};
use super::overlay_lock::OverlayLock;
use super::segment_reader::SegmentReader;
use crate::storage::{LegacyMemoryStorage, StorageEngine, SymbolLocation};

// ─────────────────────────────────────────────────────────────────────────────
// ColumnarStorage
// ─────────────────────────────────────────────────────────────────────────────

/// Disk-backed columnar [`StorageEngine`] backed by per-file segment readers
/// and a workspace-level overlay index.
///
/// Constructed by `exec_source::use_source` after the overlay is built/opened.
pub struct ColumnarStorage {
    /// Worktree root; will be used by Phase 06 SHOW operations to resolve
    /// absolute source file paths for context/body retrieval.
    #[allow(dead_code)]
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
    /// Handles `WHERE fql_kind = 'X'`, `WHERE name = 'Y'` (exact match), and
    /// `WHERE name LIKE 'pattern'` / `WHERE name MATCHES 'regex'` via the
    /// trigram index when the pattern contains a literal substring of \u22653 chars.
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
                ("name", CompareOp::Like, PredicateValue::String(val)) => {
                    self.trigram_prefilter_for_pattern(val)
                }
                ("name", CompareOp::Matches, PredicateValue::String(val)) => {
                    self.trigram_prefilter_for_regex(val)
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

    /// Compute a trigram-based candidate bitmap from a SQL `LIKE` pattern.
    ///
    /// Returns `None` when no usable literal trigram can be extracted
    /// (caller should skip the prefilter for this predicate).
    fn trigram_prefilter_for_pattern(&self, pattern: &str) -> Option<RoaringBitmap> {
        let literals = crate::filter::like_pattern_literals(pattern);
        self.intersect_literal_trigrams(&literals)
    }

    /// Compute a trigram-based candidate bitmap from a regex.
    ///
    /// Conservatively only uses literal-character runs that don't contain
    /// any regex metacharacter.  Returns `None` when no run is \u2265 3 chars.
    fn trigram_prefilter_for_regex(&self, pattern: &str) -> Option<RoaringBitmap> {
        const META: &[char] = &[
            '\\', '.', '+', '*', '?', '(', ')', '[', ']', '{', '}', '|', '^', '$',
        ];
        let mut literals: Vec<String> = Vec::new();
        let mut cur = String::new();
        for ch in pattern.chars() {
            if META.contains(&ch) {
                if !cur.is_empty() {
                    literals.push(std::mem::take(&mut cur));
                }
            } else {
                cur.push(ch);
            }
        }
        if !cur.is_empty() {
            literals.push(cur);
        }
        self.intersect_literal_trigrams(&literals)
    }

    fn intersect_literal_trigrams(&self, literals: &[String]) -> Option<RoaringBitmap> {
        let mut acc: Option<RoaringBitmap> = None;
        for lit in literals {
            if lit.len() < 3 {
                continue;
            }
            let Some(bm) = self.overlay.name_substring_candidates(lit) else {
                continue;
            };
            acc = Some(match acc {
                Some(prev) => prev & bm,
                None => bm,
            });
        }
        acc
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
        // Sort segment indices by source_path so that rows from different files
        // are emitted in a deterministic (alphabetical path, then line) order.
        // This matches the legacy backend's iteration order (parsed file-by-file
        // in path order), ensuring that ORDER BY tie-breaking on equal-name
        // symbols produces the same first-N result across both backends.
        let mut seg_order: Vec<u32> = by_segment.keys().copied().collect();
        seg_order.sort_by_key(|&idx| {
            self.overlay
                .segments()
                .get(idx as usize)
                .map(|m| m.source_path.clone())
        });

        let mut results = Vec::new();
        for seg_idx in seg_order {
            let Some(local_rows) = by_segment.get(&seg_idx) else {
                continue;
            };
            let Some(seg) = self.segments.get(seg_idx as usize) else {
                continue;
            };
            let Some(seg_meta) = self.overlay.segments().get(seg_idx as usize) else {
                continue;
            };
            // Pass the relative source path so that IN/EXCLUDE glob matching in
            // apply_clauses works against the same relative paths that the
            // legacy backend stores.  Do NOT join with worktree_root here.
            let mut seg_results = seg.materialize_rows(local_rows, Some(&seg_meta.source_path));
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
        // Deduplicate on (name, fql_kind, path, line) to match legacy backend
        // behaviour.  The legacy deduplicates on (name_id, path_id, node_kind_id,
        // line); including fql_kind here is the closest approximation available
        // in the columnar result, which does not store raw node_kind.
        {
            use std::collections::HashSet;
            type DedupeKey = (
                String,
                Option<String>,
                Option<std::path::PathBuf>,
                Option<usize>,
            );
            let mut seen: HashSet<DedupeKey> = HashSet::new();
            results.retain(|r| {
                seen.insert((r.name.clone(), r.fql_kind.clone(), r.path.clone(), r.line))
            });
        }
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
}

// ─────────────────────────────────────────────────────────────────────────────
// High-level overlay orchestration
// ─────────────────────────────────────────────────────────────────────────────

impl ColumnarStorage {
    /// Open the overlay for `commit_sha`, building it via shadow-write if absent.
    ///
    /// # Steps
    /// 1. Compute overlay path from `ctx`.
    /// 2. If the overlay opens cleanly → fast path: return immediately.
    /// 3. Otherwise acquire [`OverlayLock`], re-check inside the lock, and
    ///    build via [`ShadowWriter`] + [`OverlayBuilder`].
    /// 4. Construct and return a ready-to-query `ColumnarStorage`.
    ///
    /// `legacy` is read-only; only its [`SymbolTable`] is passed to
    /// `ShadowWriter`. Both this method and the caller accept `None` for
    /// `legacy` — if `None` the slow-path build is skipped (non-fatal).
    ///
    /// # Errors
    /// Returns `Err` only for hard failures (lock file I/O, final
    /// `Overlay::open` after a successful build). Shadow-write failures
    /// are treated as non-fatal and logged.
    pub fn warm_or_open(
        ctx: &crate::storage::ColumnarBuildContext,
        legacy: Option<&LegacyMemoryStorage>,
        worktree_path: PathBuf,
        commit_sha: &str,
    ) -> Result<Self> {
        let overlay_path = ctx.overlay_path_for(commit_sha);

        // Fast path: overlay already on disk and readable.
        if overlay_path.exists() {
            if let Ok(overlay) = Overlay::open(&overlay_path) {
                debug!(%commit_sha, "columnar warm_or_open: overlay found, fast-path load");
                let segments = Self::open_segments_from_overlay(ctx, &overlay);
                return Ok(Self::new(worktree_path, segments, overlay));
            }
            // Corrupt / schema mismatch — remove and rebuild below.
            debug!(%commit_sha, "columnar warm_or_open: overlay unreadable, will rebuild");
            let _ = std::fs::remove_file(&overlay_path);
        }

        // Slow path: build under lock.
        match OverlayLock::acquire(&overlay_path) {
            Err(e) => {
                return Err(anyhow!("overlay lock acquire failed for {commit_sha}: {e}"));
            }
            Ok(_lock) => {
                // Re-check: a peer may have built the overlay while we waited.
                if overlay_path.exists() {
                    if let Ok(overlay) = Overlay::open(&overlay_path) {
                        debug!(%commit_sha, "columnar warm_or_open: peer built overlay under lock");
                        let segments = Self::open_segments_from_overlay(ctx, &overlay);
                        return Ok(Self::new(worktree_path, segments, overlay));
                    }
                    let _ = std::fs::remove_file(&overlay_path);
                }

                // Build segments via shadow-write then persist the overlay.
                if let Some(legacy) = legacy
                    && let Some(table) = legacy.table()
                {
                    let writer = super::shadow_writer::ShadowWriter::new(
                        table,
                        &ctx.segments_dir,
                        &ctx.provider_id,
                        ctx.hash_fn.as_ref(),
                        HashMap::new(),
                    );
                    match writer.run() {
                        Ok(result) => {
                            debug!(
                                %commit_sha,
                                segments = result.count,
                                "columnar warm_or_open: shadow-write complete"
                            );
                            let builder = super::overlay_builder::OverlayBuilder::new(
                                &ctx.provider_id,
                                ctx.segments_dir.clone(),
                                worktree_path.clone(),
                                result.segment_map,
                            );
                            if let Err(e) = builder.build_and_persist(&overlay_path) {
                                tracing::warn!(
                                    %commit_sha,
                                    "columnar warm_or_open: overlay build failed: {e}"
                                );
                            } else {
                                debug!(%commit_sha, "columnar warm_or_open: overlay built");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                %commit_sha,
                                "columnar warm_or_open: shadow-write failed: {e}"
                            );
                        }
                    }
                }
                // _lock dropped here — releases OS lock.
            }
        }

        // Open whatever we built (or what was there before — best-effort).
        let overlay = Overlay::open(&overlay_path)
            .map_err(|e| anyhow!("overlay open failed for {commit_sha}: {e}"))?;
        let segments = Self::open_segments_from_overlay(ctx, &overlay);
        Ok(Self::new(worktree_path, segments, overlay))
    }

    /// Build segments + overlay for `commit_sha` without returning a
    /// `ColumnarStorage`.
    ///
    /// Convenience wrapper around [`warm_or_open`] used by background
    /// warming where the result is discarded immediately.
    ///
    /// [`warm_or_open`]: Self::warm_or_open
    ///
    /// # Errors
    /// Propagates errors from `warm_or_open`.
    pub fn warm(
        ctx: &crate::storage::ColumnarBuildContext,
        legacy: Option<&LegacyMemoryStorage>,
        worktree_path: PathBuf,
        commit_sha: &str,
    ) -> Result<()> {
        let _ = Self::warm_or_open(ctx, legacy, worktree_path, commit_sha)?;
        Ok(())
    }

    /// Open all segment readers referenced by `overlay`.
    ///
    /// Segments that cannot be opened are silently skipped — the overlay
    /// is still usable for queries that target other segments.
    fn open_segments_from_overlay(
        ctx: &crate::storage::ColumnarBuildContext,
        overlay: &Arc<Overlay>,
    ) -> Vec<Arc<SegmentReader>> {
        overlay
            .segments()
            .iter()
            .filter_map(|meta| {
                let dir = ctx.segment_dir_for(&meta.hex_content_id);
                SegmentReader::open(&dir).ok().map(Arc::new)
            })
            .collect()
    }
}
