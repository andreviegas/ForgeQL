//! [`OverlayBuilder`] — assembles and persists a workspace overlay file.
//!
//! An overlay merges all per-file segments for a given commit SHA into a
//! single queryable index.  It is written once per commit (content-addressed
//! by the commit SHA) and then shared across all sessions on that commit via
//! `Overlay::open`.
//!
//! The builder is constructed with:
//! - the segments base directory (`<bare-repo>/forgeql/segments`)
//! - the provider ID (e.g. `"git-sha1"`)
//! - the worktree root (used to compute relative source paths)
//! - a `segment_map: HashMap<PathBuf, Vec<u8>>` — absolute source path →
//!   raw content-ID bytes — produced by `ShadowWriteResult::segment_map`
//!
//! The overlay file is written atomically (temp-file + rename) so a crash
//! mid-write leaves either the old or the new file, never a partial one.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use bytemuck::cast_slice;
use fst::{IntoStreamer as _, Map as FstMap, MapBuilder, Streamer as _};
use ignore::WalkBuilder;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use tracing::{debug, info};

use super::bytes_to_hex;
use super::overlay::{EnrichEntry, RowPtr, SegmentMeta};
use super::overlay_writer;
use super::segment_builder::{POSTING_ENRICHMENT_FIELDS, overlay_budget};
use super::segment_reader::SegmentReader;

/// Builds a workspace overlay from a set of per-file segments.
pub struct OverlayBuilder {
    provider_id: String,
    segments_dir: PathBuf,
    worktree_root: PathBuf,
    /// Absolute source path → raw content-ID bytes.
    segment_map: HashMap<PathBuf, Vec<u8>>,
}

impl OverlayBuilder {
    /// Create a builder.
    ///
    /// - `provider_id`: e.g. `"git-sha1"`.
    /// - `segments_dir`: `<bare-repo>/forgeql/segments` (provider subdir added inside).
    /// - `worktree_root`: worktree checkout root (for computing relative paths).
    /// - `segment_map`: absolute source path → raw content-ID bytes from
    ///   [`ShadowWriteResult`].
    ///
    /// [`ShadowWriteResult`]: super::shadow_writer::ShadowWriteResult
    #[must_use]
    pub fn new(
        provider_id: &str,
        segments_dir: PathBuf,
        worktree_root: PathBuf,
        segment_map: HashMap<PathBuf, Vec<u8>>,
    ) -> Self {
        Self {
            provider_id: provider_id.to_owned(),
            segments_dir,
            worktree_root,
            segment_map,
        }
    }

    /// Build the overlay and write it atomically to `overlay_path`.
    ///
    /// An empty segment map writes nothing and returns `Ok` — there is no
    /// overlay to build. A non-empty map must open completely.
    ///
    /// # Errors
    /// Returns `Err` when any segment the map names is missing or unreadable
    /// — an overlay built from what remains would silently drop those files
    /// from every answer on this commit — and if writing or renaming the
    /// overlay file fails fatally.
    pub fn build_and_persist(&self, overlay_path: &Path) -> Result<()> {
        let t_total = std::time::Instant::now();

        // 1. Open segments (parallel mmap I/O).
        let mut segs = self.step1_open_segments()?;

        // 2. Sort by source_path for deterministic, path-ordered global row IDs.
        //    After this sort, all rows from "arch/" occupy a contiguous range,
        //    all rows from "drivers/" occupy the next range, and so on.
        //    This invariant is load-bearing for Phases 3–6 (path prefix → row
        //    range lookup).  Do NOT change the sort key without bumping
        //    SCHEMA_VERSION and updating the path_fst builder.
        let t_step = std::time::Instant::now();
        segs.sort_by(|a, b| a.0.cmp(&b.0));
        info!(
            ms = t_step.elapsed().as_millis(),
            mem = %crate::mem::snapshot(), "TIMING step2: sort segments"
        );

        if segs.is_empty() {
            debug!("overlay: no segments found — skipping overlay build");
            return Ok(());
        }

        // 2.5. Workspace files without a symbol segment.
        let file_only = self.step25_collect_file_only(&segs);

        // 3+4. Row offsets and global row table.
        let (row_offsets, total_rows, global_row_table) = Self::step34_build_row_index(&segs)?;

        // 4.5. Per-segment canonical row sets (dedup by name_id + fql_kind_id + line).
        let seg_dedup = Self::step45_dedup_segments(&segs);

        // 5. Merged kind postings.
        let kind_postings = Self::step5_build_kind_postings(&segs, &row_offsets, &seg_dedup)?;

        // 5.5 ∥ 6 ∥ 6a. Enrichment bitmaps, the name merge, and the trigram
        //    pass read the same immutable inputs and write disjoint outputs,
        //    so they run in parallel. The name merge still accumulates its
        //    intermediate map, so the joined peak is the overlap of the
        //    concurrent steps rather than the largest single one.
        let (enrich_res, (name_res, trigram_res)) = rayon::join(
            || Self::step55_build_enrich_bitmaps(&segs, &row_offsets, &seg_dedup),
            || {
                rayon::join(
                    || Self::step6_build_name_fst(&segs, &row_offsets),
                    || Self::step6a_build_trigrams(&segs, &row_offsets),
                )
            },
        );
        let enrich_bitmaps_bytes = enrich_res?;
        let (name_fst_bytes, name_postings_bytes) = name_res?;
        let name_trigram_postings = trigram_res?;

        // 7. Segment metadata list (source segments only — file-only entries
        //    go into the separate `file_entries` blob, not segment_metas).
        let segment_metas: Vec<SegmentMeta> = segs
            .iter()
            .enumerate()
            .map(|(seg_idx, (rel_path, hex, reader))| SegmentMeta {
                hex_content_id: hex.clone(),
                source_path: rel_path.clone(),
                row_count: reader.row_count,
                dedup_row_count: seg_dedup[seg_idx].1,
                sha256: [0u8; 32], // not used by write_v3; computed at read time
                prefix_len: 0,     // not used by write_v3; computed at read time
            })
            .collect();

        // 7.5. Cached file sizes per source segment.
        let index_files_u32 = self.step75_build_index_files(&segs);
        let index_files_bytes: &[u8] = cast_slice(&index_files_u32);

        // 7.6. File-only entries blob.
        let file_entries_bytes = self.step76_build_file_entries(&file_only);

        // 6.5. Usages-count FST (BUG-006 U3): name → total usage-site count.
        let usages_count_fst_bytes = Self::step65_build_usages_count_fst(&segs)?;

        // 8. Atomic overlay write.
        Self::step8_write_overlay(
            overlay_path,
            &overlay_writer::WriteV3Params {
                generation: 1,
                global_row_table: &global_row_table,
                kind_postings: &kind_postings,
                trigram_postings: &name_trigram_postings,
                name_fst_bytes: &name_fst_bytes,
                name_postings_bytes: &name_postings_bytes,
                segment_metas: &segment_metas,
                index_files_bytes,
                enrich_bitmaps_bytes: &enrich_bitmaps_bytes,
                file_entries_bytes: &file_entries_bytes,
                usages_count_fst_bytes: &usages_count_fst_bytes,
            },
        )?;

        info!(
            ms = t_total.elapsed().as_millis(),
            path = %overlay_path.display(),
            segments = segs.len(),
            file_only = file_only.len(),
            rows = total_rows,
            mem = %crate::mem::snapshot(), "TIMING total: build_and_persist",
        );

        Ok(())
    }

    /// Build an `OverlayBuilder` for a post-commit merge of the persistent
    /// overlay and the session's dirty overlay.
    ///
    /// After `promote_segment` moves all staging segments to the bare repo,
    /// this method assembles the complete `segment_map` needed by
    /// `build_and_persist`:
    ///
    /// - All persistent `SegmentMeta` entries whose `source_path` is **not**
    ///   shadowed by `dirty` (i.e. not in `dirty.removed_paths`).
    /// - All newly promoted dirty segments from `dirty.added`.
    ///
    /// Both sets are re-opened fresh from `ctx.segment_path_for(path, hex)` (the
    /// canonical bare-repo location after promotion).  The `source_path` on
    /// each `SegmentMeta` / `DirtySegment` is already workspace-relative, so
    /// we reconstruct the `abs_path` key as `worktree_root.join(rel_path)`,
    /// which `build_and_persist` then strips back to a relative path.
    ///
    /// Returns `None` when no segments survive (empty repo or all removed).
    #[must_use]
    pub fn from_merge(
        base_overlay: &super::overlay::Overlay,
        dirty: &super::dirty_overlay::DirtyOverlay,
        ctx: &super::build_context::ColumnarBuildContext,
        worktree_root: &std::path::Path,
    ) -> Self {
        let mut segment_map = std::collections::HashMap::new();

        // Base segments that are not shadowed by the dirty overlay.
        for meta in base_overlay.segments() {
            if dirty.shadows(&meta.source_path) {
                continue;
            }
            let abs_path = worktree_root.join(&meta.source_path);
            let hex_bytes = hex_to_bytes(&meta.hex_content_id);
            let _ = segment_map.insert(abs_path, hex_bytes);
        }

        // Newly promoted dirty segments.
        for ds in &dirty.added {
            let hex = ds.reader.content_id_hex();
            let abs_path = worktree_root.join(&ds.source_path);
            let hex_bytes = hex_to_bytes(&hex);
            let _ = segment_map.insert(abs_path, hex_bytes);
        }

        Self {
            provider_id: ctx.provider_id.clone(),
            segments_dir: ctx.segments_dir.clone(),
            worktree_root: worktree_root.to_path_buf(),
            segment_map,
        }
    }

    /// Builder for compacting a chained commit into a full overlay: the
    /// master overlay's unshadowed segments plus the chain manifest's
    /// entries, all addressed by content ID in the shared store. The same
    /// merge `from_merge` builds from a live session, derived from the
    /// persisted artefacts instead.
    #[must_use]
    pub fn from_chain(
        base_overlay: &super::overlay::Overlay,
        manifest: &super::chain_manifest::ChainManifest,
        ctx: &super::build_context::ColumnarBuildContext,
        worktree_root: &std::path::Path,
    ) -> Self {
        let removed: std::collections::HashSet<&std::path::Path> = manifest
            .removed_paths
            .iter()
            .map(std::path::PathBuf::as_path)
            .collect();
        let mut segment_map = std::collections::HashMap::new();
        for meta in base_overlay.segments() {
            if removed.contains(meta.source_path.as_path()) {
                continue;
            }
            let _ = segment_map.insert(
                worktree_root.join(&meta.source_path),
                hex_to_bytes(&meta.hex_content_id),
            );
        }
        for entry in &manifest.entries {
            let _ = segment_map.insert(
                worktree_root.join(&entry.source_path),
                hex_to_bytes(&entry.hex_content_id),
            );
        }
        Self {
            provider_id: ctx.provider_id.clone(),
            segments_dir: ctx.segments_dir.clone(),
            worktree_root: worktree_root.to_path_buf(),
            segment_map,
        }
    }
    // ─────────────────────────────────────────────────────────────────────────
    // Private step implementations extracted from `build_and_persist`.
    // ─────────────────────────────────────────────────────────────────────────

    // ── Step 1 ───────────────────────────────────────────────────────────────

    /// Open every segment the builder's map names, or refuse.
    ///
    /// A segment that is missing or unreadable is an error, never a skip: the
    /// overlay built from what remains would be self-consistent and silently
    /// smaller, and every session on that commit would then drop the file's
    /// rows from every answer. The routes that reach this — a COMMIT merging
    /// the base overlay with dirty segments, a rebuild from an inline segment
    /// map — hold no other copy of the rows, so there is nothing to repair
    /// from here; the refusal names the store directory and the first ten
    /// affected files with their causes, and the caller (a COMMIT, an open)
    /// fails with it.
    fn step1_open_segments(&self) -> Result<Vec<(PathBuf, String, SegmentReader)>> {
        let t_step = std::time::Instant::now();
        let provider_ver_dir =
            self.segments_dir
                .join(format!("{}-v{}", &self.provider_id, super::ENRICH_VER));
        let opened: Vec<_> = self
            .segment_map
            .par_iter()
            .map(|(abs_path, content_id)| {
                let hex = bytes_to_hex(content_id);
                let rel_path =
                    super::segment_source_rel(abs_path, &self.worktree_root).to_path_buf();
                let seg_path = provider_ver_dir.join(super::segment_rel_path(&rel_path, &hex));
                if !seg_path.exists() {
                    return Err((rel_path, "segment file does not exist".to_owned()));
                }
                match SegmentReader::open(&seg_path) {
                    Ok(reader) => Ok((rel_path, hex, reader)),
                    Err(e) => Err((rel_path, format!("{e:#}"))),
                }
            })
            .collect();

        let total = opened.len();
        let mut segs = Vec::with_capacity(total);
        let mut unreadable: Vec<(PathBuf, String)> = Vec::new();
        for entry in opened {
            match entry {
                Ok(seg) => segs.push(seg),
                Err(gap) => unreadable.push(gap),
            }
        }
        if !unreadable.is_empty() {
            use std::fmt::Write as _;
            unreadable.sort();
            // A wholesale loss (the whole provider-version directory reclaimed)
            // makes this O(all files) — cap the listing so the refusal stays a
            // message rather than a file dump.
            let cap = 10;
            let mut listed = unreadable
                .iter()
                .take(cap)
                .map(|(rel, cause)| format!("{} ({cause})", rel.display()))
                .collect::<Vec<_>>()
                .join("; ");
            if unreadable.len() > cap {
                let _ = write!(listed, "; … and {} more", unreadable.len() - cap);
            }
            anyhow::bail!(
                "overlay build incomplete: {} of {total} segments under {} are missing or \
                 unreadable — {listed}. Building without them would silently drop those files \
                 from every answer on this commit; restore the segment files or re-index this \
                 source",
                unreadable.len(),
                provider_ver_dir.display(),
            );
        }
        info!(
            ms = t_step.elapsed().as_millis(),
            n = segs.len(),
            mem = %crate::mem::snapshot(), "TIMING step1: open segments (parallel)",
        );
        Ok(segs)
    }

    // ── Step 2.5 ─────────────────────────────────────────────────────────────

    fn step25_collect_file_only(
        &self,
        segs: &[(PathBuf, String, SegmentReader)],
    ) -> Vec<(PathBuf, String)> {
        let t_step = std::time::Instant::now();
        let indexed: HashSet<PathBuf> = segs.iter().map(|(rel, _, _)| rel.clone()).collect();
        let file_only = collect_file_only(&self.worktree_root, &indexed);
        info!(
            ms = t_step.elapsed().as_millis(),
            n = file_only.len(),
            mem = %crate::mem::snapshot(), "TIMING step2.5: collect file-only entries",
        );
        file_only
    }

    // ── Steps 3 + 4 ──────────────────────────────────────────────────────────

    fn step34_build_row_index(
        segs: &[(PathBuf, String, SegmentReader)],
    ) -> Result<(Vec<u32>, u32, Vec<RowPtr>)> {
        let t_step = std::time::Instant::now();
        let mut row_offsets: Vec<u32> = Vec::with_capacity(segs.len());
        let mut total_rows: u32 = 0;
        for (_, _, reader) in segs {
            row_offsets.push(total_rows);
            total_rows = total_rows
                .checked_add(reader.row_count)
                .context("overflow: too many rows for u32 row count")?;
        }
        let mut global_row_table: Vec<RowPtr> = Vec::with_capacity(total_rows as usize);
        for (seg_idx, (_, _, reader)) in segs.iter().enumerate() {
            for local_row in 0..reader.row_count {
                global_row_table.push(RowPtr {
                    segment_idx: u32::try_from(seg_idx).unwrap_or(u32::MAX),
                    local_row_idx: local_row,
                });
            }
        }
        info!(
            ms = t_step.elapsed().as_millis(),
            rows = total_rows,
            mem = %crate::mem::snapshot(), "TIMING step3-4: row offsets + global_row_table",
        );
        Ok((row_offsets, total_rows, global_row_table))
    }

    // ── Step 4.5 ─────────────────────────────────────────────────────────────

    fn step45_dedup_segments(
        segs: &[(PathBuf, String, SegmentReader)],
    ) -> Vec<(RoaringBitmap, u32)> {
        let t_step = std::time::Instant::now();
        let seg_dedup: Vec<(RoaringBitmap, u32)> = segs
            .par_iter()
            .map(|(_, _, reader)| {
                let mut seen: HashSet<(u32, u32, u32)> =
                    HashSet::with_capacity(reader.row_count as usize);
                let mut canonical = RoaringBitmap::new();
                for local_row in 0..reader.row_count {
                    if seen.insert((
                        reader.name_id_of(local_row),
                        reader.fql_kind_id_of(local_row),
                        reader.line_of(local_row),
                    )) {
                        let _ = canonical.insert(local_row);
                    }
                }
                let cnt = u32::try_from(canonical.len()).unwrap_or(u32::MAX);
                (canonical, cnt)
            })
            .collect();
        info!(
            ms = t_step.elapsed().as_millis(),
            segs = segs.len(),
            mem = %crate::mem::snapshot(), "TIMING step4.5: per-segment dedup canonical row sets",
        );
        seg_dedup
    }

    // ── Step 5 ───────────────────────────────────────────────────────────────

    fn step5_build_kind_postings(
        segs: &[(PathBuf, String, SegmentReader)],
        row_offsets: &[u32],
        seg_dedup: &[(RoaringBitmap, u32)],
    ) -> Result<HashMap<String, Vec<u8>>> {
        let t_step = std::time::Instant::now();
        let mut kind_merged: HashMap<String, RoaringBitmap> = HashMap::new();
        for (seg_idx, (seg_path, _, reader)) in segs.iter().enumerate() {
            let row_offset = row_offsets[seg_idx];
            let canonical_bm = &seg_dedup[seg_idx].0;
            for entry in reader.kind_postings() {
                let (kind_id, local_bm) = entry
                    .with_context(|| format!("reading kind postings of {}", seg_path.display()))?;
                let kind_str = reader.string_of_id(kind_id);
                if kind_str.is_empty() {
                    continue;
                }
                let merged = kind_merged.entry(kind_str.to_owned()).or_default();
                // Intersect with canonical_bm to skip intra-segment duplicates.
                for local_row in local_bm & canonical_bm {
                    let _ = merged.insert(local_row + row_offset);
                }
            }
        }
        let mut kind_postings: HashMap<String, Vec<u8>> = HashMap::with_capacity(kind_merged.len());
        for (kind_str, bitmap) in &kind_merged {
            let mut bytes = Vec::new();
            bitmap
                .serialize_into(&mut bytes)
                .with_context(|| format!("serialising kind bitmap for '{kind_str}'"))?;
            let _ = kind_postings.insert(kind_str.clone(), bytes);
        }
        info!(
            ms = t_step.elapsed().as_millis(),
            kinds = kind_postings.len(),
            mem = %crate::mem::snapshot(), "TIMING step5: kind postings merge",
        );
        Ok(kind_postings)
    }

    // ── Step 5.5 ─────────────────────────────────────────────────────────────

    fn step55_build_enrich_bitmaps(
        segs: &[(PathBuf, String, SegmentReader)],
        row_offsets: &[u32],
        seg_dedup: &[(RoaringBitmap, u32)],
    ) -> Result<Vec<u8>> {
        let t_step = std::time::Instant::now();
        let mut enrich_raw: HashMap<String, RoaringBitmap> = HashMap::new();
        let mut field_seen: HashMap<String, HashSet<String>> = HashMap::new();
        let mut pruned_fields: HashSet<String> = HashSet::new();

        // Category 1: boolean flags + string enums via field_postings.
        Self::collect_posting_enrichment(
            segs,
            row_offsets,
            seg_dedup,
            &mut enrich_raw,
            &mut field_seen,
            &mut pruned_fields,
        )?;
        // Category 2: numeric fields not in POSTING_ENRICHMENT_FIELDS.
        Self::collect_numeric_enrichment(
            segs,
            row_offsets,
            seg_dedup,
            &mut enrich_raw,
            &mut field_seen,
            &mut pruned_fields,
        );

        let enrich_bitmaps_bytes = Self::serialize_enrich_bitmaps(&enrich_raw)?;
        info!(
            ms = t_step.elapsed().as_millis(),
            entries = enrich_raw.len(),
            pruned = pruned_fields.len(),
            bytes = enrich_bitmaps_bytes.len(),
            mem = %crate::mem::snapshot(), "TIMING step5.5: enrichment bitmaps",
        );
        Ok(enrich_bitmaps_bytes)
    }

    /// Category 1 of step 5.5: boolean flags + string enums sourced from each
    /// segment's `field_postings`.  A field exceeding its `overlay_budget`
    /// distinct values is pruned entirely (its already-collected keys are
    /// dropped too), so a key set is never a partial account of a field.
    fn collect_posting_enrichment(
        segs: &[(PathBuf, String, SegmentReader)],
        row_offsets: &[u32],
        seg_dedup: &[(RoaringBitmap, u32)],
        enrich_raw: &mut HashMap<String, RoaringBitmap>,
        field_seen: &mut HashMap<String, HashSet<String>>,
        pruned_fields: &mut HashSet<String>,
    ) -> Result<()> {
        for (seg_idx, (seg_path, _, reader)) in segs.iter().enumerate() {
            let row_offset = row_offsets[seg_idx];
            let canonical_bm = &seg_dedup[seg_idx].0;
            for field_name in reader.posted_fields() {
                if pruned_fields.contains(field_name) {
                    continue;
                }
                // Decoded from the segment's mmap one bitmap at a time; the
                // walk never holds a segment's postings whole.
                for entry in reader.field_postings(field_name) {
                    let (value_id, local_bm) = entry.with_context(|| {
                        format!("reading {field_name} postings of {}", seg_path.display())
                    })?;
                    let value_str = reader.string_of_id(value_id);
                    if value_str.is_empty() {
                        continue;
                    }
                    let seen = field_seen.entry(field_name.to_owned()).or_default();
                    if !seen.contains(value_str) {
                        if seen.len() >= overlay_budget(field_name) {
                            let _ = pruned_fields.insert(field_name.to_owned());
                            // Erase everything collected so far for this field.
                            let pfx = format!("{field_name}=");
                            enrich_raw.retain(|k, _| !k.starts_with(&pfx));
                            break;
                        }
                        let _ = seen.insert(value_str.to_owned());
                    }
                    if pruned_fields.contains(field_name) {
                        continue;
                    }
                    let key = format!("{field_name}={value_str}");
                    let canonical_matching: RoaringBitmap = local_bm & canonical_bm;
                    if !canonical_matching.is_empty() {
                        let bm = enrich_raw.entry(key).or_default();
                        for local_row in &canonical_matching {
                            let _ = bm.insert(local_row + row_offset);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Category 2 of step 5.5: enrichment fields not covered by the posting
    /// index (`POSTING_ENRICHMENT_FIELDS`).  Same bucket-pruning rule as the
    /// posting pass.
    ///
    /// The walk is column-major and integer-only.  A row's value for a column
    /// is already a `u32` id into that segment's own string table, so the rows
    /// accumulate as `value id -> global rows` and no text is touched while
    /// walking them; only at the end of a column is each DISTINCT id resolved
    /// once and merged into `enrich_raw`.  That is the point of the shape: the
    /// string and `format!` work is `O(segments x distinct values)`, bounded by
    /// `overlay_budget`, instead of `O(rows x fields)`.  Reading a row used to
    /// build a `HashMap<String, String>` of freshly cloned column names and
    /// copied values, and then a third `String` per field to key the map — on a
    /// 30M-row corpus, order 10^8 allocations to produce a 133 MB blob.
    ///
    /// Value ids are per-segment, which is why the accumulator is per-segment:
    /// the same text can carry different ids in two segments, so nothing keyed
    /// on an id may outlive the segment that issued it.
    ///
    /// Column-major does not reorder the pruning.  Within a column the values
    /// are merged in the order the rows first produce them — the order the
    /// row-by-row walk saw them in — and a field's budget is spent only on its
    /// own values, so no field's fate depends on when another field is read.
    fn collect_numeric_enrichment(
        segs: &[(PathBuf, String, SegmentReader)],
        row_offsets: &[u32],
        seg_dedup: &[(RoaringBitmap, u32)],
        enrich_raw: &mut HashMap<String, RoaringBitmap>,
        field_seen: &mut HashMap<String, HashSet<String>>,
        pruned_fields: &mut HashSet<String>,
    ) {
        let posting_field_set: HashSet<&str> = POSTING_ENRICHMENT_FIELDS.iter().copied().collect();
        // Cleared and refilled per column, never reallocated per row.  `order`
        // keeps first-appearance order, which the map itself does not hold.
        let mut by_value: HashMap<u32, RoaringBitmap> = HashMap::new();
        let mut order: Vec<u32> = Vec::new();
        for (seg_idx, (_, _, reader)) in segs.iter().enumerate() {
            let row_offset = row_offsets[seg_idx];
            let canonical_bm = &seg_dedup[seg_idx].0;
            for (field_name, value_ids) in reader.enrichment_columns() {
                if posting_field_set.contains(field_name) || pruned_fields.contains(field_name) {
                    continue;
                }
                by_value.clear();
                order.clear();
                for local_row in canonical_bm {
                    let Some(&value_id) = value_ids.get(local_row as usize) else {
                        continue;
                    };
                    if value_id == u32::MAX {
                        continue;
                    }
                    let rows = by_value.entry(value_id).or_insert_with(|| {
                        order.push(value_id);
                        RoaringBitmap::new()
                    });
                    let _ = rows.insert(local_row + row_offset);
                }
                Self::merge_segment_column(
                    reader,
                    field_name,
                    &order,
                    &by_value,
                    enrich_raw,
                    field_seen,
                    pruned_fields,
                );
            }
        }
    }

    /// Merge one segment's column into `enrich_raw`, resolving each distinct
    /// value id to text exactly once.
    ///
    /// `order` lists the ids in the order the rows first produced them, so the
    /// budget is spent on the same values, in the same order, that a row-by-row
    /// walk would have spent it on.  A field that runs past `overlay_budget`
    /// distinct values is pruned here exactly as the posting pass prunes it:
    /// recorded in `pruned_fields`, its already-collected keys dropped, and the
    /// rest of its values abandoned.
    fn merge_segment_column(
        reader: &SegmentReader,
        field_name: &str,
        order: &[u32],
        by_value: &HashMap<u32, RoaringBitmap>,
        enrich_raw: &mut HashMap<String, RoaringBitmap>,
        field_seen: &mut HashMap<String, HashSet<String>>,
        pruned_fields: &mut HashSet<String>,
    ) {
        let budget = overlay_budget(field_name);
        let seen = field_seen.entry(field_name.to_owned()).or_default();
        for &value_id in order {
            let value_str = reader.string_of_id(value_id);
            if value_str.is_empty() {
                continue;
            }
            if !seen.contains(value_str) {
                if seen.len() >= budget {
                    let _ = pruned_fields.insert(field_name.to_owned());
                    let pfx = format!("{field_name}=");
                    enrich_raw.retain(|k, _| !k.starts_with(&pfx));
                    return;
                }
                let _ = seen.insert(value_str.to_owned());
            }
            if let Some(rows) = by_value.get(&value_id) {
                *enrich_raw
                    .entry(format!("{field_name}={value_str}"))
                    .or_default() |= rows;
            }
        }
    }

    /// Serialise the collected enrichment bitmaps into the `enrich_bitmaps` blob:
    /// a sorted (entry-table, key-bytes, bitmap-bytes) layout.
    fn serialize_enrich_bitmaps(enrich_raw: &HashMap<String, RoaringBitmap>) -> Result<Vec<u8>> {
        let mut sorted_enrich: Vec<(&String, &RoaringBitmap)> = enrich_raw.iter().collect();
        sorted_enrich.sort_by_key(|(k, _)| k.as_str());
        let mut enrich_key_bytes: Vec<u8> = Vec::new();
        let mut enrich_bitmap_data: Vec<u8> = Vec::new();
        let mut enrich_entries: Vec<EnrichEntry> = Vec::new();
        for (key, bitmap) in &sorted_enrich {
            let mut bm_bytes = Vec::new();
            bitmap
                .serialize_into(&mut bm_bytes)
                .with_context(|| format!("serialising enrich bitmap '{key}'"))?;
            enrich_entries.push(EnrichEntry {
                key_offset: u32::try_from(enrich_key_bytes.len()).unwrap_or(u32::MAX),
                key_len: u16::try_from(key.len()).unwrap_or(u16::MAX),
                _pad: 0,
                bitmap_offset: u32::try_from(enrich_bitmap_data.len()).unwrap_or(u32::MAX),
                bitmap_len: u32::try_from(bm_bytes.len()).unwrap_or(u32::MAX),
            });
            enrich_key_bytes.extend_from_slice(key.as_bytes());
            enrich_bitmap_data.extend_from_slice(&bm_bytes);
        }
        let entry_count_le = u32::try_from(enrich_entries.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes();
        let key_data_len_le = u32::try_from(enrich_key_bytes.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes();
        let mut enrich_bitmaps_bytes: Vec<u8> = Vec::with_capacity(
            8 + enrich_entries.len() * std::mem::size_of::<EnrichEntry>()
                + enrich_key_bytes.len()
                + enrich_bitmap_data.len(),
        );
        enrich_bitmaps_bytes.extend_from_slice(&entry_count_le);
        enrich_bitmaps_bytes.extend_from_slice(&key_data_len_le);
        enrich_bitmaps_bytes.extend_from_slice(cast_slice(enrich_entries.as_slice()));
        enrich_bitmaps_bytes.extend_from_slice(&enrich_key_bytes);
        enrich_bitmaps_bytes.extend_from_slice(&enrich_bitmap_data);
        Ok(enrich_bitmaps_bytes)
    }

    // ── Step 6 ───────────────────────────────────────────────────────────────

    fn step6_build_name_fst(
        segs: &[(PathBuf, String, SegmentReader)],
        row_offsets: &[u32],
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let t_step = std::time::Instant::now();
        // `zip` truncates to the shorter side, so a short `row_offsets` would
        // drop the tail segments' names from the merge instead of failing.
        ensure!(
            segs.len() == row_offsets.len(),
            "overlay name merge got {} segments but {} row offsets",
            segs.len(),
            row_offsets.len(),
        );
        // Resolve per segment once instead of once per (segment, shard): the
        // postings blob is a hash probe, and the first-byte mask lets a shard
        // skip outright every segment that cannot hold one of its keys.
        let seg_inputs: Vec<(&[u8], u32, ShardMask)> = segs
            .par_iter()
            .zip(row_offsets)
            .map(|((_, _, reader), &row_offset)| {
                (
                    reader.name_postings_bytes(),
                    row_offset,
                    first_byte_mask(&reader.name_fst),
                )
            })
            .collect();
        let prep_ms = t_step.elapsed().as_millis();
        let t_merge = std::time::Instant::now();
        let shard_cpu_micros = std::sync::atomic::AtomicU64::new(0);
        let shards: Vec<BTreeMap<Vec<u8>, Vec<u32>>> = name_shard_bounds()
            .par_iter()
            .map(|&(byte, lo, hi)| {
                let t_shard = std::time::Instant::now();
                let mut merged: BTreeMap<Vec<u8>, Vec<u32>> = BTreeMap::new();
                for ((_, _, reader), &(name_postings_raw, row_offset, mask)) in
                    segs.iter().zip(&seg_inputs)
                {
                    if !shard_present(&mask, byte) {
                        continue;
                    }
                    let mut range = reader.name_fst.range();
                    if let Some(lo) = lo {
                        range = range.ge(lo);
                    }
                    if let Some(hi) = hi {
                        range = range.lt(hi);
                    }
                    let mut stream = range.into_stream();
                    while let Some((name_bytes, encoded)) = stream.next() {
                        let local_rows = name_postings_slice(encoded, name_postings_raw);
                        let global_rows = local_rows.iter().map(|&r| r + row_offset);
                        // A name recurs in ~4 segments, so most iterations hit an
                        // existing entry; `entry()` would allocate a key Vec for
                        // every one of them, `get_mut` allocates for none.
                        if let Some(existing) = merged.get_mut(name_bytes) {
                            existing.extend(global_rows);
                        } else {
                            let _ = merged.insert(name_bytes.to_vec(), global_rows.collect());
                        }
                    }
                }
                let _ = shard_cpu_micros.fetch_add(
                    u64::try_from(t_shard.elapsed().as_micros()).unwrap_or(u64::MAX),
                    std::sync::atomic::Ordering::Relaxed,
                );
                merged
            })
            .collect();
        let merge_ms = t_merge.elapsed().as_millis();
        let merge_cpu_ms = shard_cpu_micros.load(std::sync::atomic::Ordering::Relaxed) / 1000;
        let merged_names_len: usize = shards.iter().map(BTreeMap::len).sum();
        let mut name_postings_bytes: Vec<u8> = Vec::new();
        let mut fst_builder = MapBuilder::memory();
        // `serial_ms` is dominated by `MapBuilder::insert`: timing the three
        // sub-phases separately measured sort+dedup at 25 ms and the postings
        // write at 48 ms against 3,755 ms of insert, so this is ~98% FST build.
        // It cannot be parallelised — MapBuilder requires ascending keys.
        let t_serial = std::time::Instant::now();
        for shard in shards {
            for (name_bytes, mut rows) in shard {
                rows.sort_unstable();
                rows.dedup();
                let byte_offset = name_postings_bytes.len();
                let count = rows.len();
                for r in &rows {
                    name_postings_bytes.extend_from_slice(&r.to_le_bytes());
                }
                let packed = ((byte_offset as u64) << 32) | (count as u64);
                fst_builder
                    .insert(&name_bytes, packed)
                    .context("inserting name into overlay FST")?;
            }
        }
        let serial_ms = t_serial.elapsed().as_millis();
        let t_finalise = std::time::Instant::now();
        let name_fst_bytes = fst_builder.into_inner().context("finalising overlay FST")?;
        info!(
            ms = t_step.elapsed().as_millis(),
            prep_ms,
            merge_ms,
            merge_cpu_ms,
            serial_ms,
            finalise_ms = t_finalise.elapsed().as_millis(),
            unique_names = merged_names_len,
            fst_bytes = name_fst_bytes.len(),
            mem = %crate::mem::snapshot(), "TIMING step6: name FST + postings (parallel)",
        );
        Ok((name_fst_bytes, name_postings_bytes))
    }

    /// Step 6a: the trigram bitmaps, computed per segment in parallel.
    ///
    /// Mirrors `ast::trigram::TrigramIndex` semantics: ASCII lower-case,
    /// trigrams deduplicated per name, ascending global row IDs. Every
    /// segment walks its own (already sorted) name FST once and offsets its
    /// local rows into the global ID space; bitmap union is associative and
    /// commutative, so the per-segment maps fold in parallel and the merged
    /// content is identical to computing it from the merged name list.
    fn step6a_build_trigrams(
        segs: &[(PathBuf, String, SegmentReader)],
        row_offsets: &[u32],
    ) -> Result<HashMap<[u8; 3], Vec<u8>>> {
        let t_step = std::time::Instant::now();
        let trigram_merged: HashMap<[u8; 3], RoaringBitmap> = segs
            .par_iter()
            .enumerate()
            .map(|(seg_idx, (_, _, reader))| {
                let row_offset = row_offsets[seg_idx];
                let name_postings_raw = reader.name_postings_bytes();
                let mut local: HashMap<[u8; 3], RoaringBitmap> = HashMap::new();
                let mut stream = reader.name_fst.stream();
                while let Some((name_bytes, encoded)) = stream.next() {
                    if name_bytes.len() < 3 {
                        continue;
                    }
                    let rows = decode_name_postings_raw(encoded, name_postings_raw);
                    let mut seen: HashSet<[u8; 3]> = HashSet::new();
                    for w in name_bytes.windows(3) {
                        let t = [
                            w[0].to_ascii_lowercase(),
                            w[1].to_ascii_lowercase(),
                            w[2].to_ascii_lowercase(),
                        ];
                        if seen.insert(t) {
                            let bm = local.entry(t).or_default();
                            for r in &rows {
                                let _ = bm.insert(r + row_offset);
                            }
                        }
                    }
                }
                local
            })
            .reduce(HashMap::new, |mut merged, local| {
                for (t, bm) in local {
                    match merged.entry(t) {
                        std::collections::hash_map::Entry::Occupied(mut e) => {
                            *e.get_mut() |= bm;
                        }
                        std::collections::hash_map::Entry::Vacant(v) => {
                            let _ = v.insert(bm);
                        }
                    }
                }
                merged
            });
        let mut name_trigram_postings: HashMap<[u8; 3], Vec<u8>> =
            HashMap::with_capacity(trigram_merged.len());
        for (trigram, bitmap) in &trigram_merged {
            let mut bytes = Vec::new();
            bitmap
                .serialize_into(&mut bytes)
                .with_context(|| format!("serialising trigram bitmap {trigram:?}"))?;
            let _ = name_trigram_postings.insert(*trigram, bytes);
        }
        info!(
            ms = t_step.elapsed().as_millis(),
            trigrams = name_trigram_postings.len(),
            mem = %crate::mem::snapshot(), "TIMING step6a: trigram bitmaps (parallel)",
        );
        Ok(name_trigram_postings)
    }

    /// Step 6.5 (BUG-006 U3): merge every segment's usage postings into one
    /// FST mapping symbol name → total usage-site count across the workspace.
    ///
    /// Each segment `usages_fst` value encodes `count | (byte_offset << 32)`;
    /// the aggregate needs only the count (low 32 bits), so the postings
    /// bytes are never touched. Returns an empty vec when no segment carries
    /// usage postings (the overlay blob is then zero-length).
    fn step65_build_usages_count_fst(segs: &[(PathBuf, String, SegmentReader)]) -> Result<Vec<u8>> {
        let t_step = std::time::Instant::now();
        let seg_masks: Vec<ShardMask> = segs
            .par_iter()
            .map(|(_, _, reader)| reader.usages_fst.as_ref().map_or([0; 4], first_byte_mask))
            .collect();
        let t_merge = std::time::Instant::now();
        let shard_cpu_micros = std::sync::atomic::AtomicU64::new(0);
        let shards: Vec<BTreeMap<Vec<u8>, u64>> = name_shard_bounds()
            .par_iter()
            .map(|&(byte, lo, hi)| {
                let t_shard = std::time::Instant::now();
                let mut counts: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
                for ((_, _, reader), mask) in segs.iter().zip(&seg_masks) {
                    let Some(fst) = &reader.usages_fst else {
                        continue;
                    };
                    if !shard_present(mask, byte) {
                        continue;
                    }
                    let mut range = fst.range();
                    if let Some(lo) = lo {
                        range = range.ge(lo);
                    }
                    if let Some(hi) = hi {
                        range = range.lt(hi);
                    }
                    let mut stream = range.into_stream();
                    while let Some((name_bytes, encoded)) = stream.next() {
                        *counts.entry(name_bytes.to_vec()).or_default() += encoded & 0xFFFF_FFFF;
                    }
                }
                let _ = shard_cpu_micros.fetch_add(
                    u64::try_from(t_shard.elapsed().as_micros()).unwrap_or(u64::MAX),
                    std::sync::atomic::Ordering::Relaxed,
                );
                counts
            })
            .collect();
        let merge_ms = t_merge.elapsed().as_millis();
        let merge_cpu_ms = shard_cpu_micros.load(std::sync::atomic::Ordering::Relaxed) / 1000;
        let names: usize = shards.iter().map(BTreeMap::len).sum();
        if names == 0 {
            return Ok(Vec::new());
        }
        let t_serial = std::time::Instant::now();
        let mut fst_builder = MapBuilder::memory();
        for shard in &shards {
            for (name, count) in shard {
                fst_builder
                    .insert(name, *count)
                    .context("usages_count_fst insert")?;
            }
        }
        let serial_ms = t_serial.elapsed().as_millis();
        let t_finalise = std::time::Instant::now();
        let bytes = fst_builder
            .into_inner()
            .context("finalising usages_count FST")?;
        info!(
            ms = t_step.elapsed().as_millis(),
            merge_ms,
            merge_cpu_ms,
            serial_ms,
            finalise_ms = t_finalise.elapsed().as_millis(),
            names,
            bytes = bytes.len(),
            mem = %crate::mem::snapshot(), "TIMING step6.5: usages-count FST (parallel)"
        );
        Ok(bytes)
    }

    // ── Steps 7.5 + 7.6 ─────────────────────────────────────────────────────

    fn step75_build_index_files(&self, segs: &[(PathBuf, String, SegmentReader)]) -> Vec<u32> {
        segs.iter()
            .map(|(rel_path, _, _)| {
                let full_path = self.worktree_root.join(rel_path);
                std::fs::metadata(&full_path)
                    .map(|m| u32::try_from(m.len()).unwrap_or(u32::MAX))
                    .unwrap_or(0)
            })
            .collect()
    }

    fn step76_build_file_entries(&self, file_only: &[(PathBuf, String)]) -> Vec<u8> {
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(
            &u32::try_from(file_only.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        for (rel_path, _) in file_only {
            let full_path = self.worktree_root.join(rel_path);
            let size = std::fs::metadata(&full_path)
                .map(|m| u32::try_from(m.len()).unwrap_or(u32::MAX))
                .unwrap_or(0);
            let path_str = rel_path.to_string_lossy();
            let path_bytes = path_str.as_bytes();
            let path_len = u16::try_from(path_bytes.len()).unwrap_or(u16::MAX);
            bytes.extend_from_slice(&size.to_le_bytes());
            bytes.extend_from_slice(&path_len.to_le_bytes());
            bytes.extend_from_slice(path_bytes);
        }
        bytes
    }

    // ── Step 8 ───────────────────────────────────────────────────────────────

    fn step8_write_overlay(
        overlay_path: &Path,
        params: &overlay_writer::WriteV3Params<'_>,
    ) -> Result<()> {
        let t_step = std::time::Instant::now();
        if let Some(parent) = overlay_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating overlay dir {}", parent.display()))?;
        }
        let tmp = tempfile::NamedTempFile::new_in(
            overlay_path.parent().unwrap_or_else(|| Path::new(".")),
        )
        .context("creating temp overlay file")?;
        {
            let mut f = std::io::BufWriter::new(tmp.as_file());
            overlay_writer::write_v3(&mut f, params).context("writing v3 overlay")?;
            f.flush().context("flushing overlay buffer")?;
            tmp.as_file().sync_all().context("fsyncing overlay file")?;
        }
        let _ = tmp
            .persist(overlay_path)
            .with_context(|| format!("persisting overlay to {}", overlay_path.display()))?;
        info!(
            ms = t_step.elapsed().as_millis(),
            mem = %crate::mem::snapshot(), "TIMING step8: write v3 overlay (atomic)",
        );
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Private helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Enumerate every regular workspace file under `worktree_root` that is not
/// already in `indexed` (which contains the relative paths of source files
/// that have a full symbol segment).  Returns `(relative_path, hex_id)` pairs
/// sorted by path, using a deterministic path-derived hex ID.
///
/// Uses the same [`WalkBuilder`] configuration as [`Workspace::files`] so the
/// set of tracked files is consistent with what `FIND files` returns via the
/// filesystem-walk fallback. A derived chain manifest walks with this same
/// function, so the non-indexed files it lists for a commit are the ones a
/// full build of that commit would have listed.
pub(crate) fn collect_file_only(
    worktree_root: &Path,
    indexed: &HashSet<PathBuf>,
) -> Vec<(PathBuf, String)> {
    use std::hash::{Hash as _, Hasher as _};
    let mut entries: Vec<(PathBuf, String)> = WalkBuilder::new(worktree_root)
        .add_custom_ignore_filename(".forgeql-ignore")
        .hidden(false) // include dot-files (matches Workspace::files)
        .git_ignore(true)
        .build()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            if !entry.file_type()?.is_file() {
                return None;
            }
            let rel = entry.path().strip_prefix(worktree_root).ok()?.to_path_buf();
            if indexed.contains(&rel) {
                return None; // already has a symbol segment
            }
            // Derive a stable 32-char hex ID from the relative path.
            // These entries have no .fqsf file on disk; the ID only needs
            // to be unique and never clash with a real content hash.
            let mut h1 = std::collections::hash_map::DefaultHasher::new();
            let mut h2 = std::collections::hash_map::DefaultHasher::new();
            rel.hash(&mut h1);
            // Different seed so h1 ≠ h2 for all inputs.
            0xdead_beef_cafe_u64.hash(&mut h2);
            rel.hash(&mut h2);
            let hex = format!("{:016x}{:016x}", h1.finish(), h2.finish());
            Some((rel, hex))
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// Decode the raw `(offset, count)` pair embedded in a name FST value into
/// a list of row IDs from the postings array.
///
/// This mirrors `decode_name_postings` in `segment_reader.rs`.
fn decode_name_postings_raw(encoded: u64, name_postings: &[u8]) -> Vec<u32> {
    name_postings_slice(encoded, name_postings).to_vec()
}

/// The same decode, borrowed rather than copied.
///
/// The merge calls this once per (name, segment) pair — 3 M times on a
/// mid-sized corpus — so handing back the mapped slice instead of a fresh
/// `Vec` removes one heap allocation per call from the hot loop.
fn name_postings_slice(encoded: u64, name_postings: &[u8]) -> &[u32] {
    let count = usize::try_from(encoded & 0xFFFF_FFFF).unwrap_or(usize::MAX);
    let byte_offset = usize::try_from((encoded >> 32) & 0xFFFF_FFFF).unwrap_or(usize::MAX);
    let end = byte_offset.saturating_add(count.saturating_mul(4));
    if end > name_postings.len() {
        return &[];
    }
    #[expect(clippy::indexing_slicing, reason = "bounds checked above")]
    cast_slice::<u8, u32>(&name_postings[byte_offset..end])
}

/// One shard: its first byte, then the `(lower, upper)` key bounds it owns —
/// either end open.
type NameShard = (u8, Option<[u8; 1]>, Option<[u8; 1]>);

/// Disjoint `(lower, upper)` key bounds partitioning the whole key space by
/// first byte, in ascending order: shard `i` owns every name whose first byte
/// is `i`, and shard 0 additionally owns the empty name.
///
/// This is what lets a name merge run in parallel without costing memory. A
/// per-thread partial map would store every shared name once per thread; a
/// shard stores each name exactly once, because the shards are disjoint. And
/// because they are also in ascending order, concatenating them yields the same
/// strictly ascending key sequence a single sorted map would — which is what
/// `fst::MapBuilder` requires, and why the merged output is byte-identical.
///
/// The price is that each shard re-opens a range stream per segment. That is
/// bounded by segment count rather than row count, and `first_byte_mask` skips
/// the segments a shard has no keys in, which is most of them.
fn name_shard_bounds() -> Vec<NameShard> {
    (0..=u8::MAX)
        .map(|b| {
            let lo = if b == 0 { None } else { Some([b]) };
            (b, lo, b.checked_add(1).map(|hi| [hi]))
        })
        .collect()
}

/// A 256-bit set of the first bytes a segment's keys can start with.
type ShardMask = [u64; 4];

/// Read off the FST root node's outgoing transitions the set of first bytes any
/// key in `map` can start with — for identifier-shaped names that is a few
/// dozen of the 256, so a shard can skip most segments outright instead of
/// opening a range stream that would find nothing. Bit 0 doubles as "shard 0
/// has work", since shard 0 owns the empty key as well as the `\0`-prefixed
/// ones, and an empty key is a final root rather than a transition.
fn first_byte_mask<D: AsRef<[u8]>>(map: &FstMap<D>) -> ShardMask {
    let mut mask: ShardMask = [0; 4];
    let root = map.as_fst().root();
    let mut set = |byte: u8| {
        if let Some(word) = mask.get_mut(usize::from(byte) / 64) {
            *word |= 1_u64 << (u32::from(byte) % 64);
        }
    };
    if root.is_final() {
        set(0);
    }
    for transition in root.transitions() {
        set(transition.inp);
    }
    mask
}

/// Whether `mask` says the segment holds any key in shard `byte`.
fn shard_present(mask: &ShardMask, byte: u8) -> bool {
    mask.get(usize::from(byte) / 64)
        .is_some_and(|word| word & (1_u64 << (u32::from(byte) % 64)) != 0)
}

/// Decode a hex string (e.g. a `hex_content_id`) to raw bytes.
///
/// Used by `from_merge` to convert hex strings back to the raw content-ID
/// bytes that `build_and_persist` expects in `segment_map`.
fn hex_to_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests;
