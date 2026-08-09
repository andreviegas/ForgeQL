//! Open-time parsing helpers for the columnar overlay.
//!
//! Free functions extracted from `Overlay::open` to keep it under the line
//! limit: header / TOC / blob-range parsing and segment-metadata decoding.

use std::ops::Range;
use std::path::PathBuf;

use super::{
    EnrichEntry, HEADER_LEN, KindEntry, MAGIC, RowPtr, SCHEMA_VERSION, SegmentMeta, SegmentRecord,
    TOC_COUNT, TOC_ENTRY_NAME_LEN, TOC_ENTRY_SIZE, TocEntry, TrigramEntry,
};
use anyhow::{Context, Result, ensure};
use bytemuck::cast_slice;

/// Parse the fixed-size FQOV v3 file header; return the TOC entry count.
pub(super) fn parse_header(mmap: &[u8]) -> Result<usize> {
    ensure!(mmap.len() >= HEADER_LEN, "overlay file too short");
    ensure!(
        mmap.get(..4).is_some_and(|b| b == MAGIC),
        "invalid overlay magic"
    );
    let schema_version = u32::from_le_bytes(
        mmap.get(4..8)
            .context("header too short for schema_version")?
            .try_into()
            .context("schema_version bytes")?,
    );
    ensure!(
        schema_version == SCHEMA_VERSION,
        "overlay schema version mismatch: expected {SCHEMA_VERSION}, got {schema_version}"
    );
    let toc_count = u32::from_le_bytes(
        mmap.get(16..20)
            .context("header too short for toc_count")?
            .try_into()
            .context("toc_count bytes")?,
    ) as usize;
    Ok(toc_count)
}

/// Validate the TOC and return the 12 named blob ranges.
pub(super) fn open_blobs(mmap: &[u8], toc_count: usize) -> Result<[Range<usize>; TOC_COUNT]> {
    let toc_end = HEADER_LEN + toc_count * TOC_ENTRY_SIZE;
    ensure!(
        mmap.len() >= toc_end,
        "overlay TOC truncated: need {toc_end} bytes, file is {} bytes",
        mmap.len()
    );
    let toc = parse_toc_entries(mmap, toc_count)?;
    let blobs = find_blob_ranges(&toc)?;
    validate_blob_layout(mmap.len(), &blobs)?;
    Ok(blobs)
}

/// Build the segment-to-global-row prefix-sum table.
///
/// `offsets[i]` is the first global row ID for segment `i`.
/// `offsets[segments.len()]` is one-past-the-end (equals total row count).
pub(super) fn build_segment_offsets(segments: &[SegmentMeta]) -> Vec<u32> {
    let mut offsets = Vec::with_capacity(segments.len() + 1);
    let mut running = 0u32;
    for seg in segments {
        offsets.push(running);
        running = running.saturating_add(seg.row_count);
    }
    offsets.push(running);
    offsets
}

/// Parse the `file_entries` blob into a list of `(relative_path, file_size)` pairs.
///
/// Format: `[u32 count][repeated: [u32 size][u16 path_len][u8; path_len]]`
///
/// Gracefully skips malformed entries rather than failing.
pub(super) fn parse_file_entries(blob: &[u8]) -> Vec<(PathBuf, u32)> {
    let mut result: Vec<(PathBuf, u32)> = Vec::new();
    let Some(count_bytes) = blob.get(0..4) else {
        return result;
    };
    let count = u32::from_le_bytes(count_bytes.try_into().unwrap_or_default()) as usize;
    let mut pos = 4usize;
    for _ in 0..count {
        let Some(size_bytes) = blob.get(pos..pos + 4) else {
            break;
        };
        let size = u32::from_le_bytes(size_bytes.try_into().unwrap_or_default());
        let Some(len_bytes) = blob.get(pos + 4..pos + 6) else {
            break;
        };
        let path_len = u16::from_le_bytes(len_bytes.try_into().unwrap_or_default()) as usize;
        pos += 6;
        let Some(path_bytes) = blob.get(pos..pos + path_len) else {
            break;
        };
        pos += path_len;
        if let Ok(s) = std::str::from_utf8(path_bytes) {
            result.push((PathBuf::from(s), size));
        }
    }
    result
}

/// Byte-range layout of the (already sorted) `enrich_bitmaps` blob: the
/// `[EnrichEntry]` array, the key-string region, and the base offset of the
/// bitmap-data region -- all absolute offsets into the overlay mmap.
///
/// Zero-copy: no entry is decoded here. `Overlay` binary-searches the mmap'd
/// array directly at query time instead of walking an owned `Vec<(String,
/// Range<usize>)>` built from it.
pub(super) struct EnrichIndexLayout {
    pub(super) entries: Range<usize>,
    pub(super) keys: Range<usize>,
    pub(super) bitmap_base: usize,
}

/// Locate the `[EnrichEntry]` array, the key-string region, and the
/// bitmap-data base within the `enrich_bitmaps` blob.
///
/// `blob_base` is the absolute byte offset of `blob` within the mmap, used
/// to compute absolute ranges. Returns an empty layout (no entries) if the
/// blob header is missing, the declared lengths overrun the blob, or any
/// entry's key falls outside the key region or out of sorted order --
/// `Overlay` trusts this layout for a raw binary search, so a malformed
/// blob is rejected wholesale rather than silently searched over.
pub(super) fn parse_enrich_index(blob: &[u8], blob_base: usize) -> EnrichIndexLayout {
    let empty = EnrichIndexLayout {
        entries: blob_base..blob_base,
        keys: blob_base..blob_base,
        bitmap_base: blob_base,
    };
    if blob.len() < 8 {
        return empty;
    }
    let entry_count = blob
        .get(0..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_le_bytes) as usize;
    let key_data_len = blob
        .get(4..8)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_le_bytes) as usize;
    let entry_bytes = std::mem::size_of::<EnrichEntry>();
    let entries_end = 8 + entry_count * entry_bytes;
    if blob.len() < entries_end + key_data_len {
        return empty;
    }

    // Validate every entry's key range and the overall sort order before
    // handing out the layout: `Overlay` binary-searches this array in place
    // with `partition_point`, which is only correct over a fully sorted,
    // in-bounds sequence. A single corrupt or out-of-order entry could
    // otherwise silently misdirect the search past its well-formed
    // neighbours. This can only fail on a truncated or tampered overlay
    // file -- never on one this build's own writer produced -- and costs
    // one pass over `entry_count` small structs, no allocation.
    let Some(entries_slice) = blob.get(8..entries_end) else {
        return empty;
    };
    let entries: &[EnrichEntry] = cast_slice(entries_slice);
    let Some(key_data) = blob.get(entries_end..entries_end + key_data_len) else {
        return empty;
    };
    let mut prev_key: &[u8] = &[];
    for e in entries {
        let k_start = e.key_offset as usize;
        let Some(k_end) = k_start.checked_add(e.key_len as usize) else {
            return empty;
        };
        let Some(key) = key_data.get(k_start..k_end) else {
            return empty;
        };
        if key < prev_key {
            return empty;
        }
        prev_key = key;
    }

    EnrichIndexLayout {
        entries: blob_base + 8..blob_base + entries_end,
        keys: blob_base + entries_end..blob_base + entries_end + key_data_len,
        bitmap_base: blob_base + entries_end + key_data_len,
    }
}
/// Parse TOC entries field-by-field from the mmap.
///
/// `TocEntry` is not `Pod` due to `[u8; 56]` conflicting with
/// `object::pod::Pod`, so reads are done manually.
pub(super) fn parse_toc_entries(mmap: &[u8], toc_count: usize) -> Result<Vec<TocEntry>> {
    let mut toc = Vec::with_capacity(toc_count);
    for i in 0..toc_count {
        let base = HEADER_LEN + i * TOC_ENTRY_SIZE;
        ensure!(
            base + TOC_ENTRY_SIZE <= mmap.len(),
            "TOC entry {i} out of bounds"
        );
        let entry_bytes = mmap
            .get(base..base + TOC_ENTRY_SIZE)
            .context("TOC entry slice")?;
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

/// Locate all named blobs (TOC_COUNT of them) and return their byte ranges in TOC order.
pub(super) fn find_blob_ranges(toc: &[TocEntry]) -> Result<[Range<usize>; TOC_COUNT]> {
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
        find_one(b"file_entries")?,
        find_one(b"usages_count_fst")?,
    ])
}

/// Decode the fixed-size `SegmentRecord` slice into heap-allocated `SegmentMeta` values.
pub(super) fn decode_segment_metas(
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
        let path_str = std::str::from_utf8(
            seg_strings
                .get(path_start..path_end)
                .context("segment path slice")?,
        )
        .context("segment source path not valid UTF-8")?;
        let hex_str = std::str::from_utf8(
            seg_strings
                .get(hex_start..hex_end)
                .context("segment hex slice")?,
        )
        .context("segment hex_content_id not valid UTF-8")?;
        segments.push(SegmentMeta {
            hex_content_id: hex_str.to_owned(),
            source_path: PathBuf::from(path_str),
            row_count: rec.row_count,
            dedup_row_count: rec.dedup_row_count,
            sha256: [0u8; 32], // filled below
            prefix_len: 0,     // filled below
        });
    }

    // Compute SHA-256 and shortest unambiguous prefix for every segment in one
    // pass over all paths.  All data is local — no global registry, no lock.
    let all_hashes: Vec<[u8; 32]> = segments
        .iter()
        .map(|m| crate::node_id::sha256_of_path(m.source_path.to_str().unwrap_or("")))
        .collect();
    for (meta, &hash) in segments.iter_mut().zip(&all_hashes) {
        meta.sha256 = hash;
        meta.prefix_len = crate::node_id::shortest_prefix_len(&hash, &all_hashes);
    }

    Ok(segments)
}

/// Validate that all blob ranges fit within `mmap_len` and that
/// fixed-record blobs have sizes that are multiples of the record size.
pub(super) fn validate_blob_layout(
    mmap_len: usize,
    blobs: &[Range<usize>; TOC_COUNT],
) -> Result<()> {
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
        _, // file_entries: variable-length, validated during parse
        _, // usages_count_fst: FST bytes, no fixed record size (v14)
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::cast_slice_mut;

    /// A 4-byte-aligned blob offset, as every real one is: the writer rounds
    /// each blob start up to a multiple of 4.
    const BLOB_BASE: usize = 64;

    fn entry(key_offset: u32, key_len: u16) -> EnrichEntry {
        EnrichEntry {
            key_offset,
            key_len,
            _pad: 0,
            bitmap_offset: 0,
            bitmap_len: 0,
        }
    }

    /// Lay out an `enrich_bitmaps` blob the way the writer does, in a buffer
    /// whose first byte is 4-byte aligned.
    ///
    /// The alignment is load-bearing rather than incidental: the entry array
    /// is read with `cast_slice`, which panics rather than returns on a
    /// misaligned pointer. A bare `Vec<u8>` is only guaranteed byte
    /// alignment, so a test built on one could fail for a reason no overlay
    /// file can reproduce.
    fn blob_words(entries: &[EnrichEntry], key_data: &[u8], bitmaps: &[u8]) -> Vec<u32> {
        let entry_count = u32::try_from(entries.len()).expect("test entry count fits u32");
        let key_data_len = u32::try_from(key_data.len()).expect("test key data fits u32");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&entry_count.to_le_bytes());
        bytes.extend_from_slice(&key_data_len.to_le_bytes());
        bytes.extend_from_slice(cast_slice(entries));
        bytes.extend_from_slice(key_data);
        bytes.extend_from_slice(bitmaps);
        let mut words = vec![0u32; bytes.len().div_ceil(4)];
        cast_slice_mut::<u32, u8>(&mut words)[..bytes.len()].copy_from_slice(&bytes);
        words
    }

    fn blob(words: &[u32]) -> &[u8] {
        cast_slice(words)
    }

    /// The layout handed back when the blob is refused: three empty regions
    /// at the blob base, so every lookup finds nothing rather than searching
    /// over entries that cannot be trusted.
    #[track_caller]
    fn assert_rejected(layout: &EnrichIndexLayout) {
        assert_eq!(layout.entries, BLOB_BASE..BLOB_BASE);
        assert_eq!(layout.keys, BLOB_BASE..BLOB_BASE);
        assert_eq!(layout.bitmap_base, BLOB_BASE);
    }

    #[test]
    fn enrich_index_locates_the_entry_key_and_bitmap_regions() {
        let words = blob_words(&[entry(0, 3), entry(3, 3)], b"a=1b=2", &[7, 7, 7, 7]);
        let layout = parse_enrich_index(blob(&words), BLOB_BASE);

        // 8-byte header, then 2 x 16-byte entries, then 6 bytes of keys.
        assert_eq!(layout.entries, BLOB_BASE + 8..BLOB_BASE + 40);
        assert_eq!(layout.keys, BLOB_BASE + 40..BLOB_BASE + 46);
        assert_eq!(layout.bitmap_base, BLOB_BASE + 46);
    }

    #[test]
    fn enrich_index_accepts_a_blob_that_holds_no_entries() {
        // An index with nothing in it is well-formed, not malformed. It must
        // resolve to real (empty) regions past the header rather than to the
        // rejection sentinel, so a change that started refusing a valid empty
        // blob could not pass by looking identical to a corrupt one.
        let words = blob_words(&[], &[], &[]);
        let layout = parse_enrich_index(blob(&words), BLOB_BASE);

        assert_eq!(layout.entries, BLOB_BASE + 8..BLOB_BASE + 8);
        assert_eq!(layout.keys, BLOB_BASE + 8..BLOB_BASE + 8);
        assert_eq!(layout.bitmap_base, BLOB_BASE + 8);
    }

    #[test]
    fn enrich_index_rejects_entries_that_are_out_of_order() {
        // "b=2" ahead of "a=1". Every lookup binary-searches this array with
        // `partition_point`, whose result is unspecified over an unsorted
        // sequence, so one bad entry disqualifies the whole blob.
        let words = blob_words(&[entry(0, 3), entry(3, 3)], b"b=2a=1", &[]);
        assert_rejected(&parse_enrich_index(blob(&words), BLOB_BASE));
    }

    #[test]
    fn enrich_index_accepts_two_entries_sharing_a_key() {
        // Only a strict decrease breaks the search. Equal neighbours keep
        // `partition_point` well defined, so they must not be rejected.
        let words = blob_words(&[entry(0, 3), entry(0, 3)], b"a=1", &[]);
        let layout = parse_enrich_index(blob(&words), BLOB_BASE);

        assert_eq!(layout.entries, BLOB_BASE + 8..BLOB_BASE + 40);
    }

    #[test]
    fn enrich_index_rejects_a_key_reaching_past_the_key_region() {
        // Six bytes of key data, but the second entry claims bytes 3..12.
        let words = blob_words(&[entry(0, 3), entry(3, 9)], b"a=1b=2", &[]);
        assert_rejected(&parse_enrich_index(blob(&words), BLOB_BASE));
    }

    #[test]
    fn enrich_index_rejects_a_key_offset_far_outside_the_blob() {
        let words = blob_words(&[entry(u32::MAX, 1)], b"a=1", &[]);
        assert_rejected(&parse_enrich_index(blob(&words), BLOB_BASE));
    }

    #[test]
    fn enrich_index_rejects_a_blob_truncated_mid_entry() {
        // The header still declares two entries; the file holds one.
        let words = blob_words(&[entry(0, 3), entry(3, 3)], b"a=1b=2", &[]);
        let truncated = &blob(&words)[..8 + std::mem::size_of::<EnrichEntry>()];

        assert_rejected(&parse_enrich_index(truncated, BLOB_BASE));
    }

    #[test]
    fn enrich_index_rejects_a_blob_too_short_to_hold_a_header() {
        assert_rejected(&parse_enrich_index(&[0, 0, 0, 0], BLOB_BASE));
    }
}
