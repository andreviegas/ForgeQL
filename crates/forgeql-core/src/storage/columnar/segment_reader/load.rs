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

use super::{ColRange, ENTRY_NAME_LEN, TOC_ENTRY_SIZE, ZONEMAP_NUMERIC_FIELDS};

// ─────────────────────────────────────────────────────────────────────────────
// Table of contents
// ─────────────────────────────────────────────────────────────────────────────

/// One entry of the FQSF table of contents.
pub(super) struct TocEntry<'m> {
    /// The blob's name, read from the mapping.
    pub(super) name: &'m str,
    /// Byte range the blob occupies in the mapping.
    pub(super) range: ColRange,
}

/// The FQSF table of contents, read in place.
///
/// Every entry names its blob out of the mapping, so parsing the table copies
/// no name, and the entries are sorted by name so a lookup is a binary search
/// rather than a hash of a `String` that had to be allocated to perform it.
///
/// This is scaffolding for [`super::SegmentReader::open`]: the reader resolves
/// the blobs it will read into byte ranges and drops the table. A workspace of
/// tens of thousands of segments therefore keeps no per-segment copy of a name
/// table that is already on disk and mapped.
#[derive(Default)]
pub(super) struct Toc<'m> {
    entries: Vec<TocEntry<'m>>,
}

impl<'m> Toc<'m> {
    /// Byte range of the blob named `name`, or `None` when the segment holds
    /// no such blob.
    ///
    /// A name written twice resolves to the entry written last, which is what
    /// the insertion-ordered map this replaced returned.
    pub(super) fn get(&self, name: &str) -> Option<ColRange> {
        let start = self.entries.partition_point(|entry| entry.name < name);
        self.entries[start..]
            .iter()
            .take_while(|entry| entry.name == name)
            .last()
            .map(|entry| entry.range)
    }

    /// Every entry, in name order.
    pub(super) fn entries(&self) -> impl Iterator<Item = &TocEntry<'m>> {
        self.entries.iter()
    }
}

/// Parse the FQSF table-of-contents into one `(start, end)` byte range per
/// named blob. `mmap` must already be validated as a well-formed FQSF file
/// (magic, version, and at least a 12-byte header checked by the caller).
///
/// # Errors
/// Returns `Err` when the file is shorter than the TOC it declares, a blob name
/// is not valid UTF-8, or a blob runs past the end of the file.
pub(super) fn parse_toc<'m>(mmap: &'m [u8], file_len: usize, path: &Path) -> Result<Toc<'m>> {
    let entry_count =
        u32::from_le_bytes(mmap[8..12].try_into().context("FQSF entry_count bytes")?) as usize;
    let toc_end = 12 + entry_count * TOC_ENTRY_SIZE;
    ensure!(
        file_len >= toc_end,
        "segment {} too short for TOC (need {toc_end} bytes, have {file_len})",
        path.display()
    );

    let mut entries: Vec<TocEntry<'m>> = Vec::with_capacity(entry_count);
    for i in 0..entry_count {
        let es = 12 + i * TOC_ENTRY_SIZE;
        let entry = &mmap[es..es + TOC_ENTRY_SIZE];
        let name_end = entry[..ENTRY_NAME_LEN]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(ENTRY_NAME_LEN);
        let name = std::str::from_utf8(&entry[..name_end])
            .with_context(|| format!("blob name at TOC index {i}"))?;
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
        entries.push(TocEntry {
            name,
            range: (offset, offset + len),
        });
    }
    // Stable, so entries sharing a name stay in write order and `get` can
    // return the last of them.
    entries.sort_by(|a, b| a.name.cmp(b.name));

    Ok(Toc { entries })
}

// ─────────────────────────────────────────────────────────────────────────────
// Private blob helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Return a blob byte slice from the table of contents; `&[]` when absent.
pub(super) fn blob_slice<'m>(toc: &Toc<'_>, mmap: &'m Mmap, name: &str) -> &'m [u8] {
    let Some((start, end)) = toc.get(name) else {
        return &[];
    };
    &mmap[start..end]
}

/// Write `prefix`, `name` and `suffix` into `buf` and return them as one blob
/// name.
///
/// Every open builds a blob name per enrichment column, per posted field and
/// per occurrence role; formatting one allocated a `String` each time, which
/// across a workspace of tens of thousands of segments is millions of
/// allocations for lookups a stack buffer answers just as well. A join longer
/// than the table of contents can spell names no blob, so it yields `None` and
/// the caller skips the lookup.
pub(super) fn blob_name_in<'b>(
    buf: &'b mut [u8; ENTRY_NAME_LEN],
    prefix: &str,
    name: &str,
    suffix: &str,
) -> Option<&'b str> {
    let name_end = prefix.len() + name.len();
    let end = name_end + suffix.len();
    if end > ENTRY_NAME_LEN {
        return None;
    }
    buf[..prefix.len()].copy_from_slice(prefix.as_bytes());
    buf[prefix.len()..name_end].copy_from_slice(name.as_bytes());
    buf[name_end..end].copy_from_slice(suffix.as_bytes());
    std::str::from_utf8(&buf[..end]).ok()
}

/// Parse column metadata entries from the header byte slice.
///
/// Each entry: `[u8: name_len][u8 × name_len: name][u8: type_tag][u64 LE: element_count]`
///
/// A column's name comes back as its byte range within `header` rather than as
/// an owned `String`: the header is part of the segment's mapping, so the name
/// can be read from there for as long as the segment is open.
pub(super) fn parse_column_entries(
    header: &[u8],
    start: usize,
    column_count: u32,
) -> Result<Vec<(ColRange, u8)>> {
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
        let name = (pos, pos + name_len);
        let _ = std::str::from_utf8(&header[name.0..name.1])
            .with_context(|| format!("column {i} name is not valid UTF-8"))?;
        pos += name_len;
        let type_tag = header[pos];
        pos += 1 + 8; // type_tag + element_count (u64, not used by reader)
        cols.push((name, type_tag));
    }

    Ok(cols)
}

/// Load zone maps from `zonemap_<col>` blobs.
///
/// Keyed by the column name the builder wrote, which is always one of the
/// compiled-in [`ZONEMAP_NUMERIC_FIELDS`] — a `&'static str` every reader
/// borrows rather than a `String` every segment allocates its own copy of.
pub(super) fn load_zone_maps(
    toc: &Toc<'_>,
    mmap: &Mmap,
) -> Result<HashMap<&'static str, (u32, u32)>> {
    let mut result: HashMap<&'static str, (u32, u32)> = HashMap::new();
    let mut name_buf = [0_u8; ENTRY_NAME_LEN];

    for &(col_name, _has_sentinel) in ZONEMAP_NUMERIC_FIELDS {
        let Some(blob_name) = blob_name_in(&mut name_buf, "zonemap_", col_name, "") else {
            continue;
        };
        let data = blob_slice(toc, mmap, blob_name);
        if data.len() < 8 {
            continue;
        }
        let min = u32::from_le_bytes(data[..4].try_into().context("zonemap min bytes")?);
        let max = u32::from_le_bytes(data[4..8].try_into().context("zonemap max bytes")?);
        let _ = result.insert(col_name, (min, max));
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{ColRange, ENTRY_NAME_LEN, TOC_ENTRY_SIZE, parse_toc};

    /// Assemble the bytes of an FQSF table of contents: the 12-byte preamble
    /// `parse_toc` reads the entry count out of, then one 64-byte entry per
    /// blob, in a file of `file_len` bytes.
    fn toc_bytes(entries: &[(&str, u32, u32)], file_len: usize) -> Vec<u8> {
        let mut buf = vec![0_u8; 12 + entries.len() * TOC_ENTRY_SIZE];
        buf[8..12].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        for (i, (name, offset, len)) in entries.iter().enumerate() {
            let es = 12 + i * TOC_ENTRY_SIZE;
            buf[es..es + name.len()].copy_from_slice(name.as_bytes());
            buf[es + ENTRY_NAME_LEN..es + ENTRY_NAME_LEN + 4]
                .copy_from_slice(&offset.to_le_bytes());
            buf[es + ENTRY_NAME_LEN + 4..es + ENTRY_NAME_LEN + 8]
                .copy_from_slice(&len.to_le_bytes());
        }
        buf.resize(file_len, 0);
        buf
    }

    /// The map this table replaced was insertion-ordered, so a name written
    /// twice resolved to the entry written last, and the lookup must answer the
    /// same way — which needs the run of equal names to keep its write order,
    /// asserted below on the entries themselves. Asserting only the lookup
    /// would not distinguish an order kept on purpose from one kept by luck.
    #[test]
    fn a_name_written_twice_resolves_to_the_entry_written_last() {
        let bytes = toc_bytes(
            &[("col_a", 100, 10), ("col_a", 200, 20), ("col_b", 300, 30)],
            400,
        );
        let toc = parse_toc(&bytes, bytes.len(), Path::new("seg.fqsf")).expect("parse toc");

        assert_eq!(toc.get("col_a"), Some((200, 220)));
        assert_eq!(toc.get("col_b"), Some((300, 330)));
        assert_eq!(toc.get("col_missing"), None);

        let col_a: Vec<ColRange> = toc
            .entries()
            .filter(|entry| entry.name == "col_a")
            .map(|entry| entry.range)
            .collect();
        assert_eq!(
            col_a,
            vec![(100, 110), (200, 220)],
            "the run of equal names must stay in write order for `get` to mean the last one"
        );
    }

    /// A blob that runs past the end of the file is a corrupt segment: the
    /// parse refuses it rather than handing back a range that would slice the
    /// mapping out of bounds when something later reads it.
    #[test]
    fn a_blob_reaching_past_the_end_of_the_file_is_refused() {
        let bytes = toc_bytes(&[("col_a", 380, 40)], 400);
        assert!(parse_toc(&bytes, bytes.len(), Path::new("seg.fqsf")).is_err());
    }
}
