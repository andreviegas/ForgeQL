//! Posting blobs read in place — nothing decoded at open.
//!
//! A `.fqsf` segment stores its Roaring posting lists — `postings_fql_kind`,
//! one `postings_<field>` per enrichment field that stayed under its posting
//! budget, and `name_prefix` — as `[entry_count: u32 LE]` followed by
//! `entry_count` entries of `[key] [bitmap_len: u32 LE] [bitmap bytes]`. The
//! key is a `u32 LE` id for the kind and field blobs (a string-pool id) and
//! `[len: u8] [bytes]` for the prefix blob (a lower-cased name prefix).
//!
//! [`SegmentReader::open`] used to deserialise every bitmap of every blob into
//! a heap `HashMap` per segment, paid once by the overlay build and again by
//! the session that opened the result, and resident for as long as the
//! segments were held. Measured on a 3.06M-row, 32,748-segment corpus, that
//! decode was about 210 MiB of the ~550 MiB anonymous memory an open of every
//! segment cost (~550 → ~340 MiB; the remainder is the readers' own tables —
//! TOC, column names, string-pool bounds — which this module does not touch). Here a
//! [`PostingBlob`] is only the blob's byte range: `open` walks the entry
//! headers once to check the layout, and a lookup walks them again over the
//! mmap and deserialises the one bitmap it returns, so no posting bitmap is
//! held on the heap between lookups.
//!
//! What moved, in exchange: corrupt *bitmap bytes* (as opposed to a truncated
//! entry table, which `open` still refuses) surface at lookup time as an `Err`
//! from [`decode`], and every query-side caller treats that as "cannot narrow"
//! — the complete but slower path — never as "no rows".
//!
//! [`SegmentReader::open`]: super::SegmentReader::open

use anyhow::{Context, Result, ensure};
use memmap2::Mmap;
use roaring::RoaringBitmap;

/// How each entry of a blob keys its bitmap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum KeyKind {
    /// `u32 LE` — a kind id or an enrichment value's string-pool id.
    #[default]
    Id,
    /// `[len: u8] [bytes]` — a lower-cased name prefix.
    Bytes,
}

/// One entry's key, borrowed from the mmap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Key<'m> {
    /// A `u32` id.
    Id(u32),
    /// A byte prefix.
    Bytes(&'m [u8]),
}

/// Byte range of one posting blob within its segment's mmap.
///
/// `start == end` is a blob the TOC does not carry; a blob shorter than its
/// four-byte entry count is treated the same way. Copyable on purpose: the
/// reader keeps one of these per posted blob and nothing else.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct PostingBlob {
    start: usize,
    end: usize,
    key: KeyKind,
}

impl PostingBlob {
    /// Address the TOC range `(start, end)` as a blob keyed by `key`.
    pub(super) const fn new(range: (usize, usize), key: KeyKind) -> Self {
        Self {
            start: range.0,
            end: range.1,
            key,
        }
    }

    /// Whether the blob carries an entry count — the segment "posts" it, even
    /// when that count is zero.
    pub(super) const fn is_present(&self) -> bool {
        self.end >= self.start + 4
    }

    /// The blob's bytes; empty when absent.
    fn bytes<'m>(&self, mmap: &'m Mmap) -> &'m [u8] {
        if self.end > self.start {
            &mmap[self.start..self.end]
        } else {
            &[]
        }
    }

    /// The number of entries the header declares; 0 for an absent blob.
    pub(super) fn entry_count(&self, mmap: &Mmap) -> usize {
        let data = self.bytes(mmap);
        if data.len() < 4 {
            return 0;
        }
        u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize
    }

    /// Walk every entry header once and check that each stays inside the
    /// blob. Bitmap bytes are not decoded. `label` names the blob in the error.
    pub(super) fn validate(&self, mmap: &Mmap, label: &str) -> Result<()> {
        for entry in self.entries(mmap) {
            let _ = entry.with_context(|| format!("{label} blob"))?;
        }
        Ok(())
    }

    /// Iterate `(key, bitmap bytes)` in file order.
    ///
    /// An entry whose header runs past the blob yields `Err` and ends the
    /// walk; the caller decides whether that is a refusal (`open`) or a
    /// "cannot narrow" (a query prefilter).
    pub(super) fn entries<'m>(&self, mmap: &'m Mmap) -> Entries<'m> {
        let data = self.bytes(mmap);
        let (remaining, pos) = if data.len() < 4 {
            (0, data.len())
        } else {
            (
                u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize,
                4,
            )
        };
        Entries {
            data,
            key: self.key,
            pos,
            remaining,
            index: 0,
            failed: false,
        }
    }

    /// Bitmap bytes of the entry keyed `id`, or `None` when the blob has no
    /// such entry. Linear over the entry headers, which the writer keeps
    /// within a per-field budget.
    pub(super) fn find_id<'m>(&self, mmap: &'m Mmap, id: u32) -> Result<Option<&'m [u8]>> {
        for entry in self.entries(mmap) {
            let (key, bytes) = entry?;
            if key == Key::Id(id) {
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }

    /// Bitmap bytes of the entry keyed `wanted`, or `None` when the blob has
    /// no such entry.
    pub(super) fn find_bytes<'m>(&self, mmap: &'m Mmap, wanted: &[u8]) -> Result<Option<&'m [u8]>> {
        for entry in self.entries(mmap) {
            let (key, bytes) = entry?;
            if key == Key::Bytes(wanted) {
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }
}

/// Iterator over a blob's entries; see [`PostingBlob::entries`].
pub(super) struct Entries<'m> {
    data: &'m [u8],
    key: KeyKind,
    pos: usize,
    remaining: usize,
    index: usize,
    failed: bool,
}

impl<'m> Entries<'m> {
    fn read_one(&mut self) -> Result<(Key<'m>, &'m [u8])> {
        let key = match self.key {
            KeyKind::Id => {
                ensure!(
                    self.pos + 4 <= self.data.len(),
                    "posting blob truncated at entry {}",
                    self.index
                );
                let id = u32::from_le_bytes(
                    self.data[self.pos..self.pos + 4]
                        .try_into()
                        .context("entry id bytes")?,
                );
                self.pos += 4;
                Key::Id(id)
            }
            KeyKind::Bytes => {
                ensure!(
                    self.pos < self.data.len(),
                    "posting blob truncated at entry {}",
                    self.index
                );
                let len = self.data[self.pos] as usize;
                self.pos += 1;
                ensure!(
                    self.pos + len <= self.data.len(),
                    "posting blob truncated at prefix bytes for entry {}",
                    self.index
                );
                let bytes = &self.data[self.pos..self.pos + len];
                self.pos += len;
                Key::Bytes(bytes)
            }
        };
        ensure!(
            self.pos + 4 <= self.data.len(),
            "posting blob truncated at bitmap length for entry {}",
            self.index
        );
        let len = u32::from_le_bytes(
            self.data[self.pos..self.pos + 4]
                .try_into()
                .context("bitmap_len bytes")?,
        ) as usize;
        self.pos += 4;
        ensure!(
            self.pos + len <= self.data.len(),
            "posting blob bitmap truncated at entry {}",
            self.index
        );
        let bytes = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok((key, bytes))
    }
}

impl<'m> Iterator for Entries<'m> {
    type Item = Result<(Key<'m>, &'m [u8])>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.remaining == 0 {
            return None;
        }
        let entry = self.read_one();
        if entry.is_err() {
            self.failed = true;
        }
        self.remaining -= 1;
        self.index += 1;
        Some(entry)
    }
}

/// Deserialise one bitmap from its stored bytes.
pub(super) fn decode(bytes: &[u8]) -> Result<RoaringBitmap> {
    RoaringBitmap::deserialize_from(bytes).context("deserialising posting bitmap")
}

#[cfg(test)]
mod tests {
    use memmap2::Mmap;
    use roaring::RoaringBitmap;

    use super::{Key, KeyKind, PostingBlob, decode};

    /// Map `bytes` through a real file so the blob is read the way a segment is.
    fn mapped(bytes: &[u8]) -> (tempfile::TempDir, Mmap) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("blob.bin");
        std::fs::write(&path, bytes).expect("write");
        let file = std::fs::File::open(&path).expect("open");
        #[expect(unsafe_code, reason = "test-only mmap of a file this test wrote")]
        let mmap = unsafe { Mmap::map(&file) }.expect("mmap");
        (tmp, mmap)
    }

    fn bitmap_bytes(rows: &[u32]) -> Vec<u8> {
        let bm: RoaringBitmap = rows.iter().copied().collect();
        let mut out = Vec::new();
        bm.serialize_into(&mut out).expect("serialise");
        out
    }

    /// `[count] ([id][len][bitmap])*` — the kind and field blob layout.
    fn id_blob(entries: &[(u32, &[u32])]) -> Vec<u8> {
        let mut out = (entries.len() as u32).to_le_bytes().to_vec();
        for (id, rows) in entries {
            let bm = bitmap_bytes(rows);
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&(bm.len() as u32).to_le_bytes());
            out.extend_from_slice(&bm);
        }
        out
    }

    /// `[count] ([len: u8][prefix][len][bitmap])*` — the name-prefix layout.
    fn bytes_blob(entries: &[(&[u8], &[u32])]) -> Vec<u8> {
        let mut out = (entries.len() as u32).to_le_bytes().to_vec();
        for (prefix, rows) in entries {
            let bm = bitmap_bytes(rows);
            out.push(prefix.len() as u8);
            out.extend_from_slice(prefix);
            out.extend_from_slice(&(bm.len() as u32).to_le_bytes());
            out.extend_from_slice(&bm);
        }
        out
    }

    #[test]
    fn id_keyed_blob_walks_and_finds_without_decoding_the_rest() {
        let bytes = id_blob(&[(7, &[1, 2, 3]), (9, &[4])]);
        let (_tmp, mmap) = mapped(&bytes);
        let blob = PostingBlob::new((0, bytes.len()), KeyKind::Id);
        assert!(blob.is_present());
        assert_eq!(blob.entry_count(&mmap), 2);
        blob.validate(&mmap, "test").expect("well-formed");

        let keys: Vec<Key<'_>> = blob.entries(&mmap).map(|e| e.expect("entry").0).collect();
        assert_eq!(keys, vec![Key::Id(7), Key::Id(9)]);

        let nine = blob.find_id(&mmap, 9).expect("find").expect("posted");
        assert_eq!(
            decode(nine).expect("decode"),
            std::iter::once(4u32).collect()
        );
        assert!(blob.find_id(&mmap, 8).expect("find").is_none());
    }

    #[test]
    fn bytes_keyed_blob_finds_by_prefix() {
        let bytes = bytes_blob(&[(b"a", &[0, 1]), (b"al", &[0])]);
        let (_tmp, mmap) = mapped(&bytes);
        let blob = PostingBlob::new((0, bytes.len()), KeyKind::Bytes);
        blob.validate(&mmap, "test").expect("well-formed");
        let al = blob
            .find_bytes(&mmap, b"al")
            .expect("find")
            .expect("posted");
        assert_eq!(decode(al).expect("decode"), std::iter::once(0u32).collect());
        assert!(blob.find_bytes(&mmap, b"b").expect("find").is_none());
    }

    #[test]
    fn an_absent_or_headerless_blob_is_not_present_and_walks_empty() {
        let (_tmp, mmap) = mapped(&[1, 2, 3]);
        for blob in [
            PostingBlob::new((0, 0), KeyKind::Id),
            PostingBlob::new((0, 3), KeyKind::Id),
        ] {
            assert!(!blob.is_present());
            assert_eq!(blob.entry_count(&mmap), 0);
            assert_eq!(blob.entries(&mmap).count(), 0);
            blob.validate(&mmap, "test").expect("nothing to check");
        }
    }

    #[test]
    fn a_table_that_runs_past_the_blob_fails_validation_and_ends_the_walk() {
        let mut bytes = id_blob(&[(7, &[1])]);
        // Claim two entries; only one is there.
        bytes[..4].copy_from_slice(&2u32.to_le_bytes());
        let (_tmp, mmap) = mapped(&bytes);
        let blob = PostingBlob::new((0, bytes.len()), KeyKind::Id);
        let err = blob.validate(&mmap, "postings_x").expect_err("truncated");
        assert!(format!("{err:#}").contains("postings_x"), "{err:#}");
        let outcomes: Vec<bool> = blob.entries(&mmap).map(|e| e.is_ok()).collect();
        assert_eq!(
            outcomes,
            vec![true, false],
            "the walk stops at the first bad entry"
        );
        // The good entry is still reachable by key; the missing one is an error, not `None`.
        assert!(blob.find_id(&mmap, 7).expect("find").is_some());
        assert!(blob.find_id(&mmap, 8).is_err());
    }
}
