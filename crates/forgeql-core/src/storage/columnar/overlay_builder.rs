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

use anyhow::{Context, Result};
use bytemuck::cast_slice;
use fst::{MapBuilder, Streamer as _};
use ignore::WalkBuilder;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use tracing::{debug, info, warn};

use super::bytes_to_hex;
use super::overlay::{EnrichEntry, RowPtr, SegmentMeta};
use super::overlay_writer;
use super::segment_builder::POSTING_ENRICHMENT_FIELDS;
use super::segment_reader::SegmentReader;

/// Maximum number of distinct values tracked per enrichment field before the
/// field is pruned from the overlay to keep the blob size manageable.
const MAX_ENRICH_BUCKETS: usize = 64;

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
    /// Segments that are missing or unreadable are silently skipped with a
    /// warning; an overlay with zero segments is not written (returns `Ok`).
    ///
    /// # Errors
    /// Returns `Err` if writing or renaming the overlay file fails fatally.
    /// Build the overlay and write it atomically to `overlay_path`.
    ///
    /// Segments that are missing or unreadable are silently skipped with a
    /// warning; an overlay with zero segments is not written (returns `Ok`).
    ///
    /// # Errors
    /// Returns `Err` if writing or renaming the overlay file fails fatally.
    pub fn build_and_persist(&self, overlay_path: &Path) -> Result<()> {
        let t_total = std::time::Instant::now();

        // 1. Open segments (parallel mmap I/O).
        let mut segs = self.step1_open_segments();

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
            "TIMING step2: sort segments"
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

        // 5.5. Enrichment attribute bitmaps.
        let enrich_bitmaps_bytes =
            Self::step55_build_enrich_bitmaps(&segs, &row_offsets, &seg_dedup)?;

        // 6. Merged name FST, postings, and trigrams.
        let (name_fst_bytes, name_postings_bytes, name_trigram_postings) =
            Self::step6_build_name_fst(&segs, &row_offsets)?;

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
            },
        )?;

        info!(
            ms = t_total.elapsed().as_millis(),
            path = %overlay_path.display(),
            segments = segs.len(),
            file_only = file_only.len(),
            rows = total_rows,
            "TIMING total: build_and_persist",
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
    /// - All persistent `SegmentMeta` entries whose `hex_content_id` is **not**
    ///   shadowed by `dirty` (i.e. not in `dirty.removed_hex_ids`).
    /// - All newly promoted dirty segments from `dirty.added`.
    ///
    /// Both sets are re-opened fresh from `ctx.segment_path_for(hex)` (the
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
            if dirty.shadows(&meta.hex_content_id) {
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
    // ─────────────────────────────────────────────────────────────────────────
    // Private step implementations extracted from `build_and_persist`.
    // ─────────────────────────────────────────────────────────────────────────

    // ── Step 1 ───────────────────────────────────────────────────────────────

    fn step1_open_segments(&self) -> Vec<(PathBuf, String, SegmentReader)> {
        let t_step = std::time::Instant::now();
        let provider_ver_dir =
            self.segments_dir
                .join(format!("{}-v{}", &self.provider_id, super::ENRICH_VER));
        let segs: Vec<(PathBuf, String, SegmentReader)> = self
            .segment_map
            .par_iter()
            .filter_map(|(abs_path, content_id)| {
                let hex = bytes_to_hex(content_id);
                let seg_path = provider_ver_dir
                    .join(&hex[..2])
                    .join(format!("{}.fqsf", &hex[2..]));
                if !seg_path.exists() {
                    return None;
                }
                match SegmentReader::open(&seg_path) {
                    Ok(reader) => {
                        let rel_path = abs_path
                            .strip_prefix(&self.worktree_root)
                            .unwrap_or(abs_path)
                            .to_path_buf();
                        Some((rel_path, hex, reader))
                    }
                    Err(e) => {
                        warn!(
                            path = %seg_path.display(),
                            "overlay: skipping unreadable segment: {e:#}",
                        );
                        None
                    }
                }
            })
            .collect();
        info!(
            ms = t_step.elapsed().as_millis(),
            n = segs.len(),
            "TIMING step1: open segments (parallel)",
        );
        segs
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
            "TIMING step2.5: collect file-only entries",
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
            "TIMING step3-4: row offsets + global_row_table",
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
            "TIMING step4.5: per-segment dedup canonical row sets",
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
        for (seg_idx, (_, _, reader)) in segs.iter().enumerate() {
            let row_offset = row_offsets[seg_idx];
            let canonical_bm = &seg_dedup[seg_idx].0;
            for (&kind_id, local_bm) in &reader.kind_postings {
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
            "TIMING step5: kind postings merge",
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
        );
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
            "TIMING step5.5: enrichment bitmaps",
        );
        Ok(enrich_bitmaps_bytes)
    }

    /// Category 1 of step 5.5: boolean flags + string enums sourced from each
    /// segment's `field_postings`.  Fields exceeding `MAX_ENRICH_BUCKETS`
    /// distinct values are pruned (and their already-collected keys dropped).
    fn collect_posting_enrichment(
        segs: &[(PathBuf, String, SegmentReader)],
        row_offsets: &[u32],
        seg_dedup: &[(RoaringBitmap, u32)],
        enrich_raw: &mut HashMap<String, RoaringBitmap>,
        field_seen: &mut HashMap<String, HashSet<String>>,
        pruned_fields: &mut HashSet<String>,
    ) {
        for (seg_idx, (_, _, reader)) in segs.iter().enumerate() {
            let row_offset = row_offsets[seg_idx];
            let canonical_bm = &seg_dedup[seg_idx].0;
            for (field_name, value_map) in &reader.field_postings {
                if pruned_fields.contains(field_name.as_str()) {
                    continue;
                }
                for (&value_id, local_bm) in value_map {
                    let value_str = reader.string_of_id(value_id);
                    if value_str.is_empty() {
                        continue;
                    }
                    let seen = field_seen.entry(field_name.clone()).or_default();
                    if !seen.contains(value_str) {
                        if seen.len() >= MAX_ENRICH_BUCKETS {
                            let _ = pruned_fields.insert(field_name.clone());
                            let pfx = format!("{field_name}=");
                            enrich_raw.retain(|k, _| !k.starts_with(&pfx));
                            break;
                        }
                        let _ = seen.insert(value_str.to_owned());
                    }
                    if pruned_fields.contains(field_name.as_str()) {
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
    }

    /// Category 2 of step 5.5: numeric fields not covered by the posting index
    /// (`POSTING_ENRICHMENT_FIELDS`), read row-by-row.  Same bucket-pruning rule
    /// as the posting pass.
    fn collect_numeric_enrichment(
        segs: &[(PathBuf, String, SegmentReader)],
        row_offsets: &[u32],
        seg_dedup: &[(RoaringBitmap, u32)],
        enrich_raw: &mut HashMap<String, RoaringBitmap>,
        field_seen: &mut HashMap<String, HashSet<String>>,
        pruned_fields: &mut HashSet<String>,
    ) {
        let posting_field_set: HashSet<&str> = POSTING_ENRICHMENT_FIELDS.iter().copied().collect();
        for (seg_idx, (_, _, reader)) in segs.iter().enumerate() {
            let row_offset = row_offsets[seg_idx];
            let canonical_bm = &seg_dedup[seg_idx].0;
            for local_row in canonical_bm {
                let global_row = local_row + row_offset;
                for (field_name, value_str) in reader.enrichment_for_row(local_row) {
                    if posting_field_set.contains(field_name.as_str()) {
                        continue;
                    }
                    if pruned_fields.contains(&field_name) {
                        continue;
                    }
                    let seen = field_seen.entry(field_name.clone()).or_default();
                    if !seen.contains(&value_str) {
                        if seen.len() >= MAX_ENRICH_BUCKETS {
                            let _ = pruned_fields.insert(field_name.clone());
                            let pfx = format!("{field_name}=");
                            enrich_raw.retain(|k, _| !k.starts_with(&pfx));
                            continue;
                        }
                        let _ = seen.insert(value_str.clone());
                    }
                    if pruned_fields.contains(&field_name) {
                        continue;
                    }
                    let key = format!("{field_name}={value_str}");
                    let _ = enrich_raw.entry(key).or_default().insert(global_row);
                }
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

    #[expect(
        clippy::type_complexity,
        reason = "triple return (fst_bytes, postings_bytes, trigram_map) is self-documenting at the call site"
    )]
    fn step6_build_name_fst(
        segs: &[(PathBuf, String, SegmentReader)],
        row_offsets: &[u32],
    ) -> Result<(Vec<u8>, Vec<u8>, HashMap<[u8; 3], Vec<u8>>)> {
        let t_step = std::time::Instant::now();
        let mut merged_names: BTreeMap<Vec<u8>, Vec<u32>> = BTreeMap::new();
        for (seg_idx, (_, _, reader)) in segs.iter().enumerate() {
            let row_offset = row_offsets[seg_idx];
            let name_postings_raw = reader.name_postings_bytes();
            let mut stream = reader.name_fst.stream();
            while let Some((name_bytes, encoded)) = stream.next() {
                let local_rows = decode_name_postings_raw(encoded, name_postings_raw);
                let global_rows: Vec<u32> =
                    local_rows.into_iter().map(|r| r + row_offset).collect();
                merged_names
                    .entry(name_bytes.to_vec())
                    .or_default()
                    .extend(global_rows);
            }
        }
        let merged_names_len = merged_names.len();
        let mut name_postings_bytes: Vec<u8> = Vec::new();
        let mut fst_builder = MapBuilder::memory();
        // Build the trigram index as we walk the merged name list.
        // Mirrors `ast::trigram::TrigramIndex` semantics: ASCII lower-case,
        // dedup trigrams per name, ascending row IDs.
        let mut trigram_merged: HashMap<[u8; 3], RoaringBitmap> = HashMap::new();
        for (name_bytes, mut rows) in merged_names {
            rows.sort_unstable();
            rows.dedup();
            if name_bytes.len() >= 3 {
                let mut seen: HashSet<[u8; 3]> = HashSet::new();
                for w in name_bytes.windows(3) {
                    let t = [
                        w[0].to_ascii_lowercase(),
                        w[1].to_ascii_lowercase(),
                        w[2].to_ascii_lowercase(),
                    ];
                    if seen.insert(t) {
                        let bm = trigram_merged.entry(t).or_default();
                        for r in &rows {
                            let _ = bm.insert(*r);
                        }
                    }
                }
            }
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
        let name_fst_bytes = fst_builder.into_inner().context("finalising overlay FST")?;
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
            unique_names = merged_names_len,
            trigrams = name_trigram_postings.len(),
            fst_bytes = name_fst_bytes.len(),
            "TIMING step6: name FST + trigrams",
        );
        Ok((name_fst_bytes, name_postings_bytes, name_trigram_postings))
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
            "TIMING step8: write v3 overlay (atomic)",
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
/// filesystem-walk fallback.
fn collect_file_only(worktree_root: &Path, indexed: &HashSet<PathBuf>) -> Vec<(PathBuf, String)> {
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
    let count = usize::try_from(encoded & 0xFFFF_FFFF).unwrap_or(usize::MAX);
    let byte_offset = usize::try_from((encoded >> 32) & 0xFFFF_FFFF).unwrap_or(usize::MAX);
    let end = byte_offset + count * 4;
    if end > name_postings.len() {
        return Vec::new();
    }
    #[expect(clippy::indexing_slicing, reason = "bounds checked above")]
    cast_slice::<u8, u32>(&name_postings[byte_offset..end]).to_vec()
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
