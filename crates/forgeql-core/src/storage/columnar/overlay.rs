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
use anyhow::{Context, Result};
use bytemuck::cast_slice;
use fst::Map as FstMap;
use memmap2::{Mmap, MmapOptions};
use roaring::RoaringBitmap;

mod format;
pub use format::*;
mod parse;
use parse::{
    build_segment_offsets, decode_segment_metas, open_blobs, parse_enrich_index,
    parse_file_entries, parse_header,
};

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
    /// Pre-parsed enrichment index: sorted `(key, bitmap_mmap_range)` pairs.
    ///
    /// key = `"field=value"` (byte-lexicographic order).
    /// The mmap range refers to the serialised `RoaringBitmap` within `mmap`.
    enrich_index: Vec<(String, Range<usize>)>,
    /// All non-indexed workspace files tracked for `FIND files` fast-path.
    ///
    /// Parsed at `open()` time from the `file_entries` blob (FQOV v8+).
    /// Each entry is `(relative_path, file_size_bytes)`.
    file_entries: Vec<(PathBuf, u32)>,

    /// True when the segment list contains two or more entries with the same
    /// `source_path`.  When true, row-count-based fast-paths (GROUP BY file,
    /// GROUP BY kind) are unsafe because the overlay includes duplicate rows
    /// that the normal query pipeline eliminates via deduplication.
    has_duplicate_paths: bool,

    /// Zero-copy FST backed by a slice of the mmap.
    name_fst: FstMap<MmapSlice>,

    /// Zero-copy usages-count FST (FQOV v14, BUG-006 U3): symbol name →
    /// total usage-site count across all segments. `None` when the blob is
    /// zero-length (no usage postings anywhere in the workspace).
    usages_count_fst: Option<FstMap<MmapSlice>>,

    /// Reverse-lookup index: `(sha256, seg_idx)` pairs sorted by `sha256`.
    ///
    /// Binary-search this array to find a segment by its node_id hex prefix
    /// in O(log N) time with zero heap allocation.  Built at open time from
    /// `segments[i].sha256`.  Shared across all concurrent sessions via
    /// `Arc<Overlay>` — no per-session cost.
    seg_id_index: Vec<([u8; 32], u32)>,
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
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening overlay {}", path.display()))?;
        #[expect(unsafe_code, reason = "read-only mmap of an immutable overlay file")]
        let mmap: Arc<Mmap> = Arc::new(
            unsafe { MmapOptions::new().map(&file) }
                .with_context(|| format!("mmap overlay {}", path.display()))?,
        );
        drop(file);

        let toc_count = parse_header(&mmap)
            .with_context(|| format!("invalid overlay header in {}", path.display()))?;

        let blobs = open_blobs(&mmap, toc_count)
            .with_context(|| format!("parsing overlay TOC in {}", path.display()))?;
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
            file_entries_range,
            usages_count_fst_range,
        ] = blobs;

        let row_count = u32::try_from(row_table_range.len() / std::mem::size_of::<RowPtr>())
            .context("row count overflow")?;

        // Decode segment metadata at open time.  SHA-256 and prefix lengths
        // are computed here over all paths in one pass — no global lock.
        let seg_records: &[SegmentRecord] =
            cast_slice(mmap.get(segments_range).context("segments blob")?);
        let segments = decode_segment_metas(
            seg_records,
            mmap.get(segment_strings_range)
                .context("segment_strings blob")?,
        )?;

        // Build the reverse-lookup index: (sha256, seg_idx) sorted by sha256.
        // Shared via Arc<Overlay> — zero cost per concurrent session.
        let mut seg_id_index: Vec<([u8; 32], u32)> = segments
            .iter()
            .enumerate()
            .filter_map(|(i, m)| u32::try_from(i).ok().map(|idx| (m.sha256, idx)))
            .collect();
        seg_id_index.sort_unstable_by_key(|(h, _)| *h);

        // Build prefix-sum table: segment_offsets[i] = first global row ID for
        // segment i; segment_offsets[n] = row_count (one-past-the-end sentinel).
        let segment_offsets = build_segment_offsets(&segments);

        // Build the zero-copy name FST backed by a mmap slice.
        let name_fst = FstMap::new(MmapSlice::new(
            Arc::clone(&mmap),
            name_fst_range.start,
            name_fst_range.end,
        ))
        .context("loading name FST from overlay")?;

        // Usages-count FST (FQOV v14, BUG-006 U3): zero-length blob means no
        // segment carries usage postings.
        let usages_count_fst = if usages_count_fst_range.is_empty() {
            None
        } else {
            Some(
                FstMap::new(MmapSlice::new(
                    Arc::clone(&mmap),
                    usages_count_fst_range.start,
                    usages_count_fst_range.end,
                ))
                .context("loading usages-count FST from overlay")?,
            )
        };

        // Detect duplicate source paths — signals that the overlay contains
        // redundant rows that the query pipeline deduplicates but raw counts
        // (row_count, kind bitmap lengths) do not.
        let has_duplicate_paths = {
            let mut seen = std::collections::HashSet::with_capacity(segments.len());
            segments.iter().any(|s| !seen.insert(&s.source_path))
        };

        // Parse file_entries blob (FQOV v8): non-indexed workspace files.
        // Format: [u32 count][repeated: [u32 size][u16 path_len][u8; path_len]]
        let file_entries = parse_file_entries(mmap.get(file_entries_range).unwrap_or(&[]));

        // Parse enrichment bitmaps blob (Phase 5 / FQOV v7).
        // Build enrich_index: sorted (key, bitmap_mmap_range) pairs.
        let enrich_index = parse_enrich_index(
            mmap.get(enrich_bitmaps_range.clone()).unwrap_or(&[]),
            enrich_bitmaps_range.start,
        );

        Ok(Arc::new(Self {
            mmap,
            segments,
            row_count,
            segment_offsets,
            row_table_range,
            kind_strings_range,
            kind_index_range,
            bitmap_data_range,
            trigram_index_range,
            name_postings_range,
            index_files_range,
            enrich_index,
            file_entries,
            has_duplicate_paths,
            name_fst,
            usages_count_fst,
            seg_id_index,
        }))
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

    /// Non-indexed workspace files tracked in this overlay (FQOV v8+).
    ///
    /// Each entry is `(relative_path, file_size_bytes)`.  These are workspace
    /// files that have no symbol segment (images, docs, scripts, build outputs,
    /// …).  They carry only path + size information and complement
    /// [`Self::segments`] for `FIND files` queries.
    ///
    /// Returns an empty slice for overlays built before FQOV v8.
    #[must_use]
    pub fn file_entries(&self) -> &[(PathBuf, u32)] {
        &self.file_entries
    }

    /// Retrieve the cached file size for segment `idx`.
    #[must_use]
    pub fn file_size(&self, idx: usize) -> u32 {
        let slice: &[u32] =
            cast_slice(self.mmap.get(self.index_files_range.clone()).unwrap_or(&[]));
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
        RoaringBitmap::deserialize_from(self.mmap.get(range.clone())?).ok()
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
        for (key, range) in self.enrich_index.get(pos..).unwrap_or(&[]) {
            if !key.starts_with(&prefix) {
                break;
            }
            let value_str = key.get(prefix.len()..).unwrap_or("");
            if let Ok(v) = value_str.parse::<i64>()
                && v >= threshold
            {
                let Some(bm_bytes) = self.mmap.get(range.clone()) else {
                    continue;
                };
                if let Ok(bm) = RoaringBitmap::deserialize_from(bm_bytes) {
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
        for (key, range) in self.enrich_index.get(pos..).unwrap_or(&[]) {
            if !key.starts_with(&prefix) {
                break;
            }
            let value_str = key.get(prefix.len()..).unwrap_or("");
            if let Ok(v) = value_str.parse::<i64>()
                && v <= threshold
            {
                let Some(bm_bytes) = self.mmap.get(range.clone()) else {
                    continue;
                };
                if let Ok(bm) = RoaringBitmap::deserialize_from(bm_bytes) {
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
        let start = self.segment_offsets.get(seg_idx).copied().unwrap_or(0);
        let end = self.segment_offsets.get(seg_idx + 1).copied().unwrap_or(0);
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
        let start = self
            .segment_offsets
            .get(seg_range.start)
            .copied()
            .unwrap_or(0);
        let end = self
            .segment_offsets
            .get(seg_range.end)
            .copied()
            .unwrap_or(0);
        start..end
    }

    /// Find the segment index for a `node_id` hex prefix (e.g. `"39f52a1107c4"`).
    ///
    /// Decodes the hex string to raw bytes and binary-searches `seg_id_index`
    /// in O(log N) time with zero heap allocation.  Returns `None` when the
    /// prefix does not match any segment or when `hex_prefix` is malformed.
    ///
    /// Used by `FIND NODE` (Phase B) and future node-addressed mutation commands.
    #[must_use]
    pub fn seg_idx_for_node_id_prefix(&self, hex_prefix: &str) -> Option<u32> {
        let byte_len = hex_prefix.len() / 2;
        if byte_len == 0 || byte_len > 32 || !hex_prefix.len().is_multiple_of(2) {
            return None;
        }
        let mut prefix_bytes = [0u8; 32];
        for (i, chunk) in hex_prefix.as_bytes().chunks(2).enumerate() {
            let s = std::str::from_utf8(chunk).ok()?;
            prefix_bytes[i] = u8::from_str_radix(s, 16).ok()?;
        }
        let pos = self
            .seg_id_index
            .partition_point(|(h, _)| h[..byte_len] < prefix_bytes[..byte_len]);
        let (h, idx) = self.seg_id_index.get(pos)?;
        if h[..byte_len] == prefix_bytes[..byte_len] {
            Some(*idx)
        } else {
            None
        }
    }

    /// Every segment source path whose path-SHA-256 starts with `hex_prefix`.
    ///
    /// The single-hit [`Overlay::seg_idx_for_node_id_prefix`] stops at the first
    /// match; a bare-hex (whole-file) handle must instead be able to see a
    /// second match and refuse to guess, so this walks forward from the binary
    /// search position while the prefix still matches.
    #[must_use]
    pub fn seg_paths_for_node_id_prefix(&self, hex_prefix: &str) -> Vec<PathBuf> {
        let byte_len = hex_prefix.len() / 2;
        if byte_len == 0 || byte_len > 32 || !hex_prefix.len().is_multiple_of(2) {
            return Vec::new();
        }
        let mut prefix_bytes = [0u8; 32];
        for (i, chunk) in hex_prefix.as_bytes().chunks(2).enumerate() {
            let Ok(s) = std::str::from_utf8(chunk) else {
                return Vec::new();
            };
            match u8::from_str_radix(s, 16) {
                Ok(b) => prefix_bytes[i] = b,
                Err(_) => return Vec::new(),
            }
        }
        let pos = self
            .seg_id_index
            .partition_point(|(h, _)| h[..byte_len] < prefix_bytes[..byte_len]);
        self.seg_id_index[pos..]
            .iter()
            .take_while(|(h, _)| h[..byte_len] == prefix_bytes[..byte_len])
            .filter_map(|(_, idx)| self.segments.get(*idx as usize))
            .map(|meta| meta.source_path.clone())
            .collect()
    }
    /// Decode and return the global-row-id bitmap for a given `fql_kind`.
    ///
    /// Binary-searches the sorted `kind_index` blob and deserialises the
    /// `RoaringBitmap` from `bitmap_data` on demand.
    /// Returns `None` if the kind is absent from the overlay.
    #[must_use]
    pub fn prefilter_kind(&self, kind: &str) -> Option<RoaringBitmap> {
        let entries: &[KindEntry] =
            cast_slice(self.mmap.get(self.kind_index_range.clone()).unwrap_or(&[]));
        let kind_strings = self
            .mmap
            .get(self.kind_strings_range.clone())
            .unwrap_or(&[]);
        let kind_bytes = kind.as_bytes();

        // Binary search: entries are sorted by kind string (established at build time).
        let idx = entries.partition_point(|e| {
            let s_start = e.kind_offset as usize;
            let s_end = s_start + e.kind_len as usize;
            kind_strings
                .get(s_start..s_end)
                .is_none_or(|s| s < kind_bytes)
        });

        let e = entries.get(idx)?;
        let s_start = e.kind_offset as usize;
        let s_end = s_start + e.kind_len as usize;
        let s = kind_strings.get(s_start..s_end)?;
        if s != kind_bytes {
            return None;
        }

        let bitmap_data = self.mmap.get(self.bitmap_data_range.clone()).unwrap_or(&[]);
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
        let entries: &[KindEntry] =
            cast_slice(self.mmap.get(self.kind_index_range.clone()).unwrap_or(&[]));
        let kind_strings = self
            .mmap
            .get(self.kind_strings_range.clone())
            .unwrap_or(&[]);
        let bitmap_data = self.mmap.get(self.bitmap_data_range.clone()).unwrap_or(&[]);

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

    /// Total usage-site count for `name` across all segments (BUG-006 U3).
    ///
    /// Read from the `usages_count_fst` overlay blob. Returns 0 when the
    /// overlay carries no usage postings or the name never occurs as a
    /// usage site.
    #[must_use]
    pub fn usage_count(&self, name: &str) -> u64 {
        self.usages_count_fst
            .as_ref()
            .and_then(|fst| fst.get(name.as_bytes()))
            .unwrap_or(0)
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

        let entries: &[TrigramEntry] = cast_slice(
            self.mmap
                .get(self.trigram_index_range.clone())
                .unwrap_or(&[]),
        );
        if entries.is_empty() {
            return None;
        }

        let bitmap_data = self.mmap.get(self.bitmap_data_range.clone()).unwrap_or(&[]);

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
        let row_ptrs: &[RowPtr] =
            cast_slice(self.mmap.get(self.row_table_range.clone()).unwrap_or(&[]));
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
        let count = u32::try_from(encoded & u64::from(u32::MAX)).unwrap_or(0) as usize;
        let byte_offset =
            u32::try_from((encoded >> 32) & u64::from(u32::MAX)).unwrap_or(0) as usize;
        let postings = self
            .mmap
            .get(self.name_postings_range.clone())
            .unwrap_or(&[]);
        let end = byte_offset + count * 4;
        if end > postings.len() {
            return &[];
        }
        postings
            .get(byte_offset..end)
            .map_or(&[], cast_slice::<u8, u32>)
    }

    fn decode_postings(&self, encoded: u64) -> RoaringBitmap {
        self.decode_postings_slice(encoded)
            .iter()
            .copied()
            .collect()
    }
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
                    file_entries_bytes: &[],
                    usages_count_fst_bytes: &[],
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
                let msg = format!("{e:#}");
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
