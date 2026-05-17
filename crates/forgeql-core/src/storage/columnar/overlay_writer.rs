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

// Type-narrowed copies of usize constants — used in the on-disk header.
// Const-context casts are compile-time validated; overflow is a compile error.
#[allow(clippy::cast_possible_truncation)]
const HEADER_V3_LEN_U32: u32 = HEADER_V3_LEN as u32; // = 600
#[allow(clippy::cast_possible_truncation)]
const TOC_COUNT_U32: u32 = TOC_COUNT as u32; // = 9

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
}

/// Intermediate blobs built from the write parameters before TOC/header serialisation.
struct ComputedBlobs {
    kind_strings: Vec<u8>,
    kind_index: Vec<u8>,
    bitmap_data: Vec<u8>,
    trigram_index: Vec<u8>,
    segments: Vec<u8>,
    segment_strings: Vec<u8>,
}

/// Build the six variable-length blobs that require sorting/layout work.
///
/// The three pass-through blobs (`row_table`, `name_fst`, `name_postings`)
/// are slices of the input data and are assembled in [`write_v3`].
fn compute_blobs(params: &WriteV3Params<'_>) -> ComputedBlobs {
    // kind_strings + kind_index + bitmap_data (kind portion).
    // Entries are sorted by kind name for binary-search at query time.
    let mut kind_strings: Vec<u8> = Vec::new();
    let mut kind_entries: Vec<KindEntry> = Vec::new();
    let mut bitmap_data: Vec<u8> = Vec::new();

    let mut sorted_kinds: Vec<(&String, &Vec<u8>)> = params.kind_postings.iter().collect();
    sorted_kinds.sort_by_key(|(k, _)| k.as_str());

    for (kind_str, bm_bytes) in &sorted_kinds {
        #[allow(clippy::cast_possible_truncation)]
        let kind_entry = KindEntry {
            kind_offset: kind_strings.len() as u32,
            kind_len: kind_str.len() as u32,
            bitmap_offset: bitmap_data.len() as u32,
            bitmap_len: bm_bytes.len() as u32,
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
        #[allow(clippy::cast_possible_truncation)]
        trig_entries.push(TrigramEntry {
            trigram: tg4,
            bitmap_offset: bitmap_data.len() as u32,
            bitmap_len: bm_bytes.len() as u32,
        });
        bitmap_data.extend_from_slice(bm_bytes);
    }
    let trigram_index: Vec<u8> = cast_slice(trig_entries.as_slice()).to_vec();

    // segments + segment_strings.
    let mut segment_strings: Vec<u8> = Vec::new();
    let mut seg_records: Vec<SegmentRecord> = Vec::new();
    for meta in params.segment_metas {
        let path_bytes = meta.source_path.to_string_lossy();
        #[allow(clippy::cast_possible_truncation)]
        let rec = SegmentRecord {
            row_count: meta.row_count,
            path_offset: segment_strings.len() as u32,
            path_len: path_bytes.len() as u16,
            hex_id_offset: (segment_strings.len() + path_bytes.len()) as u32,
            hex_id_len: meta.hex_content_id.len() as u16,
        };
        segment_strings.extend_from_slice(path_bytes.as_bytes());
        segment_strings.extend_from_slice(meta.hex_content_id.as_bytes());
        seg_records.push(rec);
    }
    let segments: Vec<u8> = cast_slice(seg_records.as_slice()).to_vec();

    ComputedBlobs {
        kind_strings,
        kind_index,
        bitmap_data,
        trigram_index,
        segments,
        segment_strings,
    }
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
    let blobs = compute_blobs(params);

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

    // ── Write TOC (9 × 64 bytes = 576 bytes) ─────────────────────────────
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
