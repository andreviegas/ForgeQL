//! Writer for the FQOV v3 overlay file format.
//!
//! Replaces the v2 `bincode::serialize` step in [`super::overlay_builder`].
//!
//! The entry point is [`OverlayWriter`]. The builder drives it one blob at a
//! time, in the order the blobs occupy the file, so each buffer is freed as
//! soon as its bytes are on disk instead of being held until the whole file is
//! assembled. The fixed header and its table of contents sit at the FRONT of
//! the file but are written LAST, once every blob's offset and length is known
//! — which is why the sink has to be seekable.
//!
//! Only the order in which the bytes are written changed. The file is byte for
//! byte the one the previous whole-file writer produced, and
//! `overlay_writer/tests.rs` keeps that algorithm as an oracle to prove it.

use std::io::{self, Seek, SeekFrom, Write};

use super::overlay::{
    HEADER_LEN, HEADER_V3_LEN, MAGIC, SCHEMA_VERSION, TOC_COUNT, TOC_ENTRY_NAME_LEN,
    TOC_ENTRY_SIZE, TocEntry,
};

#[cfg(test)]
mod tests;

// Blob name constants (ASCII, ≤ `TOC_ENTRY_NAME_LEN` bytes).
pub(super) const BLOB_ROW_TABLE: &[u8] = b"row_table";
pub(super) const BLOB_KIND_STRINGS: &[u8] = b"kind_strings";
pub(super) const BLOB_KIND_INDEX: &[u8] = b"kind_index";
pub(super) const BLOB_BITMAP_DATA: &[u8] = b"bitmap_data";
pub(super) const BLOB_TRIGRAM_INDEX: &[u8] = b"trigram_index";
pub(super) const BLOB_NAME_FST: &[u8] = b"name_fst";
pub(super) const BLOB_NAME_POSTINGS: &[u8] = b"name_postings";
pub(super) const BLOB_SEGMENTS: &[u8] = b"segments";
pub(super) const BLOB_SEGMENT_STRINGS: &[u8] = b"segment_strings";
pub(super) const BLOB_INDEX_FILES: &[u8] = b"index_files";
pub(super) const BLOB_ENRICH_BITMAPS: &[u8] = b"enrich_bitmaps";
pub(super) const BLOB_FILE_ENTRIES: &[u8] = b"file_entries";
pub(super) const BLOB_USAGES_COUNT_FST: &[u8] = b"usages_count_fst";

/// The order the blobs occupy in the file.
///
/// This IS the layout: [`OverlayWriter::blob`] appends wherever the cursor
/// stands, so a blob presented out of turn would move every byte after it.
/// The writer refuses one rather than write a file that reads back differently.
pub(super) const BLOB_ORDER: [&[u8]; TOC_COUNT] = [
    BLOB_ROW_TABLE,
    BLOB_KIND_STRINGS,
    BLOB_KIND_INDEX,
    BLOB_BITMAP_DATA,
    BLOB_TRIGRAM_INDEX,
    BLOB_NAME_FST,
    BLOB_NAME_POSTINGS,
    BLOB_SEGMENTS,
    BLOB_SEGMENT_STRINGS,
    BLOB_INDEX_FILES,
    BLOB_ENRICH_BITMAPS,
    BLOB_FILE_ENTRIES,
    BLOB_USAGES_COUNT_FST,
];

// On-disk header constants as fixed-width ints, expressed with literals to
// avoid usize casts. Compile-time assertions below keep these in sync with the
// usize originals in overlay.rs.
const HEADER_LEN_U64: u64 = 24_u64; // = HEADER_LEN

const HEADER_V3_LEN_U32: u32 = 24_u32 + 13_u32 * 64_u32; // = HEADER_LEN + TOC_COUNT * TOC_ENTRY_SIZE

const TOC_COUNT_U32: u32 = 13_u32; // = TOC_COUNT

const _: () = assert!(
    HEADER_LEN as u64 == HEADER_LEN_U64,
    "HEADER_LEN_U64 out of sync with overlay.rs"
);

const _: () = assert!(
    HEADER_V3_LEN_U32 as usize == HEADER_V3_LEN,
    "HEADER_V3_LEN_U32 out of sync with overlay.rs"
);

const _: () = assert!(
    TOC_COUNT_U32 as usize == TOC_COUNT,
    "TOC_COUNT_U32 out of sync with overlay.rs"
);

/// Casts `v: usize` to `u32`, returning an `InvalidData` I/O error on overflow.
#[inline]
pub(super) fn to_u32(v: usize, ctx: &'static str) -> io::Result<u32> {
    u32::try_from(v).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, ctx))
}

/// Casts `v: usize` to `u16`, returning an `InvalidData` I/O error on overflow.
#[inline]
pub(super) fn to_u16(v: usize, ctx: &'static str) -> io::Result<u16> {
    u16::try_from(v).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, ctx))
}

/// Sink for one blob's bytes.
///
/// It counts what it forwards, so the length recorded in the table of contents
/// is what was actually written rather than what the caller believed it would
/// write. A blob assembled from many small pieces — every serialised bitmap in
/// `bitmap_data`, say — therefore needs no buffer of its own.
pub(super) struct BlobSink<'a, W: Write> {
    out: &'a mut W,
    written: u64,
}

impl<W: Write> Write for BlobSink<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.out.write(buf)?;
        self.written = self
            .written
            .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        Ok(n)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.out.write_all(buf)?;
        self.written = self
            .written
            .saturating_add(u64::try_from(buf.len()).unwrap_or(u64::MAX));
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Writes an FQOV v3 overlay file one blob at a time.
///
/// [`Self::new`] lays down the fixed header and reserves the table-of-contents
/// region; [`Self::blob`] appends one blob and records where it landed;
/// [`Self::finish`] seeks back and fills the reserved region in.
pub(super) struct OverlayWriter<W: Write + Seek> {
    out: W,
    toc: [TocEntry; TOC_COUNT],
    /// How many blobs have been written; indexes [`BLOB_ORDER`].
    written: usize,
    /// Absolute byte position of the end of the last blob.
    file_pos: u32,
}

impl<W: Write + Seek> OverlayWriter<W> {
    /// Start an overlay file: the fixed 24-byte header, then a zeroed
    /// placeholder for the table of contents that [`Self::finish`] overwrites.
    ///
    /// # Errors
    /// Propagates I/O errors from the sink.
    pub(super) fn new(mut out: W, generation: u64) -> io::Result<Self> {
        // ── Write fixed 24-byte header ────────────────────────────────────────
        out.write_all(&MAGIC)?;
        out.write_all(&SCHEMA_VERSION.to_le_bytes())?;
        out.write_all(&generation.to_le_bytes())?;
        out.write_all(&TOC_COUNT_U32.to_le_bytes())?;
        out.write_all(&0u32.to_le_bytes())?; // reserved

        debug_assert_eq!(
            HEADER_LEN + TOC_COUNT * TOC_ENTRY_SIZE,
            HEADER_V3_LEN,
            "HEADER_V3_LEN invariant"
        );

        // Reserve the TOC region so the first blob lands at HEADER_V3_LEN, the
        // offset the reader expects; `finish` overwrites these bytes in place.
        out.write_all(&[0u8; TOC_COUNT * TOC_ENTRY_SIZE])?;

        Ok(Self {
            out,
            toc: [TocEntry {
                name: [0u8; TOC_ENTRY_NAME_LEN],
                offset: 0,
                len: 0,
            }; TOC_COUNT],
            written: 0,
            file_pos: HEADER_V3_LEN_U32,
        })
    }

    /// Append one blob, calling `write` to produce its bytes.
    ///
    /// Blobs must arrive in [`BLOB_ORDER`]; `name` is checked against it.
    ///
    /// # Errors
    /// Returns `InvalidInput` if `name` is not the blob due next, `InvalidData`
    /// if the blob or the file outgrows the u32 offsets the format stores, and
    /// propagates I/O errors from the sink.
    pub(super) fn blob(
        &mut self,
        name: &[u8],
        write: impl FnOnce(&mut BlobSink<'_, W>) -> io::Result<()>,
    ) -> io::Result<()> {
        let _ = self.append_blob(name, write)?;
        Ok(())
    }

    /// Append a blob whose length was worked out before its bytes were, and
    /// refuse the file if the two disagree.
    ///
    /// `kind_index` and `trigram_index` are written BEFORE the `bitmap_data`
    /// they point into, and `segments` before the `segment_strings` it points
    /// into, so their offsets come from sizes rather than from bytes. One byte
    /// of drift there misplaces every entry after it and the file still parses:
    /// a reader would serve the wrong rows rather than fail. This turns that
    /// into a refusal.
    ///
    /// # Errors
    /// As [`Self::blob`], plus `InvalidData` when the blob does not write
    /// exactly `expected` bytes.
    pub(super) fn blob_of_len(
        &mut self,
        name: &[u8],
        expected: u32,
        write: impl FnOnce(&mut BlobSink<'_, W>) -> io::Result<()>,
    ) -> io::Result<()> {
        let written = self.append_blob(name, write)?;
        if written != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "overlay blob {} wrote {written} bytes where its index predicted {expected}",
                    String::from_utf8_lossy(name),
                ),
            ));
        }
        Ok(())
    }

    /// Append one blob and return the number of bytes it wrote.
    fn append_blob(
        &mut self,
        name: &[u8],
        write: impl FnOnce(&mut BlobSink<'_, W>) -> io::Result<()>,
    ) -> io::Result<u32> {
        let index = self.written;
        if index >= TOC_COUNT || BLOB_ORDER[index] != name {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "overlay blob written out of layout order",
            ));
        }
        debug_assert!(name.len() <= TOC_ENTRY_NAME_LEN, "blob name too long");

        // Blobs read via `cast_slice` require 4-byte alignment within the mmap;
        // every blob is aligned to 4 bytes regardless of size — an empty one
        // included, so that its recorded offset is the aligned position.
        let aligned = self.file_pos.checked_add(3).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "overlay file too large for u32 offsets",
            )
        })? & !3;
        let pad = usize::try_from(aligned - self.file_pos).unwrap_or(0);
        if pad > 0 {
            self.out.write_all(&[0u8; 3][..pad])?;
        }

        let mut sink = BlobSink {
            out: &mut self.out,
            written: 0,
        };
        write(&mut sink)?;
        let len = to_u32(
            usize::try_from(sink.written).unwrap_or(usize::MAX),
            "overlay blob too large for u32 offsets",
        )?;

        let entry = &mut self.toc[index];
        entry.name[..name.len()].copy_from_slice(name);
        entry.offset = aligned;
        entry.len = len;

        self.file_pos = aligned.checked_add(len).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "overlay file too large for u32 offsets",
            )
        })?;
        self.written += 1;
        Ok(len)
    }

    /// Where a blob already written landed: `(offset, len)`.
    ///
    /// Lets a test slice one blob out of the file the writer produced without
    /// re-implementing the table of contents to find it.
    #[cfg(test)]
    pub(super) fn blob_extent(&self, index: usize) -> (usize, usize) {
        let entry = &self.toc[index];
        (
            usize::try_from(entry.offset).unwrap_or(0),
            usize::try_from(entry.len).unwrap_or(0),
        )
    }

    /// Total bytes the file holds so far, padding included.
    pub(super) const fn bytes_written(&self) -> u32 {
        self.file_pos
    }

    /// Write the table of contents into the region [`Self::new`] reserved, and
    /// hand the sink back so the caller can flush and persist it.
    ///
    /// # Errors
    /// Refuses a file that is missing a blob — every TOC slot names a blob the
    /// reader looks for, and a slot left zeroed would point it at offset 0 —
    /// and propagates I/O errors from the sink.
    pub(super) fn finish(mut self) -> io::Result<W> {
        if self.written != TOC_COUNT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "overlay finished with a blob missing",
            ));
        }
        let end = self.out.stream_position()?;
        let _ = self.out.seek(SeekFrom::Start(HEADER_LEN_U64))?;
        // Field-by-field because TocEntry is not `Pod` (the `[u8; 56]` name
        // field conflicts with `object::pod::Pod` in the dependency graph).
        for entry in &self.toc {
            self.out.write_all(&entry.name)?;
            self.out.write_all(&entry.offset.to_le_bytes())?;
            self.out.write_all(&entry.len.to_le_bytes())?;
        }
        let _ = self.out.seek(SeekFrom::Start(end))?;
        Ok(self.out)
    }
}
