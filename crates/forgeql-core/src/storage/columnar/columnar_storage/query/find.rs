//! `FIND symbols` / `FIND usages` / indexed-files queries for [`ColumnarStorage`].
use std::collections::{HashMap, HashSet};
use std::path::Path;

use roaring::RoaringBitmap;

use crate::filter::apply_clauses;
use crate::ir::{Clauses, CompareOp, PredicateValue};
use crate::result::SymbolMatch;
use crate::storage::columnar::columnar_storage::ColumnarStorage;
use crate::storage::columnar::columnar_storage::fast_paths::{
    glob_to_path_prefix, group_by_file_fast_path_eligible, group_by_kind_fast_path_eligible,
    has_any_indexed_predicate, order_by_name_desc_fast_path, order_by_name_fast_path,
    order_by_name_kind_desc_fast_path, order_by_name_kind_fast_path, passes_resolve_glob,
};
use crate::storage::columnar::segment_builder::ZONEMAP_NUMERIC_FIELDS;

impl ColumnarStorage {
    #[expect(
        clippy::too_many_lines,
        reason = "Multiple indexed fast-paths plus a general materialise pipeline; splitting further would obscure the query plan structure"
    )]
    pub(super) fn find_symbols_impl(&self, clauses: &Clauses, _root: &Path) -> Vec<SymbolMatch> {
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
        // ── Phase 9 fast-paths — GROUP BY file / fql_kind ────────────────────
        // When there are no WHERE predicates, GROUP BY file and GROUP BY fql_kind
        // can be served from overlay metadata or kind bitmaps without
        // materialising any individual symbol rows.
        // The count-based fast-paths are only valid when the overlay has no
        // duplicate source paths.  If duplicates exist, row_count and kind-bitmap
        // lengths overcount; fall through to the normal pipeline which deduplicates.
        let no_dup_paths = !self.overlay.has_duplicate_paths();
        if group_by_kind_fast_path_eligible(clauses, self.dirty.is_empty()) && no_dup_paths {
            return self.fast_group_by_kind(clauses);
        }
        if group_by_file_fast_path_eligible(clauses, self.dirty.is_empty()) && no_dup_paths {
            return self.fast_group_by_file(clauses);
        }
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
            return results;
        }
        // ── ORDER BY name ASC LIMIT N WHERE fql_kind='X' fast-path ───────────
        // Same as above but the kind bitmap is pre-loaded and used to gate which
        // FST rows are materialised.
        if let Some(kind) = order_by_name_kind_fast_path(clauses)
            && self.dirty.is_empty()
            && let Some(kind_bm) = self.overlay.prefilter_kind(kind)
        {
            let need = clauses
                .limit
                .unwrap_or(0)
                .saturating_add(clauses.offset.unwrap_or(0))
                .max(1);
            let mut results =
                self.overlay
                    .stream_names_asc_kind_filtered(need, &kind_bm, &self.segments);
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
            return results;
        }
        // ── ORDER BY name DESC LIMIT N fast-path ─────────────────────────────
        if order_by_name_desc_fast_path(clauses) && self.dirty.is_empty() {
            let need = clauses
                .limit
                .unwrap_or(0)
                .saturating_add(clauses.offset.unwrap_or(0))
                .max(1);
            let mut results = self.overlay.stream_names_desc(need, &self.segments);
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
            return results;
        }
        // ── ORDER BY name DESC LIMIT N WHERE fql_kind='X' fast-path ──────────
        if let Some(kind) = order_by_name_kind_desc_fast_path(clauses)
            && self.dirty.is_empty()
            && let Some(kind_bm) = self.overlay.prefilter_kind(kind)
        {
            let need = clauses
                .limit
                .unwrap_or(0)
                .saturating_add(clauses.offset.unwrap_or(0))
                .max(1);
            let mut results =
                self.overlay
                    .stream_names_desc_kind_filtered(need, &kind_bm, &self.segments);
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
            return results;
        }
        // ─────────────────────────────────────────────────────────────────────
        let has_path_filter = clauses.in_glob.is_some() || clauses.exclude_glob.is_some();

        let mut by_segment: HashMap<u32, RoaringBitmap> = if has_path_filter
            && !has_any_indexed_predicate(clauses, &self.overlay)
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
        results
    }

    pub(super) fn find_usages_impl(
        &self,
        name: &str,
        clauses: &Clauses,
        _root: &Path,
    ) -> Vec<SymbolMatch> {
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
        results
    }

    pub(super) fn indexed_files_impl(&self) -> Vec<crate::result::FileEntry> {
        let segs = self.overlay.segments();
        let file_only = self.overlay.file_entries();
        let mut entries = Vec::with_capacity(
            segs.len()
                .saturating_add(file_only.len())
                .saturating_add(self.dirty.added.len()),
        );

        // Base: persistent overlay segments with mmap-cached sizes.
        // Skip any segment shadowed (replaced or deleted) by the dirty overlay.
        for (idx, seg) in segs.iter().enumerate() {
            if self.dirty.shadows(&seg.hex_content_id) {
                continue;
            }
            let ext = seg
                .source_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            let size = u64::from(self.overlay.file_size(idx));
            let depth = Some(seg.source_path.components().count());
            entries.push(crate::result::FileEntry {
                path: seg.source_path.clone(),
                extension: ext,
                size,
                depth,
                count: None,
            });
        }

        // File-only entries (FQOV v8+): non-indexed workspace files tracked
        // only for path + size.  These are never shadowed by the dirty overlay
        // because the dirty overlay only holds symbol segments.
        for (rel_path, size) in file_only {
            let ext = rel_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            let depth = Some(rel_path.components().count());
            entries.push(crate::result::FileEntry {
                path: rel_path.clone(),
                extension: ext,
                size: u64::from(*size),
                depth,
                count: None,
            });
        }

        // Overlay: dirty segments (files changed in this session).
        // Read actual on-disk size — only 1 syscall per mutated file.
        for ds in &self.dirty.added {
            let ext = ds
                .source_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            let size = self
                .worktree_root
                .join(&ds.source_path)
                .metadata()
                .map(|m| m.len())
                .unwrap_or(0);
            let depth = Some(ds.source_path.components().count());
            entries.push(crate::result::FileEntry {
                path: ds.source_path.clone(),
                extension: ext,
                size,
                depth,
                count: None,
            });
        }

        entries
    }
}
