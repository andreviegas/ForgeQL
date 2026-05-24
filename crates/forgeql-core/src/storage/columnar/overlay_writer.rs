//! Writer for the FQOV v3 overlay file format.
//!
//! Replaces the v2 `bincode::serialize` step in [`super::overlay_builder`].
//!
//! The public entry point is [`write_v3`], which accepts pre-built data from
//! the builder pipeline and emits the complete header + TOC + blobs sequence.

use std::collections::HashMap;
use std::io::{self, Write};

use bytemuck::cast_slice;

use super::overlay::{
    HEADER_LEN, HEADER_V3_LEN, KindEntry, MAGIC, RowPtr, SCHEMA_VERSION, SegmentMeta,
    SegmentRecord, TOC_COUNT, TOC_ENTRY_NAME_LEN, TOC_ENTRY_SIZE, TocEntry, TrigramEntry,
};

// Blob name constants (ASCII, ≤ `TOC_ENTRY_NAME_LEN` bytes).
const BLOB_ROW_TABLE: &[u8] = b"row_table";
const BLOB_KIND_STRINGS: &[u8] = b"kind_strings";
const BLOB_KIND_INDEX: &[u8] = b"kind_index";
const BLOB_BITMAP_DATA: &[u8] = b"bitmap_data";
const BLOB_TRIGRAM_INDEX: &[u8] = b"trigram_index";
const BLOB_NAME_FST: &[u8] = b"name_fst";
const BLOB_NAME_POSTINGS: &[u8] = b"name_postings";
const BLOB_SEGMENTS: &[u8] = b"segments";
const BLOB_SEGMENT_STRINGS: &[u8] = b"segment_strings";
const BLOB_INDEX_FILES: &[u8] = b"index_files";
const BLOB_ENRICH_BITMAPS: &[u8] = b"enrich_bitmaps";
const BLOB_FILE_ENTRIES: &[u8] = b"file_entries";

// On-disk header constants as u32, expressed with u32 literals to avoid usize→u32 casts.
// Compile-time assertions below keep these in sync with the usize originals in overlay.rs.
const HEADER_V3_LEN_U32: u32 = 24_u32 + 12_u32 * 64_u32; // = HEADER_LEN + TOC_COUNT * TOC_ENTRY_SIZE
const TOC_COUNT_U32: u32 = 12_u32; // = TOC_COUNT
const _: () = assert!(
    HEADER_V3_LEN_U32 as usize == HEADER_V3_LEN,
    "HEADER_V3_LEN_U32 out of sync with overlay.rs"
);
const _: () = assert!(
    TOC_COUNT_U32 as usize == TOC_COUNT,
    "TOC_COUNT_U32 out of sync with overlay.rs"
);

/// Input parameters for [`write_v3`].
///
/// Groups all pre-serialised data from the builder pipeline to stay under
/// Clippy's argument-count limit.
pub(super) struct WriteV3Params<'a> {
    pub(super) generation: u64,
    pub(super) global_row_table: &'a [RowPtr],
    pub(super) kind_postings: &'a HashMap<String, Vec<u8>>,
    pub(super) trigram_postings: &'a HashMap<[u8; 3], Vec<u8>>,
    pub(super) name_fst_bytes: &'a [u8],
    pub(super) name_postings_bytes: &'a [u8],
    pub(super) segment_metas: &'a [SegmentMeta],
    pub(super) index_files_bytes: &'a [u8],
    /// Serialised enrichment bitmaps blob (Phase 5 / FQOV v7).
    /// Pass `&[]` for older overlays or when no enrichment data is available.
    pub(super) enrich_bitmaps_bytes: &'a [u8],
    /// Serialised file-only entries blob (FQOV v8).
    /// Format: `[u32 count][repeated: [u32 size][u16 path_len][u8; path_len]]`.
    /// Pass `&[]` when no file-only entries are present.
    pub(super) file_entries_bytes: &'a [u8],
}
struct ComputedBlobs {
    kind_strings: Vec<u8>,
    kind_index: Vec<u8>,
    bitmap_data: Vec<u8>,
    trigram_index: Vec<u8>,
    segments: Vec<u8>,
    segment_strings: Vec<u8>,
}

/// Casts `v: usize` to `u32`, returning an `InvalidData` I/O error on overflow.
#[inline]
fn to_u32(v: usize, ctx: &'static str) -> io::Result<u32> {
    u32::try_from(v).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, ctx))
}

/// Casts `v: usize` to `u16`, returning an `InvalidData` I/O error on overflow.
#[inline]
fn to_u16(v: usize, ctx: &'static str) -> io::Result<u16> {
    u16::try_from(v).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, ctx))
}
/// Build the six variable-length blobs that require sorting/layout work.
///
/// The three pass-through blobs (`row_table`, `name_fst`, `name_postings`)
/// are slices of the input data and are assembled in [`write_v3`].
fn compute_blobs(params: &WriteV3Params<'_>) -> io::Result<ComputedBlobs> {
    // kind_strings + kind_index + bitmap_data (kind portion).
    // Entries are sorted by kind name for binary-search at query time.
    let mut kind_strings: Vec<u8> = Vec::new();
    let mut kind_entries: Vec<KindEntry> = Vec::new();
    let mut bitmap_data: Vec<u8> = Vec::new();

    let mut sorted_kinds: Vec<(&String, &Vec<u8>)> = params.kind_postings.iter().collect();
    sorted_kinds.sort_by_key(|(k, _)| k.as_str());

    for (kind_str, bm_bytes) in &sorted_kinds {
        let kind_entry = KindEntry {
            kind_offset: to_u32(kind_strings.len(), "kind strings offset exceeds u32::MAX")?,
            kind_len: to_u32(kind_str.len(), "kind name too long for u32")?,
            bitmap_offset: to_u32(bitmap_data.len(), "bitmap data offset exceeds u32::MAX")?,
            bitmap_len: to_u32(bm_bytes.len(), "kind bitmap too large for u32")?,
        };
        kind_strings.extend_from_slice(kind_str.as_bytes());
        bitmap_data.extend_from_slice(bm_bytes);
        kind_entries.push(kind_entry);
    }
    let kind_index: Vec<u8> = cast_slice(kind_entries.as_slice()).to_vec();

    // trigram_index: sorted by trigram bytes; bitmap_data continues.
    let mut trig_entries: Vec<TrigramEntry> = Vec::new();
    let mut sorted_trigs: Vec<(&[u8; 3], &Vec<u8>)> = params.trigram_postings.iter().collect();
    sorted_trigs.sort_by_key(|(t, _)| **t);

    for (tg, bm_bytes) in &sorted_trigs {
        let mut tg4 = [0u8; 4];
        tg4[..3].copy_from_slice(tg.as_ref());
        trig_entries.push(TrigramEntry {
            trigram: tg4,
            bitmap_offset: to_u32(bitmap_data.len(), "bitmap data offset exceeds u32::MAX")?,
            bitmap_len: to_u32(bm_bytes.len(), "trigram bitmap too large for u32")?,
        });
        bitmap_data.extend_from_slice(bm_bytes);
    }
    let trigram_index: Vec<u8> = cast_slice(trig_entries.as_slice()).to_vec();

    // segments + segment_strings.
    let mut segment_strings: Vec<u8> = Vec::new();
    let mut seg_records: Vec<SegmentRecord> = Vec::new();
    for meta in params.segment_metas {
        let path_bytes = meta.source_path.to_string_lossy();
        let rec = SegmentRecord {
            row_count: meta.row_count,
            path_offset: to_u32(
                segment_strings.len(),
                "segment path offset exceeds u32::MAX",
            )?,
            hex_id_offset: to_u32(
                segment_strings.len() + path_bytes.len(),
                "segment hex-id offset exceeds u32::MAX",
            )?,
            dedup_row_count: meta.dedup_row_count,
            path_len: to_u16(path_bytes.len(), "segment source path too long for u16")?,
            hex_id_len: to_u16(meta.hex_content_id.len(), "hex content ID too long for u16")?,
        };
        segment_strings.extend_from_slice(path_bytes.as_bytes());
        segment_strings.extend_from_slice(meta.hex_content_id.as_bytes());
        seg_records.push(rec);
    }
    let segments: Vec<u8> = cast_slice(seg_records.as_slice()).to_vec();

    Ok(ComputedBlobs {
        kind_strings,
        kind_index,
        bitmap_data,
        trigram_index,
        segments,
        segment_strings,
    })
}

/// Write a complete FQOV v3 overlay file to `out`.
///
/// The caller provides the pre-serialised data from the builder pipeline via
/// [`WriteV3Params`].  All blobs are written after a fixed
/// `HEADER_LEN + TOC_COUNT × TOC_ENTRY_SIZE` = 600-byte header+TOC region.
/// TOC offsets are absolute from the start of the file.
///
/// # Errors
/// Propagates I/O errors from `out`.
pub(super) fn write_v3(out: &mut impl Write, params: &WriteV3Params<'_>) -> io::Result<()> {
    let blobs = compute_blobs(params)?;

    let row_table_blob: &[u8] = cast_slice(params.global_row_table);
    // Blobs read via `cast_slice` require 4-byte alignment within the mmap;
    // every blob is aligned to 4 bytes regardless of size.
    let named_blobs: [(&[u8], &[u8]); TOC_COUNT] = [
        (BLOB_ROW_TABLE, row_table_blob),
        (BLOB_KIND_STRINGS, &blobs.kind_strings),
        (BLOB_KIND_INDEX, &blobs.kind_index),
        (BLOB_BITMAP_DATA, &blobs.bitmap_data),
        (BLOB_TRIGRAM_INDEX, &blobs.trigram_index),
        (BLOB_NAME_FST, params.name_fst_bytes),
        (BLOB_NAME_POSTINGS, params.name_postings_bytes),
        (BLOB_SEGMENTS, &blobs.segments),
        (BLOB_SEGMENT_STRINGS, &blobs.segment_strings),
        (BLOB_INDEX_FILES, params.index_files_bytes),
        (BLOB_ENRICH_BITMAPS, params.enrich_bitmaps_bytes),
        (BLOB_FILE_ENTRIES, params.file_entries_bytes),
    ];

    // ── Compute TOC offsets ───────────────────────────────────────────────
    let mut current_offset: u32 = HEADER_V3_LEN_U32;
    let mut toc = [TocEntry {
        name: [0u8; TOC_ENTRY_NAME_LEN],
        offset: 0,
        len: 0,
    }; TOC_COUNT];
    for (i, (name, data)) in named_blobs.iter().enumerate() {
        debug_assert!(name.len() <= TOC_ENTRY_NAME_LEN, "blob name too long");
        let aligned = (current_offset + 3) & !3;
        toc[i].name[..name.len()].copy_from_slice(name);
        toc[i].offset = aligned;
        let data_len = u32::try_from(data.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "overlay blob too large for u32 offsets",
            )
        })?;
        toc[i].len = data_len;
        current_offset = aligned.checked_add(data_len).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "overlay file too large for u32 offsets",
            )
        })?;
    }

    // ── Write fixed 24-byte header ────────────────────────────────────────
    out.write_all(&MAGIC)?;
    out.write_all(&SCHEMA_VERSION.to_le_bytes())?;
    out.write_all(&params.generation.to_le_bytes())?;
    out.write_all(&TOC_COUNT_U32.to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?; // reserved

    debug_assert_eq!(
        HEADER_LEN + TOC_COUNT * TOC_ENTRY_SIZE,
        HEADER_V3_LEN,
        "HEADER_V3_LEN invariant"
    );

    // ── Write TOC (11 × 64 bytes = 704 bytes) ────────────────────────────
    // Field-by-field because TocEntry is not `Pod` (the `[u8; 56]` name
    // field conflicts with `object::pod::Pod` in the dependency graph).
    for entry in &toc {
        out.write_all(&entry.name)?;
        out.write_all(&entry.offset.to_le_bytes())?;
        out.write_all(&entry.len.to_le_bytes())?;
    }

    // ── Write blobs (with alignment padding) ─────────────────────────────
    let mut file_pos: u32 = HEADER_V3_LEN_U32;
    for (i, (_, data)) in named_blobs.iter().enumerate() {
        let target = toc[i].offset;
        if target > file_pos {
            let pad = vec![0u8; (target - file_pos) as usize];
            out.write_all(&pad)?;
        }
        out.write_all(data)?;
        file_pos = target + toc[i].len;
    }

    Ok(())
}
