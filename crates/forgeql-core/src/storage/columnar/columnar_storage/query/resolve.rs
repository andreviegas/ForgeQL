//! Name -> source-location resolution for [`ColumnarStorage`] (the `resolve_impl` core).

use roaring::RoaringBitmap;

use std::path::Path;

use crate::ir::{Clauses, CompareOp, PredicateValue};
use crate::storage::SymbolLocation;
use crate::storage::columnar::columnar_storage::ColumnarStorage;
use crate::storage::columnar::columnar_storage::fast_paths::{
    passes_resolve_glob, split_qualified_name,
};
use crate::storage::columnar::segment_builder::ZONEMAP_NUMERIC_FIELDS;
/// Candidate `(segment_index, local_row)` pairs produced by a resolve scan:
/// `(all, preferred)` — every passing candidate and the `prefer_kinds` subset.
type ResolveCandidates = (Vec<(u32, u32)>, Vec<(u32, u32)>);
impl ColumnarStorage {
    /// Core columnar resolve used by all three `StorageEngine::resolve_*` methods.
    ///
    /// Algorithm:
    /// 1. Split qualified name (`Owner::member` / `Owner.member`).
    /// 2. FST name lookup via the overlay bitmap.
    /// 3. Filter candidates by enclosing-type and IN/EXCLUDE glob. WHERE
    ///    predicates are NOT applied — they filter SHOW output, not resolution.
    /// 4. Collect two lists — `all` (every passing candidate) and `preferred`
    ///    (candidates whose `fql_kind` is in `prefer_kinds`, if given).
    /// 5. Pick: last preferred candidate → last definition candidate → last overall.
    /// 6. Convert the chosen row to a [`SymbolLocation`].
    pub(super) fn resolve_impl(
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
        if !self.dirty.is_empty()
            && let Some(loc) =
                self.resolve_in_dirty(lookup_name, enclosing_owner, clauses, root, prefer_kinds)
        {
            return Some(loc);
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
                    .is_none_or(|meta| !self.dirty.shadows(&meta.source_path))
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
        self.prune_seg_order_by_zone_maps(&mut seg_order, clauses);

        // Collect every candidate that passes all filters; `preferred` also matches
        // `prefer_kinds` (if given).
        let (all, preferred) = self.collect_resolve_candidates(
            seg_order,
            &by_segment,
            clauses,
            enclosing_owner,
            prefer_kinds,
        );
        if all.is_empty() {
            return None;
        }

        let chosen = self.pick_best_resolved(&all, &preferred);
        chosen.map(|(seg_idx, local_row)| self.location_for_row(seg_idx, local_row, root))
    }

    /// Zone-map prune for numeric range predicates: drop (or, for impossible
    /// predicates on u32 columns, clear) candidate segments in `seg_order` that
    /// cannot satisfy a numeric `WHERE`. Same field-alias and negative-value
    /// rules as `find_symbols`.
    fn prune_seg_order_by_zone_maps(&self, seg_order: &mut Vec<u32>, clauses: &Clauses) {
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
    }

    /// Scan the candidate segments in `seg_order`, applying the enclosing-type
    /// filter (for qualified names), the enrichment-postings prefilter, and the
    /// `WHERE` predicates. Returns `(all, preferred)`: every passing `(seg, row)`
    /// pair, and the subset whose `fql_kind` also matches `prefer_kinds`.
    fn collect_resolve_candidates(
        &self,
        seg_order: Vec<u32>,
        by_segment: &std::collections::HashMap<u32, RoaringBitmap>,
        clauses: &Clauses,
        enclosing_owner: Option<&str>,
        prefer_kinds: Option<&[&str]>,
    ) -> ResolveCandidates {
        let mut all: Vec<(u32, u32)> = Vec::new();
        let mut preferred: Vec<(u32, u32)> = Vec::new();

        for seg_idx in seg_order {
            let Some(local_rows) = by_segment.get(&seg_idx) else {
                continue;
            };
            let Some(seg) = self.segments.get(seg_idx as usize) else {
                continue;
            };
            if self.overlay.segments().get(seg_idx as usize).is_none() {
                continue;
            }
            // Enrichment-postings prefilter — bitmap intersection per allowlisted
            // field before any per-row work.  Mirrors the same step in materialize_all.
            let local_rows = seg.prefilter_enrichment_postings(local_rows.clone(), clauses);
            if local_rows.is_empty() {
                continue;
            }

            for local_row in local_rows {
                // Enclosing-type filter for qualified names.
                if let Some(owner) = enclosing_owner
                    && seg
                        .extra_field_str("enclosing_type", local_row)
                        .unwrap_or("")
                        != owner
                {
                    continue;
                }

                // WHERE predicates on a SHOW statement filter the output rows
                // (body lines, callees, members), never the addressed symbol
                // row itself — evaluating them against the candidate row turned
                // every filtered SHOW into a false symbol-not-found, so none
                // are applied during resolution.
                let fql_kind_str = seg.fql_kind_of(local_row);
                all.push((seg_idx, local_row));
                if let Some(kinds) = prefer_kinds
                    && kinds.contains(&fql_kind_str)
                {
                    preferred.push((seg_idx, local_row));
                }
            }
        }

        (all, preferred)
    }

    /// Pick the winning candidate, mirroring the legacy "last-write-wins" strategy:
    /// last preferred → last definition (non-empty `fql_kind`) → last overall.
    fn pick_best_resolved(
        &self,
        all: &[(u32, u32)],
        preferred: &[(u32, u32)],
    ) -> Option<(u32, u32)> {
        if preferred.is_empty() {
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
        }
    }
}

impl ColumnarStorage {
    /// Stage 1 of `resolve_impl`: scan the dirty overlay's added segments before
    /// the persistent index, so a name that is new in (or shadowed by) the dirty
    /// overlay always wins. Returns the alphabetically-last match (preferring
    /// `prefer_kinds`), or `None` to fall through to the persistent index.
    fn resolve_in_dirty(
        &self,
        lookup_name: &str,
        enclosing_owner: Option<&str>,
        clauses: &Clauses,
        root: &Path,
        prefer_kinds: Option<&[&str]>,
    ) -> Option<SymbolLocation> {
        let mut dirty_all: Vec<SymbolLocation> = Vec::new();
        let mut dirty_preferred: Vec<SymbolLocation> = Vec::new();
        for ds in &self.dirty.added {
            // Apply IN/EXCLUDE glob filter — mirrors segments_passing_path_filter
            // from Stage 2; without it, `IN 'file'` is silently ignored for dirty
            // segments, causing wrong-file resolution.
            if !passes_resolve_glob(&ds.source_path, clauses) {
                continue;
            }
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
                    ordinal: ds.reader.ordinal_of(local_row),
                };
                if prefer_kinds.is_some_and(|kinds| kinds.contains(&fql_kind_str)) {
                    dirty_preferred.push(loc.clone());
                }
                dirty_all.push(loc);
            }
        }
        // Sort by path ascending so .pop() returns the alphabetically-last match —
        // identical tie-breaking to Stage 2's seg_order sort, eliminating any
        // insertion-order (edit-order) dependency.
        dirty_preferred.sort_by(|a, b| a.path.cmp(&b.path));
        if let Some(last) = dirty_preferred.pop() {
            return Some(last);
        }
        dirty_all.sort_by(|a, b| a.path.cmp(&b.path));
        dirty_all.pop()
    }
}
