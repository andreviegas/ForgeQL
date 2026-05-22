#![allow(clippy::redundant_pub_crate)]
//! [`Overlay`] — workspace-level merged index for the columnar backend.
//!
//! Keyed by commit SHA, stored at
//! `<bare-repo>/forgeql/overlays/<provider_id>/<commit_sha>.bin`.
//!
//! The overlay merges N per-file segments into a single queryable index
//! shared across all [`ColumnarStorage`] sessions on the same commit SHA.
//! Multiple sessions mmap the same file; the OS reference-counts physical
//! pages so RSS does not multiply by session count.
//!
//! # FQOV v3 file format
//!
//! ```text
//! [0..4]           b"FQOV"            magic
//! [4..8]           schema_version: u32 = 3 (little-endian)
//! [8..16]          generation: u64 (little-endian)
//! [16..20]         toc_count: u32 = 9
//! [20..24]         _reserved: u32 = 0
//! [24..24+9*64]    9 × 64-byte TocEntry records
//! [600..]          blob data (absolute offsets from TocEntry)
//! ```
//!
//! Named blobs (TOC order):
//! 1. `row_table`       — `[RowPtr]` flat array (zero-copy via `cast_slice`)
//! 2. `kind_strings`    — concatenated UTF-8 kind name bytes
//! 3. `kind_index`      — `[KindEntry]` sorted by kind name (binary search)
//! 4. `bitmap_data`     — all serialised `RoaringBitmap` bytes (kinds + trigrams)
//! 5. `trigram_index`   — `[TrigramEntry]` sorted by trigram bytes
//! 6. `name_fst`        — FST bytes for name → postings lookup
//! 7. `name_postings`   — flat `[u32]` global row IDs
//! 8. `segments`        — `[SegmentRecord]` fixed-size per-segment metadata
//! 9. `segment_strings` — concatenated path + hex-id UTF-8 strings

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::segment_reader::{MmapSlice, SegmentReader};
use crate::result::SymbolMatch;
use anyhow::{Context, Result, ensure};
use bytemuck::{Pod, Zeroable, cast_slice};
use fst::Map as FstMap;
use memmap2::{Mmap, MmapOptions};
use roaring::RoaringBitmap;

// ─────────────────────────────────────────────────────────────────────────────
// On-disk constants
// ─────────────────────────────────────────────────────────────────────────────

/// Magic bytes at the start of every overlay file.
pub(crate) const MAGIC: [u8; 4] = *b"FQOV";
/// Current schema version.  Bump on any breaking format change.
///
/// History:
/// - **1**: initial overlay format.
/// - **2**: adds `name_trigram_postings`.
/// - **3**: TOC-based mmap format; large blobs are zero-copy.
/// - **4**: segments sorted by `source_path` (path-ordered global row IDs).
///   Prerequisite for path-prefix → row-range fast-paths (Phases 3–6).
/// - **5**: `SegmentRecord` gains `dedup_row_count` field (20 bytes, was 16).
///   Per-segment unique-(name,kind,line) counts enable GROUP BY file fast-path.
///   Kind bitmaps are deduplicated at build time, enabling GROUP BY kind fast-path.
/// - **6**: `index_files` blob added; file sizes cached for `FIND files` fast-path.
/// - **7**: `enrich_bitmaps` blob added; per-(field,value) global bitmaps for
///   O(1) enrichment-predicate prefiltering (Phase 5).
pub(crate) const SCHEMA_VERSION: u32 = 7;

/// Number of bytes in the fixed header (before the TOC).
pub(crate) const HEADER_LEN: usize = 24;

/// Byte size of one TOC entry (matches FQSF `TOC_ENTRY_SIZE`).
pub(crate) const TOC_ENTRY_SIZE: usize = 64;

/// Max byte length of a blob name within a `TocEntry`.
pub(crate) const TOC_ENTRY_NAME_LEN: usize = 56;

/// Number of named blobs in an FQOV v7 file (10 original + `enrich_bitmaps`).
pub(super) const TOC_COUNT: usize = 11;

/// Total byte size of the header + TOC region (= 24 + 11 * 64 = 728).
pub(super) const HEADER_V3_LEN: usize = HEADER_LEN + TOC_COUNT * TOC_ENTRY_SIZE;

// ─────────────────────────────────────────────────────────────────────────────
// Fixed-size Pod types (all #[repr(C)], fields ordered to avoid gaps)
// ─────────────────────────────────────────────────────────────────────────────

/// One entry in the FQOV v3 TOC (64 bytes, matching FQSF layout).
///
/// Not derived as `Pod` because `[u8; 56]` conflicts with `object::pod::Pod`
/// in the dependency graph.  Read/write is done field-by-field instead.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct TocEntry {
    /// NUL-padded blob name (ASCII, ≤ `TOC_ENTRY_NAME_LEN` bytes used).
    pub(crate) name: [u8; 56],
    /// Absolute byte offset into the overlay file.
    pub(crate) offset: u32,
    /// Byte length of the blob.
    pub(crate) len: u32,
}

/// Pointer from a global row ID into a specific segment.
///
/// `#[repr(C)]` + `bytemuck::Pod` enables zero-copy reads from the
/// `row_table` blob via `cast_slice`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct RowPtr {
    /// Index into the overlay's segment list.
    pub segment_idx: u32,
    /// Row index within that segment.
    pub local_row_idx: u32,
}

/// FQOV v3: one entry in the `kind_index` blob, sorted by kind name.
///
/// All fields are `u32` to avoid implicit padding.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(super) struct KindEntry {
    /// Byte offset into the `kind_strings` blob.
    pub(super) kind_offset: u32,
    /// Byte length of the kind name.
    pub(super) kind_len: u32,
    /// Byte offset into the `bitmap_data` blob.
    pub(super) bitmap_offset: u32,
    /// Byte length of the serialised `RoaringBitmap`.
    pub(super) bitmap_len: u32,
}

/// FQOV v3: one entry in the `trigram_index` blob, sorted by trigram bytes.
///
/// `trigram[0..3]` holds the actual trigram; `trigram[3]` is reserved = 0
/// (provides 4-byte alignment without an explicit pad field).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(super) struct TrigramEntry {
    /// Bytes 0–2: trigram; byte 3: reserved = 0.
    pub(super) trigram: [u8; 4],
    /// Byte offset into the `bitmap_data` blob.
    pub(super) bitmap_offset: u32,
    /// Byte length of the serialised `RoaringBitmap`.
    pub(super) bitmap_len: u32,
}

/// FQOV v3: fixed-size metadata record for one segment in the `segments` blob.
///
/// Strings are resolved from `segment_strings` at open time.
/// Fields ordered to pack two `u16`s at the end — 20 bytes total, no gaps.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(super) struct SegmentRecord {
    pub(super) row_count: u32,
    /// Byte offset of the source-path string in `segment_strings`.
    pub(super) path_offset: u32,
    /// Byte offset of the hex content-ID string in `segment_strings`.
    pub(super) hex_id_offset: u32,
    /// Number of unique (name, fql_kind, line) tuples in this segment.
    ///
    /// Used by the GROUP BY file fast-path to return deduplicated symbol counts
    /// without materialising individual rows.  Added in SCHEMA_VERSION 5.
    pub(super) dedup_row_count: u32,
    /// Byte length of the source-path string.
    pub(super) path_len: u16,
    /// Byte length of the hex content-ID string (≤ 40 for SHA-1 hex).
    pub(super) hex_id_len: u16,
}

/// FQOV v7: one entry in the `enrich_bitmaps` blob.
///
/// Blob layout:
///   `[u32: entry_count][u32: key_data_len][EnrichEntry × entry_count]`
///   `[key_strings bytes][bitmap_data bytes]`
///
/// Keys are `"field=value"` strings sorted lexicographically.
/// Bitmap data is serialised `RoaringBitmap`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(super) struct EnrichEntry {
    /// Byte offset into the key-strings region of the `enrich_bitmaps` blob.
    pub(super) key_offset: u32,
    /// Byte length of the key string (= `"field=value".len()`).
    pub(super) key_len: u16,
    /// Reserved, must be zero.
    pub(super) _pad: u16,
    /// Byte offset into the bitmap-data region of the `enrich_bitmaps` blob.
    pub(super) bitmap_offset: u32,
    /// Byte length of the serialised `RoaringBitmap`.
    pub(super) bitmap_len: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Heap-allocated segment metadata (decoded at open time)
// ─────────────────────────────────────────────────────────────────────────────

/// Per-segment metadata stored in the overlay's segment table.
#[derive(Debug, Clone)]
pub struct SegmentMeta {
    /// Hex-encoded content ID — directory name under
    /// `segments/<provider_id>/`.
    pub hex_content_id: String,
    /// Source file path **relative to the worktree root**.
    pub source_path: PathBuf,
    /// Number of rows in this segment (== symbols in the source file).
    pub row_count: u32,
    /// Number of unique (name, fql_kind, line) tuples in this segment.
    ///
    /// This is the deduplicated symbol count used by the GROUP BY file fast-path.
    pub dedup_row_count: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Overlay reader (v3 — mmap-backed, zero-copy for large blobs)
// ─────────────────────────────────────────────────────────────────────────────

/// Workspace-level merged index shared across all [`ColumnarStorage`] instances
/// on the same commit SHA.
///
/// All large blobs (`row_table`, `kind_index`, `trigram_index`, `bitmap_data`,
/// `name_postings`) are accessed directly from the mmap — no heap copy at open
/// time.  Kind and trigram bitmaps are decoded transiently per query; the OS
/// page cache keeps hot pages resident.
pub struct Overlay {
    /// The memory-mapped overlay file (keeps all blob ranges alive).
    mmap: Arc<Mmap>,
    /// Segments decoded at open time (String/PathBuf are heap-allocated anyway).
    segments: Vec<SegmentMeta>,
    /// Total row count (= row_table blob size / sizeof(RowPtr)).
    row_count: u32,
    generation: u64,

    /// Inclusive-start global row-ID for each segment (length = segments.len() + 1).
    ///
    /// `segment_offsets[i]` is the first global row ID belonging to segment `i`.
    /// `segment_offsets[segments.len()]` equals `row_count` (one-past-the-end
    /// sentinel).  Built from the path-sorted `segments` list so that
    /// `segment_offsets[i]..segment_offsets[i+1]` is the contiguous global
    /// row-ID range for segment `i` — the foundation for the path fast-paths
    /// added in Phases 4–6.
    segment_offsets: Vec<u32>,

    // Byte ranges within `mmap` for each zero-copy blob.
    row_table_range: Range<usize>,
    kind_strings_range: Range<usize>,
    kind_index_range: Range<usize>,
    bitmap_data_range: Range<usize>,
    trigram_index_range: Range<usize>,
    name_postings_range: Range<usize>,
    index_files_range: Range<usize>,
    /// Byte range of the `enrich_bitmaps` blob within `mmap` (Phase 5 / FQOV v7).
    /// Retained for potential future re-parsing; not read after `open()`.
    #[allow(dead_code)]
    enrich_bitmaps_range: Range<usize>,
    /// Pre-parsed enrichment index: sorted `(key, bitmap_mmap_range)` pairs.
    ///
    /// key = `"field=value"` (byte-lexicographic order).
    /// The mmap range refers to the serialised `RoaringBitmap` within `mmap`.
    enrich_index: Vec<(String, Range<usize>)>,

    /// True when the segment list contains two or more entries with the same
    /// `source_path`.  When true, row-count-based fast-paths (GROUP BY file,
    /// GROUP BY kind) are unsafe because the overlay includes duplicate rows
    /// that the normal query pipeline eliminates via deduplication.
    has_duplicate_paths: bool,

    /// Zero-copy FST backed by a slice of the mmap.
    name_fst: FstMap<MmapSlice>,
}

impl Overlay {
    /// Open an FQOV v3 overlay file and memory-map it.
    ///
    /// Large blobs stay mmap-resident; segment metadata is decoded into a
    /// heap-allocated `Vec<SegmentMeta>` at open time.
    ///
    /// # Errors
    /// Returns `Err` for missing/truncated files, wrong magic, unsupported
    /// schema version, misaligned blobs, or a corrupt name FST.
    #[allow(clippy::too_many_lines)]
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening overlay {}", path.display()))?;
        #[allow(unsafe_code)] // read-only mmap of immutable overlay file
        let mmap: Arc<Mmap> = Arc::new(
            unsafe { MmapOptions::new().map(&file) }
                .with_context(|| format!("mmap overlay {}", path.display()))?,
        );
        drop(file);

        ensure!(mmap.len() >= HEADER_LEN, "overlay file too short");
        ensure!(
            mmap[..4] == MAGIC,
            "invalid overlay magic in {}",
            path.display()
        );

        #[allow(clippy::indexing_slicing)]
        let schema_version =
            u32::from_le_bytes(mmap[4..8].try_into().context("schema_version bytes")?);
        ensure!(
            schema_version == SCHEMA_VERSION,
            "overlay schema version mismatch in {}: expected {SCHEMA_VERSION}, got {schema_version}",
            path.display()
        );

        #[allow(clippy::indexing_slicing)]
        let generation = u64::from_le_bytes(mmap[8..16].try_into().context("generation bytes")?);
        #[allow(clippy::indexing_slicing)]
        let toc_count =
            u32::from_le_bytes(mmap[16..20].try_into().context("toc_count bytes")?) as usize;

        let toc_end = HEADER_LEN + toc_count * TOC_ENTRY_SIZE;
        ensure!(
            mmap.len() >= toc_end,
            "overlay TOC truncated: need {toc_end} bytes, file is {} bytes",
            mmap.len()
        );

        let toc = parse_toc_entries(&mmap, toc_count)?;

        let blobs = find_blob_ranges(&toc)?;
        validate_blob_layout(mmap.len(), &blobs)?;
        let [
            row_table_range,
            kind_strings_range,
            kind_index_range,
            bitmap_data_range,
            trigram_index_range,
            name_fst_range,
            name_postings_range,
            segments_range,
            segment_strings_range,
            index_files_range,
            enrich_bitmaps_range,
        ] = blobs;

        let row_count = u32::try_from(row_table_range.len() / std::mem::size_of::<RowPtr>())
            .context("row count overflow")?;

        // Decode segment metadata at open time.
        #[allow(clippy::indexing_slicing)]
        let seg_records: &[SegmentRecord] = cast_slice(&mmap[segments_range]);
        #[allow(clippy::indexing_slicing)]
        #[allow(clippy::indexing_slicing)]
        let segments = decode_segment_metas(seg_records, &mmap[segment_strings_range])?;

        // Build prefix-sum table: segment_offsets[i] = first global row ID for
        // segment i; segment_offsets[n] = row_count (one-past-the-end sentinel).
        let mut segment_offsets: Vec<u32> = Vec::with_capacity(segments.len() + 1);
        let mut running = 0u32;
        for seg in &segments {
            segment_offsets.push(running);
            running = running.saturating_add(seg.row_count);
        }
        segment_offsets.push(running);

        // Build the zero-copy name FST backed by a mmap slice.
        let name_fst = FstMap::new(MmapSlice::new(
            Arc::clone(&mmap),
            name_fst_range.start,
            name_fst_range.end,
        ))
        .context("loading name FST from overlay")?;

        // Detect duplicate source paths — signals that the overlay contains
        // redundant rows that the query pipeline deduplicates but raw counts
        // (row_count, kind bitmap lengths) do not.
        let has_duplicate_paths = {
            let mut seen = std::collections::HashSet::with_capacity(segments.len());
            segments.iter().any(|s| !seen.insert(&s.source_path))
        };

        // Parse enrichment bitmaps blob (Phase 5 / FQOV v7).
        // Build enrich_index: sorted (key, bitmap_mmap_range) pairs.
        let mut enrich_index: Vec<(String, Range<usize>)> = Vec::new();
        {
            #[allow(clippy::indexing_slicing)]
            let blob = &mmap[enrich_bitmaps_range.clone()];
            if blob.len() >= 8 {
                #[allow(clippy::indexing_slicing)]
                let entry_count =
                    u32::from_le_bytes(blob[0..4].try_into().unwrap_or_default()) as usize;
                #[allow(clippy::indexing_slicing)]
                let key_data_len =
                    u32::from_le_bytes(blob[4..8].try_into().unwrap_or_default()) as usize;
                let entry_bytes = std::mem::size_of::<EnrichEntry>();
                let entries_end = 8 + entry_count * entry_bytes;
                if blob.len() >= entries_end + key_data_len {
                    #[allow(clippy::indexing_slicing)]
                    let entries: &[EnrichEntry] = cast_slice(&blob[8..entries_end]);
                    #[allow(clippy::indexing_slicing)]
                    let key_data = &blob[entries_end..entries_end + key_data_len];
                    let bitmap_base = enrich_bitmaps_range.start + entries_end + key_data_len;
                    for e in entries {
                        let k_start = e.key_offset as usize;
                        let k_end = k_start + e.key_len as usize;
                        if k_end > key_data.len() {
                            continue;
                        }
                        #[allow(clippy::indexing_slicing)]
                        if let Ok(key) = std::str::from_utf8(&key_data[k_start..k_end]) {
                            let b_start = bitmap_base + e.bitmap_offset as usize;
                            let b_end = b_start + e.bitmap_len as usize;
                            enrich_index.push((key.to_owned(), b_start..b_end));
                        }
                    }
                }
            }
        }

        Ok(Arc::new(Self {
            mmap,
            segments,
            row_count,
            generation,
            segment_offsets,
            row_table_range,
            kind_strings_range,
            kind_index_range,
            bitmap_data_range,
            trigram_index_range,
            name_postings_range,
            index_files_range,
            enrich_bitmaps_range,
            enrich_index,
            has_duplicate_paths,
            name_fst,
        }))
    }

    /// Monotonic generation counter — bumped by every reindex.
    #[allow(dead_code)]
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns `true` if the overlay contains two or more segments with the
    /// same `source_path`, indicating that raw row-count metrics include
    /// duplicates and count-based fast-paths must be skipped.
    #[must_use]
    pub const fn has_duplicate_paths(&self) -> bool {
        self.has_duplicate_paths
    }

    /// Ordered list of segments in this overlay.
    #[must_use]
    pub fn segments(&self) -> &[SegmentMeta] {
        &self.segments
    }

    /// Retrieve the cached file size for segment `idx`.
    #[must_use]
    pub fn file_size(&self, idx: usize) -> u32 {
        #[allow(clippy::indexing_slicing)]
        let slice: &[u32] = cast_slice(&self.mmap[self.index_files_range.clone()]);
        slice.get(idx).copied().unwrap_or(0)
    }

    // ─────────────────────────────────────────────────────────────────────
    // Phase 5: enrichment bitmap prefiltering (FQOV v7)
    // ─────────────────────────────────────────────────────────────────────

    /// Returns `true` if any enrichment bitmap entry exists for `field`.
    ///
    /// Used to check predicate eligibility without deserialising bitmaps.
    #[must_use]
    pub fn has_enrichment_field(&self, field: &str) -> bool {
        let prefix = format!("{field}=");
        let pos = self
            .enrich_index
            .partition_point(|(k, _)| k.as_str() < prefix.as_str());
        self.enrich_index
            .get(pos)
            .is_some_and(|(k, _)| k.starts_with(&prefix))
    }

    /// Look up the global row bitmap for `field = value`.
    ///
    /// Returns `None` if no bitmap was stored for this (field, value) pair.
    #[must_use]
    pub fn prefilter_enrichment_eq(&self, field: &str, value: &str) -> Option<RoaringBitmap> {
        let target = format!("{field}={value}");
        let pos = self
            .enrich_index
            .partition_point(|(k, _)| k.as_str() < target.as_str());
        let (key, range) = self.enrich_index.get(pos)?;
        if key != &target {
            return None;
        }
        #[allow(clippy::indexing_slicing)]
        RoaringBitmap::deserialize_from(&self.mmap[range.clone()]).ok()
    }

    /// Union global row bitmaps for `field >= threshold` (numeric enrichment fields).
    ///
    /// Returns `None` if no enrichment bitmaps were stored for this field.
    #[must_use]
    pub fn prefilter_enrichment_ge(&self, field: &str, threshold: i64) -> Option<RoaringBitmap> {
        let prefix = format!("{field}=");
        let pos = self
            .enrich_index
            .partition_point(|(k, _)| k.as_str() < prefix.as_str());
        let mut result: Option<RoaringBitmap> = None;
        #[allow(clippy::indexing_slicing)]
        for (key, range) in &self.enrich_index[pos..] {
            if !key.starts_with(&prefix) {
                break;
            }
            #[allow(clippy::indexing_slicing)]
            let value_str = &key[prefix.len()..];
            if let Ok(v) = value_str.parse::<i64>()
                && v >= threshold
            {
                #[allow(clippy::indexing_slicing)]
                if let Ok(bm) = RoaringBitmap::deserialize_from(&self.mmap[range.clone()]) {
                    result = Some(match result {
                        Some(prev) => prev | bm,
                        None => bm,
                    });
                }
            }
        }
        result
    }

    /// Union global row bitmaps for `field <= threshold` (numeric enrichment fields).
    ///
    /// Returns `None` if no enrichment bitmaps were stored for this field.
    #[must_use]
    pub fn prefilter_enrichment_le(&self, field: &str, threshold: i64) -> Option<RoaringBitmap> {
        let prefix = format!("{field}=");
        let pos = self
            .enrich_index
            .partition_point(|(k, _)| k.as_str() < prefix.as_str());
        let mut result: Option<RoaringBitmap> = None;
        #[allow(clippy::indexing_slicing)]
        for (key, range) in &self.enrich_index[pos..] {
            if !key.starts_with(&prefix) {
                break;
            }
            #[allow(clippy::indexing_slicing)]
            let value_str = &key[prefix.len()..];
            if let Ok(v) = value_str.parse::<i64>()
                && v <= threshold
            {
                #[allow(clippy::indexing_slicing)]
                if let Ok(bm) = RoaringBitmap::deserialize_from(&self.mmap[range.clone()]) {
                    result = Some(match result {
                        Some(prev) => prev | bm,
                        None => bm,
                    });
                }
            }
        }
        result
    }

    /// Total number of rows across all segments.
    #[must_use]
    pub const fn row_count(&self) -> u32 {
        self.row_count
    }

    /// Global row-ID range for segment `seg_idx`.
    ///
    /// Returns `segment_offsets[seg_idx]..segment_offsets[seg_idx + 1]`.
    /// The range is guaranteed to be contiguous because segments are stored in
    /// path-sorted order (FQOV v4+).
    ///
    /// Returns an empty range (`0..0`) when `seg_idx >= segments.len()`.
    #[must_use]
    pub fn segment_row_range(&self, seg_idx: usize) -> std::ops::Range<u32> {
        if seg_idx >= self.segments.len() {
            return 0..0;
        }
        // segment_offsets has length segments.len() + 1, so both indices exist.
        #[allow(clippy::indexing_slicing)]
        let start = self.segment_offsets[seg_idx];
        #[allow(clippy::indexing_slicing)]
        let end = self.segment_offsets[seg_idx + 1];
        start..end
    }

    /// Returns the contiguous range of segment indices `[lo, hi)` whose
    /// `source_path` starts with `prefix`.
    ///
    /// Segments are path-sorted (FQOV v4+), so all matching paths form a
    /// contiguous block.  Two `partition_point` calls locate the boundaries
    /// in O(log N) time without allocating.
    ///
    /// Returns `0..0` when no segments match.
    /// Returns `0..self.segments.len()` when `prefix` is empty.
    #[must_use]
    pub fn path_seg_range(&self, prefix: &str) -> Range<usize> {
        if prefix.is_empty() {
            return 0..self.segments.len();
        }
        let lo = self
            .segments
            .partition_point(|s| s.source_path.as_os_str().as_encoded_bytes() < prefix.as_bytes());
        // Upper bound: smallest string > all strings that start with `prefix`.
        // Walk from the right, skip 0xFF bytes, increment the first byte < 0xFF.
        let hi = {
            let mut upper = prefix.as_bytes().to_vec();
            loop {
                match upper.last_mut() {
                    None => break self.segments.len(),
                    Some(b) if *b < 0xFF => {
                        *b += 1;
                        break self.segments.partition_point(|s| {
                            s.source_path.as_os_str().as_encoded_bytes() < upper.as_slice()
                        });
                    }
                    Some(_) => {
                        let _ = upper.pop();
                    }
                }
            }
        };
        lo..hi
    }

    /// Global row-ID range for all segments whose `source_path` starts with `prefix`.
    ///
    /// Combines `path_seg_range` with the `segment_offsets` prefix-sum table to
    /// return the contiguous `start..end` range in O(log N) time.
    ///
    /// Returns `0..0` when no segments match.
    #[must_use]
    pub fn path_row_range(&self, prefix: &str) -> Range<u32> {
        let seg_range = self.path_seg_range(prefix);
        if seg_range.is_empty() {
            return 0..0;
        }
        #[allow(clippy::indexing_slicing)]
        let start = self.segment_offsets[seg_range.start];
        #[allow(clippy::indexing_slicing)]
        let end = self.segment_offsets[seg_range.end];
        start..end
    }

    /// Decode and return the global-row-id bitmap for a given `fql_kind`.
    ///
    /// Binary-searches the sorted `kind_index` blob and deserialises the
    /// `RoaringBitmap` from `bitmap_data` on demand.
    /// Returns `None` if the kind is absent from the overlay.
    #[must_use]
    pub fn prefilter_kind(&self, kind: &str) -> Option<RoaringBitmap> {
        #[allow(clippy::indexing_slicing)]
        let entries: &[KindEntry] = cast_slice(&self.mmap[self.kind_index_range.clone()]);
        #[allow(clippy::indexing_slicing)]
        let kind_strings = &self.mmap[self.kind_strings_range.clone()];
        let kind_bytes = kind.as_bytes();

        // Binary search: entries are sorted by kind string (established at build time).
        let idx = entries.partition_point(|e| {
            let s_start = e.kind_offset as usize;
            let s_end = s_start + e.kind_len as usize;
            #[allow(clippy::indexing_slicing)]
            kind_strings
                .get(s_start..s_end)
                .is_none_or(|s| s < kind_bytes)
        });

        let e = entries.get(idx)?;
        let s_start = e.kind_offset as usize;
        let s_end = s_start + e.kind_len as usize;
        #[allow(clippy::indexing_slicing)]
        let s = kind_strings.get(s_start..s_end)?;
        if s != kind_bytes {
            return None;
        }

        #[allow(clippy::indexing_slicing)]
        let bitmap_data = &self.mmap[self.bitmap_data_range.clone()];
        let bm_start = e.bitmap_offset as usize;
        let bm_end = bm_start + e.bitmap_len as usize;
        let bm_bytes = bitmap_data.get(bm_start..bm_end)?;
        RoaringBitmap::deserialize_from(bm_bytes).ok()
    }

    /// Iterate every kind in the index and return `(kind_name, global_row_count)` pairs.
    ///
    /// Used by the `GROUP BY fql_kind` fast-path to produce per-kind counts without
    /// materialising any individual symbol rows.  The pairs are returned in the same
    /// sorted order as the `kind_index` blob (lexicographic by kind name).
    ///
    /// An optional `path_mask` narrows the count to only rows in a specific path range.
    #[must_use]
    pub(super) fn kind_global_counts(
        &self,
        path_mask: Option<&RoaringBitmap>,
    ) -> Vec<(String, usize)> {
        #[allow(clippy::indexing_slicing)]
        let entries: &[KindEntry] = cast_slice(&self.mmap[self.kind_index_range.clone()]);
        #[allow(clippy::indexing_slicing)]
        let kind_strings = &self.mmap[self.kind_strings_range.clone()];
        #[allow(clippy::indexing_slicing)]
        let bitmap_data = &self.mmap[self.bitmap_data_range.clone()];

        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let s_start = e.kind_offset as usize;
            let s_end = s_start + e.kind_len as usize;
            let Some(s_bytes) = kind_strings.get(s_start..s_end) else {
                continue;
            };
            let Ok(kind_name) = std::str::from_utf8(s_bytes) else {
                continue;
            };
            let bm_start = e.bitmap_offset as usize;
            let bm_end = bm_start + e.bitmap_len as usize;
            let Some(bm_bytes) = bitmap_data.get(bm_start..bm_end) else {
                continue;
            };
            let Ok(bm) = RoaringBitmap::deserialize_from(bm_bytes) else {
                continue;
            };
            let count = match path_mask {
                Some(mask) => usize::try_from((bm & mask).len()).unwrap_or(0),
                None => usize::try_from(bm.len()).unwrap_or(0),
            };
            if count > 0 {
                out.push((kind_name.to_owned(), count));
            }
        }
        out
    }

    /// Look up all global row IDs for a given symbol name (exact match).
    #[must_use]
    pub fn lookup_name_bitmap(&self, name: &str) -> RoaringBitmap {
        let Some(encoded) = self.name_fst.get(name.as_bytes()) else {
            return RoaringBitmap::new();
        };
        self.decode_postings(encoded)
    }

    /// Trigram-based candidate prefilter for substring search over names.
    ///
    /// Binary-searches the sorted `trigram_index` blob for each distinct
    /// 3-byte window of `substr` (ASCII-lowercased) and intersects the
    /// resulting bitmaps on demand.
    ///
    /// Returns:
    /// - `None` when `substr` is shorter than 3 bytes (no prefilter possible).
    /// - `None` when the trigram index is empty (no prefilter possible).
    /// - `Some(empty)` when any trigram is absent (no row can match).
    /// - `Some(bitmap)` of candidate global row IDs.
    #[must_use]
    pub fn name_substring_candidates(&self, substr: &str) -> Option<RoaringBitmap> {
        let bytes = substr.as_bytes();
        if bytes.len() < 3 {
            return None;
        }

        #[allow(clippy::indexing_slicing)]
        let entries: &[TrigramEntry] = cast_slice(&self.mmap[self.trigram_index_range.clone()]);
        if entries.is_empty() {
            return None;
        }

        #[allow(clippy::indexing_slicing)]
        let bitmap_data = &self.mmap[self.bitmap_data_range.clone()];

        // Collect unique trigrams (ASCII-lowercased).
        let mut trigrams: Vec<[u8; 3]> = Vec::new();
        for w in bytes.windows(3) {
            let t = [
                w[0].to_ascii_lowercase(),
                w[1].to_ascii_lowercase(),
                w[2].to_ascii_lowercase(),
            ];
            if !trigrams.contains(&t) {
                trigrams.push(t);
            }
        }

        // Decode bitmaps for each trigram via binary search.
        let mut bitmaps: Vec<RoaringBitmap> = Vec::with_capacity(trigrams.len());
        for t in &trigrams {
            let idx = entries.partition_point(|e| &e.trigram[..3] < t.as_ref());
            // An out-of-bounds index means the trigram is larger than every
            // stored entry — it is absent.  A mismatch at idx also means absent.
            // Either way, no row can possibly match.
            let Some(e) = entries.get(idx) else {
                return Some(RoaringBitmap::new());
            };
            if e.trigram[..3] != *t {
                // Trigram absent — no row can match.
                return Some(RoaringBitmap::new());
            }
            let bm_start = e.bitmap_offset as usize;
            let bm_end = bm_start + e.bitmap_len as usize;
            let bm_bytes = bitmap_data.get(bm_start..bm_end)?;
            let bm = RoaringBitmap::deserialize_from(bm_bytes).ok()?;
            bitmaps.push(bm);
        }

        if bitmaps.is_empty() {
            return None;
        }

        // Intersect in ascending cardinality order for fastest convergence.
        bitmaps.sort_unstable_by_key(RoaringBitmap::len);
        let mut result = bitmaps.remove(0);
        for bm in bitmaps {
            result &= bm;
            if result.is_empty() {
                break;
            }
        }
        Some(result)
    }

    /// Resolve a global row ID to a `RowPtr`.
    ///
    /// Returns `None` if `global_id` is out of range (defensive check).
    #[must_use]
    pub fn resolve_global(&self, global_id: u32) -> Option<RowPtr> {
        #[allow(clippy::indexing_slicing)]
        let row_ptrs: &[RowPtr] = cast_slice(&self.mmap[self.row_table_range.clone()]);
        row_ptrs.get(global_id as usize).copied()
    }

    // ─────────────────────────────────────────────────────────────────────
    // Private helpers
    // ─────────────────────────────────────────────────────────────────────

    /// Stream symbol rows in ascending name order from the name FST.
    ///
    /// Walks the overlay's `name_fst` (keys are already in lexicographic
    /// order), decodes each name's postings, and materialises each row via
    /// [`SegmentReader::materialize_one_row`].  Stops after exhausting the
    /// first FST key whose addition pushes the result count to at least
    /// `need`, so that all rows sharing a name are present before
    /// `apply_clauses` applies tie-breaking.
    ///
    /// Used by the `ORDER BY name ASC LIMIT N` fast-path in
    /// [`ColumnarStorage::find_symbols`].
    pub(crate) fn stream_names_asc(
        &self,
        need: usize,
        segments: &[Arc<SegmentReader>],
    ) -> Vec<SymbolMatch> {
        use fst::Streamer as _;
        let mut results = Vec::new();
        let mut stream = self.name_fst.stream();
        while let Some((_name_bytes, encoded)) = stream.next() {
            let slice = self.decode_postings_slice(encoded);
            for &global_id in slice {
                let Some(ptr) = self.resolve_global(global_id) else {
                    continue;
                };
                let Some(seg) = segments.get(ptr.segment_idx as usize) else {
                    continue;
                };
                let Some(meta) = self.segments.get(ptr.segment_idx as usize) else {
                    continue;
                };
                if let Some(row) = seg.materialize_one_row(ptr.local_row_idx, &meta.source_path) {
                    results.push(row);
                }
            }
            // Complete the current name group fully before checking the budget
            // so apply_clauses can correctly tie-break rows with the same name.
            if results.len() >= need {
                break;
            }
        }
        results
    }

    /// Like [`stream_names_asc`] but only emits rows whose global row ID is in `kind_bm`.
    ///
    /// Used by the `ORDER BY name ASC LIMIT N WHERE fql_kind = 'X'` fast-path to
    /// stream FST names in lexicographic order, filtering to a specific kind without
    /// materialising the full kind bitmap first and without distributing it across segments.
    pub(super) fn stream_names_asc_kind_filtered(
        &self,
        need: usize,
        kind_bm: &RoaringBitmap,
        segments: &[Arc<SegmentReader>],
    ) -> Vec<SymbolMatch> {
        use fst::Streamer as _;
        let mut results = Vec::new();
        let mut stream = self.name_fst.stream();
        while let Some((_name_bytes, encoded)) = stream.next() {
            let slice = self.decode_postings_slice(encoded);
            // Only process rows that are in the kind bitmap.
            for &global_id in slice {
                if kind_bm.contains(global_id) {
                    let Some(ptr) = self.resolve_global(global_id) else {
                        continue;
                    };
                    let Some(seg) = segments.get(ptr.segment_idx as usize) else {
                        continue;
                    };
                    let Some(meta) = self.segments.get(ptr.segment_idx as usize) else {
                        continue;
                    };
                    if let Some(row) = seg.materialize_one_row(ptr.local_row_idx, &meta.source_path)
                    {
                        results.push(row);
                    }
                }
            }
            if results.len() >= need {
                break;
            }
        }
        results
    }
    /// Streams name entries in DESC order using a Bounded Min-Heap over a forward walk of the FST.
    pub(super) fn stream_names_desc(
        &self,
        need: usize,
        segments: &[Arc<SegmentReader>],
    ) -> Vec<SymbolMatch> {
        use fst::Streamer as _;
        use std::collections::BinaryHeap;

        let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::new();
        let mut stream = self.name_fst.stream();

        while let Some((name_bytes, encoded)) = stream.next() {
            let name_str = String::from_utf8_lossy(name_bytes).into_owned();
            let slice = self.decode_postings_slice(encoded);
            for &global_id in slice {
                if let Some(ptr) = self.resolve_global(global_id)
                    && let Some(seg) = segments.get(ptr.segment_idx as usize)
                    && let Some(meta) = self.segments.get(ptr.segment_idx as usize)
                {
                    let line = seg.u32_at("line", ptr.local_row_idx);
                    let path = meta.source_path.to_string_lossy().into_owned();
                    let entry = HeapEntry {
                        name: name_str.clone(),
                        global_id,
                        line,
                        path,
                    };
                    heap.push(entry);
                    if heap.len() > need {
                        let _ = heap.pop();
                    }
                }
            }
        }

        let mut results = Vec::new();
        while let Some(entry) = heap.pop() {
            results.push(entry);
        }
        results.reverse();

        let mut matched_symbols = Vec::new();
        for entry in results {
            if let Some(ptr) = self.resolve_global(entry.global_id)
                && let Some(seg) = segments.get(ptr.segment_idx as usize)
                && let Some(meta) = self.segments.get(ptr.segment_idx as usize)
                && let Some(row) = seg.materialize_one_row(ptr.local_row_idx, &meta.source_path)
            {
                matched_symbols.push(row);
            }
        }
        matched_symbols
    }

    /// Streams kind-filtered name entries in DESC order using a Bounded Min-Heap over a forward walk of the FST.
    pub(super) fn stream_names_desc_kind_filtered(
        &self,
        need: usize,
        kind_bm: &RoaringBitmap,
        segments: &[Arc<SegmentReader>],
    ) -> Vec<SymbolMatch> {
        use fst::Streamer as _;
        use std::collections::BinaryHeap;

        let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::new();
        let mut stream = self.name_fst.stream();

        while let Some((name_bytes, encoded)) = stream.next() {
            let name_str = String::from_utf8_lossy(name_bytes).into_owned();
            let slice = self.decode_postings_slice(encoded);
            for &global_id in slice {
                if kind_bm.contains(global_id)
                    && let Some(ptr) = self.resolve_global(global_id)
                    && let Some(seg) = segments.get(ptr.segment_idx as usize)
                    && let Some(meta) = self.segments.get(ptr.segment_idx as usize)
                {
                    let line = seg.u32_at("line", ptr.local_row_idx);
                    let path = meta.source_path.to_string_lossy().into_owned();
                    let entry = HeapEntry {
                        name: name_str.clone(),
                        global_id,
                        line,
                        path,
                    };
                    heap.push(entry);
                    if heap.len() > need {
                        let _ = heap.pop();
                    }
                }
            }
        }

        let mut results = Vec::new();
        while let Some(entry) = heap.pop() {
            results.push(entry);
        }
        results.reverse();

        let mut matched_symbols = Vec::new();
        for entry in results {
            if let Some(ptr) = self.resolve_global(entry.global_id)
                && let Some(seg) = segments.get(ptr.segment_idx as usize)
                && let Some(meta) = self.segments.get(ptr.segment_idx as usize)
                && let Some(row) = seg.materialize_one_row(ptr.local_row_idx, &meta.source_path)
            {
                matched_symbols.push(row);
            }
        }
        matched_symbols
    }
    fn decode_postings_slice(&self, encoded: u64) -> &[u32] {
        #[allow(clippy::cast_possible_truncation)]
        let count = (encoded & 0xFFFF_FFFF) as usize;
        #[allow(clippy::cast_possible_truncation)]
        let byte_offset = ((encoded >> 32) & 0xFFFF_FFFF) as usize;
        #[allow(clippy::indexing_slicing)]
        let postings = &self.mmap[self.name_postings_range.clone()];
        let end = byte_offset + count * 4;
        if end > postings.len() {
            return &[];
        }
        #[allow(clippy::indexing_slicing)]
        cast_slice::<u8, u32>(&postings[byte_offset..end])
    }

    fn decode_postings(&self, encoded: u64) -> RoaringBitmap {
        self.decode_postings_slice(encoded)
            .iter()
            .copied()
            .collect()
    }
}
// ─────────────────────────────────────────────────────────────────────────────
// Private helpers (extracted from Overlay::open to keep the function ≤ 100 lines)
// ─────────────────────────────────────────────────────────────────────────────

/// Parse TOC entries field-by-field from the mmap.
///
/// `TocEntry` is not `Pod` due to `[u8; 56]` conflicting with
/// `object::pod::Pod`, so reads are done manually.
fn parse_toc_entries(mmap: &[u8], toc_count: usize) -> Result<Vec<TocEntry>> {
    let mut toc = Vec::with_capacity(toc_count);
    for i in 0..toc_count {
        let base = HEADER_LEN + i * TOC_ENTRY_SIZE;
        ensure!(
            base + TOC_ENTRY_SIZE <= mmap.len(),
            "TOC entry {i} out of bounds"
        );
        #[allow(clippy::indexing_slicing)]
        let entry_bytes = &mmap[base..base + TOC_ENTRY_SIZE];
        let mut name = [0u8; TOC_ENTRY_NAME_LEN];
        name.copy_from_slice(&entry_bytes[..TOC_ENTRY_NAME_LEN]);
        let offset = u32::from_le_bytes(
            entry_bytes[TOC_ENTRY_NAME_LEN..TOC_ENTRY_NAME_LEN + 4]
                .try_into()
                .context("TOC entry offset bytes")?,
        );
        let len = u32::from_le_bytes(
            entry_bytes[TOC_ENTRY_NAME_LEN + 4..TOC_ENTRY_NAME_LEN + 8]
                .try_into()
                .context("TOC entry len bytes")?,
        );
        toc.push(TocEntry { name, offset, len });
    }
    Ok(toc)
}

/// Locate all ten named blobs and return their byte ranges in TOC order.
fn find_blob_ranges(toc: &[TocEntry]) -> Result<[Range<usize>; 11]> {
    let find_one = |name: &[u8]| -> Result<Range<usize>> {
        for entry in toc {
            let stored = entry
                .name
                .iter()
                .position(|&b| b == 0)
                .map_or_else(|| entry.name.as_ref(), |n| &entry.name[..n]);
            if stored == name {
                let s = entry.offset as usize;
                return Ok(s..s + entry.len as usize);
            }
        }
        anyhow::bail!(
            "blob {:?} not found in overlay TOC",
            std::str::from_utf8(name).unwrap_or("?")
        )
    };
    Ok([
        find_one(b"row_table")?,
        find_one(b"kind_strings")?,
        find_one(b"kind_index")?,
        find_one(b"bitmap_data")?,
        find_one(b"trigram_index")?,
        find_one(b"name_fst")?,
        find_one(b"name_postings")?,
        find_one(b"segments")?,
        find_one(b"segment_strings")?,
        find_one(b"index_files")?,
        find_one(b"enrich_bitmaps")?,
    ])
}

/// Decode the fixed-size `SegmentRecord` slice into heap-allocated `SegmentMeta` values.
fn decode_segment_metas(
    seg_records: &[SegmentRecord],
    seg_strings: &[u8],
) -> Result<Vec<SegmentMeta>> {
    let mut segments = Vec::with_capacity(seg_records.len());
    for rec in seg_records {
        let path_start = rec.path_offset as usize;
        let path_end = path_start + rec.path_len as usize;
        let hex_start = rec.hex_id_offset as usize;
        let hex_end = hex_start + rec.hex_id_len as usize;
        ensure!(
            path_end <= seg_strings.len() && hex_end <= seg_strings.len(),
            "segment string index out of bounds"
        );
        #[allow(clippy::indexing_slicing)]
        let path_str = std::str::from_utf8(&seg_strings[path_start..path_end])
            .context("segment source path not valid UTF-8")?;
        #[allow(clippy::indexing_slicing)]
        let hex_str = std::str::from_utf8(&seg_strings[hex_start..hex_end])
            .context("segment hex_content_id not valid UTF-8")?;
        segments.push(SegmentMeta {
            hex_content_id: hex_str.to_owned(),
            source_path: PathBuf::from(path_str),
            row_count: rec.row_count,
            dedup_row_count: rec.dedup_row_count,
        });
    }
    Ok(segments)
}

/// Validate that all blob ranges fit within `mmap_len` and that
/// fixed-record blobs have sizes that are multiples of the record size.
fn validate_blob_layout(mmap_len: usize, blobs: &[Range<usize>; TOC_COUNT]) -> Result<()> {
    let [
        row_table_r,
        _,
        kind_index_r,
        _,
        trigram_r,
        _,
        _,
        segments_r,
        _,
        index_files_r,
        _, // enrich_bitmaps: no size constraint
    ] = blobs;
    let max_end = blobs.iter().map(|r| r.end).max().unwrap_or(0);
    ensure!(
        mmap_len >= max_end,
        "overlay file truncated: need {max_end} bytes, got {mmap_len}"
    );
    ensure!(
        row_table_r.len() % std::mem::size_of::<RowPtr>() == 0,
        "row_table blob size not a multiple of RowPtr size"
    );
    ensure!(
        kind_index_r.len() % std::mem::size_of::<KindEntry>() == 0,
        "kind_index blob size not a multiple of KindEntry size"
    );
    ensure!(
        trigram_r.len() % std::mem::size_of::<TrigramEntry>() == 0,
        "trigram_index blob size not a multiple of TrigramEntry size"
    );
    ensure!(
        segments_r.len() % std::mem::size_of::<SegmentRecord>() == 0,
        "segments blob size not a multiple of SegmentRecord size"
    );
    ensure!(
        index_files_r.len() % std::mem::size_of::<u32>() == 0,
        "index_files blob size not a multiple of u32 size"
    );
    let segment_count = segments_r.len() / std::mem::size_of::<SegmentRecord>();
    let file_count = index_files_r.len() / std::mem::size_of::<u32>();
    ensure!(
        segment_count == file_count,
        "mismatched segments and index_files: segment_count={segment_count}, file_count={file_count}"
    );
    Ok(())
}

#[derive(Eq, PartialEq)]
struct HeapEntry {
    name: String,
    global_id: u32,
    line: u32,
    path: String,
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // We want a MIN-heap, meaning the element with the LOWEST rank (first to be discarded in DESC query)
        // should be at the top of the heap.
        // Lowest rank means:
        // 1. Alphabetically smaller name.
        // 2. Or, same name, but larger line.
        // 3. Or, same name, same line, but lexicographically larger path.
        match other.name.cmp(&self.name) {
            std::cmp::Ordering::Equal => match self.line.cmp(&other.line) {
                std::cmp::Ordering::Equal => self.path.cmp(&other.path),
                ord => ord,
            },
            ord => ord,
        }
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::collections::HashMap;
    use std::io::{BufWriter, Write};

    use roaring::RoaringBitmap;

    use super::*;
    use crate::storage::columnar::overlay_writer::{self, write_v3};

    /// Build a minimal FQOV v3 overlay in a tempfile.
    ///
    /// Only `trigram_postings` and `row_count` are populated; all other blobs
    /// are empty or trivially valid.
    fn make_test_overlay(
        trigram_postings: &HashMap<[u8; 3], Vec<u8>>,
        row_count: u32,
    ) -> tempfile::NamedTempFile {
        let empty_fst = fst::MapBuilder::memory()
            .into_inner()
            .expect("empty FST bytes");
        let row_table: Vec<RowPtr> = (0..row_count)
            .map(|i| RowPtr {
                segment_idx: 0,
                local_row_idx: i,
            })
            .collect();

        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        {
            let mut f = BufWriter::new(tmp.as_file());
            write_v3(
                &mut f,
                &overlay_writer::WriteV3Params {
                    generation: 1,
                    global_row_table: &row_table,
                    kind_postings: &HashMap::new(),
                    trigram_postings,
                    name_fst_bytes: &empty_fst,
                    name_postings_bytes: &[],
                    segment_metas: &[],
                    index_files_bytes: &[],
                    enrich_bitmaps_bytes: &[],
                },
            )
            .expect("write_v3");
            f.flush().expect("flush");
        }
        tmp
    }

    /// Attempting to open a non-existent file returns an error (not a panic).
    #[test]
    fn open_missing_file_returns_err() {
        let result = Overlay::open(Path::new("/nonexistent/overlay.bin"));
        assert!(result.is_err(), "expected Err for missing file");
    }

    /// A file with invalid magic returns a descriptive error.
    #[test]
    fn open_wrong_magic_returns_err() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let mut data = vec![0u8; HEADER_LEN];
        data[..4].copy_from_slice(b"XXXX");
        std::fs::write(tmp.path(), &data).expect("write");
        match Overlay::open(tmp.path()) {
            Ok(_) => panic!("expected Err for wrong magic, but got Ok"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("magic"), "error should mention magic: {msg}");
            }
        }
    }

    /// `name_substring_candidates` returns `None` for sub-trigram inputs.
    #[test]
    fn substring_candidates_none_for_short_input() {
        // Non-empty trigram index so we reach the length check.
        let mut trig = HashMap::new();
        let bm: RoaringBitmap = std::iter::once(0u32).collect();
        let mut bm_bytes = Vec::new();
        bm.serialize_into(&mut bm_bytes).unwrap();
        trig.insert(*b"abc", bm_bytes);

        let tmp = make_test_overlay(&trig, 1);
        let overlay = Overlay::open(tmp.path()).expect("open");

        assert!(overlay.name_substring_candidates("ab").is_none());
        assert!(overlay.name_substring_candidates("").is_none());
    }

    /// `name_substring_candidates` intersects per-trigram bitmaps correctly
    /// and short-circuits to `Some(empty)` when a trigram is absent.
    #[test]
    fn substring_candidates_intersects_and_misses() {
        let rows: RoaringBitmap = [0u32, 2].iter().copied().collect();
        let mut trig = HashMap::new();
        for t in [*b"alp", *b"lph", *b"pha"] {
            let mut bytes = Vec::new();
            rows.serialize_into(&mut bytes).unwrap();
            trig.insert(t, bytes);
        }

        let tmp = make_test_overlay(&trig, 3);
        let overlay = Overlay::open(tmp.path()).expect("open");

        // Single trigram "alp" → {0, 2}.
        let got = overlay.name_substring_candidates("alp").expect("some");
        assert_eq!(got.iter().collect::<Vec<_>>(), vec![0u32, 2]);

        // "alpha" trigrams: alp, lph, pha — all present, intersection {0, 2}.
        let got = overlay.name_substring_candidates("alpha").expect("some");
        assert_eq!(got.iter().collect::<Vec<_>>(), vec![0u32, 2]);

        // ASCII case-insensitivity.
        let got = overlay.name_substring_candidates("ALP").expect("some");
        assert_eq!(got.iter().collect::<Vec<_>>(), vec![0u32, 2]);

        // Missing trigram → Some(empty).
        let got = overlay.name_substring_candidates("zzz").expect("some");
        assert!(got.is_empty());
    }
}
