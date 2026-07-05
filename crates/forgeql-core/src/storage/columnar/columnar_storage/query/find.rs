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
    pub(super) fn find_symbols_impl(&self, clauses: &Clauses, _root: &Path) -> Vec<SymbolMatch> {
        // Query pipeline:
        //   Stage 1 — prefilter_global: intersect indexed predicates (kind
        //             bitmap, name FST, trigram / short-prefix LIKE index) into a
        //             candidate global row-ID bitmap.
        //   Stage 2 — partition by segment, then prune the survivors: IN/EXCLUDE
        //             path globs, dirty-overlay shadows, and numeric zone maps.
        //   Stage 3 — materialise the surviving rows, then union the dirty overlay.
        //   Stage 4 — deduplicate on (name, fql_kind, path, line).
        //   Stage 5 — apply residual WHERE, ORDER BY, LIMIT, OFFSET.
        // GROUP BY and ORDER BY name fast-paths short-circuit the pipeline. The
        // count-based GROUP BY paths are only valid when source paths are unique;
        // duplicates overcount, so fall through to the deduplicating pipeline.
        let no_dup_paths = !self.overlay.has_duplicate_paths();
        if group_by_kind_fast_path_eligible(clauses, self.dirty.is_empty()) && no_dup_paths {
            return self.fast_group_by_kind(clauses);
        }
        if group_by_file_fast_path_eligible(clauses, self.dirty.is_empty()) && no_dup_paths {
            return self.fast_group_by_file(clauses);
        }
        if let Some(results) = self.try_order_by_name_fast_paths(clauses) {
            return results;
        }

        let mut by_segment = self.build_candidate_segments(clauses);
        self.prune_candidate_segments(&mut by_segment, clauses);

        let mut results = self.materialize_all(&by_segment, clauses);
        // Stage 3b — union dirty overlay rows (empty when the overlay is empty).
        if !self.dirty.is_empty() {
            let mut dirty_results = self.dirty.materialize_all(clauses);
            results.append(&mut dirty_results);
        }
        dedupe_symbol_matches(&mut results);
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
            // Session infrastructure, not source: the worktree gitfile pointer
            // and forgeql's own runtime artifacts (`.forgeql-session`, …).
            if crate::result::FileEntry::is_runtime_artifact(rel_path) {
                continue;
            }
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

impl ColumnarStorage {
    /// The four `ORDER BY name [DESC] [WHERE fql_kind=...] LIMIT N` fast-paths.
    ///
    /// Each streams the first `limit + offset` rows directly from the name FST
    /// in lexicographic order, materialising only those rows. All are gated on
    /// an empty dirty overlay because dirty rows are not path-sorted and could
    /// carry names that precede committed rows already streamed. Returns `None`
    /// when no name-ordered fast-path applies, so the caller runs the pipeline.
    fn try_order_by_name_fast_paths(&self, clauses: &Clauses) -> Option<Vec<SymbolMatch>> {
        if !self.dirty.is_empty() {
            return None;
        }
        let need = fast_path_need(clauses);
        let mut results = if order_by_name_fast_path(clauses) {
            self.overlay.stream_names_asc(need, &self.segments)
        } else if order_by_name_desc_fast_path(clauses) {
            self.overlay.stream_names_desc(need, &self.segments)
        } else if let Some(kind) = order_by_name_kind_fast_path(clauses) {
            let kind_bm = self.overlay.prefilter_kind(kind)?;
            self.overlay
                .stream_names_asc_kind_filtered(need, &kind_bm, &self.segments)
        } else if let Some(kind) = order_by_name_kind_desc_fast_path(clauses) {
            let kind_bm = self.overlay.prefilter_kind(kind)?;
            self.overlay
                .stream_names_desc_kind_filtered(need, &kind_bm, &self.segments)
        } else {
            return None;
        };
        dedupe_symbol_matches(&mut results);
        apply_clauses(&mut results, clauses);
        Some(results)
    }

    /// Build the initial `segment index -> local row bitmap` candidate map.
    ///
    /// Fast path (a path filter is present but no indexed predicate is
    /// available): seed every path-matching segment with all its rows, skipping
    /// the global prefilter and per-segment grouping. Normal path: global
    /// prefilter, group by segment, then IN / EXCLUDE path prune.
    fn build_candidate_segments(&self, clauses: &Clauses) -> HashMap<u32, RoaringBitmap> {
        let has_path_filter = clauses.in_glob.is_some() || !clauses.exclude_globs.is_empty();
        if has_path_filter && !has_any_indexed_predicate(clauses, &self.overlay) {
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
            // Phase 6 — build path_floor before prefilter_global so it can serve
            // as the baseline universe: when no indexed predicate matches it is
            // returned directly; when one does, the result is already intersected
            // with the path range.
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
            if let Some(allowed) = self.segments_passing_path_filter(clauses) {
                map.retain(|seg_idx, _| allowed.contains(seg_idx));
            }
            map
        }
    }

    /// Prune candidate segments that cannot contribute rows.
    ///
    /// Stage 2d drops persistent segments shadowed by the dirty overlay (a file
    /// changed or deleted this session keeps only its dirty version). Stage 2c
    /// drops segments whose zone maps rule out a numeric WHERE predicate
    /// (`line > N`, `usages >= N`, ...). Both steps are additive: segments
    /// lacking the relevant metadata are always kept.
    fn prune_candidate_segments(
        &self,
        by_segment: &mut HashMap<u32, RoaringBitmap>,
        clauses: &Clauses,
    ) {
        if !self.dirty.is_empty() {
            by_segment.retain(|&seg_idx, _| {
                self.overlay
                    .segments()
                    .get(seg_idx as usize)
                    .is_none_or(|meta| !self.dirty.shadows(&meta.hex_content_id))
            });
        }
        for pred in &clauses.where_predicates {
            if let PredicateValue::Number(val_i64) = &pred.value {
                // The FQL parser emits "usages" but the zone-map column is
                // written as "usages_count" by the segment builder.
                let col = match pred.field.as_str() {
                    "usages" => "usages_count",
                    other => other,
                };
                // Impossible-predicate short-circuit for u32 columns: no stored
                // value satisfies col < 0, col <= negative, or col = negative.
                let impossible = ZONEMAP_NUMERIC_FIELDS.iter().any(|(f, _)| *f == col)
                    && match pred.op {
                        CompareOp::Lt => *val_i64 <= 0,
                        CompareOp::Lte | CompareOp::Eq => *val_i64 < 0,
                        _ => false,
                    };
                if impossible {
                    by_segment.clear();
                    return;
                }
                if let Ok(val_u32) = u32::try_from(*val_i64)
                    && let Some(allowed) = self.segments_passing_zone_map(col, pred.op, val_u32)
                {
                    by_segment.retain(|seg_idx, _| allowed.contains(seg_idx));
                }
            }
        }
    }
}

/// Rows to stream from an ordered fast-path: `limit + offset`, at least 1.
fn fast_path_need(clauses: &Clauses) -> usize {
    clauses
        .limit
        .unwrap_or(0)
        .saturating_add(clauses.offset.unwrap_or(0))
        .max(1)
}

/// Deduplicate symbol results on `(name, fql_kind, path, line)`.
///
/// Mirrors the legacy backend, which deduplicates on
/// `(name_id, path_id, node_kind_id, line)`. The columnar result does not store
/// raw `node_kind`, so `fql_kind` is the closest available approximation.
fn dedupe_symbol_matches(results: &mut Vec<SymbolMatch>) {
    type DedupeKey = (
        String,
        Option<String>,
        Option<std::path::PathBuf>,
        Option<usize>,
    );
    let mut seen: HashSet<DedupeKey> = HashSet::new();
    results.retain(|r| seen.insert((r.name.clone(), r.fql_kind.clone(), r.path.clone(), r.line)));
}
