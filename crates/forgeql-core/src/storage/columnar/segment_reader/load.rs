//! Open-time loader helpers for `SegmentReader`.
//!
//! Free functions that parse the FQSF table-of-contents and decode the column
//! metadata, zone-map, and name-posting blobs, split out of `segment_reader.rs`.
//! The Roaring posting blobs are not decoded here — see `postings.rs`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use bytemuck::cast_slice;
use memmap2::Mmap;

use super::{ENTRY_NAME_LEN, TOC_ENTRY_SIZE, ZONEMAP_NUMERIC_FIELDS};
/// Parse the FQSF table-of-contents into one `(start, end)` byte range per
/// named blob. `mmap` must already be validated as a well-formed FQSF file
/// (magic, version, and at least a 12-byte header checked by the caller).
pub(super) fn parse_toc(
    mmap: &[u8],
    file_len: usize,
    path: &Path,
) -> Result<HashMap<String, (usize, usize)>> {
    let entry_count =
        u32::from_le_bytes(mmap[8..12].try_into().context("FQSF entry_count bytes")?) as usize;
    let toc_end = 12 + entry_count * TOC_ENTRY_SIZE;
    ensure!(
        file_len >= toc_end,
        "segment {} too short for TOC (need {toc_end} bytes, have {file_len})",
        path.display()
    );

    let mut blobs: HashMap<String, (usize, usize)> = HashMap::with_capacity(entry_count);
    for i in 0..entry_count {
        let es = 12 + i * TOC_ENTRY_SIZE;
        let entry = &mmap[es..es + TOC_ENTRY_SIZE];
        let name_end = entry[..ENTRY_NAME_LEN]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(ENTRY_NAME_LEN);
        let name = std::str::from_utf8(&entry[..name_end])
            .with_context(|| format!("blob name at TOC index {i}"))?
            .to_owned();
        let offset = u32::from_le_bytes(
            entry[ENTRY_NAME_LEN..ENTRY_NAME_LEN + 4]
                .try_into()
                .context("blob offset")?,
        ) as usize;
        let len = u32::from_le_bytes(
            entry[ENTRY_NAME_LEN + 4..ENTRY_NAME_LEN + 8]
                .try_into()
                .context("blob length")?,
        ) as usize;
        ensure!(
            offset + len <= file_len,
            "blob '{name}' extends beyond file end ({offset} + {len} > {file_len})"
        );
        let _ = blobs.insert(name, (offset, offset + len));
    }
    Ok(blobs)
}

// ─────────────────────────────────────────────────────────────────────────────
// Private blob helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Return a blob byte slice from the blobs map; `&[]` when absent.
fn blob_slice<'m>(blobs: &HashMap<String, (usize, usize)>, mmap: &'m Mmap, name: &str) -> &'m [u8] {
    let Some(&(start, end)) = blobs.get(name) else {
        return &[];
    };
    &mmap[start..end]
}

/// Parse column metadata entries from the header byte slice.
///
/// Each entry: `[u8: name_len][u8 × name_len: name][u8: type_tag][u64 LE: element_count]`
pub(super) fn parse_column_entries(
    header: &[u8],
    start: usize,
    column_count: u32,
) -> Result<Vec<(String, u8)>> {
    let mut pos = start;
    let mut cols = Vec::with_capacity(column_count as usize);

    for i in 0..column_count {
        ensure!(
            pos < header.len(),
            "header truncated at column entry {i}: pos {pos} ≥ len {}",
            header.len()
        );
        let name_len = header[pos] as usize;
        pos += 1;
        ensure!(
            pos + name_len + 1 + 8 <= header.len(),
            "column entry {i} is truncated (name_len={name_len})"
        );
        let name = std::str::from_utf8(&header[pos..pos + name_len])
            .with_context(|| format!("column {i} name is not valid UTF-8"))?
            .to_owned();
        pos += name_len;
        let type_tag = header[pos];
        pos += 1 + 8; // type_tag + element_count (u64, not used by reader)
        cols.push((name, type_tag));
    }

    Ok(cols)
}

/// Load zone maps from `zonemap_<col>` blobs.
pub(super) fn load_zone_maps(
    blobs: &HashMap<String, (usize, usize)>,
    mmap: &Mmap,
) -> Result<HashMap<String, (u32, u32)>> {
    let mut result: HashMap<String, (u32, u32)> = HashMap::new();
    for (col_name, _has_sentinel) in ZONEMAP_NUMERIC_FIELDS {
        let blob_name = format!("zonemap_{col_name}");
        let data = blob_slice(blobs, mmap, &blob_name);
        if data.len() < 8 {
            continue;
        }
        let min = u32::from_le_bytes(data[..4].try_into().context("zonemap min bytes")?);
        let max = u32::from_le_bytes(data[4..8].try_into().context("zonemap max bytes")?);
        let _ = result.insert((*col_name).to_owned(), (min, max));
    }
    Ok(result)
}

/// Decode FST-encoded name posting.
///
/// FST value layout: `(count as u64) | ((byte_offset as u64) << 32)` where
/// `byte_offset` is a byte index into `name_postings.bin` pointing to
/// `count` consecutive `u32 LE` row IDs.
pub(super) fn decode_name_postings(encoded: u64, name_postings: &[u8]) -> Vec<u32> {
    let count = usize::try_from(encoded & 0xFFFF_FFFF).unwrap_or(usize::MAX);
    let byte_offset = usize::try_from((encoded >> 32) & 0xFFFF_FFFF).unwrap_or(usize::MAX);

    let end = byte_offset + count * 4;
    if end > name_postings.len() {
        return Vec::new();
    }
    #[expect(clippy::indexing_slicing, reason = "bounds checked above")]
    cast_slice::<u8, u32>(&name_postings[byte_offset..end]).to_vec()
}
