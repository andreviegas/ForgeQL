//! Byte-identity tests for [`OverlayWriter`](super::OverlayWriter).
//!
//! The writer changed from assembling the whole file in memory to streaming it
//! blob by blob. Nothing about the file it produces was meant to change, and no
//! corpus checksum can prove that on its own — the checksums this project has
//! measured are not reproducible run to run. So the old layout algorithm is
//! kept here as an oracle and the two are compared byte for byte.

use std::io::{self, Cursor, Write};

use super::{
    BLOB_ORDER, HEADER_V3_LEN_U32, MAGIC, OverlayWriter, SCHEMA_VERSION, TOC_COUNT, TOC_COUNT_U32,
    TOC_ENTRY_NAME_LEN, TocEntry,
};

/// The layout algorithm as it stood before the writer streamed: every blob's
/// offset computed up front, then the header, then the whole table of contents,
/// then the blobs with their alignment padding.
///
/// This is the oracle, and the only copy of it. `OverlayWriter` writes the same
/// bytes in a different order; keeping the algorithm it replaced is the cheap
/// way to keep that claim true as the writer is changed again.
fn reference_bytes(generation: u64, blobs: &[&[u8]; TOC_COUNT]) -> Vec<u8> {
    let mut toc = [TocEntry {
        name: [0u8; TOC_ENTRY_NAME_LEN],
        offset: 0,
        len: 0,
    }; TOC_COUNT];

    let mut current_offset: u32 = HEADER_V3_LEN_U32;
    for (i, (name, data)) in BLOB_ORDER.iter().zip(blobs.iter()).enumerate() {
        let aligned = (current_offset + 3) & !3;
        toc[i].name[..name.len()].copy_from_slice(name);
        toc[i].offset = aligned;
        toc[i].len = u32::try_from(data.len()).expect("test blob fits u32");
        current_offset = aligned + toc[i].len;
    }

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&TOC_COUNT_U32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for entry in &toc {
        out.extend_from_slice(&entry.name);
        out.extend_from_slice(&entry.offset.to_le_bytes());
        out.extend_from_slice(&entry.len.to_le_bytes());
    }

    let mut file_pos: u32 = HEADER_V3_LEN_U32;
    for (i, data) in blobs.iter().enumerate() {
        let target = toc[i].offset;
        if target > file_pos {
            let pad = usize::try_from(target - file_pos).unwrap_or(0);
            out.resize(out.len() + pad, 0);
        }
        out.extend_from_slice(data);
        file_pos = target + toc[i].len;
    }
    out
}

/// The same file through the streaming writer, one whole blob at a time.
fn streamed_bytes(generation: u64, blobs: &[&[u8]; TOC_COUNT]) -> Vec<u8> {
    let mut cur = Cursor::new(Vec::new());
    {
        let mut w = OverlayWriter::new(&mut cur, generation).expect("start overlay");
        for (name, data) in BLOB_ORDER.iter().zip(blobs.iter()) {
            w.blob(name, |sink| sink.write_all(data))
                .expect("write blob");
        }
        let _ = w.finish().expect("finish overlay");
    }
    cur.into_inner()
}

/// Every blob empty: the file is the header and its table of contents, and the
/// first blob offset is still the one the reader expects.
#[test]
fn an_overlay_of_empty_blobs_matches_the_whole_file_writer() {
    let blobs: [&[u8]; TOC_COUNT] = [&[]; TOC_COUNT];
    assert_eq!(streamed_bytes(1, &blobs), reference_bytes(1, &blobs));
}

/// Unaligned blob lengths push every following blob along by padding. A
/// streaming writer that aligned after a blob rather than before the next one
/// would still produce a self-consistent file — and a different one.
#[test]
fn unaligned_blob_lengths_pad_exactly_like_the_whole_file_writer() {
    let sizes = [1usize, 2, 3, 5, 7, 0, 4, 9, 0, 0, 13, 0, 1];
    let payloads: Vec<Vec<u8>> = sizes
        .iter()
        .enumerate()
        .map(|(i, &n)| vec![u8::try_from(i + 1).unwrap_or(0xFF); n])
        .collect();
    let blobs: [&[u8]; TOC_COUNT] = std::array::from_fn(|i| payloads[i].as_slice());
    assert_eq!(streamed_bytes(7, &blobs), reference_bytes(7, &blobs));
}

/// A trailing run of empty blobs keeps the padding of the last blob that had
/// content, so the file is longer than its last content byte. Dropping that
/// padding shortens the file and moves no offset in the table of contents, so
/// nothing else in the format would reveal it.
#[test]
fn trailing_empty_blobs_keep_the_padding_of_the_last_payload() {
    let payload = vec![0xABu8; 5];
    let mut blobs: [&[u8]; TOC_COUNT] = [&[]; TOC_COUNT];
    blobs[TOC_COUNT - 4] = payload.as_slice();

    let streamed = streamed_bytes(2, &blobs);
    assert_eq!(streamed, reference_bytes(2, &blobs));
    // 856 bytes of header + TOC, 5 bytes of payload, then 3 bytes of padding
    // to the aligned offset the next (empty) blob records.
    assert_eq!(streamed.len(), 856 + 5 + 3);
}

/// A blob assembled from many pieces records what it actually wrote, not a
/// length the caller predicted. `bitmap_data` is written that way: one
/// serialised bitmap at a time, with no buffer holding the whole blob.
#[test]
fn a_blob_written_in_pieces_records_the_bytes_it_wrote() {
    let mut blobs: [&[u8]; TOC_COUNT] = [&[]; TOC_COUNT];
    blobs[3] = b"abcdefghij";
    let whole = reference_bytes(3, &blobs);

    let mut cur = Cursor::new(Vec::new());
    {
        let mut w = OverlayWriter::new(&mut cur, 3).expect("start overlay");
        for (i, name) in BLOB_ORDER.iter().enumerate() {
            if i == 3 {
                w.blob(name, |sink| {
                    for chunk in [&b"abc"[..], b"defg", b"hij"] {
                        sink.write_all(chunk)?;
                    }
                    Ok(())
                })
                .expect("piecewise blob");
            } else {
                w.blob(name, |_| Ok(())).expect("empty blob");
            }
        }
        let _ = w.finish().expect("finish overlay");
    }
    assert_eq!(cur.into_inner(), whole);
}

/// A blob presented out of turn is refused. It would land where the previous
/// blob ended and move every byte after it, and the table of contents would
/// describe that new file perfectly — so the reader could never tell.
#[test]
fn a_blob_written_out_of_layout_order_is_refused() {
    let mut cur = Cursor::new(Vec::new());
    let mut w = OverlayWriter::new(&mut cur, 1).expect("start overlay");
    let err = w
        .blob(BLOB_ORDER[1], |sink| sink.write_all(b"x"))
        .expect_err("a blob out of layout order must be refused");
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

/// Finishing early is refused rather than leaving a table-of-contents slot
/// zeroed, which would point the reader at offset 0 with length 0.
#[test]
fn finishing_before_every_blob_is_written_is_refused() {
    let mut cur = Cursor::new(Vec::new());
    let mut w = OverlayWriter::new(&mut cur, 1).expect("start overlay");
    w.blob(BLOB_ORDER[0], |sink| sink.write_all(b"row"))
        .expect("first blob");
    let err = w
        .finish()
        .expect_err("an unfinished overlay must be refused");
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}
