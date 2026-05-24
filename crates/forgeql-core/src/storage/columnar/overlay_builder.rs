#![allow(clippy::redundant_pub_crate)]
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
    #[allow(clippy::too_many_lines)]
    pub fn build_and_persist(&self, overlay_path: &Path) -> Result<()> {
        let t_total = std::time::Instant::now();

        // 1. Collect valid (relative_source_path, hex, SegmentReader) triples.
        //    Opening each segment is independent mmap I/O — run in parallel.
        let t_step = std::time::Instant::now();
        let provider_ver_dir =
            self.segments_dir
                .join(format!("{}-v{}", &self.provider_id, super::ENRICH_VER));
        let mut segs: Vec<(PathBuf, String, SegmentReader)> = self
            .segment_map
            .par_iter()
            .filter_map(|(abs_path, content_id)| {
                let hex = bytes_to_hex(content_id);
                let seg_path = provider_ver_dir.join(&hex[..2]).join(format!("{}.fqsf", &hex[2..]));

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
                        warn!(path = %seg_path.display(), "overlay: skipping unreadable segment: {e:#}");
                        None
                    }
                }
            })
            .collect();

        info!(
            ms = t_step.elapsed().as_millis(),
            n = segs.len(),
            "TIMING step1: open segments (parallel)"
        );

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

        // 2.5. Collect all workspace files not covered by a symbol segment.
        //      These become file-only entries written to the `file_entries`
        //      overlay blob (FQOV v8) so that `FIND files WHERE extension = 'X'`
        //      can use the overlay fast path for ANY file type without a
        //      filesystem walk.
        let t_step = std::time::Instant::now();
        let indexed_rel_paths: HashSet<PathBuf> =
            segs.iter().map(|(rel, _, _)| rel.clone()).collect();
        let file_only = collect_file_only(&self.worktree_root, &indexed_rel_paths);
        info!(
            ms = t_step.elapsed().as_millis(),
            n = file_only.len(),
            "TIMING step2.5: collect file-only entries"
        );

        // 3. Compute cumulative row offsets.
        let t_step = std::time::Instant::now();
        let mut row_offsets: Vec<u32> = Vec::with_capacity(segs.len());
        let mut total_rows: u32 = 0;
        for (_, _, reader) in &segs {
            row_offsets.push(total_rows);
            total_rows = total_rows
                .checked_add(reader.row_count)
                .context("overflow: too many rows for u32 row count")?;
        }

        // 4. Build global_row_table.
        let mut global_row_table: Vec<RowPtr> = Vec::with_capacity(total_rows as usize);
        for (seg_idx, (_, _, reader)) in segs.iter().enumerate() {
            for local_row in 0..reader.row_count {
                global_row_table.push(RowPtr {
                    #[allow(clippy::cast_possible_truncation)]
                    segment_idx: seg_idx as u32,
                    local_row_idx: local_row,
                });
            }
        }
        info!(
            ms = t_step.elapsed().as_millis(),
            rows = total_rows,
            "TIMING step3-4: row offsets + global_row_table"
        );

        // 4.5. Compute per-segment canonical row sets: for each segment, keep only
        //      the first occurrence of each (name_id, fql_kind_id, line) triple.
        //      This deduplicates tree-sitter AST nodes that map to the same symbol.
        //      `dedup_counts[seg_idx]` is stored in the overlay for the GROUP BY
        //      file fast-path; `canonical_bms[seg_idx]` gates kind bitmap merging.
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
                #[allow(clippy::cast_possible_truncation)]
                let cnt = canonical.len() as u32;
                (canonical, cnt)
            })
            .collect();
        info!(
            ms = t_step.elapsed().as_millis(),
            segs = segs.len(),
            "TIMING step4.5: per-segment dedup canonical row sets"
        );
        // 5. Build kind postings by merging per-segment kind bitmaps.
        //    Only canonical rows (deduplicated by name_id+fql_kind_id+line) are
        //    inserted so that kind-bitmap cardinalities match the query pipeline
        //    dedup output.  Each segment uses its own string-pool IDs; resolve
        //    to strings via `segment_reader.string_of_id`.
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

        // Serialise the merged kind bitmaps.
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
            "TIMING step5: kind postings merge"
        );

        // 5.5. Build enrichment attribute bitmaps (Phase 5 / FQOV v7).
        //
        // For each (field, value) pair across all canonical rows, build a global
        // RoaringBitmap keyed as "field=value".  Fields with more than
        // MAX_ENRICH_BUCKETS distinct values are pruned to keep the blob small.
        //
        // Two categories:
        //   Category 1 — fields in POSTING_ENRICHMENT_FIELDS: use field_postings
        //                (sparse path; avoids iterating every row).
        //   Category 2 — numeric/other fields: iterate canonical rows via
        //                enrichment_for_row (only for fields not in category 1).
        #[allow(clippy::items_after_statements)]
        const MAX_ENRICH_BUCKETS: usize = 64;
        let t_step = std::time::Instant::now();
        let mut enrich_raw: HashMap<String, RoaringBitmap> = HashMap::new();
        let mut field_seen: HashMap<String, HashSet<String>> = HashMap::new();
        let mut pruned_fields: HashSet<String> = HashSet::new();

        let posting_field_set: HashSet<&str> = POSTING_ENRICHMENT_FIELDS.iter().copied().collect();

        // Category 1: boolean flags + string enums via field_postings.
        // Category 1: boolean flags + string enums via field_postings.
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

        // Category 2: numeric fields not in POSTING_ENRICHMENT_FIELDS.
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

        // Serialise enrichment bitmaps into the `enrich_bitmaps` blob.
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
            #[allow(clippy::cast_possible_truncation)]
            enrich_entries.push(EnrichEntry {
                key_offset: enrich_key_bytes.len() as u32,
                key_len: key.len() as u16,
                _pad: 0,
                bitmap_offset: enrich_bitmap_data.len() as u32,
                bitmap_len: bm_bytes.len() as u32,
            });
            enrich_key_bytes.extend_from_slice(key.as_bytes());
            enrich_bitmap_data.extend_from_slice(&bm_bytes);
        }
        #[allow(clippy::cast_possible_truncation)]
        let entry_count_le = (enrich_entries.len() as u32).to_le_bytes();
        #[allow(clippy::cast_possible_truncation)]
        let key_data_len_le = (enrich_key_bytes.len() as u32).to_le_bytes();
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
        info!(
            ms = t_step.elapsed().as_millis(),
            entries = enrich_entries.len(),
            pruned = pruned_fields.len(),
            bytes = enrich_bitmaps_bytes.len(),
            "TIMING step5.5: enrichment bitmaps"
        );

        // 6. Build merged name FST + postings.
        //    Accumulate (name_bytes → Vec<global_row_id>) in a BTreeMap so
        //    we can insert into the FST in sorted order (FST requires it).
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
            // Trigram inserts: every distinct 3-byte window of the lower-cased
            // name maps to all global row IDs that share that name.
            if name_bytes.len() >= 3 {
                let mut seen: std::collections::HashSet<[u8; 3]> = std::collections::HashSet::new();
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
            #[allow(clippy::cast_possible_truncation)]
            let packed = ((byte_offset as u64) << 32) | (count as u64);
            fst_builder
                .insert(&name_bytes, packed)
                .context("inserting name into overlay FST")?;
        }
        let name_fst_bytes = fst_builder.into_inner().context("finalising overlay FST")?;

        // Serialise per-trigram bitmaps for storage in the payload.
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
            "TIMING step6: name FST + trigrams"
        );

        // 7. Build SegmentMeta list (source segments only — file-only entries
        //    go into the separate `file_entries` blob, not segment_metas).
        let segment_metas: Vec<SegmentMeta> = segs
            .iter()
            .enumerate()
            .map(|(seg_idx, (rel_path, hex, reader))| SegmentMeta {
                hex_content_id: hex.clone(),
                source_path: rel_path.clone(),
                row_count: reader.row_count,
                dedup_row_count: seg_dedup[seg_idx].1,
            })
            .collect();

        // 7.5. Build cached file sizes array (one u32 per source segment).
        let mut index_files_u32 = Vec::with_capacity(segs.len());
        for (rel_path, _, _) in &segs {
            let full_path = self.worktree_root.join(rel_path);
            #[allow(clippy::cast_possible_truncation)]
            let size = std::fs::metadata(&full_path)
                .map(|m| m.len() as u32)
                .unwrap_or(0);
            index_files_u32.push(size);
        }
        let index_files_bytes: &[u8] = cast_slice(&index_files_u32);

        // 7.6. Serialize file-only entries to the `file_entries` blob.
        //      Format: [u32 count][repeated: [u32 size][u16 path_len][u8; path_len]]
        //      These are workspace files without a symbol segment; they are
        //      tracked so FIND files can answer extension queries from the
        //      overlay fast path without a filesystem walk.
        let mut file_entries_bytes: Vec<u8> = Vec::new();
        {
            #[allow(clippy::cast_possible_truncation)]
            file_entries_bytes.extend_from_slice(&(file_only.len() as u32).to_le_bytes());
            for (rel_path, _hex) in &file_only {
                let full_path = self.worktree_root.join(rel_path);
                #[allow(clippy::cast_possible_truncation)]
                let size = std::fs::metadata(&full_path)
                    .map(|m| m.len() as u32)
                    .unwrap_or(0);
                let path_str = rel_path.to_string_lossy();
                let path_bytes = path_str.as_bytes();
                #[allow(clippy::cast_possible_truncation)]
                let path_len = path_bytes.len() as u16;
                file_entries_bytes.extend_from_slice(&size.to_le_bytes());
                file_entries_bytes.extend_from_slice(&path_len.to_le_bytes());
                file_entries_bytes.extend_from_slice(path_bytes);
            }
        }

        // 8. Write the FQOV v3 overlay atomically (temp file → fsync → rename).
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
            let generation: u64 = 1;
            overlay_writer::write_v3(
                &mut f,
                &overlay_writer::WriteV3Params {
                    generation,
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
            )
            .context("writing v3 overlay")?;
            f.flush().context("flushing overlay buffer")?;
            tmp.as_file().sync_all().context("fsyncing overlay file")?;
        }

        let _ = tmp
            .persist(overlay_path)
            .with_context(|| format!("persisting overlay to {}", overlay_path.display()))?;
        info!(
            ms = t_step.elapsed().as_millis(),
            "TIMING step8: write v3 overlay (atomic)"
        );

        info!(
            ms = t_total.elapsed().as_millis(),
            path = %overlay_path.display(),
            segments = segs.len(),
            file_only = file_only.len(),
            rows = total_rows,
            "TIMING total: build_and_persist"
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
    #[allow(clippy::cast_possible_truncation)]
    let count = (encoded & 0xFFFF_FFFF) as usize;
    #[allow(clippy::cast_possible_truncation)]
    let byte_offset = ((encoded >> 32) & 0xFFFF_FFFF) as usize;
    let end = byte_offset + count * 4;
    if end > name_postings.len() {
        return Vec::new();
    }
    #[allow(clippy::indexing_slicing)] // bounds checked above
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
