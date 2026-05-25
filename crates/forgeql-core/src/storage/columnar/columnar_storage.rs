//! [`ColumnarStorage`] — the columnar disk-backed [`StorageEngine`].
//!
//! Implements the `StorageEngine` trait using the workspace overlay index
//! and per-file segment readers introduced in Phase 04/05.
//!
//! Query execution for `FIND symbols USING 'columnar'`:
//! 1. **Prefilter** — use the overlay's merged Roaring bitmaps to narrow
//!    the candidate global row IDs via indexed predicates (`fql_kind`, `name`).
//! 2. **Group** — partition candidate global row IDs by segment index.
//! 3. **Materialize** — for each segment, call `SegmentReader::materialize_rows`
//!    with the per-segment local row bitmap and the absolute source path.
//! 4. **Residual filter** — apply remaining clauses (other `WHERE`, `IN`,
//!    `EXCLUDE`, `ORDER BY`, `GROUP BY`, `LIMIT`, `OFFSET`) via
//!    `filter::apply_clauses`.
//!
//! `SHOW` commands (`resolve_symbol`, etc.) are out of scope for Phase 05 and
//! return a "Phase 06" error so callers can fall back to the legacy backend.
use std::path::PathBuf;
use std::sync::Arc;

use crate::ast::index::IndexStats;
use crate::ast::lang::LanguageRegistry;

use super::dirty_overlay::DirtyOverlay;
use super::overlay::Overlay;
use super::segment_reader::SegmentReader;

mod commit;
mod fast_paths;
mod query;

// ─────────────────────────────────────────────────────────────────────────────
// ColumnarStorage
// ─────────────────────────────────────────────────────────────────────────────

/// Disk-backed columnar [`StorageEngine`] backed by per-file segment readers
/// and a workspace-level overlay index.
///
pub struct ColumnarStorage {
    /// Worktree root; used to resolve absolute source file paths and strip
    /// prefixes when computing relative paths for `DirtyOverlay`.
    worktree_root: PathBuf,
    /// Per-segment readers in the same order as `overlay.segments()`.
    segments: Vec<Arc<SegmentReader>>,
    /// Workspace overlay shared across sessions on the same commit SHA.
    overlay: Arc<Overlay>,
    /// Per-session in-RAM mutations on top of the persistent overlay.
    ///
    /// Always empty at session start. Populated by PhaseFT2 `reindex_files`.
    /// Queried by `find_symbols` / `find_usages` to union persistent + dirty rows.
    pub(crate) dirty: DirtyOverlay,
    /// Staging directory for per-session reindexed segments (`.forgeql-staging/`).
    staging_dir: PathBuf,
    /// Language registry used by `reindex_files` to parse modified files.
    lang_registry: Arc<LanguageRegistry>,
    /// Path to the delta file that persists the dirty overlay across restarts.
    ///
    /// Written after every `reindex_files` / `purge_file` call.
    /// Included in `BEGIN TRANSACTION` checkpoint commits (so `git reset --hard`
    /// restores it automatically on `ROLLBACK`) but excluded from user-facing
    /// `COMMIT MESSAGE` commits via `git::CLEAN_COMMIT_EXCLUDED`.
    delta_path: PathBuf,
    /// Pre-computed index stats for `index_stats()`.
    ///
    /// Populated at construction from `overlay.row_count()` so that
    /// columnar sessions appear in `SHOW SOURCES` without a full scan.
    stats: IndexStats,
}

impl ColumnarStorage {
    /// Create a new `ColumnarStorage` from an open overlay and its segments.
    ///
    /// `segments` **must** be in the same order as `overlay.segments()`.
    #[must_use]
    pub fn new(
        worktree_root: PathBuf,
        segments: Vec<Arc<SegmentReader>>,
        overlay: Arc<Overlay>,
        lang_registry: Arc<LanguageRegistry>,
    ) -> Self {
        let staging_dir = worktree_root.join(super::STAGING_DIR_NAME);
        let delta_path = worktree_root.join(super::DELTA_FILE_NAME);
        let stats = IndexStats {
            rows: overlay.row_count() as usize,
            ..IndexStats::default()
        };
        Self {
            worktree_root,
            segments,
            overlay,
            dirty: DirtyOverlay::new(),
            staging_dir,
            lang_registry,
            delta_path,
            stats,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fast_paths::pattern_as_prefix;

    #[test]
    fn prefix_one_char_wildcard() {
        assert_eq!(pattern_as_prefix("k%"), Some(b"k".to_vec()));
    }

    #[test]
    fn prefix_one_char_underscore_wildcard() {
        // 'k' is the literal prefix; '_' is a single-char wildcard → stop
        assert_eq!(pattern_as_prefix("k_%"), Some(b"k".to_vec()));
    }

    #[test]
    fn prefix_two_chars_wildcard() {
        assert_eq!(pattern_as_prefix("ab%"), Some(b"ab".to_vec()));
    }

    #[test]
    fn prefix_three_char_literal_returns_none() {
        // 3-char literal → None so trigrams handle it
        assert_eq!(pattern_as_prefix("abc%"), None);
    }

    #[test]
    fn prefix_two_char_literal_then_underscore() {
        // 'k_a%' — 'k' literal, then '_' wildcard → 1-char prefix
        assert_eq!(pattern_as_prefix("k_a%"), Some(b"k".to_vec()));
    }

    #[test]
    fn prefix_starts_with_percent_returns_none() {
        assert_eq!(pattern_as_prefix("%foo"), None);
    }

    #[test]
    fn prefix_starts_with_underscore_returns_none() {
        assert_eq!(pattern_as_prefix("_k%"), None);
    }

    #[test]
    fn prefix_suffix_pattern_returns_none() {
        assert_eq!(pattern_as_prefix("%k"), None);
    }

    #[test]
    fn prefix_case_insensitive() {
        // Builder lowercases names; pattern_as_prefix must lowercase too.
        assert_eq!(pattern_as_prefix("AB%"), Some(b"ab".to_vec()));
    }
}
