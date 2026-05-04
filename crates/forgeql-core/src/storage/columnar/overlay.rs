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
use roaring::RoaringBitmap;
use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────────────────────
// On-disk constants
// ─────────────────────────────────────────────────────────────────────────────

/// Magic bytes at the start of every overlay file.
pub(crate) const MAGIC: [u8; 4] = *b"FQOV";
/// Current schema version.  Bump on any breaking format change.
pub(crate) const SCHEMA_VERSION: u32 = 1;
/// Number of bytes occupied by the fixed header before the bincode payload.
pub(crate) const HEADER_LEN: usize = 24;

// ─────────────────────────────────────────────────────────────────────────────
// On-disk data structures (bincode-serialised)
// ─────────────────────────────────────────────────────────────────────────────

/// Pointer from a global row ID into a specific segment.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct RowPtr {
    /// Index into the overlay's segment list.
    pub segment_idx: u32,
    /// Row index within that segment.
    pub local_row_idx: u32,
}

/// Per-segment metadata stored in the overlay's segment table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SegmentMeta {
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
pub(crate) struct OverlayPayload {
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
}

// ─────────────────────────────────────────────────────────────────────────────
// Overlay reader
// ─────────────────────────────────────────────────────────────────────────────

/// Workspace-level merged index shared across all [`ColumnarStorage`] instances
/// on the same commit SHA.
pub(crate) struct Overlay {
    payload: OverlayPayload,
    /// Decoded bitmaps for O(1) `fql_kind` prefilter.
    kind_bitmaps: HashMap<String, RoaringBitmap>,
    /// Decoded FST for name-to-global-row-id lookup.
    name_fst: FstMap<Vec<u8>>,
    generation: u64,
}

impl Overlay {
    /// Open an overlay file from disk and decode it into memory.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read, the magic/version is wrong,
    /// the payload is truncated, or the bincode or FST data is corrupt.
    pub(crate) fn open(path: &Path) -> Result<Arc<Self>> {
        let data =
            std::fs::read(path).with_context(|| format!("reading overlay {}", path.display()))?;

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
        let payload: OverlayPayload =
            bincode::deserialize(&data[HEADER_LEN..HEADER_LEN + payload_len])
                .context("deserialising overlay payload")?;

        // Decode kind bitmaps.
        let mut kind_bitmaps = HashMap::with_capacity(payload.kind_postings.len());
        for (kind, bytes) in &payload.kind_postings {
            let bm = RoaringBitmap::deserialize_from(bytes.as_slice())
                .with_context(|| format!("decoding kind bitmap for '{kind}'"))?;
            let _ = kind_bitmaps.insert(kind.clone(), bm);
        }

        // Decode FST.
        let name_fst =
            FstMap::new(payload.name_fst_bytes.clone()).context("loading name FST from overlay")?;

        Ok(Arc::new(Self {
            payload,
            kind_bitmaps,
            name_fst,
            generation,
        }))
    }

    /// Monotonic generation counter — bumped by every reindex.
    /// Returns the overlay generation (reserved for Phase 07 staleness checks).
    #[allow(dead_code)]
    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    /// Ordered list of segments in this overlay.
    pub(crate) fn segments(&self) -> &[SegmentMeta] {
        &self.payload.segments
    }

    /// Total number of rows across all segments.
    pub(crate) const fn row_count(&self) -> u32 {
        #[allow(clippy::cast_possible_truncation)]
        let len = self.payload.global_row_table.len() as u32;
        len
    }

    /// Get the precomputed global-row-id bitmap for a given `fql_kind`.
    ///
    /// Returns `None` if the kind is not present in any segment.
    pub(crate) fn prefilter_kind(&self, kind: &str) -> Option<&RoaringBitmap> {
        self.kind_bitmaps.get(kind)
    }

    /// Look up all global row IDs for a given symbol name (exact match).
    pub(crate) fn lookup_name_bitmap(&self, name: &str) -> RoaringBitmap {
        let Some(encoded) = self.name_fst.get(name.as_bytes()) else {
            return RoaringBitmap::new();
        };
        self.decode_postings(encoded)
    }

    /// Resolve a global row ID to a `RowPtr`.
    ///
    /// Returns `None` if `global_id` is out of range (should not happen
    /// with a valid overlay, but is checked defensively).
    pub(crate) fn resolve_global(&self, global_id: u32) -> Option<RowPtr> {
        self.payload
            .global_row_table
            .get(global_id as usize)
            .copied()
    }

    // ─────────────────────────────────────────────────────────────────────
    // Private helpers
    // ─────────────────────────────────────────────────────────────────────

    fn decode_postings(&self, encoded: u64) -> RoaringBitmap {
        #[allow(clippy::cast_possible_truncation)]
        let count = (encoded & 0xFFFF_FFFF) as usize;
        #[allow(clippy::cast_possible_truncation)]
        let byte_offset = ((encoded >> 32) & 0xFFFF_FFFF) as usize;
        let postings = &self.payload.name_postings_bytes;
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
}
