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

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bytemuck::cast_slice;
use fst::{MapBuilder, Streamer as _};
use rayon::prelude::*;
use roaring::RoaringBitmap;
use tracing::{debug, info, warn};

use super::bytes_to_hex;
use super::overlay::{HEADER_LEN, MAGIC, OverlayPayload, RowPtr, SCHEMA_VERSION, SegmentMeta};
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

        // 2. Sort by hex_content_id for deterministic global row IDs.
        let t_step = std::time::Instant::now();
        segs.sort_by(|a, b| a.1.cmp(&b.1));
        info!(
            ms = t_step.elapsed().as_millis(),
            "TIMING step2: sort segments"
        );

        if segs.is_empty() {
            debug!("overlay: no segments found — skipping overlay build");
            return Ok(());
        }

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

        // 5. Build kind postings by merging per-segment kind bitmaps.
        //    Each segment uses its own string-pool IDs; resolve to strings
        //    via `segment_reader.string_of_id`.
        let t_step = std::time::Instant::now();
        let mut kind_merged: HashMap<String, RoaringBitmap> = HashMap::new();
        for (seg_idx, (_, _, reader)) in segs.iter().enumerate() {
            let row_offset = row_offsets[seg_idx];
            for (&kind_id, local_bm) in &reader.kind_postings {
                let kind_str = reader.string_of_id(kind_id);
                if kind_str.is_empty() {
                    continue;
                }
                let merged = kind_merged.entry(kind_str.to_owned()).or_default();
                for local_row in local_bm {
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

        // 7. Build SegmentMeta list.
        let segment_metas: Vec<SegmentMeta> = segs
            .iter()
            .map(|(rel_path, hex, reader)| SegmentMeta {
                hex_content_id: hex.clone(),
                source_path: rel_path.clone(),
                row_count: reader.row_count,
            })
            .collect();

        // 8. Serialise payload.
        let t_step = std::time::Instant::now();
        let payload = OverlayPayload {
            segments: segment_metas,
            global_row_table,
            kind_postings,
            name_fst_bytes,
            name_postings_bytes,
            name_trigram_postings,
        };
        let payload_bytes = bincode::serialize(&payload).context("serialising overlay payload")?;
        info!(
            ms = t_step.elapsed().as_millis(),
            bytes = payload_bytes.len(),
            "TIMING step7-8: build metas + serialize payload"
        );

        // 9. Build fixed 24-byte header.
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(&MAGIC);
        header.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
        let generation: u64 = 1;
        header.extend_from_slice(&generation.to_le_bytes());
        #[allow(clippy::cast_possible_truncation)]
        header.extend_from_slice(&(payload_bytes.len() as u64).to_le_bytes());
        debug_assert_eq!(header.len(), HEADER_LEN, "header length invariant");

        // 10. Atomic write: temp file → fsync → rename.
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
            let mut f = tmp.as_file();
            f.write_all(&header).context("writing overlay header")?;
            f.write_all(&payload_bytes)
                .context("writing overlay payload")?;
            f.sync_all().context("fsyncing overlay file")?;
        }

        let _ = tmp
            .persist(overlay_path)
            .with_context(|| format!("persisting overlay to {}", overlay_path.display()))?;
        info!(
            ms = t_step.elapsed().as_millis(),
            "TIMING step10: atomic write (fsync + rename)"
        );

        info!(
            ms = t_total.elapsed().as_millis(),
            path = %overlay_path.display(),
            segments = segs.len(),
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
