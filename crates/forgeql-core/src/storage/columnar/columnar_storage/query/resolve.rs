//! Name -> source-location resolution for [`ColumnarStorage`] (the `resolve_impl` core).

use roaring::RoaringBitmap;

use std::path::Path;

use crate::filter::eval_predicate;
use crate::ir::{Clauses, CompareOp, PredicateValue};
use crate::result::SymbolMatch;
use crate::storage::SymbolLocation;
use crate::storage::columnar::columnar_storage::ColumnarStorage;
use crate::storage::columnar::columnar_storage::fast_paths::{
    passes_resolve_glob, split_qualified_name,
};
use crate::storage::columnar::segment_builder::ZONEMAP_NUMERIC_FIELDS;
use crate::storage::columnar::segment_reader::SegmentReader;
impl ColumnarStorage {
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
    #[expect(
        clippy::too_many_lines,
        reason = "Three-phase resolution: dirty-overlay scan, persistent-overlay scan with zone-map pruning, and best-candidate selection"
    )]
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
                let sm = build_symbol_match(seg, local_row, relative_path);
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
                let sm = build_symbol_match(&ds.reader, local_row, &ds.source_path);
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

/// Build a `SymbolMatch` for one row, used to evaluate WHERE predicates during
/// resolution. Shared by the dirty-overlay and persistent-segment scans, which
/// differ only in the reader and source path they pull the row from.
fn build_symbol_match(reader: &SegmentReader, local_row: u32, source_path: &Path) -> SymbolMatch {
    let fql_kind_str = reader.fql_kind_of(local_row);
    let line_num = reader.line_of(local_row);
    SymbolMatch {
        name: reader.name_of(local_row).to_owned(),
        node_kind: None,
        fql_kind: (!fql_kind_str.is_empty()).then(|| fql_kind_str.to_owned()),
        language: {
            let l = reader.language_of(local_row);
            (!l.is_empty()).then(|| l.to_owned())
        },
        path: Some(source_path.to_path_buf()),
        line: (line_num != 0).then_some(line_num as usize),
        usages_count: Some(reader.usages_count_of(local_row) as usize),
        fields: reader.enrichment_for_row(local_row),
        count: None,
        node_id: reader
            .ordinal_of(local_row)
            .map(|ord| crate::node_id::make_node_id(&source_path.to_string_lossy(), ord)),
    }
}
