//! On-disk format records and constants for the columnar overlay.
//!
//! Split out of `overlay.rs`: the fixed-size `#[repr(C)]` `Pod` record types,
//! the schema/layout constants, and the heap-decoded [`SegmentMeta`].

use std::path::PathBuf;

use bytemuck::{Pod, Zeroable};
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
/// - **8**: `file_entries` blob added; all non-indexed workspace files (images,
///   docs, build artefacts, …) are tracked as path+size pairs so that
///   `FIND files WHERE extension = 'cmake'` (and any other type) can use the
///   overlay fast path without a filesystem walk.
/// - **9**: no layout change — content invalidation. The language registry
///   gained the structured-text formats (XML family, DBC, INI, justfile,
///   Make, CMake, reStructuredText in 0.87–0.91), so overlays built by older
///   binaries are missing every row those files now contribute. Bumping the
///   version forces `warm_or_open` to rebuild them with the full registry.
/// - **10**: no layout change — content invalidation. CMake and Make gained
///   `control_flow` config sections, so `if()`/`foreach()`/`while()` blocks
///   and Make conditionals now emit addressable control-flow rows that v9
///   overlays are missing.
/// - **11**: no layout change — content invalidation. Control-flow rows from
///   grammars without a `condition` field (CMake, Make) were emitted nameless
///   in v10 (unfindable by FIND); they are now named by the construct's first
///   line.
/// - **12**: no layout change — content invalidation for ENRICH_VER 22
///   (BUG-019: C/Rust shift rows gain `fql_kind = "shift_expression"`).
pub(crate) const SCHEMA_VERSION: u32 = 12;

/// Number of bytes in the fixed header (before the TOC).
pub(crate) const HEADER_LEN: usize = 24;

/// Byte size of one TOC entry (matches FQSF `TOC_ENTRY_SIZE`).
pub(crate) const TOC_ENTRY_SIZE: usize = 64;

/// Max byte length of a blob name within a `TocEntry`.
pub(crate) const TOC_ENTRY_NAME_LEN: usize = 56;

/// Number of named blobs in an FQOV v8 file (11 original + `file_entries`).
pub(crate) const TOC_COUNT: usize = 12;

/// Total byte size of the header + TOC region (= 24 + 12 * 64 = 792).
pub(crate) const HEADER_V3_LEN: usize = HEADER_LEN + TOC_COUNT * TOC_ENTRY_SIZE;

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
pub(crate) struct KindEntry {
    /// Byte offset into the `kind_strings` blob.
    pub(crate) kind_offset: u32,
    /// Byte length of the kind name.
    pub(crate) kind_len: u32,
    /// Byte offset into the `bitmap_data` blob.
    pub(crate) bitmap_offset: u32,
    /// Byte length of the serialised `RoaringBitmap`.
    pub(crate) bitmap_len: u32,
}

/// FQOV v3: one entry in the `trigram_index` blob, sorted by trigram bytes.
///
/// `trigram[0..3]` holds the actual trigram; `trigram[3]` is reserved = 0
/// (provides 4-byte alignment without an explicit pad field).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct TrigramEntry {
    /// Bytes 0–2: trigram; byte 3: reserved = 0.
    pub(crate) trigram: [u8; 4],
    /// Byte offset into the `bitmap_data` blob.
    pub(crate) bitmap_offset: u32,
    /// Byte length of the serialised `RoaringBitmap`.
    pub(crate) bitmap_len: u32,
}

/// FQOV v3: fixed-size metadata record for one segment in the `segments` blob.
///
/// Strings are resolved from `segment_strings` at open time.
/// Fields ordered to pack two `u16`s at the end — 20 bytes total, no gaps.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct SegmentRecord {
    pub(crate) row_count: u32,
    /// Byte offset of the source-path string in `segment_strings`.
    pub(crate) path_offset: u32,
    /// Byte offset of the hex content-ID string in `segment_strings`.
    pub(crate) hex_id_offset: u32,
    /// Number of unique (name, fql_kind, line) tuples in this segment.
    ///
    /// Used by the GROUP BY file fast-path to return deduplicated symbol counts
    /// without materialising individual rows.  Added in SCHEMA_VERSION 5.
    pub(crate) dedup_row_count: u32,
    /// Byte length of the source-path string.
    pub(crate) path_len: u16,
    /// Byte length of the hex content-ID string (≤ 40 for SHA-1 hex).
    pub(crate) hex_id_len: u16,
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
pub(crate) struct EnrichEntry {
    /// Byte offset into the key-strings region of the `enrich_bitmaps` blob.
    pub(crate) key_offset: u32,
    /// Byte length of the key string (= `"field=value".len()`).
    pub(crate) key_len: u16,
    /// Reserved, must be zero.
    pub(crate) _pad: u16,
    /// Byte offset into the bitmap-data region of the `enrich_bitmaps` blob.
    pub(crate) bitmap_offset: u32,
    /// Byte length of the serialised `RoaringBitmap`.
    pub(crate) bitmap_len: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// Heap-allocated segment metadata (decoded at open time)
// ─────────────────────────────────────────────────────────────────────────────

/// Per-segment metadata stored in the overlay's segment table.
///
/// Decoded from the overlay at open time.  `sha256` and `prefix_len` are
/// derived from `source_path` — no extra on-disk storage required.
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
    /// Full SHA-256 of the normalized `source_path` string.
    ///
    /// Computed once in `decode_segment_metas()` from the path already present
    /// in the overlay.  Never recomputed at query time.
    pub sha256: [u8; 32],
    /// Number of lowercase-hex characters of `sha256` needed to uniquely
    /// identify this segment among all segments in the overlay.
    ///
    /// Minimum 12, grows by 2 on collision.  Used by `segment_id()` and
    /// `node_id()` without any further computation.
    pub prefix_len: u8,
}

impl SegmentMeta {
    /// The display `segment_id` string — the shortest unambiguous hex prefix
    /// of this segment's SHA-256 path hash.
    ///
    /// Zero cost at query time: reads pre-computed fields, formats hex only.
    /// No SHA-256, no lock, no global state.
    #[must_use]
    pub fn segment_id(&self) -> String {
        crate::node_id::hex_prefix(&self.sha256, self.prefix_len)
    }

    /// Build a `node_id` for the symbol at `ordinal` within this segment.
    ///
    /// Format: `n{segment_id}.{ordinal:04}`.  Zero cost beyond string formatting.
    #[must_use]
    pub fn node_id(&self, ordinal: u32) -> String {
        crate::node_id::format_node_id(&self.sha256, self.prefix_len, ordinal)
    }
}
