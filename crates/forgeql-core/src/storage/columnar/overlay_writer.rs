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
    KindEntry, MAGIC, SCHEMA_VERSION, RowPtr, SegmentMeta, SegmentRecord, TocEntry,
    TrigramEntry, HEADER_LEN, TOC_COUNT, TOC_ENTRY_NAME_LEN, TOC_ENTRY_SIZE, HEADER_V3_LEN,
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

/// Write a complete FQOV v3 overlay file to `out`.
///
/// The caller provides the pre-serialised data from the builder pipeline.
/// `kind_postings` and `trigram_postings` map kind/trigram keys to
/// already-serialised `RoaringBitmap` bytes.
///
/// All blobs are written after a fixed
/// `HEADER_LEN + TOC_COUNT × TOC_ENTRY_SIZE` = 600-byte header+TOC region.
/// TOC offsets are absolute from the start of the file.
///
/// # Errors
/// Propagates I/O errors from `out`.
pub(super) fn write_v3(
    out: &mut impl Write,
    generation: u64,
    global_row_table: &[RowPtr],
    kind_postings: &HashMap<String, Vec<u8>>,
    trigram_postings: &HashMap<[u8; 3], Vec<u8>>,
    name_fst_bytes: &[u8],
    name_postings_bytes: &[u8],
    segment_metas: &[SegmentMeta],
) -> io::Result<()> {
    // ── 1. Build blobs ────────────────────────────────────────────────────

    // row_table: flat cast of RowPtr slice.
    let row_table_blob: &[u8] = cast_slice(global_row_table);

    // kind_strings + kind_index + bitmap_data (kind portion).
    // Entries are sorted by kind name for binary-search at query time.
    let mut kind_strings: Vec<u8> = Vec::new();
    let mut kind_entries: Vec<KindEntry> = Vec::new();
    let mut bitmap_data: Vec<u8> = Vec::new();

    let mut sorted_kinds: Vec<(&String, &Vec<u8>)> = kind_postings.iter().collect();
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
    let kind_index_blob: Vec<u8> = cast_slice(kind_entries.as_slice()).to_vec();

    // trigram_index: sorted by trigram bytes; bitmap_data continues.
    let mut trig_entries: Vec<TrigramEntry> = Vec::new();
    let mut sorted_trigs: Vec<(&[u8; 3], &Vec<u8>)> = trigram_postings.iter().collect();
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
    let trigram_index_blob: Vec<u8> = cast_slice(trig_entries.as_slice()).to_vec();

    // segments + segment_strings.
    let mut segment_strings: Vec<u8> = Vec::new();
    let mut seg_records: Vec<SegmentRecord> = Vec::new();
    for meta in segment_metas {
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
    let segments_blob: Vec<u8> = cast_slice(seg_records.as_slice()).to_vec();

    // ── 2. Compute TOC offsets (4-byte aligned) ───────────────────────────
    // Blobs that are read via `cast_slice` (KindEntry, TrigramEntry, SegmentRecord,
    // RowPtr) require 4-byte alignment within the mmap.  We align every blob
    // to 4 bytes so the invariant holds regardless of blob order or size.
    let named_blobs: [(&[u8], &[u8]); TOC_COUNT] = [
        (BLOB_ROW_TABLE,       row_table_blob),
        (BLOB_KIND_STRINGS,    &kind_strings),
        (BLOB_KIND_INDEX,      &kind_index_blob),
        (BLOB_BITMAP_DATA,     &bitmap_data),
        (BLOB_TRIGRAM_INDEX,   &trigram_index_blob),
        (BLOB_NAME_FST,        name_fst_bytes),
        (BLOB_NAME_POSTINGS,   name_postings_bytes),
        (BLOB_SEGMENTS,        &segments_blob),
        (BLOB_SEGMENT_STRINGS, &segment_strings),
    ];

    let mut current_offset: u32 = HEADER_V3_LEN as u32;
    let mut toc = [TocEntry { name: [0u8; TOC_ENTRY_NAME_LEN], offset: 0, len: 0 }; TOC_COUNT];
    for (i, (name, data)) in named_blobs.iter().enumerate() {
        debug_assert!(name.len() <= TOC_ENTRY_NAME_LEN, "blob name too long");
        // Align offset to 4 bytes.
        let aligned = (current_offset + 3) & !3;
        toc[i].name[..name.len()].copy_from_slice(name);
        toc[i].offset = aligned;
        #[allow(clippy::cast_possible_truncation)]
        { toc[i].len = data.len() as u32; }
        current_offset = aligned.checked_add(data.len() as u32).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "overlay file too large for u32 offsets",
            )
        })?;
    }

    // ── 3. Write fixed 24-byte header ─────────────────────────────────────
    out.write_all(&MAGIC)?;
    out.write_all(&SCHEMA_VERSION.to_le_bytes())?;
    out.write_all(&generation.to_le_bytes())?;
    out.write_all(&(TOC_COUNT as u32).to_le_bytes())?;
    out.write_all(&0u32.to_le_bytes())?; // reserved

    debug_assert_eq!(
        HEADER_LEN + TOC_COUNT * TOC_ENTRY_SIZE,
        HEADER_V3_LEN,
        "HEADER_V3_LEN invariant"
    );

    // ── 4. Write TOC (9 × 64 bytes = 576 bytes) ───────────────────────────
    // Write field-by-field because TocEntry is not `Pod` (the `[u8; 56]` name
    // field conflicts with `object::pod::Pod` in the dependency graph).
    for entry in &toc {
        out.write_all(&entry.name)?;
        out.write_all(&entry.offset.to_le_bytes())?;
        out.write_all(&entry.len.to_le_bytes())?;
    }

    // ── 5. Write blobs (with alignment padding) ───────────────────────────
    // Track the current write position so we can emit exactly the right
    // number of zero pad bytes before each blob.
    let mut file_pos: u32 = HEADER_V3_LEN as u32;
    for (i, (_, data)) in named_blobs.iter().enumerate() {
        let target = toc[i].offset;
        if target > file_pos {
            // Emit alignment padding.
            let pad = vec![0u8; (target - file_pos) as usize];
            out.write_all(&pad)?;
        }
        out.write_all(data)?;
        file_pos = target + toc[i].len;
    }

    Ok(())
}
