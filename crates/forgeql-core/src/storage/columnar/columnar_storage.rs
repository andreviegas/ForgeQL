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

use anyhow::{Context as _, Result, anyhow};
use roaring::RoaringBitmap;
use tracing::{debug, info};

use crate::ast::enrich::default_enrichers;
use crate::ast::index::IndexStats;
use crate::ast::index::{SymbolTable, index_file};
use crate::ast::lang::LanguageRegistry;
use crate::ast::query::glob_matches;
use crate::filter::{TOPK_THRESHOLD, apply_clauses, collect_top_k, eval_predicate, order_cmp};
use crate::ir::{Clauses, CompareOp, OrderBy, PredicateValue, SortDirection};
use crate::result::SymbolMatch;
use crate::workspace::Workspace;

use super::bytes_to_hex;
use super::delta_file::DeltaFile;
use super::dirty_overlay::DirtyOverlay;
use super::overlay::{Overlay, RowPtr};
use super::overlay_lock::OverlayLock;
use super::segment_builder::{SegmentBuilder, ZONEMAP_NUMERIC_FIELDS, is_valid_segment};
use super::segment_reader::SegmentReader;
use crate::storage::git_sha1_provider::git_blob_sha1;
use crate::storage::{LegacyMemoryStorage, StorageEngine, SymbolLocation};

// ─────────────────────────────────────────────────────────────────────────────
// ColumnarStorage
// ─────────────────────────────────────────────────────────────────────────────

/// Over-fetch factor for the running top-K trim in [`materialize_all`].
///
/// We keep `K * TOPK_OVER_FETCH` rows in the working set before each trim so
/// that the subsequent deduplication and `apply_clauses` passes never lose a
/// row that would otherwise appear in the final top-K result.
const TOPK_OVER_FETCH: usize = 4;

/// Disk-backed columnar [`StorageEngine`] backed by per-file segment readers
/// and a workspace-level overlay index.
///
pub struct ColumnarStorage {
    /// Worktree root; used to resolve absolute source file paths and strip
    /// prefixes when computing relative paths for `DirtyOverlay`.
    worktree_root: PathBuf,
    /// Per-segment readers in the same order as `overlay.segments()`.
    segments: Vec<Arc<SegmentReader>>,
    /// Workspace overlay shared across sessions on the same commit SHA.
    overlay: Arc<Overlay>,
    /// Per-session in-RAM mutations on top of the persistent overlay.
    ///
    /// Always empty at session start. Populated by PhaseFT2 `reindex_files`.
    /// Queried by `find_symbols` / `find_usages` to union persistent + dirty rows.
    pub(crate) dirty: DirtyOverlay,
    /// Staging directory for per-session reindexed segments (`.forgeql-staging/`).
    staging_dir: PathBuf,
    /// Language registry used by `reindex_files` to parse modified files.
    lang_registry: Arc<LanguageRegistry>,
    /// Path to the delta file that persists the dirty overlay across restarts.
    ///
    /// Written after every `reindex_files` / `purge_file` call.
    /// Included in `BEGIN TRANSACTION` checkpoint commits (so `git reset --hard`
    /// restores it automatically on `ROLLBACK`) but excluded from user-facing
    /// `COMMIT MESSAGE` commits via `git::CLEAN_COMMIT_EXCLUDED`.
    delta_path: PathBuf,
    /// Pre-computed index stats for `index_stats()`.
    ///
    /// Populated at construction from `overlay.row_count()` so that
    /// columnar sessions appear in `SHOW SOURCES` without a full scan.
    stats: IndexStats,
}

impl ColumnarStorage {
    /// Create a new `ColumnarStorage` from an open overlay and its segments.
    ///
    /// `segments` **must** be in the same order as `overlay.segments()`.
    #[must_use]
    pub fn new(
        worktree_root: PathBuf,
        segments: Vec<Arc<SegmentReader>>,
        overlay: Arc<Overlay>,
        lang_registry: Arc<LanguageRegistry>,
    ) -> Self {
        let staging_dir = worktree_root.join(".forgeql-staging");
        let delta_path = worktree_root.join(".forgeql-columnar-delta");
        let stats = IndexStats {
            rows: overlay.row_count() as usize,
            ..IndexStats::default()
        };
        Self {
            worktree_root,
            segments,
            overlay,
            dirty: DirtyOverlay::new(),
            staging_dir,
            lang_registry,
            delta_path,
            stats,
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
    /// `path_floor` — when the caller knows a contiguous path row-range,
    /// it passes it here so that (a) the fallback universe is bounded to
    /// that range instead of the full table, and (b) every per-predicate
    /// bitmap is intersected with the path range immediately, keeping
    /// intermediate results small.
    fn prefilter_global(
        &self,
        clauses: &Clauses,
        path_floor: Option<RoaringBitmap>,
    ) -> RoaringBitmap {
        let mut result: Option<RoaringBitmap> = path_floor;

        for pred in &clauses.where_predicates {
            let Some(kind_bm) = (match (pred.field.as_str(), &pred.op, &pred.value) {
                ("fql_kind", CompareOp::Eq, PredicateValue::String(val)) => {
                    // When the kind is absent in every segment, return an empty
                    // bitmap immediately rather than None.  None would fall
                    // through to the full-table scan, causing ~8 s regressions
                    // for unknown-kind queries.  See Phase 06d, Root cause 1.
                    Some(self.overlay.prefilter_kind(val).unwrap_or_default())
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
                    // For 1-2 char leading literals, use the per-segment name
                    // prefix index (faster than trigrams for very short keys).
                    // For 3+ char literals, fall through to the trigram index.
                    pattern_as_prefix(val).map_or_else(
                        || self.trigram_prefilter_for_pattern(val),
                        |prefix| self.short_prefix_global_bitmap(&prefix),
                    )
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

    /// Build a global candidate bitmap using per-segment name prefix indexes.
    ///
    /// For each segment:
    /// - If the segment has a `name_prefix` index, look up `prefix` in it
    ///   and map the resulting local row IDs to global row IDs.
    /// - If the segment has no prefix index (old format), include ALL its
    ///   rows as candidates (cannot prune).
    ///
    /// Returns `None` when NO segment has a prefix index (caller should
    /// fall through to a different prefilter or full scan).
    fn short_prefix_global_bitmap(&self, prefix: &[u8]) -> Option<RoaringBitmap> {
        let mut result = RoaringBitmap::new();
        let mut any_had_index = false;
        let mut seg_base: u32 = 0;
        for (seg_idx, seg) in self.segments.iter().enumerate() {
            let row_count = self
                .overlay
                .segments()
                .get(seg_idx)
                .map_or(seg.row_count, |m| m.row_count);
            if seg.name_prefix.is_empty() {
                // No prefix index — include all rows from this segment.
                for local_row in 0..row_count {
                    let _ = result.insert(seg_base + local_row);
                }
            } else {
                any_had_index = true;
                if let Some(local_bm) = seg.name_prefix.get(prefix) {
                    for local_row in local_bm {
                        let _ = result.insert(seg_base + local_row);
                    }
                }
            }
            seg_base = seg_base.saturating_add(row_count);
        }
        if any_had_index { Some(result) } else { None }
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

    /// Return the set of segment indices that *could* satisfy a numeric
    /// range predicate (`WHERE col OP val`) based on their zone maps.
    ///
    /// A segment is pruned (excluded from the returned set) when its
    /// `(min, max)` range provably cannot satisfy the predicate:
    /// - `col > val`  → prune when `seg.max ≤ val`
    /// - `col >= val` → prune when `seg.max < val`
    /// - `col < val`  → prune when `seg.min ≥ val`
    /// - `col <= val` → prune when `seg.min > val`
    /// - `col = val`  → prune when `val < seg.min || val > seg.max`
    ///
    /// Returns `None` when no segment has a zone map for the column
    /// (nothing can be pruned; caller should skip this optimisation).
    fn segments_passing_zone_map(
        &self,
        col: &str,
        op: CompareOp,
        val: u32,
    ) -> Option<HashSet<u32>> {
        let mut any_zone_map = false;
        let mut allowed: HashSet<u32> = HashSet::new();
        for (idx, seg) in self.segments.iter().enumerate() {
            let Some(&(min, max)) = seg.zone_maps.get(col) else {
                // No zone map for this segment — cannot prune, include it.
                if let Ok(seg_idx) = u32::try_from(idx) {
                    let _ = allowed.insert(seg_idx);
                }
                continue;
            };
            any_zone_map = true;
            let passes = match op {
                CompareOp::Gt => max > val,
                CompareOp::Gte => max >= val,
                CompareOp::Lt => min < val,
                CompareOp::Lte => min <= val,
                CompareOp::Eq => val >= min && val <= max,
                // Non-range operators — cannot prune.
                _ => true,
            };
            if passes && let Ok(seg_idx) = u32::try_from(idx) {
                let _ = allowed.insert(seg_idx);
            }
        }
        if any_zone_map { Some(allowed) } else { None }
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
        //
        // Phase 2 note: after the FQOV v4 format change, segments are stored in
        // path order in the overlay.  For queries over the full index this sort
        // is therefore a no-op (segment indices are already path-sorted).  For
        // queries filtered to a subset (IN / EXCLUDE / kind bitmap), the subset
        // keys are still path-ordered so this sort remains O(k log k) but fast.
        let mut seg_order: Vec<u32> = by_segment.keys().copied().collect();
        seg_order.sort_by_key(|&idx| {
            self.overlay
                .segments()
                .get(idx as usize)
                .map(|m| m.source_path.clone())
        });

        // Early-exit cap: when no ORDER BY and no GROUP BY and an explicit LIMIT
        // was set, stop opening segment files once the fetch budget is exhausted.
        // We fetch cap+1 so that `total > results.len()` stays reliable — the
        // one extra row signals "more results exist" to the caller.
        //
        // NOTE: when `clauses.limit` is `None` we do NOT fall back to the
        // engine's DEFAULT_QUERY_LIMIT here.  exec_find injects an explicit
        // limit before calling find_symbols so that path is already covered;
        // callers that invoke find_symbols directly (tests, etc.) still get all
        // matching rows as expected.
        let fetch_cap: Option<usize> = if clauses.order_by.is_none() && clauses.group_by.is_none() {
            clauses.limit.map(|c| c.saturating_add(1))
        } else {
            None
        };

        // Top-K running trim (Phase 8): when ORDER BY is set, LIMIT is small,
        // and GROUP BY is absent, periodically discard accumulated rows that
        // cannot possibly make the final top-K.  This bounds peak result memory
        // to O(K * TOPK_OVER_FETCH) instead of O(total_matching_rows).
        //
        // We trim whenever the working set exceeds K * TOPK_OVER_FETCH rows,
        // keeping K * TOPK_OVER_FETCH / 2 survivors.  The over-fetch factor of
        // 4 means we retain 4× the requested LIMIT throughout the scan so that
        // later deduplication + apply_clauses passes never drop a row that would
        // otherwise belong in the final top-K.
        let topk_trim: Option<usize> = if clauses.order_by.is_some()
            && clauses.group_by.is_none()
            && clauses.offset.unwrap_or(0) == 0
            && clauses.limit.is_some_and(|k| k <= TOPK_THRESHOLD)
        {
            clauses.limit
        } else {
            None
        };

        let mut results = Vec::new();
        for seg_idx in seg_order {
            // Early-exit: checked before opening the segment file so that we
            // don't pay I/O cost once the fetch budget is exhausted.
            if fetch_cap.is_some_and(|cap| results.len() >= cap) {
                break;
            }

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
            // Apply WHERE predicates per-segment before counting toward the
            // fetch cap.  This filters both enrichment-posting false positives
            // (segments without a posting blob pass ALL rows through
            // `prefilter_enrichment_postings`) and trigram false positives
            // (e.g. `name LIKE 'alloc%'` matches names containing "alloc" but
            // not necessarily starting with it).  Without this pre-filter,
            // the cap is exhausted by non-matching rows and `apply_clauses`
            // then returns fewer results than LIMIT requested.
            if fetch_cap.is_some() || topk_trim.is_some() {
                for predicate in &clauses.where_predicates {
                    let pred = predicate.clone();
                    seg_results.retain(|item| eval_predicate(item, &pred));
                }
            }
            // Trim within this segment to avoid overshooting the fetch budget.
            if let Some(cap) = fetch_cap {
                let remaining = cap.saturating_sub(results.len());
                seg_results.truncate(remaining);
            }
            results.append(&mut seg_results);

            // Running top-K trim: shed rows that cannot make the final top-K.
            // Fires whenever the working set exceeds K * TOPK_OVER_FETCH.
            if let Some(k) = topk_trim {
                let trim_at = k.saturating_mul(TOPK_OVER_FETCH);
                if results.len() > trim_at {
                    let keep = k.saturating_mul(TOPK_OVER_FETCH / 2).max(k);
                    results = collect_top_k(std::mem::take(&mut results), keep, |a, b| {
                        order_cmp(a, b, clauses)
                    });
                }
            }
        }
        results
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Module-level helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Extract a leading-literal prefix from a SQL `LIKE` pattern.
///
/// Returns `Some(prefix_bytes)` only when the pattern starts with exactly
/// 1 or 2 literal UTF-8 characters before the first `%` or `_` wildcard.
/// Longer literals are handled by the trigram index.  Zero-length prefixes
/// (pattern starts with `%`) return `None` — nothing to prune.
///
/// The returned bytes are the lowercase UTF-8 encoding of the prefix
/// characters, matching the encoding used by the builder.
fn pattern_as_prefix(pattern: &str) -> Option<Vec<u8>> {
    let mut prefix_bytes: Vec<u8> = Vec::new();
    let mut char_count = 0usize;
    let mut chars = pattern.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch == '%' || ch == '_' {
            break;
        }
        let lower_ch = ch.to_lowercase();
        for lc in lower_ch {
            let mut buf = [0u8; 4];
            let s = lc.encode_utf8(&mut buf);
            prefix_bytes.extend_from_slice(s.as_bytes());
        }
        char_count += 1;
        if char_count == 2 {
            // Stop accumulating at 2 chars.  But if the literal continues
            // (3rd character is not a wildcard and not end-of-string), the
            // trigram index is a stronger prefilter — return None so the
            // caller falls through to trigram_prefilter_for_pattern.
            if let Some(&(_, next)) = chars.peek()
                && next != '%'
                && next != '_'
            {
                return None; // 3+ char literal — use trigrams
            }
            break;
        }
    }
    if char_count == 1 || char_count == 2 {
        Some(prefix_bytes)
    } else {
        None
    }
}

/// Extract a literal directory prefix from a glob pattern for `path_row_range` clamping.
///
/// Returns the longest literal path prefix (including the trailing `/`) that
/// appears before the first wildcard character (`*`, `?`, or `[`).
/// Returns `None` when the glob has no such prefix (e.g. `*.c`, `**/*.c`).
///
/// Examples:
/// - `"include/**"`    → `Some("include/")`
/// - `"drivers/net/**"` → `Some("drivers/net/")`
/// - `"*.c"`           → `None`
/// - `"**/*.c"`        → `None`
fn glob_to_path_prefix(glob: &str) -> Option<&str> {
    let wild_pos = glob.find(['*', '?', '['])?;
    let up_to = &glob[..wild_pos];
    let slash_pos = up_to.rfind('/')?;
    Some(&glob[..=slash_pos])
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

/// Returns `true` when `clauses` contains at least one predicate that
/// [`ColumnarStorage::prefilter_global`] can answer with a bitmap narrower
/// than the full row-ID universe.  Used to decide whether the "fast path"
/// in `find_symbols` can skip the global bitmap entirely.
/// Return `true` when `ORDER BY name ASC LIMIT N` fast-path is eligible.
///
/// Conditions: ORDER BY name ASC, explicit LIMIT, no GROUP BY, no WHERE
/// predicates, no path filter.  The caller also gates on the dirty overlay
/// being empty so dirty rows cannot shadow committed rows with earlier names.
fn order_by_name_fast_path(clauses: &Clauses) -> bool {
    matches!(
        &clauses.order_by,
        Some(OrderBy { field, direction: SortDirection::Asc }) if field == "name"
    ) && clauses.limit.is_some()
        && clauses.group_by.is_none()
        && clauses.where_predicates.is_empty()
        && clauses.in_glob.is_none()
        && clauses.exclude_glob.is_none()
}

fn has_any_indexed_predicate(clauses: &Clauses) -> bool {
    clauses.where_predicates.iter().any(|pred| {
        matches!(
            (pred.field.as_str(), &pred.op),
            ("fql_kind", CompareOp::Eq)
                | ("name", CompareOp::Eq | CompareOp::Like | CompareOp::Matches)
        )
    })
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
    #[allow(clippy::too_many_lines)]
    fn resolve_impl(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
        prefer_kinds: Option<&[&str]>,
    ) -> Option<SymbolLocation> {
        let (lookup_name, enclosing_owner) = split_qualified_name(name);

        // Stage 1 — Dirty overlay: scan added segments before touching the persistent
        // index.  This ensures names that are *new* in the dirty overlay (not yet in
        // the persistent FST) are resolved, and dirty always wins over persistent.
        if !self.dirty.is_empty() {
            let mut dirty_all: Vec<SymbolLocation> = Vec::new();
            let mut dirty_preferred: Vec<SymbolLocation> = Vec::new();
            for ds in &self.dirty.added {
                let row_ids = ds.reader.lookup_name(lookup_name);
                if row_ids.is_empty() {
                    continue;
                }
                let bm: RoaringBitmap = row_ids.into_iter().collect();
                let bm = ds.reader.prefilter_enrichment_postings(bm, clauses);
                for local_row in bm {
                    if let Some(owner) = enclosing_owner
                        && ds
                            .reader
                            .extra_field_str("enclosing_type", local_row)
                            .unwrap_or("")
                            != owner
                    {
                        continue;
                    }
                    let fql_kind_str = ds.reader.fql_kind_of(local_row);
                    let line_num = ds.reader.line_of(local_row);
                    let sm = SymbolMatch {
                        name: ds.reader.name_of(local_row).to_owned(),
                        node_kind: None,
                        fql_kind: (!fql_kind_str.is_empty()).then(|| fql_kind_str.to_owned()),
                        language: {
                            let l = ds.reader.language_of(local_row);
                            (!l.is_empty()).then(|| l.to_owned())
                        },
                        path: Some(ds.source_path.clone()),
                        line: (line_num != 0).then_some(line_num as usize),
                        usages_count: Some(ds.reader.usages_count_of(local_row) as usize),
                        fields: ds.reader.enrichment_for_row(local_row),
                        count: None,
                    };
                    if clauses
                        .where_predicates
                        .iter()
                        .any(|p| !eval_predicate(&sm, p))
                    {
                        continue;
                    }
                    let blob_sha: Option<[u8; 20]> = ds.reader.content_id[..].try_into().ok();
                    let enrichment = ds.reader.enrichment_for_row(local_row);
                    let loc = SymbolLocation {
                        path: root.join(&ds.source_path),
                        byte_range: ds.reader.byte_start_of(local_row) as usize
                            ..ds.reader.byte_end_of(local_row) as usize,
                        line: line_num as usize,
                        language_id: 0,
                        node_kind: fql_kind_str.to_owned(),
                        enrichment,
                        blob_sha,
                    };
                    if prefer_kinds.is_some_and(|kinds| kinds.contains(&fql_kind_str)) {
                        dirty_preferred.push(loc.clone());
                    }
                    dirty_all.push(loc);
                }
            }
            if let Some(last) = dirty_preferred.pop() {
                return Some(last);
            }
            if let Some(last) = dirty_all.pop() {
                return Some(last);
            }
        }

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

        // Stage 2d — drop persistent segments shadowed by the dirty overlay.
        if !self.dirty.is_empty() {
            seg_order.retain(|&seg_idx| {
                self.overlay
                    .segments()
                    .get(seg_idx as usize)
                    .is_none_or(|meta| !self.dirty.shadows(&meta.hex_content_id))
            });
        }

        // Segment-level path prefilter — skip entire segments whose source_path
        // does not match IN/EXCLUDE globs.  The per-row passes_resolve_glob call
        // below is removed because every row inside a retained segment shares the
        // same source_path and therefore passes trivially.
        if let Some(allowed) = self.segments_passing_path_filter(clauses) {
            seg_order.retain(|seg_idx| allowed.contains(seg_idx));
        }

        // Zone-map prune for numeric range predicates.
        // Same field-alias and negative-value rules as in find_symbols.
        'zone: for pred in &clauses.where_predicates {
            if let PredicateValue::Number(val_i64) = &pred.value {
                let col = match pred.field.as_str() {
                    "usages" => "usages_count",
                    other => other,
                };
                // Impossible-predicate short-circuit for u32 columns: no stored
                // value can satisfy col < 0 (val <= 0 for Lt), col <= negative,
                // or col = negative.  Clear all candidates without needing zone maps.
                let impossible = ZONEMAP_NUMERIC_FIELDS.iter().any(|(f, _)| *f == col)
                    && match pred.op {
                        CompareOp::Lt => *val_i64 <= 0,
                        CompareOp::Lte | CompareOp::Eq => *val_i64 < 0,
                        _ => false,
                    };
                if impossible {
                    seg_order.clear();
                    break 'zone;
                }
                if let Ok(val_u32) = u32::try_from(*val_i64)
                    && let Some(allowed) = self.segments_passing_zone_map(col, pred.op, val_u32)
                {
                    seg_order.retain(|seg_idx| allowed.contains(seg_idx));
                }
            }
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

    #[allow(clippy::too_many_lines)]
    fn find_symbols(&self, clauses: &Clauses, _root: &Path) -> Result<Vec<SymbolMatch>> {
        // ── Query plan (Phase 06d + fast-path) ──────────────────────────────
        // Stage 1  — prefilter_global: intersect indexed predicates
        //             (fql_kind bitmap, exact name FST, trigram index for 3+
        //             char LIKE patterns, short-prefix index for 1-2 char LIKE)
        //             → candidate global row-ID bitmap.
        // Stage 2a — group_by_segment: partition global IDs by segment index.
        // Stage 2b — path prefilter: drop segments whose source_path doesn't
        //             match IN / EXCLUDE globs.
        //
        // FAST PATH (2a+2b combined): when a path filter is present but no
        // indexed predicate is available (fql_kind=, name=/LIKE/MATCHES),
        // prefilter_global would return the path-floor bitmap (Phase 6) but
        // group_by_segment would still split it across every matching segment.
        // Instead, iterate only
        // the path-filtered segments and seed their local bitmaps directly.
        // This is the common case for enrichment-only queries such as
        // `WHERE is_recursive = 'true' IN 'drivers/**'`.
        //
        // Stage 2c — zone-map prune: drop segments whose numeric column range
        //             cannot satisfy a WHERE col OP val predicate.
        // Stage 3  — materialize_all: for each surviving segment, narrow local
        //             rows via enrichment-posting prefilter, then materialise.
        // Stage 4  — deduplicate on (name, fql_kind, path, line).
        // Stage 5  — apply_clauses: residual WHERE, ORDER BY, LIMIT, OFFSET.
        // ─────────────────────────────────────────────────────────────────────
        // ── ORDER BY name ASC LIMIT N fast-path ──────────────────────────────
        // Stream the first (limit + offset) rows directly from the name FST in
        // lexicographic order, materialising only those rows.  The dirty overlay
        // is skipped (gated on is_empty) because dirty rows are not path-sorted
        // and could have names that precede committed rows already streamed.
        if order_by_name_fast_path(clauses) && self.dirty.is_empty() {
            let need = clauses
                .limit
                .unwrap_or(0)
                .saturating_add(clauses.offset.unwrap_or(0))
                .max(1);
            let mut results = self.overlay.stream_names_asc(need, &self.segments);
            // Deduplicate on (name, fql_kind, path, line) — same as Stage 4.
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
            return Ok(results);
        }
        // ─────────────────────────────────────────────────────────────────────
        let has_path_filter = clauses.in_glob.is_some() || clauses.exclude_glob.is_some();

        let mut by_segment: HashMap<u32, RoaringBitmap> = if has_path_filter
            && !has_any_indexed_predicate(clauses)
        {
            // Fast path: skip Stage 1 + group_by_segment entirely.
            // Directly seed by_segment with all local rows for every segment
            // whose source_path passes the IN / EXCLUDE glob.
            let mut map: HashMap<u32, RoaringBitmap> = HashMap::new();
            for (idx, meta) in self.overlay.segments().iter().enumerate() {
                if passes_resolve_glob(&meta.source_path, clauses)
                    && let (Some(seg), Ok(seg_idx)) = (self.segments.get(idx), u32::try_from(idx))
                {
                    let _ = map.insert(seg_idx, (0..seg.row_count).collect());
                }
            }
            map
        } else {
            // Normal path: global prefilter → group by segment → path prune.
            // Phase 6 — build path_floor bitmap before calling prefilter_global
            // so the prefilter can use it as the baseline universe.  This
            // means (a) when no indexed predicate matches, path_floor is
            // returned directly instead of building a full-universe bitmap, and
            // (b) when a kind / name predicate matches, the resulting bitmap is
            // already intersected with the path range.
            let path_floor = clauses
                .in_glob
                .as_deref()
                .and_then(glob_to_path_prefix)
                .map(|prefix| {
                    let row_range = self.overlay.path_row_range(prefix);
                    row_range.collect::<RoaringBitmap>()
                });
            let candidates = self.prefilter_global(clauses, path_floor);
            let mut map = self.group_by_segment(&candidates);
            // Stage 2b — drop segments whose source_path does not match the
            // IN / EXCLUDE glob filters.
            if let Some(allowed) = self.segments_passing_path_filter(clauses) {
                map.retain(|seg_idx, _| allowed.contains(seg_idx));
            }
            map
        };

        // Stage 2d — drop persistent segments shadowed by the dirty overlay.
        // When a file has been changed or deleted in this session, its old
        // persistent segment is filtered here so only the dirty version appears.
        if !self.dirty.is_empty() {
            by_segment.retain(|&seg_idx, _| {
                self.overlay
                    .segments()
                    .get(seg_idx as usize)
                    .is_none_or(|meta| !self.dirty.shadows(&meta.hex_content_id))
            });
        }

        // Stage 2c — drop segments that cannot satisfy numeric range predicates
        // (WHERE line > N, WHERE usages >= N, etc.) using zone maps.
        // This prune step is purely additive — segments that lack a zone map
        // for the predicate column are always kept.
        //
        // Field aliases: the FQL parser emits "usages" but the zone-map file
        // is written as "usages_count" by the segment builder.  Map here so
        // zone-map pruning fires correctly for usages predicates.
        //
        // Negative-value short-circuit: u32 columns (line, usages_count, …)
        // cannot satisfy col < 0 or col <= (negative).  Detect this without
        // requiring zone-map files to exist and clear all candidates eagerly.
        'zone: for pred in &clauses.where_predicates {
            if let PredicateValue::Number(val_i64) = &pred.value {
                let col = match pred.field.as_str() {
                    "usages" => "usages_count",
                    other => other,
                };
                // Impossible-predicate short-circuit for u32 columns: no stored
                // value can satisfy col < 0 (val <= 0 for Lt), col <= negative,
                // or col = negative.  Clear all candidates without needing zone maps.
                let impossible = ZONEMAP_NUMERIC_FIELDS.iter().any(|(f, _)| *f == col)
                    && match pred.op {
                        CompareOp::Lt => *val_i64 <= 0,
                        CompareOp::Lte | CompareOp::Eq => *val_i64 < 0,
                        _ => false,
                    };
                if impossible {
                    by_segment.clear();
                    break 'zone;
                }
                if let Ok(val_u32) = u32::try_from(*val_i64)
                    && let Some(allowed) = self.segments_passing_zone_map(col, pred.op, val_u32)
                {
                    by_segment.retain(|seg_idx, _| allowed.contains(seg_idx));
                }
            }
        }
        let mut results = self.materialize_all(&by_segment, clauses);
        // Stage 3b — union dirty overlay rows (empty when dirty overlay is empty).
        if !self.dirty.is_empty() {
            let mut dirty_results = self.dirty.materialize_all(clauses);
            results.append(&mut dirty_results);
        }
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
        // Phase 05 scope: exact-name FST lookup on persistent overlay.
        let candidates = self.overlay.lookup_name_bitmap(name);
        // Drop persistent segments shadowed by the dirty overlay.
        let mut by_segment = self.group_by_segment(&candidates);
        if !self.dirty.is_empty() {
            by_segment.retain(|&seg_idx, _| {
                self.overlay
                    .segments()
                    .get(seg_idx as usize)
                    .is_none_or(|meta| !self.dirty.shadows(&meta.hex_content_id))
            });
        }
        let mut results = self.materialize_all(&by_segment, clauses);
        // Union dirty overlay rows for this name.
        if !self.dirty.is_empty() {
            let mut dirty_results = self.dirty.lookup_name_results(name, clauses);
            results.append(&mut dirty_results);
        }
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
        // Only consider function-like kinds for SHOW body.  Using the preference
        // list still allows the fallback to any row when the function-kind rows
        // are filtered out by clauses, but we check the returned kind below so
        // that non-function symbols produce an actionable error instead of
        // silently resolving to whatever enclosing function happens to contain
        // the last type reference.
        const BODY_KINDS: &[&str] = &["function", "method", "constructor", "destructor"]; // macros excluded: C preprocessor macros have no function_definition in the AST

        let Some(loc) = self.resolve_impl(name, clauses, root, Some(BODY_KINDS)) else {
            return Ok(None);
        };

        // `resolve_impl` uses BODY_KINDS as a *preference*, not a hard filter:
        // if no function-kind rows exist it falls back to the last row with any
        // non-empty fql_kind.  Guard against that by checking the resolved kind.
        // Exception: allow member declarations (fql_kind outside BODY_KINDS)
        // that carry a `body_symbol` redirect — they are C++ method stubs
        // created by MemberEnricher and should follow the redirect below.
        if !BODY_KINDS.contains(&loc.node_kind.as_str())
            && !loc.enrichment.contains_key("body_symbol")
        {
            anyhow::bail!(
                "'{name}' is not a function (found fql_kind: [{}]). \
                 Use FIND symbols WHERE name = '{name}' to locate the definition, \
                 then SHOW LINES n-m OF 'file' to read it.",
                loc.node_kind
            );
        }

        // Follow the `body_symbol` redirect for C++ out-of-line definitions.
        // The redirect is resolved without user clauses — matches legacy behaviour.
        if let Some(target) = loc.enrichment.get("body_symbol").cloned()
            && let Some(redirected) =
                self.resolve_impl(&target, &Clauses::default(), root, Some(BODY_KINDS))
        {
            return Ok(Some(redirected));
        }
        Ok(Some(loc))
    }

    fn index_stats(&self) -> Option<&IndexStats> {
        Some(&self.stats)
    }

    fn locate_definition(&self, name: &str) -> Option<(PathBuf, usize)> {
        self.resolve_impl(
            name,
            &crate::ir::Clauses::default(),
            &self.worktree_root,
            None,
        )
        .map(|loc| (loc.path, loc.line))
    }

    fn build(&mut self, _workspace: &Workspace) -> Result<()> {
        // The columnar backend is populated by ShadowWriter during the legacy
        // build; it does not expose its own independent build path.
        Err(anyhow::anyhow!(
            "ColumnarStorage::build is not callable directly; \
             use shadow_write via LegacyMemoryStorage"
        ))
    }

    #[allow(clippy::too_many_lines)]
    fn reindex_files(&mut self, paths: &[PathBuf]) -> Result<()> {
        std::fs::create_dir_all(&self.staging_dir)?;
        let mut parser = tree_sitter::Parser::new();
        let enrichers = default_enrichers();

        for path in paths {
            // Strip worktree prefix to get the relative path stored in the overlay.
            let rel_path = path
                .strip_prefix(&self.worktree_root)
                .unwrap_or(path)
                .to_path_buf();

            // Shadow the persistent segment for this path so it is excluded from
            // queries while the new dirty segment takes precedence.
            // Capture old_hex now — it is the `replaces_hex` stored in the dirty
            // segment, so the delta file correctly records which persistent segment
            // this new entry supersedes.  (Bug: passing `hex_content_id` here would
            // store the *new* hash as `replaces_hex`, corrupting FT4 promotion.)
            let old_hex = self.path_to_hex_content_id(&rel_path).unwrap_or_default();
            if !old_hex.is_empty() {
                self.dirty.remove_hex(old_hex.clone());
            }
            // Also evict any previously-staged dirty segment for this path (re-edit).
            drop(self.dirty.remove_stale_for_path(&rel_path));

            if !path.exists() {
                // Purge-only — removal already applied above.
                continue;
            }

            let Some(lang) = self.lang_registry.language_for_path(path) else {
                // Unknown language — skip silently; persistent rows remain shadowed.
                continue;
            };

            let bytes = std::fs::read(path)?;
            let content_id_bytes = git_blob_sha1(&bytes);
            let hex_content_id = bytes_to_hex(&content_id_bytes);

            let seg_path = self.staging_dir.join(format!("{hex_content_id}.fqsf"));

            if !is_valid_segment(&seg_path) {
                parser
                    .set_language(&lang.tree_sitter_language())
                    .map_err(|e| anyhow::anyhow!("tree-sitter language error: {e}"))?;

                let mut table = SymbolTable::default();
                // index_file re-reads from disk; acceptable for the mutation path.
                let _ = index_file(
                    &mut parser,
                    path,
                    &mut table,
                    &enrichers,
                    lang.as_ref(),
                    None,
                    None,
                );

                let mut builder = SegmentBuilder::new("git-sha1", &content_id_bytes);
                for row in &table.rows {
                    #[allow(clippy::cast_possible_truncation)]
                    let row_id = builder.emit_row(
                        table.name_of(row),
                        table.fql_kind_of(row),
                        table.language_of(row),
                        row.line as u32,
                        row.byte_range.start as u32,
                        row.byte_range.end as u32,
                        row.usages_count,
                    );
                    for (key, val) in table.resolve_fields(&row.fields) {
                        builder.set_field(row_id, &key, val.as_str());
                    }
                }

                builder.flush(&seg_path)?;
            }

            let seg_reader = SegmentReader::open(&seg_path)?;
            self.dirty
                .add_segment(Arc::new(seg_reader), rel_path, old_hex);
        }
        self.save_delta()?;
        Ok(())
    }

    fn purge_file(&mut self, path: &Path) -> Result<()> {
        let rel_path = path
            .strip_prefix(&self.worktree_root)
            .unwrap_or(path)
            .to_path_buf();
        if let Some(old_hex) = self.path_to_hex_content_id(&rel_path) {
            self.dirty.remove_hex(old_hex);
        }
        drop(self.dirty.remove_stale_for_path(&rel_path));
        self.save_delta()?;
        Ok(())
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

    fn flush_delta(&mut self) -> Result<()> {
        self.save_delta()
    }

    fn reload_dirty_from_delta(&mut self) -> Result<()> {
        // Always GC first — safe for both ROLLBACK (removes orphaned staging
        // dirs from after the checkpoint) and reconnect (no-op when delta+staging
        // are already in sync).
        self.reload_delta_after_rollback()
    }

    fn commit_dirty(
        &mut self,
        new_commit_oid: &str,
        ctx: &crate::storage::ColumnarBuildContext,
    ) -> Result<()> {
        // Delegate to the inherent method which has access to all private fields.
        Self::commit_dirty_inner(self, new_commit_oid, ctx)
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
    #[allow(clippy::too_many_lines)]
    pub fn warm_or_open(
        ctx: &crate::storage::ColumnarBuildContext,
        legacy: Option<&LegacyMemoryStorage>,
        worktree_path: PathBuf,
        commit_sha: &str,
        lang_registry: Arc<LanguageRegistry>,
    ) -> Result<Self> {
        let overlay_path = ctx.overlay_path_for(commit_sha);

        // Fast path: overlay already on disk and readable.
        if overlay_path.exists() {
            if let Ok(overlay) = Overlay::open(&overlay_path) {
                debug!(%commit_sha, "columnar warm_or_open: overlay found, fast-path load");
                let segments = Self::open_segments_from_overlay(ctx, &overlay);
                let mut storage = Self::new(worktree_path, segments, overlay, lang_registry);
                if let Err(e) = storage.load_delta() {
                    tracing::warn!(%commit_sha, "columnar warm_or_open: delta load failed (non-fatal): {e}");
                }
                return Ok(storage);
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
                        let mut storage =
                            Self::new(worktree_path, segments, overlay, Arc::clone(&lang_registry));
                        if let Err(e) = storage.load_delta() {
                            tracing::warn!(%commit_sha, "columnar warm_or_open: delta load failed (non-fatal): {e}");
                        }
                        return Ok(storage);
                    }
                    let _ = std::fs::remove_file(&overlay_path);
                }

                // Build segments + overlay. Prefer the inline fast-path when
                // segments were already written per-file during build_index.
                let segment_map_opt = legacy.and_then(|l| l.prebuilt_segment_map.clone());

                if let Some(segment_map) = segment_map_opt {
                    // Fast-path: segments written inline — skip ShadowWriter.
                    let t_sw = std::time::Instant::now();
                    info!(
                        ms = t_sw.elapsed().as_millis(),
                        %commit_sha,
                        segments = segment_map.len(),
                        "TIMING warm_or_open: inline segments (no shadow-write)"
                    );
                    let builder = super::overlay_builder::OverlayBuilder::new(
                        &ctx.provider_id,
                        ctx.segments_dir.clone(),
                        worktree_path.clone(),
                        segment_map,
                    );
                    if let Err(e) = builder.build_and_persist(&overlay_path) {
                        tracing::warn!(
                            %commit_sha,
                            "columnar warm_or_open: overlay build failed: {e}"
                        );
                    } else {
                        debug!(%commit_sha, "columnar warm_or_open: overlay built (inline path)");
                    }
                } else if let Some(legacy) = legacy
                    && let Some(table) = legacy.table()
                {
                    // Legacy path: shadow-write from the merged SymbolTable.
                    let writer = super::shadow_writer::ShadowWriter::new(
                        table,
                        &ctx.segments_dir,
                        &ctx.provider_id,
                        ctx.hash_fn.as_ref(),
                        HashMap::new(),
                    );
                    let t_sw = std::time::Instant::now();
                    match writer.run() {
                        Ok(result) => {
                            info!(
                                ms = t_sw.elapsed().as_millis(),
                                %commit_sha,
                                segments = result.count,
                                "TIMING warm_or_open: shadow-write"
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
        let mut storage = Self::new(worktree_path, segments, overlay, lang_registry);
        if let Err(e) = storage.load_delta() {
            tracing::warn!(%commit_sha, "columnar warm_or_open: delta load failed (non-fatal): {e}");
        }
        Ok(storage)
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
        // Background warming never calls reindex_files; use an empty registry.
        let registry = Arc::new(LanguageRegistry::new(vec![]));
        let _ = Self::warm_or_open(ctx, legacy, worktree_path, commit_sha, registry)?;
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
                let dir = ctx.segment_path_for(&meta.hex_content_id);
                SegmentReader::open(&dir).ok().map(Arc::new)
            })
            .collect()
    }
}

impl ColumnarStorage {
    /// Mutable access to the per-session dirty overlay.
    ///
    /// Used by PhaseFT2 `reindex_files` and PhaseFT3 delta-file loading.
    pub const fn dirty_mut(&mut self) -> &mut DirtyOverlay {
        &mut self.dirty
    }

    /// Read-only access to the per-session dirty overlay.
    #[must_use]
    pub const fn dirty(&self) -> &DirtyOverlay {
        &self.dirty
    }

    /// Look up the `hex_content_id` of the persistent overlay segment for a
    /// given worktree-relative path, if one exists.
    fn path_to_hex_content_id(&self, rel_path: &Path) -> Option<String> {
        self.overlay
            .segments()
            .iter()
            .find(|m| m.source_path == rel_path)
            .map(|m| m.hex_content_id.clone())
    }

    // ─────────────────────────────────────────────────────────────────────
    // PhaseFT3: delta file helpers
    // ─────────────────────────────────────────────────────────────────────

    /// Serialize the current dirty overlay to `.forgeql-columnar-delta`.
    ///
    /// Delegates to [`DeltaFile::save`].  Called at the end of every
    /// `reindex_files` / `purge_file` and at the start of `BEGIN TRANSACTION`
    /// so the overlay state survives server restarts and `ROLLBACK`.
    fn save_delta(&self) -> Result<()> {
        DeltaFile::save(&self.dirty, &self.delta_path)
    }

    /// Load the delta file and restore the dirty overlay.
    ///
    /// No-op when `.forgeql-columnar-delta` does not exist (empty session).
    /// Called from `warm_or_open` (reconnect) and `reload_delta_after_rollback`.
    pub fn load_delta(&mut self) -> Result<()> {
        if self.delta_path.exists() {
            match DeltaFile::load(&self.delta_path, &self.staging_dir) {
                Ok(dirty) => self.dirty = dirty,
                Err(e) => {
                    tracing::warn!(
                        path = %self.delta_path.display(),
                        "columnar delta load failed, resetting dirty overlay: {e}"
                    );
                    self.dirty = DirtyOverlay::new();
                    let valid: &[String] = &[];
                    DeltaFile::gc_orphaned_staging(valid, &self.staging_dir);
                }
            }
        }
        Ok(())
    }

    /// Called by `ROLLBACK` after `git reset --hard` restores the worktree.
    ///
    /// Reads the valid hex IDs from the restored delta file, GCs any orphaned
    /// staging directories, then reloads the dirty overlay from the delta.
    pub fn reload_delta_after_rollback(&mut self) -> Result<()> {
        let valid_hexes = DeltaFile::read_valid_hexes(&self.delta_path);
        DeltaFile::gc_orphaned_staging(&valid_hexes, &self.staging_dir);
        self.dirty = DirtyOverlay::new();
        self.load_delta()
    }

    // ─────────────────────────────────────────────────────────────────────
    // PhaseFT4: commit_dirty — promote staging segments + build new overlay
    // ─────────────────────────────────────────────────────────────────────

    /// Called from `exec_commit` after the git commit succeeds.
    ///
    /// Promotes all staging segments to the bare-repo segment store, builds a
    /// new overlay for `new_commit_oid` by merging the persistent overlay with
    /// the dirty overlay, then swaps the session to the new overlay and clears
    /// all dirty state.
    ///
    /// # Errors
    /// Returns `Err` when segment promotion, overlay build/open, or staging-dir
    /// cleanup fails.  `exec_commit` treats this as non-fatal: the session falls
    /// back to its stale overlay; the next `USE` will rebuild from legacy.
    fn commit_dirty_inner(
        &mut self,
        new_commit_oid: &str,
        ctx: &super::build_context::ColumnarBuildContext,
    ) -> Result<()> {
        // 1. Promote staging segments → bare-repo segment store.
        //    Idempotent: skips any hex that is already there.
        for ds in &self.dirty.added {
            let hex = ds.reader.content_id_hex();
            let src = self.staging_dir.join(format!("{hex}.fqsf"));
            let dst = ctx.segment_path_for(&hex);
            promote_segment(&src, &dst)?;
        }

        // 2. Build new overlay = merge(persistent, dirty).
        //    All segments are re-opened fresh from the bare repo after promotion.
        let new_overlay_path = ctx.overlay_path_for(new_commit_oid);
        let builder = super::overlay_builder::OverlayBuilder::from_merge(
            &self.overlay,
            &self.dirty,
            ctx,
            &self.worktree_root,
        );
        builder.build_and_persist(&new_overlay_path)?;

        // 3. Swap to the new overlay (Overlay::open returns Arc<Overlay>).
        let new_overlay = Overlay::open(&new_overlay_path)
            .with_context(|| format!("open new overlay at {}", new_overlay_path.display()))?;
        let new_segments = Self::open_segments_from_overlay(ctx, &new_overlay);
        self.overlay = new_overlay;
        self.segments = new_segments;
        self.stats.rows = self.overlay.row_count() as usize;

        // 4. Clear dirty state and staging directory.
        self.dirty = DirtyOverlay::new();
        clear_staging_dir(&self.staging_dir)?;

        // 5. Remove the delta file — no pending changes after commit.
        let _ = std::fs::remove_file(&self.delta_path);

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PhaseFT4: private filesystem helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Promote a staging `.fqsf` segment file to the bare-repo segment store.
///
/// Prefers `rename(2)` for an atomic, zero-copy move on the same filesystem.
/// Falls back to `fs::copy` when the rename fails (cross-device or lost race).
/// The `dst.exists()` guard makes promotion idempotent.
fn promote_segment(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Ok(()); // already promoted — idempotent
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create segment parent dir {}", parent.display()))?;
    }
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    // Rename failed: cross-device or concurrent promotion won the race.
    if dst.exists() {
        return Ok(()); // lost race — peer already promoted
    }
    // True cross-device: copy the single .fqsf file.
    std::fs::copy(src, dst)
        .with_context(|| format!("copy segment {} → {}", src.display(), dst.display()))
        .map(|_| ())
}

/// Delete all entries inside the staging directory without removing the
/// directory itself (avoids a `create_dir_all` on the next `reindex_files`).
fn clear_staging_dir(staging_dir: &Path) -> Result<()> {
    if !staging_dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(staging_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("remove staging subdir {}", path.display()))?;
        } else {
            std::fs::remove_file(&path)
                .with_context(|| format!("remove staging file {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::pattern_as_prefix;

    #[test]
    fn prefix_one_char_wildcard() {
        assert_eq!(pattern_as_prefix("k%"), Some(b"k".to_vec()));
    }

    #[test]
    fn prefix_one_char_underscore_wildcard() {
        // 'k' is the literal prefix; '_' is a single-char wildcard → stop
        assert_eq!(pattern_as_prefix("k_%"), Some(b"k".to_vec()));
    }

    #[test]
    fn prefix_two_chars_wildcard() {
        assert_eq!(pattern_as_prefix("ab%"), Some(b"ab".to_vec()));
    }

    #[test]
    fn prefix_three_char_literal_returns_none() {
        // 3-char literal → None so trigrams handle it
        assert_eq!(pattern_as_prefix("abc%"), None);
    }

    #[test]
    fn prefix_two_char_literal_then_underscore() {
        // 'k_a%' — 'k' literal, then '_' wildcard → 1-char prefix
        assert_eq!(pattern_as_prefix("k_a%"), Some(b"k".to_vec()));
    }

    #[test]
    fn prefix_starts_with_percent_returns_none() {
        assert_eq!(pattern_as_prefix("%foo"), None);
    }

    #[test]
    fn prefix_starts_with_underscore_returns_none() {
        assert_eq!(pattern_as_prefix("_k%"), None);
    }

    #[test]
    fn prefix_suffix_pattern_returns_none() {
        assert_eq!(pattern_as_prefix("%k"), None);
    }

    #[test]
    fn prefix_case_insensitive() {
        // Builder lowercases names; pattern_as_prefix must lowercase too.
        assert_eq!(pattern_as_prefix("AB%"), Some(b"ab".to_vec()));
    }
}
