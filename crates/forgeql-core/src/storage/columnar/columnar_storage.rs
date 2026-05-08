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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use roaring::RoaringBitmap;
use tracing::debug;

use crate::ast::index::IndexStats;
use crate::ast::query::glob_matches;
use crate::filter::{apply_clauses, eval_predicate};
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
                    // When the kind is absent in every segment, return an empty
                    // bitmap immediately rather than None.  None would fall
                    // through to the full-table scan, causing ~8 s regressions
                    // for unknown-kind queries.  See Phase 06d, Root cause 1.
                    Some(
                        self.overlay
                            .prefilter_kind(val)
                            .cloned()
                            .unwrap_or_default(),
                    )
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

    /// Return the set of segment indices whose `source_path` passes
    /// `clauses.in_glob` AND `clauses.exclude_glob`.
    ///
    /// Returns `None` when neither filter is set (caller should treat as
    /// "all segments allowed").  Used to prune non-matching segments
    /// *before* `group_by_segment` so they are never opened or materialised.
    fn segments_passing_path_filter(&self, clauses: &Clauses) -> Option<HashSet<u32>> {
        if clauses.in_glob.is_none() && clauses.exclude_glob.is_none() {
            return None;
        }
        let mut allowed = HashSet::new();
        for (idx, meta) in self.overlay.segments().iter().enumerate() {
            if passes_resolve_glob(&meta.source_path, clauses)
                && let Ok(seg_idx) = u32::try_from(idx)
            {
                let _ = allowed.insert(seg_idx);
            }
        }
        Some(allowed)
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
    fn materialize_all(
        &self,
        by_segment: &HashMap<u32, RoaringBitmap>,
        clauses: &Clauses,
    ) -> Vec<SymbolMatch> {
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

            // Stage 3a — narrow local row set using per-segment enrichment
            // posting bitmaps before materialisation.  Falls back to the
            // full local set when no posting file exists for a given predicate.
            let narrowed = seg.prefilter_enrichment_postings(local_rows.clone(), clauses);
            if narrowed.is_empty() {
                continue;
            }

            // Pass the relative source path so that IN/EXCLUDE glob matching in
            // apply_clauses works against the same relative paths that the
            // legacy backend stores.  Do NOT join with worktree_root here.
            let mut seg_results = seg.materialize_rows(&narrowed, Some(&seg_meta.source_path));
            results.append(&mut seg_results);
        }
        results
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Resolve helpers — shared by the three StorageEngine::resolve_* methods
// ─────────────────────────────────────────────────────────────────────────────

/// Split a qualified name (`Owner::member` or `Owner.member`) into
/// `(lookup_name, Some(owner))`.  Returns `(name, None)` for bare names.
///
/// Tries `::` first (Rust / C++) then `.` (Python / JS), mirroring the
/// legacy resolver's `split_qualified_name`.
fn split_qualified_name(name: &str) -> (&str, Option<&str>) {
    if let Some(pos) = name.rfind("::") {
        let (owner, member) = (&name[..pos], &name[pos + 2..]);
        if !owner.is_empty() && !member.is_empty() {
            return (member, Some(owner));
        }
    }
    if let Some(pos) = name.rfind('.') {
        let (owner, member) = (&name[..pos], &name[pos + 1..]);
        if !owner.is_empty() && !member.is_empty() {
            return (member, Some(owner));
        }
    }
    (name, None)
}

/// Check whether a workspace-relative `path` passes the `IN` / `EXCLUDE`
/// glob filters stored in `clauses`.
///
/// Operates on relative paths (as stored in [`SegmentMeta::source_path`]) —
/// no worktree-root stripping is needed.
fn passes_resolve_glob(relative_path: &Path, clauses: &Clauses) -> bool {
    let in_ok = clauses
        .in_glob
        .as_deref()
        .is_none_or(|glob| glob_matches(relative_path, glob));
    let excl_ok = clauses
        .exclude_glob
        .as_deref()
        .is_none_or(|glob| !glob_matches(relative_path, glob));
    in_ok && excl_ok
}

impl ColumnarStorage {
    /// Build a [`SymbolLocation`] from a single segment row.
    ///
    /// `seg_idx` indexes into both `self.segments` and
    /// `self.overlay.segments()` (they are kept in the same order by
    /// [`ColumnarStorage::new`]).
    fn location_for_row(&self, seg_idx: u32, local_row: u32, root: &Path) -> SymbolLocation {
        let seg = &self.segments[seg_idx as usize];
        let seg_meta = &self.overlay.segments()[seg_idx as usize];
        // Absolute path: join worktree root with the segment's workspace-relative path.
        let path = root.join(&seg_meta.source_path);
        let byte_start = seg.byte_start_of(local_row) as usize;
        let byte_end = seg.byte_end_of(local_row) as usize;
        let line = seg.line_of(local_row) as usize;
        // Columnar segments do not store the raw tree-sitter `node_kind`; use
        // `fql_kind` as a proxy.  `show_signature` accepts the universal fql_kind
        // names ("function", "method") and applies the body-stripping path, so
        // signature output is identical to the legacy backend.
        let node_kind = seg.fql_kind_of(local_row).to_owned();
        let enrichment = seg.enrichment_for_row(local_row);
        // Derive a blob SHA-1 hint from the segment's content_id.  Segments
        // written by ShadowWriter use a 20-byte SHA-1 as their content ID;
        // test helpers may use a shorter hash.  Cast only when the length is
        // exactly 20 so callers receive `None` for non-SHA1 providers.
        let blob_sha: Option<[u8; 20]> = seg.content_id[..].try_into().ok();
        SymbolLocation {
            path,
            byte_range: byte_start..byte_end,
            line,
            // Columnar segments store language as a string, not an interned u32 ID.
            // `language_id` is not used by any SHOW path, so 0 is safe here.
            language_id: 0,
            node_kind,
            enrichment,
            blob_sha,
        }
    }

    /// Core columnar resolve used by all three `StorageEngine::resolve_*` methods.
    ///
    /// Algorithm:
    /// 1. Split qualified name (`Owner::member` / `Owner.member`).
    /// 2. FST name lookup via the overlay bitmap.
    /// 3. Filter candidates by enclosing-type, IN/EXCLUDE glob, and WHERE predicates.
    /// 4. Collect two lists — `all` (every passing candidate) and `preferred`
    ///    (candidates whose `fql_kind` is in `prefer_kinds`, if given).
    /// 5. Pick: last preferred candidate → last definition candidate → last overall.
    /// 6. Convert the chosen row to a [`SymbolLocation`].
    fn resolve_impl(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
        prefer_kinds: Option<&[&str]>,
    ) -> Option<SymbolLocation> {
        let (lookup_name, enclosing_owner) = split_qualified_name(name);

        let global_bm = self.overlay.lookup_name_bitmap(lookup_name);
        if global_bm.is_empty() {
            return None;
        }
        let by_segment = self.group_by_segment(&global_bm);

        // Iterate segments in alphabetical source-path order for deterministic output.
        let mut seg_order: Vec<u32> = by_segment.keys().copied().collect();
        seg_order.sort_by_key(|&idx| {
            self.overlay
                .segments()
                .get(idx as usize)
                .map(|m| m.source_path.clone())
        });

        // Segment-level path prefilter — skip entire segments whose source_path
        // does not match IN/EXCLUDE globs.  The per-row passes_resolve_glob call
        // below is removed because every row inside a retained segment shares the
        // same source_path and therefore passes trivially.
        if let Some(allowed) = self.segments_passing_path_filter(clauses) {
            seg_order.retain(|seg_idx| allowed.contains(seg_idx));
        }

        // `all` — every candidate that passes all filters.
        // `preferred` — subset that also matches `prefer_kinds` (if given).
        let mut all: Vec<(u32, u32)> = Vec::new();
        let mut preferred: Vec<(u32, u32)> = Vec::new();

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
            let relative_path = &seg_meta.source_path;

            // 2. Enrichment-postings prefilter — bitmap intersection per allowlisted
            //    field before any per-row work.  Mirrors the same step in materialize_all.
            let local_rows = seg.prefilter_enrichment_postings(local_rows.clone(), clauses);
            if local_rows.is_empty() {
                continue;
            }

            for local_row in local_rows {
                // 1. Enclosing-type filter for qualified names.
                if let Some(owner) = enclosing_owner
                    && seg
                        .extra_field_str("enclosing_type", local_row)
                        .unwrap_or("")
                        != owner
                {
                    continue;
                }

                // 3. WHERE predicate filter — build a lightweight SymbolMatch for evaluation.
                let fql_kind_str = seg.fql_kind_of(local_row);
                let line_num = seg.line_of(local_row);
                let sm = SymbolMatch {
                    name: seg.name_of(local_row).to_owned(),
                    node_kind: None,
                    fql_kind: (!fql_kind_str.is_empty()).then(|| fql_kind_str.to_owned()),
                    language: {
                        let l = seg.language_of(local_row);
                        (!l.is_empty()).then(|| l.to_owned())
                    },
                    path: Some(relative_path.clone()),
                    line: (line_num != 0).then_some(line_num as usize),
                    usages_count: Some(seg.usages_count_of(local_row) as usize),
                    fields: seg.enrichment_for_row(local_row),
                    count: None,
                };
                if clauses
                    .where_predicates
                    .iter()
                    .any(|p| !eval_predicate(&sm, p))
                {
                    continue;
                }

                all.push((seg_idx, local_row));
                if let Some(kinds) = prefer_kinds
                    && kinds.contains(&fql_kind_str)
                {
                    preferred.push((seg_idx, local_row));
                }
            }
        }

        if all.is_empty() {
            return None;
        }

        // Pick best candidate — mirrors the legacy "last-write-wins" strategy.
        // Preference order: last preferred → last definition (non-empty fql_kind) → last overall.
        let chosen = if preferred.is_empty() {
            all.iter()
                .rposition(|&(si, lr)| {
                    self.segments
                        .get(si as usize)
                        .is_some_and(|s| !s.fql_kind_of(lr).is_empty())
                })
                .and_then(|i| all.get(i).copied())
                .or_else(|| all.last().copied())
        } else {
            preferred.last().copied()
        };

        chosen.map(|(seg_idx, local_row)| self.location_for_row(seg_idx, local_row, root))
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
        let mut by_segment = self.group_by_segment(&candidates);

        // Stage 2b — drop segments whose source_path does not match the
        // IN / EXCLUDE glob filters.  This avoids opening and materialising
        // thousands of segments for narrow-path queries (e.g. IN 'drivers/**').
        if let Some(allowed) = self.segments_passing_path_filter(clauses) {
            by_segment.retain(|seg_idx, _| allowed.contains(seg_idx));
        }

        let mut results = self.materialize_all(&by_segment, clauses);
        // Deduplicate on (name, fql_kind, path, line) to match legacy backend
        // behaviour.  The legacy deduplicates on (name_id, path_id, node_kind_id,
        // line); including fql_kind here is the closest approximation available
        // in the columnar result, which does not store raw node_kind.
        {
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
        let mut results = self.materialize_all(&by_segment, clauses);
        apply_clauses(&mut results, clauses);
        Ok(results)
    }

    fn resolve_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        Ok(self.resolve_impl(name, clauses, root, None))
    }

    fn resolve_type_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        // Prefer struct / class / enum / union / type_alias / trait / interface rows
        // that have members — mirrors the legacy resolver's type-preference scan.
        const TYPE_KINDS: &[&str] = &[
            "class",
            "struct",
            "enum",
            "union",
            "type_alias",
            "trait",
            "interface",
        ];
        Ok(self.resolve_impl(name, clauses, root, Some(TYPE_KINDS)))
    }

    fn resolve_body_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        let Some(loc) = self.resolve_impl(name, clauses, root, None) else {
            return Ok(None);
        };
        // Follow the `body_symbol` redirect for C++ out-of-line definitions.
        // The redirect is resolved without user clauses — matches legacy behaviour
        // (`index.find_def(target)` ignores clauses).
        if let Some(target) = loc.enrichment.get("body_symbol").cloned() {
            const BODY_KINDS: &[&str] =
                &["function", "method", "constructor", "destructor", "macro"];
            if let Some(redirected) =
                self.resolve_impl(&target, &Clauses::default(), root, Some(BODY_KINDS))
            {
                return Ok(Some(redirected));
            }
        }
        Ok(Some(loc))
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
        workspace: &Workspace,
        file: &str,
    ) -> Result<serde_json::Value> {
        let root = workspace.root();
        let mut entries: Vec<(usize, serde_json::Value)> = Vec::new();

        for (seg_idx, seg_meta) in self.overlay.segments().iter().enumerate() {
            // Filter: does this segment's source file match the `file` pattern?
            if !crate::ast::query::glob_matches(&seg_meta.source_path, file) {
                continue;
            }
            let seg = &self.segments[seg_idx];
            let abs_path = root.join(&seg_meta.source_path);
            let rel_path = workspace.relative(&abs_path).display().to_string();

            for row in 0..seg.row_count {
                let name = seg.name_of(row).to_owned();
                let fql_kind = seg.fql_kind_of(row).to_owned();
                let line = seg.line_of(row) as usize;
                let kind = if fql_kind.is_empty() {
                    "unknown"
                } else {
                    &fql_kind
                };
                entries.push((
                    line,
                    serde_json::json!({
                        "name": name,
                        "fql_kind": kind,
                        "path": rel_path,
                        "line": line,
                    }),
                ));
            }
        }

        entries.sort_by_key(|(line, _)| *line);
        let results: Vec<serde_json::Value> = entries.into_iter().map(|(_, v)| v).collect();

        Ok(serde_json::json!({
            "op":      "show_outline",
            "file":    file,
            "results": results,
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
