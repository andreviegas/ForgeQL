//! Per-session in-RAM mutations on top of the persistent columnar overlay.
//!
//! [`DirtyOverlay`] tracks changed files within a session.  It is always empty
//! at session start (populated only when `reindex_files` is wired in during
//! PhaseFT2).  Even when empty, the plumbing in [`ColumnarStorage`] passes
//! through it, so adding PhaseFT2 requires no further structural changes here.
//!
//! ## Query semantics
//!
//! Results are the **union** of persistent overlay rows and dirty rows, with
//! dirty rows taking precedence: any persistent segment whose
//! `hex_content_id` appears in `removed_hex_ids` is silently omitted.
//!
//! [`ColumnarStorage`]: super::columnar_storage::ColumnarStorage
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use roaring::RoaringBitmap;

use super::segment_reader::SegmentReader;
use crate::ir::Clauses;
use crate::result::SymbolMatch;

// ─────────────────────────────────────────────────────────────────────────────
// DirtySegment
// ─────────────────────────────────────────────────────────────────────────────

/// A single changed-file segment held in the dirty overlay.
pub struct DirtySegment {
    /// The segment reader for the new version of the file.
    pub reader: Arc<SegmentReader>,
    /// Workspace-relative source path of the file this segment was built from.
    ///
    /// Passed as `source_path` to `materialize_rows` so that query results
    /// carry the correct relative path (matching persistent overlay behaviour).
    pub source_path: PathBuf,
    /// `SegmentMeta::hex_content_id` of the persistent segment being replaced,
    /// or an empty string when this is a brand-new file with no prior entry.
    pub replaces_hex: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// DirtyOverlay
// ─────────────────────────────────────────────────────────────────────────────

/// Per-session in-RAM mutations on top of the persistent columnar overlay.
///
/// Queries union `ColumnarStorage`'s persistent segments + `DirtyOverlay`
/// segments, filtering out rows whose hex content ID is in `removed_hex_ids`.
///
/// All methods are O(n) in the number of dirty segments, which is small
/// (proportional to files changed in one session).
#[derive(Default)]
pub struct DirtyOverlay {
    /// Segments built from the new versions of changed files.
    ///
    /// Written to `.forgeql-staging/<hex>/` by PhaseFT2.  In PhaseFT1 this
    /// Vec is always empty — the struct exists to wire up the plumbing.
    pub added: Vec<DirtySegment>,

    /// Hex content IDs of persistent segments that are hidden from queries.
    ///
    /// Populated when a file is changed (`replaces_hex` of the old segment)
    /// or deleted (`purge_file`).  Querying code skips any persistent segment
    /// whose `hex_content_id` is in this set.
    pub removed_hex_ids: HashSet<String>,
}

impl DirtyOverlay {
    /// Create an empty `DirtyOverlay`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when no segments have been added and no blobs removed.
    ///
    /// When `true`, `ColumnarStorage` can skip all dirty-overlay logic for
    /// maximum query performance — identical to pre-PhaseFT1 behaviour.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed_hex_ids.is_empty()
    }

    /// Whether the persistent segment with this hex content ID is shadowed.
    ///
    /// Called by `find_symbols` / `find_usages` to filter out persistent
    /// segments for files that have been changed or deleted.
    #[must_use]
    pub fn shadows(&self, hex_content_id: &str) -> bool {
        self.removed_hex_ids.contains(hex_content_id)
    }

    /// Add a new dirty segment for a changed file.
    ///
    /// If `replaces_hex` is non-empty it is added to `removed_hex_ids` so
    /// that the old persistent segment is hidden from queries.
    ///
    /// Called by PhaseFT2's `reindex_files`.
    pub fn add_segment(
        &mut self,
        reader: Arc<SegmentReader>,
        source_path: PathBuf,
        replaces_hex: String,
    ) {
        if !replaces_hex.is_empty() {
            let _ = self.removed_hex_ids.insert(replaces_hex.clone());
        }
        self.added.push(DirtySegment {
            reader,
            source_path,
            replaces_hex,
        });
    }

    /// Mark a persistent segment hex as removed without adding a replacement.
    ///
    /// Used for deleted files (`purge_file`) where the file no longer exists
    /// and therefore no new segment is built.
    pub fn remove_hex(&mut self, hex: String) {
        let _ = self.removed_hex_ids.insert(hex);
    }

    /// Remove any previously staged dirty segment for `source_path`.
    ///
    /// Called when the same file is changed a second time within a session.
    /// Returns the `replaces_hex` of the removed entry (so the caller can
    /// re-add the original hex to `removed_hex_ids` if needed).
    ///
    /// Note: the entry in `removed_hex_ids` from the *first* change is left
    /// intact — the persistent segment is still shadowed.
    pub fn remove_stale_for_path(&mut self, source_path: &Path) -> Option<String> {
        let pos = self
            .added
            .iter()
            .position(|ds| ds.source_path == source_path)?;
        let removed = self.added.remove(pos);
        Some(removed.replaces_hex)
    }

    #[must_use]
    pub fn staged_hex_ids(&self) -> Vec<String> {
        self.added
            .iter()
            .map(|ds| ds.reader.content_id_hex())
            .collect()
    }

    // ─────────────────────────────────────────────────────────────────────
    // Materialisation helpers — called by ColumnarStorage
    // ─────────────────────────────────────────────────────────────────────

    /// Materialize all dirty rows that pass `clauses`.
    ///
    /// ORDER BY / LIMIT are **not** applied here — the caller merges this
    /// result with persistent rows and applies those clauses once over the
    /// combined set.
    ///
    /// Returns an empty `Vec` when `self.added` is empty.
    #[must_use]
    pub fn materialize_all(&self, clauses: &Clauses) -> Vec<SymbolMatch> {
        if self.added.is_empty() {
            return Vec::new();
        }
        let mut results = Vec::new();
        for ds in &self.added {
            let all_rows: RoaringBitmap = (0..ds.reader.row_count).collect();
            // Apply enrichment postings prefilter when available; degrades
            // gracefully for segments that lack posting files.
            let rows = ds.reader.prefilter_enrichment_postings(all_rows, clauses);
            let mut seg_results = ds.reader.materialize_rows(&rows, Some(&ds.source_path));
            results.append(&mut seg_results);
        }
        results
    }

    /// All dirty rows for the given exact symbol `name`.
    ///
    /// Uses the per-segment FST for an O(log n) name lookup instead of
    /// a full linear scan.  Returns an empty `Vec` when no dirty segment
    /// contains `name`.
    #[must_use]
    pub fn lookup_name_results(&self, name: &str, clauses: &Clauses) -> Vec<SymbolMatch> {
        if self.added.is_empty() {
            return Vec::new();
        }
        let mut results = Vec::new();
        for ds in &self.added {
            let row_ids = ds.reader.lookup_name(name);
            if row_ids.is_empty() {
                continue;
            }
            let bm: RoaringBitmap = row_ids.into_iter().collect();
            let rows = ds.reader.prefilter_enrichment_postings(bm, clauses);
            let mut seg_results = ds.reader.materialize_rows(&rows, Some(&ds.source_path));
            results.append(&mut seg_results);
        }
        results
    }
}
// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_overlay_is_empty() {
        let d = DirtyOverlay::new();
        assert!(d.is_empty());
        assert!(!d.shadows("abc123"));
        assert!(d.materialize_all(&crate::ir::Clauses::default()).is_empty());
    }

    #[test]
    fn remove_hex_shadows_correctly() {
        let mut d = DirtyOverlay::new();
        assert!(!d.shadows("deadbeef"));
        d.remove_hex("deadbeef".to_owned());
        assert!(d.shadows("deadbeef"));
        assert!(!d.shadows("cafebabe"));
        assert!(!d.is_empty());
    }

    #[test]
    fn remove_stale_for_unknown_path_returns_none() {
        let mut d = DirtyOverlay::new();
        assert!(
            d.remove_stale_for_path(Path::new("no/such/file.cpp"))
                .is_none()
        );
    }
}
