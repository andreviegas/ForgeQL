#![allow(clippy::redundant_pub_crate)]
//! [`Overlay`] — workspace-level merged index for the columnar backend.
//!
//! Keyed by commit SHA, stored at
//! `<bare-repo>/forgeql/overlays/<provider_id>/<commit_sha>.bin`.
//!
//! The overlay merges N per-file segments into a single queryable index
//! shared across all [`ColumnarStorage`] sessions on the same commit SHA.
//! Multiple sessions mmap the same file; the OS reference-counts physical
//! pages so RSS does not multiply by session count.
//!
//! File format:
//! ```text
//! [0..4]   b"FQOV"           magic
//! [4..8]   schema_version: u32 (little-endian)
//! [8..16]  generation: u64 (little-endian)
//! [16..24] payload_len: u64 (little-endian)
//! [24..]   bincode-serialised OverlayPayload
//! ```

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use bytemuck::cast_slice;
use fst::Map as FstMap;
use memmap2::MmapOptions;
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────────────────────
// On-disk constants
// ─────────────────────────────────────────────────────────────────────────────

/// Magic bytes at the start of every overlay file.
pub(crate) const MAGIC: [u8; 4] = *b"FQOV";
/// Current schema version.  Bump on any breaking format change.
///
/// History:
/// - **1**: initial overlay format (segments, global_row_table,
///   kind_postings, name_fst_bytes, name_postings_bytes).
/// - **2**: adds `name_trigram_postings` for fast `name LIKE`/`MATCHES`
///   prefiltering.  Old (v1) overlay files are rebuilt on next `USE`.
pub(crate) const SCHEMA_VERSION: u32 = 2;
/// Number of bytes occupied by the fixed header before the bincode payload.
pub(crate) const HEADER_LEN: usize = 24;

// ─────────────────────────────────────────────────────────────────────────────
// On-disk data structures (bincode-serialised)
// ─────────────────────────────────────────────────────────────────────────────

/// Pointer from a global row ID into a specific segment.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RowPtr {
    /// Index into the overlay's segment list.
    pub segment_idx: u32,
    /// Row index within that segment.
    pub local_row_idx: u32,
}

/// Per-segment metadata stored in the overlay's segment table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentMeta {
    /// Hex-encoded content ID — directory name under
    /// `segments/<provider_id>/`.
    pub hex_content_id: String,
    /// Source file path **relative to the worktree root**.
    pub source_path: std::path::PathBuf,
    /// Number of rows in this segment (== symbols in the source file).
    pub row_count: u32,
}

/// The bincode-serialised body written after the fixed header.
#[derive(Serialize, Deserialize)]
pub struct OverlayPayload {
    /// Segments in stable sort order (by `hex_content_id`).
    pub segments: Vec<SegmentMeta>,
    /// `global_row_id → (segment_idx, local_row_idx)`.
    /// Indexed by global row ID (u32 index into this Vec).
    pub global_row_table: Vec<RowPtr>,
    /// `fql_kind` string → serialised [`RoaringBitmap`] bytes covering
    /// global row IDs with that kind.
    pub kind_postings: HashMap<String, Vec<u8>>,
    /// Raw FST bytes for name-to-global-row-id lookup.
    /// The FST value encodes `(byte_offset_into_postings << 32) | count`.
    pub name_fst_bytes: Vec<u8>,
    /// Flat array of u32 global row IDs; indexed by `(offset, count)` pairs
    /// encoded in the name FST values.
    pub name_postings_bytes: Vec<u8>,
    /// Trigram → serialised [`RoaringBitmap`] of global row IDs whose
    /// **lower-cased** name contains that 3-byte window.  Used as a
    /// candidate prefilter for `name LIKE 'pattern'` and `name MATCHES`
    /// predicates — mirrors the legacy `TrigramIndex`.
    #[serde(default)]
    pub name_trigram_postings: HashMap<[u8; 3], Vec<u8>>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Overlay reader
// ─────────────────────────────────────────────────────────────────────────────

/// Workspace-level merged index shared across all [`ColumnarStorage`] instances
/// on the same commit SHA.
pub struct Overlay {
    /// Ordered list of segments.
    segments: Vec<SegmentMeta>,
    /// `global_row_id → (segment_idx, local_row_idx)`.
    global_row_table: Vec<RowPtr>,
    /// Flat array of u32 global row IDs for name postings decode.
    name_postings_bytes: Vec<u8>,
    /// Decoded bitmaps for O(1) `fql_kind` prefilter.
    kind_bitmaps: HashMap<String, RoaringBitmap>,
    /// Decoded FST for name-to-global-row-id lookup.
    name_fst: FstMap<Vec<u8>>,
    /// Decoded trigram → row-id bitmaps for `name LIKE`/`MATCHES` prefilter.
    trigram_bitmaps: HashMap<[u8; 3], RoaringBitmap>,
    generation: u64,
}

impl Overlay {
    /// Open an overlay file from disk and decode it into memory.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read, the magic/version is wrong,
    /// the payload is truncated, or the bincode or FST data is corrupt.
    pub fn open(path: &Path) -> Result<Arc<Self>> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening overlay {}", path.display()))?;
        #[allow(unsafe_code)] // read-only mmap of immutable overlay file
        let data = unsafe { MmapOptions::new().map(&file) }
            .with_context(|| format!("mmap overlay {}", path.display()))?;
        drop(file);

        ensure!(data.len() >= HEADER_LEN, "overlay file too short");
        ensure!(
            data[..4] == MAGIC,
            "invalid overlay magic in {}",
            path.display()
        );

        #[allow(clippy::indexing_slicing)] // bounds checked by ensure! above
        let schema_version =
            u32::from_le_bytes(data[4..8].try_into().context("schema_version bytes")?);
        ensure!(
            schema_version == SCHEMA_VERSION,
            "overlay schema version mismatch in {}: expected {SCHEMA_VERSION}, got {schema_version}",
            path.display()
        );

        #[allow(clippy::indexing_slicing)]
        let generation = u64::from_le_bytes(data[8..16].try_into().context("generation bytes")?);
        #[allow(clippy::indexing_slicing)]
        #[allow(clippy::cast_possible_truncation)]
        let payload_len =
            u64::from_le_bytes(data[16..24].try_into().context("payload_len bytes")?) as usize;

        ensure!(
            data.len() >= HEADER_LEN + payload_len,
            "overlay file truncated: expected {} bytes, got {}",
            HEADER_LEN + payload_len,
            data.len()
        );

        #[allow(clippy::indexing_slicing)]
        let mut payload: OverlayPayload =
            bincode::deserialize(&data[HEADER_LEN..HEADER_LEN + payload_len])
                .context("deserialising overlay payload")?;
        // mmap is no longer needed — bincode has copied all data to the heap.
        drop(data);

        // Decode kind bitmaps.
        let mut kind_bitmaps = HashMap::with_capacity(payload.kind_postings.len());
        for (kind, bytes) in &payload.kind_postings {
            let bm = RoaringBitmap::deserialize_from(bytes.as_slice())
                .with_context(|| format!("decoding kind bitmap for '{kind}'"))?;
            let _ = kind_bitmaps.insert(kind.clone(), bm);
        }

        // Decode FST — move bytes to avoid a second heap copy (no .clone()).
        let fst_bytes = std::mem::take(&mut payload.name_fst_bytes);
        let name_fst = FstMap::new(fst_bytes).context("loading name FST from overlay")?;

        // Decode trigram bitmaps (absent in v1 overlays — empty map is fine).
        let mut trigram_bitmaps = HashMap::with_capacity(payload.name_trigram_postings.len());
        for (trigram, bytes) in &payload.name_trigram_postings {
            let bm = RoaringBitmap::deserialize_from(bytes.as_slice())
                .with_context(|| format!("decoding trigram bitmap {trigram:?}"))?;
            let _ = trigram_bitmaps.insert(*trigram, bm);
        }

        // Move the remaining live fields out of the payload before dropping it.
        let segments = std::mem::take(&mut payload.segments);
        let global_row_table = std::mem::take(&mut payload.global_row_table);
        let name_postings_bytes = std::mem::take(&mut payload.name_postings_bytes);

        Ok(Arc::new(Self {
            segments,
            global_row_table,
            name_postings_bytes,
            kind_bitmaps,
            name_fst,
            trigram_bitmaps,
            generation,
        }))
    }

    /// Monotonic generation counter — bumped by every reindex.
    /// Returns the overlay generation (reserved for Phase 07 staleness checks).
    #[allow(dead_code)]
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Ordered list of segments in this overlay.
    #[must_use]
    pub fn segments(&self) -> &[SegmentMeta] {
        &self.segments
    }

    /// Total number of rows across all segments.
    #[must_use]
    pub const fn row_count(&self) -> u32 {
        #[allow(clippy::cast_possible_truncation)]
        let len = self.global_row_table.len() as u32;
        len
    }

    /// Get the precomputed global-row-id bitmap for a given `fql_kind`.
    ///
    /// Returns `None` if the kind is not present in any segment.
    #[must_use]
    pub fn prefilter_kind(&self, kind: &str) -> Option<&RoaringBitmap> {
        self.kind_bitmaps.get(kind)
    }

    /// Look up all global row IDs for a given symbol name (exact match).
    #[must_use]
    pub fn lookup_name_bitmap(&self, name: &str) -> RoaringBitmap {
        let Some(encoded) = self.name_fst.get(name.as_bytes()) else {
            return RoaringBitmap::new();
        };
        self.decode_postings(encoded)
    }

    /// Trigram-based candidate prefilter for substring search over names.
    ///
    /// Returns the intersection of the per-trigram global-row-id bitmaps
    /// for every consecutive 3-byte window of `substr` (ASCII-lowercased).
    ///
    /// Returns:
    /// - `None` when `substr` is shorter than 3 bytes — caller must fall
    ///   back to a full scan (no prefilter possible).
    /// - `Some(empty)` when at least one trigram is absent from the index
    ///   (no row can match) **or** the trigram index is empty (v1 overlay
    ///   lacking the section — caller should fall back rather than treat
    ///   the empty result as authoritative).
    /// - `Some(bitmap)` of candidate global row IDs whose name contains
    ///   every trigram of `substr`.  Caller must still evaluate the full
    ///   `LIKE`/`MATCHES` predicate to reject false positives.
    #[must_use]
    pub fn name_substring_candidates(&self, substr: &str) -> Option<RoaringBitmap> {
        let bytes = substr.as_bytes();
        if bytes.len() < 3 {
            return None;
        }
        if self.trigram_bitmaps.is_empty() {
            // v1 overlay without trigram section — no prefilter possible.
            return None;
        }
        let mut trigrams: Vec<[u8; 3]> = Vec::new();
        for w in bytes.windows(3) {
            let t = [
                w[0].to_ascii_lowercase(),
                w[1].to_ascii_lowercase(),
                w[2].to_ascii_lowercase(),
            ];
            if !trigrams.contains(&t) {
                trigrams.push(t);
            }
        }
        let mut bitmaps: Vec<&RoaringBitmap> = Vec::with_capacity(trigrams.len());
        for t in &trigrams {
            match self.trigram_bitmaps.get(t) {
                Some(bm) => bitmaps.push(bm),
                None => return Some(RoaringBitmap::new()),
            }
        }
        bitmaps.sort_unstable_by_key(|bm| bm.len());
        let mut result = bitmaps[0].clone();
        for bm in &bitmaps[1..] {
            result &= *bm;
            if result.is_empty() {
                break;
            }
        }
        Some(result)
    }

    /// Resolve a global row ID to a `RowPtr`.
    ///
    /// Returns `None` if `global_id` is out of range (should not happen
    /// with a valid overlay, but is checked defensively).
    #[must_use]
    pub fn resolve_global(&self, global_id: u32) -> Option<RowPtr> {
        self.global_row_table.get(global_id as usize).copied()
    }

    // ─────────────────────────────────────────────────────────────────────
    // Private helpers
    // ─────────────────────────────────────────────────────────────────────

    fn decode_postings(&self, encoded: u64) -> RoaringBitmap {
        #[allow(clippy::cast_possible_truncation)]
        let count = (encoded & 0xFFFF_FFFF) as usize;
        #[allow(clippy::cast_possible_truncation)]
        let byte_offset = ((encoded >> 32) & 0xFFFF_FFFF) as usize;
        let postings = &self.name_postings_bytes;
        let end = byte_offset + count * 4;
        if end > postings.len() {
            return RoaringBitmap::new();
        }
        #[allow(clippy::indexing_slicing)] // bounds checked above
        cast_slice::<u8, u32>(&postings[byte_offset..end])
            .iter()
            .copied()
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Attempting to open a non-existent file returns an error (not a panic).
    #[test]
    fn open_missing_file_returns_err() {
        let result = Overlay::open(std::path::Path::new("/nonexistent/overlay.bin"));
        assert!(result.is_err(), "expected Err for missing file");
    }

    /// A file with invalid magic returns a descriptive error.
    #[test]
    fn open_wrong_magic_returns_err() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        // Write a header with wrong magic.
        let mut data = vec![0u8; HEADER_LEN];
        data[..4].copy_from_slice(b"XXXX");
        std::fs::write(tmp.path(), &data).expect("write");
        match Overlay::open(tmp.path()) {
            Ok(_) => panic!("expected Err for wrong magic, but got Ok"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(msg.contains("magic"), "error should mention magic: {msg}");
            }
        }
    }

    /// `name_substring_candidates` returns `None` for sub-trigram queries.
    #[test]
    fn substring_candidates_none_for_short_input() {
        let overlay = Overlay {
            segments: Vec::new(),
            global_row_table: Vec::new(),
            name_postings_bytes: Vec::new(),
            kind_bitmaps: HashMap::new(),
            name_fst: FstMap::default(),
            trigram_bitmaps: {
                // Non-empty so we exercise the length check, not the v1 fallback.
                let mut m = HashMap::new();
                let _ = m.insert(*b"abc", RoaringBitmap::new());
                m
            },
            generation: 1,
        };
        assert!(overlay.name_substring_candidates("ab").is_none());
        assert!(overlay.name_substring_candidates("").is_none());
    }

    /// `name_substring_candidates` intersects per-trigram bitmaps and
    /// short-circuits to an empty bitmap when a trigram is missing.
    #[test]
    fn substring_candidates_intersects_and_misses() {
        let mut bm_alp = RoaringBitmap::new();
        let _ = bm_alp.insert(0); // alpha
        let _ = bm_alp.insert(2); // alphabet
        let mut bm_lph = RoaringBitmap::new();
        let _ = bm_lph.insert(0);
        let _ = bm_lph.insert(2);
        let mut bm_pha = RoaringBitmap::new();
        let _ = bm_pha.insert(0);
        let _ = bm_pha.insert(2);
        let mut trigram_bitmaps = HashMap::new();
        let _ = trigram_bitmaps.insert(*b"alp", bm_alp);
        let _ = trigram_bitmaps.insert(*b"lph", bm_lph);
        let _ = trigram_bitmaps.insert(*b"pha", bm_pha);

        let overlay = Overlay {
            segments: Vec::new(),
            global_row_table: Vec::new(),
            name_postings_bytes: Vec::new(),
            kind_bitmaps: HashMap::new(),
            name_fst: FstMap::default(),
            trigram_bitmaps,
            generation: 1,
        };

        // "alp" hits a single trigram with rows {0, 2}.
        let got = overlay.name_substring_candidates("alp").expect("some");
        assert_eq!(got.iter().collect::<Vec<_>>(), vec![0, 2]);
        // "alpha" trigrams: alp, lph, pha — all present, intersection {0, 2}.
        let got = overlay.name_substring_candidates("alpha").expect("some");
        assert_eq!(got.iter().collect::<Vec<_>>(), vec![0, 2]);
        // ASCII case-insensitivity.
        let got = overlay.name_substring_candidates("ALP").expect("some");
        assert_eq!(got.iter().collect::<Vec<_>>(), vec![0, 2]);
        // Missing trigram \u2192 Some(empty).
        let got = overlay.name_substring_candidates("zzz").expect("some");
        assert!(got.is_empty());
    }
}
