//! Core query execution for [`super::ColumnarStorage`].
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use roaring::RoaringBitmap;

use crate::ast::enrich::default_enrichers;
use crate::ast::index::{
    IndexContext, IndexStats, OrdinalHint, OrdinalRemapper, SymbolTable, index_file,
};
use crate::filter::{apply_clauses, eval_predicate};
use crate::ir::{Clauses, CompareOp, PredicateValue};
use crate::result::{FindNodeResult, SymbolMatch};
use crate::workspace::Workspace;

use super::super::bytes_to_hex;
use super::super::segment_builder::{
    RowId, SegmentBuilder, SymbolRow, ZONEMAP_NUMERIC_FIELDS, is_valid_segment,
};
use super::super::segment_reader::SegmentReader;
use super::ColumnarStorage;
use super::fast_paths::{
    glob_to_path_prefix, group_by_file_fast_path_eligible, group_by_kind_fast_path_eligible,
    has_any_indexed_predicate, order_by_name_desc_fast_path, order_by_name_fast_path,
    order_by_name_kind_desc_fast_path, order_by_name_kind_fast_path, passes_resolve_glob,
    split_qualified_name,
};
use crate::storage::git_sha1_provider::git_blob_sha1;
use crate::storage::{StorageEngine, SymbolLocation};
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
            ordinal: seg.ordinal_of(local_row),
        }
    }

    /// Resolve a `node_id` against the dirty segments only.
    ///
    /// A node created this session (via `INSERT … NODE`, or in a brand-new file)
    /// is materialized from its dirty segment via `make_node_id(source_path,
    /// ordinal)` with an ordinal beyond the committed high-water mark, so the
    /// committed lookup in [`Self::find_node`] misses it. Rebuild the id the same
    /// way `materialize_rows` does and resolve name / line / end_line / rev / nav
    /// from the dirty row. Returns `None` when no dirty segment owns this id. (BUG-008)
    fn find_node_in_dirty(
        &self,
        node_id: &str,
        ordinal: u32,
        root: &Path,
    ) -> Option<FindNodeResult> {
        for ds in &self.dirty.added {
            let path_str = ds.source_path.to_string_lossy();
            if crate::node_id::make_node_id(&path_str, ordinal) != node_id {
                continue;
            }
            let dseg = ds.reader.as_ref();
            let drow = (0..dseg.row_count).find(|&r| dseg.ordinal_of(r) == Some(ordinal))?;

            let name = dseg.name_of(drow).to_owned();
            let fql_kind = dseg.fql_kind_of(drow).to_owned();
            let line = dseg.line_of(drow) as usize;
            let rev = crate::node_id::format_rev(dseg.rev_of(drow));
            let path = root.join(&ds.source_path);
            #[allow(clippy::naive_bytecount)]
            let end_line = {
                let byte_end = dseg.byte_end_of(drow) as usize;
                if byte_end == 0 {
                    line
                } else {
                    let file_bytes = std::fs::read(&path).unwrap_or_default();
                    let end = byte_end.min(file_bytes.len());
                    file_bytes[..end].iter().filter(|&&b| b == b'\n').count() + 1
                }
            };
            // Nav ids must match what materialize_rows emits for this file.
            let opt_nav = |ord: u32| -> Option<String> {
                if ord == u32::MAX {
                    None
                } else {
                    Some(crate::node_id::make_node_id(&path_str, ord))
                }
            };
            return Some(FindNodeResult {
                node_id: node_id.to_owned(),
                fql_kind,
                name,
                path,
                line,
                end_line,
                rev,
                parent_node_id: opt_nav(dseg.parent_ordinal_of(drow)),
                first_child_node_id: opt_nav(dseg.first_child_ordinal_of(drow)),
                next_sibling_node_id: opt_nav(dseg.next_sibling_ordinal_of(drow)),
                prev_sibling_node_id: opt_nav(dseg.prev_sibling_ordinal_of(drow)),
            });
        }
        None
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
    #[expect(
        clippy::too_many_lines,
        reason = "Three-phase resolution: dirty-overlay scan, persistent-overlay scan with zone-map pruning, and best-candidate selection"
    )]
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
                // Apply IN/EXCLUDE glob filter — mirrors segments_passing_path_filter
                // from Stage 2.  Without this, `IN 'file'` clauses are silently
                // ignored for dirty segments, causing wrong-file resolution.
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
                        node_id: ds.reader.ordinal_of(local_row).map(|ord| {
                            crate::node_id::make_node_id(&ds.source_path.to_string_lossy(), ord)
                        }),
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
                        ordinal: ds.reader.ordinal_of(local_row),
                    };
                    if prefer_kinds.is_some_and(|kinds| kinds.contains(&fql_kind_str)) {
                        dirty_preferred.push(loc.clone());
                    }
                    dirty_all.push(loc);
                }
            }
            // Sort by path (alphabetical ascending) so .pop() returns the
            // alphabetically-last match — identical tie-breaking to Stage 2's
            // seg_order sort.  Eliminates insertion-order (edit-order) dependency.
            dirty_preferred.sort_by(|a, b| a.path.cmp(&b.path));
            if let Some(last) = dirty_preferred.pop() {
                return Some(last);
            }
            dirty_all.sort_by(|a, b| a.path.cmp(&b.path));
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
                    node_id: seg.ordinal_of(local_row).map(|ord| {
                        crate::node_id::make_node_id(&relative_path.to_string_lossy(), ord)
                    }),
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

    fn find_node(&self, node_id: &str, root: &Path) -> Result<Option<FindNodeResult>> {
        let stripped = node_id.strip_prefix('n').unwrap_or(node_id);
        let (hex_prefix, ord_str) = stripped
            .split_once('.')
            .ok_or_else(|| anyhow::anyhow!("invalid node_id format: {node_id}"))?;
        let ordinal: u32 = ord_str
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid ordinal in node_id: {node_id}"))?;

        // Committed path: the node existed at index time. Resolve its committed
        // row; when a dirty segment exists for this file (reindexed this session)
        // prefer its byte positions via a name + fql_kind proximity lookup, since
        // the committed byte_end can be stale after a prior edit shifted bytes.
        if let Some(seg_idx) = self
            .overlay
            .seg_idx_for_node_id_prefix(hex_prefix)
            .map(|i| i as usize)
            && let Some(seg) = self.segments.get(seg_idx)
            && let Some(seg_meta) = self.overlay.segments().get(seg_idx)
            && let Some(local_row) =
                (0..seg.row_count).find(|&r| seg.ordinal_of(r) == Some(ordinal))
        {
            let name_str = seg.name_of(local_row);
            let fql_kind_str = seg.fql_kind_of(local_row);
            let committed_line = seg.line_of(local_row);

            let live_lookup: Option<(&SegmentReader, u32)> = self
                .dirty
                .added
                .iter()
                .find(|ds| ds.source_path == seg_meta.source_path)
                .and_then(|ds| {
                    let rows = ds.reader.lookup_name(name_str);
                    if rows.is_empty() {
                        return None;
                    }
                    rows.into_iter()
                        .filter(|&r| ds.reader.fql_kind_of(r) == fql_kind_str)
                        .min_by_key(|&r| {
                            u64::from(ds.reader.line_of(r)).abs_diff(u64::from(committed_line))
                        })
                        .map(|row| (&*ds.reader, row))
                });

            let (data_seg, data_row): (&SegmentReader, u32) =
                live_lookup.map_or((&**seg, local_row), |(s, r)| (s, r));

            let name = data_seg.name_of(data_row).to_owned();
            let fql_kind = data_seg.fql_kind_of(data_row).to_owned();
            let line = data_seg.line_of(data_row) as usize;
            let rev = crate::node_id::format_rev(data_seg.rev_of(data_row));
            let path = root.join(&seg_meta.source_path);
            #[allow(clippy::naive_bytecount)]
            let end_line = {
                let byte_end = data_seg.byte_end_of(data_row) as usize;
                if byte_end == 0 {
                    line
                } else {
                    let file_bytes = std::fs::read(&path).unwrap_or_default();
                    let end = byte_end.min(file_bytes.len());
                    file_bytes[..end].iter().filter(|&&b| b == b'\n').count() + 1
                }
            };
            let opt_nav = |ord: u32| -> Option<String> {
                if ord == u32::MAX {
                    None
                } else {
                    Some(seg_meta.node_id(ord))
                }
            };
            // Nav pointers come from the committed segment — ordinals are layout-stable
            // (DFS order doesn't change when a body is replaced).
            return Ok(Some(FindNodeResult {
                node_id: node_id.to_owned(),
                fql_kind,
                name,
                path,
                line,
                end_line,
                rev,
                parent_node_id: opt_nav(seg.parent_ordinal_of(local_row)),
                first_child_node_id: opt_nav(seg.first_child_ordinal_of(local_row)),
                next_sibling_node_id: opt_nav(seg.next_sibling_ordinal_of(local_row)),
                prev_sibling_node_id: opt_nav(seg.prev_sibling_ordinal_of(local_row)),
            }));
        }

        // Dirty path: a node created this session (via INSERT NODE, or in a
        // brand-new file) is materialized only from its dirty segment with an
        // ordinal beyond the committed high-water mark; resolve it there so the
        // node_id FIND symbols just returned is usable without a COMMIT. (BUG-008)
        if let Some(result) = self.find_node_in_dirty(node_id, ordinal, root) {
            return Ok(Some(result));
        }

        Err(anyhow::anyhow!("node_id not found: {node_id}"))
    }

    fn find_node_id_at_line(&self, rel_path: &str, line: usize) -> Option<String> {
        // Dirty overlay (post-mutation segments) takes priority over committed.
        if !self.dirty.is_empty() {
            for ds in &self.dirty.added {
                if ds.source_path.to_str() == Some(rel_path) {
                    let local_row = (0..ds.reader.row_count)
                        .find(|&r| ds.reader.line_of(r) as usize == line)?;
                    let ord = ds.reader.ordinal_of(local_row)?;
                    return Some(crate::node_id::make_node_id(rel_path, ord));
                }
            }
        }
        // Fallback: committed overlay.
        let seg_idx = self
            .overlay
            .segments()
            .iter()
            .position(|s| s.source_path.to_str() == Some(rel_path))?;
        let seg = self.segments.get(seg_idx)?;
        let seg_meta = self.overlay.segments().get(seg_idx)?;
        let local_row = (0..seg.row_count).find(|&r| seg.line_of(r) as usize == line)?;
        Some(seg_meta.node_id(seg.ordinal_of(local_row).unwrap_or(0)))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Single linear resolver: build the newline index, fold the chosen segment's rows, pick the innermost-containing node per line — splitting scatters tightly-coupled state"
    )]
    fn innermost_nodes_for_lines(
        &self,
        rel_path: &str,
        root: &Path,
        start: usize,
        end: usize,
    ) -> Vec<Option<(String, usize)>> {
        // Fold one segment's rows into `out`, keeping for each line the
        // smallest-span node that contains it (ties resolve to the deeper/later
        // DFS row, which `<=` selects because children follow parents in order).
        fn fold_segment(
            out: &mut [Option<(String, usize)>],
            best_span: &mut [usize],
            reader: &SegmentReader,
            newlines: &[usize],
            start: usize,
            end: usize,
            node_id_of: &dyn Fn(u32) -> Option<String>,
        ) {
            for r in 0..reader.row_count {
                let Some(ord) = reader.ordinal_of(r) else {
                    continue;
                };
                let node_start = reader.line_of(r) as usize;
                if node_start == 0 || node_start > end {
                    continue;
                }
                let byte_end = reader.byte_end_of(r) as usize;
                let node_end = if byte_end == 0 {
                    node_start
                } else {
                    content_end_line(newlines, byte_end)
                };
                if node_end < start {
                    continue;
                }
                let node_span = node_end - node_start;
                let lo = node_start.max(start);
                let hi = node_end.min(end);
                for line in lo..=hi {
                    let idx = line - start;
                    if node_span <= best_span[idx]
                        && let Some(id) = node_id_of(ord)
                    {
                        out[idx] = Some((id, node_start));
                        best_span[idx] = node_span;
                    }
                }
            }
        }

        if start == 0 || end < start {
            return Vec::new();
        }
        let span = end - start + 1;
        let abs_path = root.join(rel_path);
        let Ok(file_bytes) = std::fs::read(&abs_path) else {
            return Vec::new();
        };
        // Byte offsets of every '\n', ascending, so a node ending at `byte_end`
        // resolves to a 1-based end line via `partition_point` — the same
        // newline-count rule `find_node` uses to derive `end_line`.
        let newlines: Vec<usize> = file_bytes
            .iter()
            .enumerate()
            .filter_map(|(i, &b)| (b == b'\n').then_some(i))
            .collect();
        let mut out: Vec<Option<(String, usize)>> = vec![None; span];
        let mut best_span: Vec<usize> = vec![usize::MAX; span];

        // Prefer a dirty (reindexed-this-session) segment: its byte offsets match
        // the file on disk. Otherwise use the committed segment only when it is
        // content-addressed-fresh — stale offsets must never fabricate handles.
        if let Some(ds) = self
            .dirty
            .added
            .iter()
            .find(|ds| ds.source_path.to_str() == Some(rel_path))
        {
            let node_id_of = |ord: u32| Some(crate::node_id::make_node_id(rel_path, ord));
            fold_segment(
                &mut out,
                &mut best_span,
                &ds.reader,
                &newlines,
                start,
                end,
                &node_id_of,
            );
            return out;
        }

        let Some(seg_idx) = self
            .overlay
            .segments()
            .iter()
            .position(|s| s.source_path.to_str() == Some(rel_path))
        else {
            return Vec::new();
        };
        if !self.is_path_fresh(Path::new(rel_path), root) {
            return Vec::new();
        }
        let (Some(seg), Some(seg_meta)) = (
            self.segments.get(seg_idx),
            self.overlay.segments().get(seg_idx),
        ) else {
            return Vec::new();
        };
        let node_id_of = |ord: u32| Some(seg_meta.node_id(ord));
        fold_segment(
            &mut out,
            &mut best_span,
            seg,
            &newlines,
            start,
            end,
            &node_id_of,
        );
        out
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Multiple indexed fast-paths plus a general materialise pipeline; splitting further would obscure the query plan structure"
    )]
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
        // ── Phase 9 fast-paths — GROUP BY file / fql_kind ────────────────────
        // When there are no WHERE predicates, GROUP BY file and GROUP BY fql_kind
        // can be served from overlay metadata or kind bitmaps without
        // materialising any individual symbol rows.
        // The count-based fast-paths are only valid when the overlay has no
        // duplicate source paths.  If duplicates exist, row_count and kind-bitmap
        // lengths overcount; fall through to the normal pipeline which deduplicates.
        let no_dup_paths = !self.overlay.has_duplicate_paths();
        if group_by_kind_fast_path_eligible(clauses, self.dirty.is_empty()) && no_dup_paths {
            return Ok(self.fast_group_by_kind(clauses));
        }
        if group_by_file_fast_path_eligible(clauses, self.dirty.is_empty()) && no_dup_paths {
            return Ok(self.fast_group_by_file(clauses));
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
            return Ok(results);
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
            return Ok(results);
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
            return Ok(results);
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
            return Ok(results);
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

    fn indexed_files(&self) -> Option<Vec<crate::result::FileEntry>> {
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

        Some(entries)
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
            let rel_path = path
                .strip_prefix(&self.worktree_root)
                .unwrap_or(path)
                .to_path_buf();

            // Build ordinal hints from the most-recent version of this segment:
            // prefer an existing dirty entry (re-edit within a transaction) over
            // the committed segment (first edit).  This keeps node_ids stable
            // across every reindex — including the one triggered by COMMIT MESSAGE
            // when dirty segments are promoted to committed.
            let hints: Vec<OrdinalHint> = {
                let seg: Option<&SegmentReader> = self
                    .dirty
                    .added
                    .iter()
                    .find(|ds| ds.source_path == rel_path)
                    .map(|ds| ds.reader.as_ref())
                    .or_else(|| {
                        self.overlay
                            .segments()
                            .iter()
                            .enumerate()
                            .find(|(_, m)| m.source_path == rel_path)
                            .and_then(|(idx, _)| self.segments.get(idx).map(Arc::as_ref))
                    });
                seg.map_or_else(Vec::new, |seg| {
                    (0..seg.row_count)
                        .filter_map(|row| {
                            let ordinal = seg.ordinal_of(row)?;
                            Some(OrdinalHint {
                                name: seg.name_of(row).to_owned(),
                                fql_kind: seg.fql_kind_of(row).to_owned(),
                                parent_ordinal: seg.parent_ordinal_of(row),
                                guard_group_id: seg
                                    .extra_field_str("guard_group_id", row)
                                    .map(str::to_owned),
                                guard_branch: seg
                                    .extra_field_str("guard_branch", row)
                                    .map(str::to_owned),
                                first_body_statement_fingerprint: seg
                                    .extra_field_str("first_body_statement_fingerprint", row)
                                    .map(str::to_owned),
                                content_hash: seg
                                    .extra_field_str("content_hash", row)
                                    .map(str::to_owned),
                                ordinal,
                            })
                        })
                        .collect()
                })
            };
            let remapper = OrdinalRemapper::from_previous(hints);

            // Shadow the persistent segment and evict any stale dirty entry
            // AFTER capturing hints so the remapper references valid data.
            let old_hex = self.path_to_hex_content_id(&rel_path).unwrap_or_default();
            if !old_hex.is_empty() {
                self.dirty.remove_hex(old_hex.clone());
            }
            drop(self.dirty.remove_stale_for_path(&rel_path));

            if !path.exists() {
                continue;
            }

            let Some(lang) = self.lang_registry.language_for_path(path) else {
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
                {
                    let mut ctx = IndexContext {
                        path,
                        language: lang.as_ref(),
                        enrichers: &enrichers,
                        macro_table: None,
                        ordinal_remapper: Some(remapper),
                        table: &mut table,
                    };
                    let _ = index_file(&mut parser, &mut ctx, None);
                }
                for enricher in &enrichers {
                    enricher.post_pass(&mut table, None);
                }

                let mut builder = SegmentBuilder::new("git-sha1", &content_id_bytes);

                let mut ordinal_row: Vec<(u32, u32, u32)> = Vec::new();
                for row in &table.rows {
                    let row_id = builder.emit_row(SymbolRow {
                        name: table.name_of(row),
                        fql_kind: table.fql_kind_of(row),
                        language: table.language_of(row),
                        line: u32::try_from(row.line).unwrap_or(u32::MAX),
                        byte_start: u32::try_from(row.byte_range.start).unwrap_or(u32::MAX),
                        byte_end: u32::try_from(row.byte_range.end).unwrap_or(u32::MAX),
                        usages_count: row.usages_count,
                    });
                    if let Some(ordinal) = row.ordinal {
                        builder.set_ordinal(row_id, ordinal);
                        builder.set_parent_ordinal(row_id, row.parent_ordinal);
                        builder.set_rev(row_id, row.rev);
                        ordinal_row.push((ordinal, row_id.0, row.parent_ordinal));
                    }
                    for (key, val) in table.resolve_fields(&row.fields) {
                        if key == "parent_ordinal" {
                            continue;
                        }
                        builder.set_field(row_id, &key, val.as_str());
                    }
                }
                // Nav post-pass: fill first_child/next_sibling/prev_sibling and the
                // typed parent_ordinal/rev columns so reindexed segments carry the
                // same navigation + identity data as the initial shadow_writer build.
                {
                    let mut by_parent: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
                    let mut ord_to_row: HashMap<u32, u32> = HashMap::new();
                    for &(ord, rid, parent) in &ordinal_row {
                        by_parent.entry(parent).or_default().push((ord, rid));
                        let _ = ord_to_row.insert(ord, rid);
                    }
                    for (parent_ord, mut children) in by_parent {
                        children.sort_unstable_by_key(|&(ord, _)| ord);
                        if let Some(&parent_rid) = ord_to_row.get(&parent_ord)
                            && let Some(&(first_ord, _)) = children.first()
                        {
                            builder.set_first_child_ordinal(RowId(parent_rid), first_ord);
                        }
                        for i in 0..children.len() {
                            let (_, this_rid) = children[i];
                            if i > 0 {
                                builder
                                    .set_prev_sibling_ordinal(RowId(this_rid), children[i - 1].0);
                            }
                            if i + 1 < children.len() {
                                builder
                                    .set_next_sibling_ordinal(RowId(this_rid), children[i + 1].0);
                            }
                        }
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

    fn is_path_fresh(&self, rel_path: &Path, _root: &Path) -> bool {
        // A dirty segment for this path was built from the current file on
        // disk, so its line/byte data is authoritative — always fresh.
        if self.dirty.added.iter().any(|ds| ds.source_path == rel_path) {
            return true;
        }
        // No committed segment → nothing indexed for this path; there is no
        // stale absolute line data to serve.
        let Some(stored_hex) = self.path_to_hex_content_id(rel_path) else {
            return true;
        };
        // Compare the committed segment's content hash against the live file.
        // The committed overlay is git-sha1 content-addressed (see reindex_files
        // and the shadow-write hash_fn), so any divergence — HEAD advanced past
        // the cached overlay, a file reverted while git-clean, or an edit made
        // outside ForgeQL — surfaces here as a hash mismatch.
        let abs = self.worktree_root.join(rel_path);
        let Ok(bytes) = std::fs::read(&abs) else {
            // Unreadable (e.g. deleted): don't force a reindex loop — let the
            // normal mutation/query path surface the I/O error.
            return true;
        };
        bytes_to_hex(&git_blob_sha1(&bytes)) == stored_hex
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
                let node_id = seg.ordinal_of(row).map(|ord| seg_meta.node_id(ord));
                entries.push((
                    line,
                    serde_json::json!({
                        "name": name,
                        "fql_kind": kind,
                        "path": rel_path,
                        "line": line,
                        "node_id": node_id,
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

/// 1-based source line of a node's last content byte, from the file's sorted
/// newline byte offsets and the node's exclusive `byte_end`.
///
/// Trailing newline bytes are trimmed first: tree-sitter often folds the
/// terminating `\n` into a node's range (Markdown headings/paragraphs
/// especially), which would push the end line one past the node's last content
/// line and let a 1-line node spuriously "contain" the next line. Trimming is
/// harmless for code, whose `byte_end` sits at a closing token, not a newline.
fn content_end_line(newlines: &[usize], byte_end: usize) -> usize {
    let mut end = byte_end;
    while end > 0 && newlines.binary_search(&(end - 1)).is_ok() {
        end -= 1;
    }
    newlines.partition_point(|&nl| nl < end) + 1
}

#[cfg(test)]
mod innermost_resolver_tests {
    use super::content_end_line;

    #[test]
    fn content_end_line_excludes_trailing_newline() {
        // File "ab\ncd\n": newline bytes at indices 2 and 5.
        let newlines = [2usize, 5];
        // A node whose tree-sitter range folds in the terminating '\n'
        // (byte_end just past line 1's '\n' = 3) must still end on line 1, not
        // spill onto line 2 — otherwise a 1-line Markdown heading spuriously
        // "contains" the next line and wins the innermost-node pick.
        assert_eq!(content_end_line(&newlines, 3), 1);
        // No trailing newline: byte_end at line 1's last content byte.
        assert_eq!(content_end_line(&newlines, 2), 1);
        // Ends on line 2's '\n' (byte 5) → line 2.
        assert_eq!(content_end_line(&newlines, 5), 2);
    }
}
