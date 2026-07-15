//! Node-id <-> source-location lookup for [`ColumnarStorage`]: find_node, innermost spans, content-end-line helpers.

use anyhow::Result;
use std::path::Path;
use std::path::PathBuf;

use crate::result::FindNodeResult;
use crate::storage::StorageEngine;
use crate::storage::SymbolLocation;
use crate::storage::columnar::columnar_storage::ColumnarStorage;
use crate::storage::columnar::segment_reader::SegmentReader;

impl ColumnarStorage {
    /// Build a [`SymbolLocation`] from a single segment row.
    ///
    /// `seg_idx` indexes into both `self.segments` and
    /// `self.overlay.segments()` (they are kept in the same order by
    /// [`ColumnarStorage::new`]).
    pub(super) fn location_for_row(
        &self,
        seg_idx: u32,
        local_row: u32,
        root: &Path,
    ) -> SymbolLocation {
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
                    content_end_line_in_bytes(&file_bytes, byte_end)
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
    pub(super) fn find_node_impl(
        &self,
        node_id: &str,
        root: &Path,
    ) -> Result<Option<FindNodeResult>> {
        let stripped = node_id.strip_prefix('n').unwrap_or(node_id);
        // A dotless `n<hex>` is a whole-file (or whole-directory) handle: the
        // ordinal is what makes an id point *inside* a file, so its absence
        // means the file itself. Resolution is a different problem — path
        // fingerprint instead of ordinal — so it gets its own path.
        let Some((hex_prefix, ord_str)) = stripped.split_once('.') else {
            return self.find_path_node(node_id, stripped, root).map(Some);
        };
        let ordinal: u32 = ord_str
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid ordinal in node_id: {node_id}"))?;

        // Dirty-first: when this file was reindexed this session, its dirty
        // segment holds the ordinals that SHOW/FIND emit for it, and
        // find_node_id_at_line reads those same dirty ordinals. Resolve here so
        // reads and writes agree. Committed-first resolution could map an ordinal
        // the OrdinalRemapper reassigned to a different committed node and silently
        // edit the wrong line (BUG-011); this also covers nodes created this
        // session beyond the committed high-water mark (BUG-008).
        if let Some(result) = self.find_node_in_dirty(node_id, ordinal, root) {
            return Ok(Some(result));
        }

        // Committed path: the node existed at index time. Resolve its committed
        // row; when a dirty segment exists for this file (reindexed this session)
        // prefer its byte positions via a name + fql_kind proximity lookup, since
        // the committed byte_end can be stale after a prior edit shifted bytes.
        if let Some(seg_idx) = self
            .overlay
            .seg_idx_for_node_id_prefix(hex_prefix)
            .map(|i| i as usize)
            && let Some(seg) = self.segments.get(seg_idx)
            && self.overlay.segments().get(seg_idx).is_some()
            && let Some(local_row) =
                (0..seg.row_count).find(|&r| seg.ordinal_of(r) == Some(ordinal))
        {
            return Ok(self.build_committed_node_result(node_id, ordinal, root, seg_idx, local_row));
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

    /// Resolve a bare-hex handle — `n<hex>` — to the file or directory whose
    /// path fingerprints to `<hex>`, and synthesize a node for it.
    ///
    /// The synthesized node is never stored. Everything in it is derived from
    /// the path (which is what `<hex>` already encodes) and from the bytes on
    /// disk, so it costs no index space, needs no `ENRICH_VER` bump, and cannot
    /// be served stale from a cached segment.
    pub(super) fn find_path_node(
        &self,
        node_id: &str,
        hex: &str,
        root: &Path,
    ) -> Result<FindNodeResult> {
        let hex = crate::storage::path_node::validate_hex(node_id, hex)?;
        // Fast path: the file catalogs are in RAM — a binary search over the
        // committed segment table, plus the (small) set of files reindexed this
        // session. Directories are in none of them, and neither is a file
        // created this session before the overlay was rebuilt; those fall
        // through to the shared worktree resolver, which every backend uses.
        let hits = self.indexed_paths_for_hex(&hex);
        match hits.len() {
            0 => crate::storage::path_node::resolve_in_worktree(node_id, &hex, root),
            1 => crate::storage::path_node::file_node(node_id, &hits[0], root),
            // Never guess: the caller may be about to delete it.
            n => Err(anyhow::anyhow!(
                "ambiguous node_id {node_id}: prefix matches {n} paths: {}",
                hits.iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// File paths matching `hex` across the three in-RAM file catalogs:
    /// reindexed this session, committed segments, and non-indexed files the
    /// overlay tracks. Directories are not in any of them.
    fn indexed_paths_for_hex(&self, hex: &str) -> Vec<PathBuf> {
        let mut hits: Vec<PathBuf> = Vec::new();
        for ds in &self.dirty.added {
            if crate::storage::path_node::path_matches_hex(&ds.source_path, hex) {
                hits.push(ds.source_path.clone());
            }
        }
        hits.extend(self.overlay.seg_paths_for_node_id_prefix(hex));
        for (path, _) in self.overlay.file_entries() {
            if crate::storage::path_node::path_matches_hex(path, hex) {
                hits.push(path.clone());
            }
        }
        hits.sort();
        hits.dedup();
        hits
    }

    /// Resolve a committed node (one that existed at index time) by its segment
    /// prefix + ordinal. Returns `None` when no committed segment/row matches
    /// (caller falls through to the dirty path); `Some(inner)` when the committed
    /// path applies — `inner` is the resolved [`FindNodeResult`], or the
    /// dirty-segment fallback when the committed row's bytes are stale (BUG-012).
    /// Build the [`FindNodeResult`] for a committed node located at `seg_idx` /
    /// `local_row`. When a dirty segment exists for the file, prefers its live
    /// byte positions via a name + `fql_kind` proximity lookup; if the committed
    /// row's live row is gone, falls back to the dirty segment by ordinal, or
    /// reports `None` rather than a phantom range (BUG-012).
    fn build_committed_node_result(
        &self,
        node_id: &str,
        ordinal: u32,
        root: &Path,
        seg_idx: usize,
        local_row: u32,
    ) -> Option<FindNodeResult> {
        let seg = self.segments.get(seg_idx)?;
        let seg_meta = self.overlay.segments().get(seg_idx)?;

        let name_str = seg.name_of(local_row);
        let fql_kind_str = seg.fql_kind_of(local_row);
        let committed_line = seg.line_of(local_row);

        let dirty_for_path = self
            .dirty
            .added
            .iter()
            .find(|ds| ds.source_path == seg_meta.source_path);

        let live_lookup: Option<(&SegmentReader, u32)> = dirty_for_path.and_then(|ds| {
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

        // No live row matched this committed node by name. When the file was
        // edited this session, the committed line/byte_end are stale: the node may
        // have been deleted or relocated, leaving a committed line past EOF that
        // yields an inverted span the mutation path rejects (BUG-012). Resolve by
        // ordinal in the dirty segment instead; if the node is gone, report
        // not-found rather than a phantom range.
        let (data_seg, data_row): (&SegmentReader, u32) = match live_lookup {
            Some(hit) => hit,
            None if dirty_for_path.is_some() => {
                return self.find_node_in_dirty(node_id, ordinal, root);
            }
            None => (&**seg, local_row),
        };

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
                content_end_line_in_bytes(&file_bytes, byte_end)
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
        Some(FindNodeResult {
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
        })
    }

    pub(super) fn find_node_id_at_line_impl(&self, rel_path: &str, line: usize) -> Option<String> {
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

    /// The innermost row of `rel_path` whose byte span contains `byte`.
    ///
    /// Innermost = smallest span, so a field inside a struct wins over the struct
    /// that encloses it. Pure byte arithmetic — no file is read.
    pub(super) fn find_node_id_at_byte_impl(&self, rel_path: &str, byte: usize) -> Option<String> {
        let pick =
            |reader: &crate::storage::columnar::segment_reader::SegmentReader| -> Option<u32> {
                let mut best: Option<(u32, usize)> = None;
                for r in 0..reader.row_count {
                    if reader.ordinal_of(r).is_none() {
                        continue;
                    }
                    let start = reader.byte_start_of(r) as usize;
                    let end = reader.byte_end_of(r) as usize;
                    if end == 0 || byte < start || byte >= end {
                        continue;
                    }
                    let span = end - start;
                    if best.is_none_or(|(_, best_span)| span < best_span) {
                        best = Some((r, span));
                    }
                }
                best.map(|(r, _)| r)
            };

        // Dirty overlay (post-mutation segments) takes priority over committed,
        // exactly as the line lookup does — reads and writes must agree.
        if !self.dirty.is_empty() {
            for ds in &self.dirty.added {
                if ds.source_path.to_str() == Some(rel_path) {
                    let row = pick(&ds.reader)?;
                    let ord = ds.reader.ordinal_of(row)?;
                    return Some(crate::node_id::make_node_id(rel_path, ord));
                }
            }
        }
        let seg_idx = self
            .overlay
            .segments()
            .iter()
            .position(|s| s.source_path.to_str() == Some(rel_path))?;
        let seg = self.segments.get(seg_idx)?;
        let seg_meta = self.overlay.segments().get(seg_idx)?;
        let row = pick(seg)?;
        Some(seg_meta.node_id(seg.ordinal_of(row)?))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "Single linear resolver: build the newline index, fold the chosen segment's rows, pick the innermost-containing node per line — splitting scatters tightly-coupled state"
    )]
    pub(super) fn innermost_nodes_for_lines_impl(
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
                        // Surface block members under the shared block handle, matching
                        // FIND/outline: a member tagged with block_ord/block_off resolves to
                        // `{seg}.{block_ord}` with a block-relative offset.
                        let (surf_id, surf_start) = match (
                            reader.extra_field_str("block_ord", r),
                            reader.extra_field_str("block_off", r),
                        ) {
                            (Some(bord), Some(boff)) => (
                                crate::node_id::block_node_id(&id, bord),
                                node_start.saturating_sub(
                                    boff.parse::<usize>().unwrap_or(1).saturating_sub(1),
                                ),
                            ),
                            _ => (id, node_start),
                        };
                        out[idx] = Some((surf_id, surf_start));
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

/// 1-based end line of a node's last content byte, computed from raw file bytes.
/// Trims trailing newline bytes exactly like [`content_end_line`]: tree-sitter folds
/// the terminating newline — and, for a Markdown block, the following blank line —
/// into a node's byte range, which would otherwise push `end_line` past the last
/// content line and make a whole-node `CHANGE NODE` swallow the separator blank
/// (merging the node with the next block). A no-op for code, whose `byte_end` sits
/// on a closing token rather than a newline.
#[allow(clippy::naive_bytecount)]
fn content_end_line_in_bytes(bytes: &[u8], byte_end: usize) -> usize {
    let mut end = byte_end.min(bytes.len());
    while end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
    }
    bytes[..end].iter().filter(|&&b| b == b'\n').count() + 1
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

    #[test]
    fn content_end_line_in_bytes_trims_trailing_blank() {
        use super::content_end_line_in_bytes;
        // "row\n\nnext\n": a Markdown-style block whose tree-sitter range folds in
        // the terminating newline AND the following blank line.
        let bytes = b"row\n\nnext\n";
        // byte_end past the blank line's '\n' must still report the content line
        // (line 1), not the blank (line 2) — this is the whole-node merge bug.
        assert_eq!(content_end_line_in_bytes(bytes, 5), 1);
        // No trailing newline: ends on the content line.
        assert_eq!(content_end_line_in_bytes(bytes, 3), 1);
        // Content on line 3.
        assert_eq!(content_end_line_in_bytes(bytes, 9), 3);
    }
}
