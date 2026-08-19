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
use std::io::{self, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, ensure};
use bytemuck::cast_slice;
use fst::{IntoStreamer as _, Map as FstMap, MapBuilder, Streamer as _};
use ignore::WalkBuilder;
use rayon::prelude::*;
use roaring::RoaringBitmap;
use tracing::{debug, info};

use super::bytes_to_hex;
use super::overlay::{EnrichEntry, KindEntry, RowPtr, SegmentRecord, TrigramEntry};
use super::overlay_writer::{
    BLOB_BITMAP_DATA, BLOB_ENRICH_BITMAPS, BLOB_FILE_ENTRIES, BLOB_INDEX_FILES, BLOB_KIND_INDEX,
    BLOB_KIND_STRINGS, BLOB_NAME_FST, BLOB_NAME_POSTINGS, BLOB_ROW_TABLE, BLOB_SEGMENT_STRINGS,
    BLOB_SEGMENTS, BLOB_TRIGRAM_INDEX, BLOB_USAGES_COUNT_FST, OverlayWriter, to_u16, to_u32,
};
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

/// The blobs an overlay build produces before the file is opened.
///
/// They are handed to the write as one value so that each can be dropped the
/// moment its bytes are on disk, rather than all of them staying alive until
/// the whole file is assembled. Everything else the file holds is produced
/// during the write, at the point the layout calls for it.
struct OverlayBlobs {
    global_row_table: Vec<RowPtr>,
    /// Merged per-kind row sets, still as bitmaps: serialising them here would
    /// mean holding both forms at once, and the write needs only their sizes
    /// before it needs their bytes.
    kind_postings: HashMap<String, RoaringBitmap>,
    trigram_postings: HashMap<[u8; 3], RoaringBitmap>,
    name_fst_bytes: Vec<u8>,
    name_postings_bytes: Vec<u8>,
    /// Enrichment `field=value` row sets, keyed by the string the key table
    /// stores; also kept as bitmaps, for the same reason.
    enrich_raw: HashMap<String, RoaringBitmap>,
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
        let enrich_raw = enrich_res?;
        let (name_fst_bytes, name_postings_bytes) = name_res?;
        let trigram_postings = trigram_res?;

        // 7 + 7.5 + 7.6 + 6.5 + 8. Everything still to be produced is produced
        //    inside the write, at the point the file's layout puts it, and every
        //    buffer is dropped as soon as its bytes are on disk. The file is
        //    unchanged: the blobs go down in exactly the order they always did,
        //    and the header that describes them is filled in last.
        self.step8_write_overlay(
            overlay_path,
            &segs,
            &file_only,
            &seg_dedup,
            OverlayBlobs {
                global_row_table,
                kind_postings,
                trigram_postings,
                name_fst_bytes,
                name_postings_bytes,
                enrich_raw,
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
    ) -> Result<HashMap<String, RoaringBitmap>> {
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
        info!(
            ms = t_step.elapsed().as_millis(),
            kinds = kind_merged.len(),
            mem = %crate::mem::snapshot(), "TIMING step5: kind postings merge",
        );
        Ok(kind_merged)
    }

    // ── Step 5.5 ─────────────────────────────────────────────────────────────

    fn step55_build_enrich_bitmaps(
        segs: &[(PathBuf, String, SegmentReader)],
        row_offsets: &[u32],
        seg_dedup: &[(RoaringBitmap, u32)],
    ) -> Result<HashMap<String, RoaringBitmap>> {
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

        info!(
            ms = t_step.elapsed().as_millis(),
            entries = enrich_raw.len(),
            pruned = pruned_fields.len(),
            mem = %crate::mem::snapshot(), "TIMING step5.5: enrichment bitmaps",
        );
        Ok(enrich_raw)
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
    /// commutative, so segments accumulate into whichever worker map picks
    /// them up and the merged content is identical to computing it from the
    /// merged name list.
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the worker's slot is deliberately held for the whole segment: \
                  no other thread can lock it, so there is nothing to contend \
                  with, and re-locking per name would buy nothing"
    )]
    fn step6a_build_trigrams(
        segs: &[(PathBuf, String, SegmentReader)],
        row_offsets: &[u32],
    ) -> Result<HashMap<[u8; 3], RoaringBitmap>> {
        let t_step = std::time::Instant::now();

        // One partial map per rayon worker, not one per segment. Mapping each
        // segment to its own trigram map and reducing pairwise meant the number
        // of maps alive at once followed rayon's split tree rather than the
        // pool, and each one held whole bitmaps. A worker is the only thread
        // that can run with its own index, so its slot is uncontended and it
        // merges every segment it is given straight into it.
        let workers = rayon::current_num_threads().max(1);
        let partials: Vec<Mutex<HashMap<[u8; 3], RoaringBitmap>>> =
            (0..workers).map(|_| Mutex::new(HashMap::new())).collect();

        segs.par_iter()
            .enumerate()
            .try_for_each(|(seg_idx, (_, _, reader))| -> Result<()> {
                let slot = rayon::current_thread_index().unwrap_or(0).min(workers - 1);
                let mut local = partials[slot]
                    .lock()
                    .map_err(|_| anyhow!("trigram partial map poisoned"))?;

                let row_offset = row_offsets[seg_idx];
                let name_postings_raw = reader.name_postings_bytes();
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
                Ok(())
            })?;

        // At most `workers` maps enter the merge, and the union is commutative,
        // so the tree reduce below cannot change the answer — only how many
        // partial maps exist while it runs.
        let mut collected: Vec<HashMap<[u8; 3], RoaringBitmap>> = Vec::with_capacity(workers);
        for slot in partials {
            collected.push(
                slot.into_inner()
                    .map_err(|_| anyhow!("trigram partial map poisoned"))?,
            );
        }
        let trigram_merged: HashMap<[u8; 3], RoaringBitmap> =
            collected
                .into_par_iter()
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

        info!(
            ms = t_step.elapsed().as_millis(),
            trigrams = trigram_merged.len(),
            mem = %crate::mem::snapshot(), "TIMING step6a: trigram bitmaps (parallel)",
        );
        Ok(trigram_merged)
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

    /// Create the overlay file, write every blob into it, and rename it into
    /// place.
    ///
    /// The write goes to a temp file in the destination directory and is then
    /// renamed, so a crash mid-write leaves either the old overlay or the new
    /// one, never a partial file.
    fn step8_write_overlay(
        &self,
        overlay_path: &Path,
        segs: &[(PathBuf, String, SegmentReader)],
        file_only: &[(PathBuf, String)],
        seg_dedup: &[(RoaringBitmap, u32)],
        blobs: OverlayBlobs,
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
        let bytes = {
            let mut writer = OverlayWriter::new(std::io::BufWriter::new(tmp.as_file()), 1)
                .context("starting v3 overlay")?;
            self.write_blobs(&mut writer, segs, file_only, seg_dedup, blobs)?;
            let bytes = writer.bytes_written();
            let mut f = writer.finish().context("writing v3 overlay")?;
            f.flush().context("flushing overlay buffer")?;
            tmp.as_file().sync_all().context("fsyncing overlay file")?;
            bytes
        };
        let _ = tmp
            .persist(overlay_path)
            .with_context(|| format!("persisting overlay to {}", overlay_path.display()))?;
        info!(
            ms = t_step.elapsed().as_millis(),
            bytes,
            mem = %crate::mem::snapshot(), "TIMING step8: write v3 overlay (atomic)",
        );
        Ok(())
    }

    /// Write an overlay holding only a row table, a name FST and trigram
    /// bitmaps — no segments — so a test can open a real file.
    ///
    /// It goes through the same write as a real build, so a test that opens
    /// the result is also exercising the writer rather than a stand-in for it.
    #[cfg(test)]
    pub(super) fn write_overlay_for_test(
        overlay_path: &Path,
        global_row_table: Vec<RowPtr>,
        name_fst_bytes: Vec<u8>,
        trigram_postings: HashMap<[u8; 3], RoaringBitmap>,
    ) -> Result<()> {
        let builder = Self::new("test", PathBuf::new(), PathBuf::new(), HashMap::new());
        builder.step8_write_overlay(
            overlay_path,
            &[],
            &[],
            &[],
            OverlayBlobs {
                global_row_table,
                kind_postings: HashMap::new(),
                trigram_postings,
                name_fst_bytes,
                name_postings_bytes: Vec::new(),
                enrich_raw: HashMap::new(),
            },
        )
    }

    /// Write every blob, in the order the file lays them out.
    ///
    /// The sequence below IS the layout — `overlay_writer::BLOB_ORDER` holds
    /// the same order and the writer refuses a blob presented out of turn.
    /// Each buffer is dropped as soon as its bytes are on disk, and the blobs
    /// no earlier step needed (the segment table, the per-file sizes, the
    /// file-only entries, the usages-count FST) are produced here rather than
    /// up front, so they allocate against a heap the earlier blobs have already
    /// been freed from.
    fn write_blobs<W: Write + Seek>(
        &self,
        w: &mut OverlayWriter<W>,
        segs: &[(PathBuf, String, SegmentReader)],
        file_only: &[(PathBuf, String)],
        seg_dedup: &[(RoaringBitmap, u32)],
        blobs: OverlayBlobs,
    ) -> Result<()> {
        let OverlayBlobs {
            global_row_table,
            kind_postings,
            trigram_postings,
            name_fst_bytes,
            name_postings_bytes,
            enrich_raw,
        } = blobs;

        // row_table: eight bytes for every row in the commit, and the first
        // blob in the file — so the largest single buffer is also the first
        // one freed.
        w.blob(BLOB_ROW_TABLE, |sink| {
            sink.write_all(cast_slice(&global_row_table))
        })?;
        drop(global_row_table);

        Self::write_bitmap_blobs(w, kind_postings, trigram_postings)?;

        w.blob(BLOB_NAME_FST, |sink| sink.write_all(&name_fst_bytes))?;
        drop(name_fst_bytes);

        w.blob(BLOB_NAME_POSTINGS, |sink| {
            sink.write_all(&name_postings_bytes)
        })?;
        drop(name_postings_bytes);

        Self::write_segment_blobs(w, segs, seg_dedup)?;

        // 7.5. Cached file sizes per source segment.
        let index_files_u32 = self.step75_build_index_files(segs);
        w.blob(BLOB_INDEX_FILES, |sink| {
            sink.write_all(cast_slice(&index_files_u32))
        })?;
        drop(index_files_u32);

        w.blob(BLOB_ENRICH_BITMAPS, |sink| {
            Self::write_enrich_bitmaps(sink, &enrich_raw)
        })?;
        drop(enrich_raw);

        // 7.6. File-only entries blob.
        let file_entries_bytes = self.step76_build_file_entries(file_only);
        w.blob(BLOB_FILE_ENTRIES, |sink| {
            sink.write_all(&file_entries_bytes)
        })?;
        drop(file_entries_bytes);

        // 6.5. Usages-count FST: name → total usage-site count. It goes last in
        //      the file, so it is also built last — its shard maps hold a key
        //      for every name in the commit, and by this point every other blob
        //      has been written and freed.
        let usages_count_fst_bytes = Self::step65_build_usages_count_fst(segs)?;
        w.blob(BLOB_USAGES_COUNT_FST, |sink| {
            sink.write_all(&usages_count_fst_bytes)
        })?;

        Ok(())
    }

    /// Write `kind_strings`, `kind_index`, `bitmap_data` and `trigram_index`.
    ///
    /// The two index blobs are written before the payload they point into, so
    /// their offsets come from each bitmap's serialised size rather than from
    /// its bytes — which is what lets the payload go straight to the file.
    /// Holding the merged rows as bitmaps until here removes the two copies the
    /// previous writer made at the build's peak: one serialised buffer per
    /// bitmap, and one concatenation of them all.
    fn write_bitmap_blobs<W: Write + Seek>(
        w: &mut OverlayWriter<W>,
        kind_postings: HashMap<String, RoaringBitmap>,
        trigram_postings: HashMap<[u8; 3], RoaringBitmap>,
    ) -> Result<()> {
        // Sorted by kind name for binary search at query time.
        let mut kinds: Vec<(&str, &RoaringBitmap)> = kind_postings
            .iter()
            .map(|(kind, bitmap)| (kind.as_str(), bitmap))
            .collect();
        kinds.sort_by_key(|(kind, _)| *kind);

        w.blob(BLOB_KIND_STRINGS, |sink| {
            for (kind, _) in &kinds {
                sink.write_all(kind.as_bytes())?;
            }
            Ok(())
        })?;

        let mut kind_entries: Vec<KindEntry> = Vec::with_capacity(kinds.len());
        let mut kind_offset: u32 = 0;
        let mut bitmap_offset: u32 = 0;
        for (kind, bitmap) in &kinds {
            let kind_len = to_u32(kind.len(), "kind name too long for u32")?;
            let bitmap_len = to_u32(bitmap.serialized_size(), "kind bitmap too large for u32")?;
            kind_entries.push(KindEntry {
                kind_offset,
                kind_len,
                bitmap_offset,
                bitmap_len,
            });
            kind_offset = kind_offset
                .checked_add(kind_len)
                .context("kind strings offset exceeds u32::MAX")?;
            bitmap_offset = bitmap_offset
                .checked_add(bitmap_len)
                .context("bitmap data offset exceeds u32::MAX")?;
        }
        w.blob(BLOB_KIND_INDEX, |sink| {
            sink.write_all(cast_slice(kind_entries.as_slice()))
        })?;
        drop(kind_entries);

        // The trigram bitmaps share bitmap_data with the kind ones, so their
        // offsets carry on from where those ended.
        let mut trigrams: Vec<(&[u8; 3], &RoaringBitmap)> = trigram_postings.iter().collect();
        trigrams.sort_by_key(|(trigram, _)| **trigram);

        let mut trigram_entries: Vec<TrigramEntry> = Vec::with_capacity(trigrams.len());
        for (trigram, bitmap) in &trigrams {
            let mut tg4 = [0u8; 4];
            tg4[..3].copy_from_slice(trigram.as_ref());
            let bitmap_len = to_u32(bitmap.serialized_size(), "trigram bitmap too large for u32")?;
            trigram_entries.push(TrigramEntry {
                trigram: tg4,
                bitmap_offset,
                bitmap_len,
            });
            bitmap_offset = bitmap_offset
                .checked_add(bitmap_len)
                .context("bitmap data offset exceeds u32::MAX")?;
        }

        // Every kind bitmap, then every trigram bitmap, serialised straight
        // into the file.
        w.blob_of_len(BLOB_BITMAP_DATA, bitmap_offset, |sink| {
            for (_, bitmap) in &kinds {
                bitmap.serialize_into(&mut *sink)?;
            }
            for (_, bitmap) in &trigrams {
                bitmap.serialize_into(&mut *sink)?;
            }
            Ok(())
        })?;
        drop(kinds);
        drop(kind_postings);

        w.blob(BLOB_TRIGRAM_INDEX, |sink| {
            sink.write_all(cast_slice(trigram_entries.as_slice()))
        })?;
        drop(trigram_entries);
        drop(trigrams);
        drop(trigram_postings);
        Ok(())
    }

    /// Write `segments` and `segment_strings`.
    ///
    /// The records carry offsets into the strings, so the offsets are laid out
    /// first — the file puts the records first — and the string bytes are then
    /// written straight out of the segment list. Neither the `Vec<SegmentMeta>`
    /// that used to carry them to the writer nor its clone of every path and
    /// content ID is built at all.
    fn write_segment_blobs<W: Write + Seek>(
        w: &mut OverlayWriter<W>,
        segs: &[(PathBuf, String, SegmentReader)],
        seg_dedup: &[(RoaringBitmap, u32)],
    ) -> Result<()> {
        let mut seg_records: Vec<SegmentRecord> = Vec::with_capacity(segs.len());
        let mut string_offset: u32 = 0;
        for (seg_idx, (rel_path, hex, reader)) in segs.iter().enumerate() {
            let path_len = to_u16(
                rel_path.to_string_lossy().len(),
                "segment source path too long for u16",
            )?;
            let hex_id_len = to_u16(hex.len(), "hex content ID too long for u16")?;
            let path_offset = string_offset;
            let hex_id_offset = path_offset
                .checked_add(u32::from(path_len))
                .context("segment hex-id offset exceeds u32::MAX")?;
            seg_records.push(SegmentRecord {
                row_count: reader.row_count,
                path_offset,
                hex_id_offset,
                dedup_row_count: seg_dedup[seg_idx].1,
                path_len,
                hex_id_len,
            });
            string_offset = hex_id_offset
                .checked_add(u32::from(hex_id_len))
                .context("segment path offset exceeds u32::MAX")?;
        }
        w.blob(BLOB_SEGMENTS, |sink| {
            sink.write_all(cast_slice(seg_records.as_slice()))
        })?;
        drop(seg_records);

        w.blob_of_len(BLOB_SEGMENT_STRINGS, string_offset, |sink| {
            for (rel_path, hex, _) in segs {
                sink.write_all(rel_path.to_string_lossy().as_bytes())?;
                sink.write_all(hex.as_bytes())?;
            }
            Ok(())
        })
        .map_err(Into::into)
    }

    /// Write the `enrich_bitmaps` blob:
    /// `[u32 entry_count][u32 key_data_len][EnrichEntry × entry_count]`
    /// `[key bytes][bitmap bytes]`, keys sorted lexicographically.
    ///
    /// The entries carry offsets into the two regions that follow them, so both
    /// regions are measured before anything is written — a bitmap's serialised
    /// size is known without serialising it. That is what lets the payload go
    /// straight to the file: the previous version built the concatenated bitmap
    /// region and then a second buffer holding the whole blob, so the
    /// enrichment data existed three times over at the moment it was handed to
    /// the writer.
    fn write_enrich_bitmaps(
        out: &mut impl Write,
        enrich_raw: &HashMap<String, RoaringBitmap>,
    ) -> io::Result<()> {
        let mut sorted_enrich: Vec<(&String, &RoaringBitmap)> = enrich_raw.iter().collect();
        sorted_enrich.sort_by_key(|(key, _)| key.as_str());

        let mut entries: Vec<EnrichEntry> = Vec::with_capacity(sorted_enrich.len());
        let mut key_offset: u32 = 0;
        let mut bitmap_offset: u32 = 0;
        for (key, bitmap) in &sorted_enrich {
            let bitmap_len = u32::try_from(bitmap.serialized_size()).unwrap_or(u32::MAX);
            entries.push(EnrichEntry {
                key_offset,
                key_len: u16::try_from(key.len()).unwrap_or(u16::MAX),
                _pad: 0,
                bitmap_offset,
                bitmap_len,
            });
            key_offset = key_offset.saturating_add(u32::try_from(key.len()).unwrap_or(u32::MAX));
            bitmap_offset = bitmap_offset.saturating_add(bitmap_len);
        }

        out.write_all(
            &u32::try_from(entries.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        )?;
        out.write_all(&key_offset.to_le_bytes())?;
        out.write_all(cast_slice(entries.as_slice()))?;
        drop(entries);
        for (key, _) in &sorted_enrich {
            out.write_all(key.as_bytes())?;
        }
        for (_, bitmap) in &sorted_enrich {
            bitmap.serialize_into(&mut *out)?;
        }
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
