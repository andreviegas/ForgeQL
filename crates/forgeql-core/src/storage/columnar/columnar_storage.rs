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
use super::open_cache::SharedOpen;
use super::overlay::Overlay;
use super::segment_reader::SegmentReader;

mod commit;
mod fast_paths;
// The row budget is one bound with several enforcement sites — the legacy
// backend and the dirty-union check reuse these rather than growing their own.
pub(in crate::storage) use fast_paths::{
    find_max_rows, row_budget_exceeded, usages_budget_exceeded,
};
mod query;
mod upstream_chain;
mod usage_adjust;

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
    /// The committed half of this commit's index, shared with every other
    /// session on the same commit.
    ///
    /// Held as the whole cache entry rather than as copies of its contents, and
    /// that is load-bearing rather than tidiness: the shared-open cache tracks
    /// entries weakly, so a session holding only clones of the overlay and the
    /// readers keeps *those* alive while letting the entry die. The next
    /// session then finds nothing to reuse and decodes its own. The cache was
    /// exactly that dead until an integration test that deletes the files
    /// between two opens caught it.
    shared: Arc<SharedOpen>,
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
    /// The commit whose overlay this session serves from. Equal to the
    /// attach commit for a flat open; for a chain attach it is the chain's
    /// master commit, so a manifest written at the next COMMIT names the
    /// real overlay and the chain stays one layer deep.
    pub(crate) master_commit: String,
    /// Pre-computed index stats for `index_stats()`.
    ///
    /// Populated at construction from `overlay.row_count()` so that
    /// columnar sessions appear in `SHOW SOURCES` without a full scan.
    stats: IndexStats,
    /// Source paths whose staged index state was dropped by the last
    /// `load_delta` (previous-generation delta or missing staging segment).
    /// The session layer drains this via `take_pending_reindex_paths` and
    /// re-indexes the files from the worktree.
    pub(crate) pending_reindex: Vec<PathBuf>,

    /// Token dictionary + trigram postings backing substring `FIND usages`.
    ///
    /// Built on the first substring query rather than at open: the build cost
    /// is the same either way, and a session that only ever names identifiers
    /// should not pay it. Covers the persistent overlay only — the dirty
    /// overlay is scanned per query, because it changes as files are edited
    /// and a cached copy of it would go stale.
    substring_index: std::sync::OnceLock<SubstringIndex>,

    /// The correction that makes `usages` the commit's own count on a session
    /// with dirty rows — see the `usage_adjust` module. Built on
    /// first use, rebuilt when the dirty overlay it was built from changes;
    /// `None` until a dirty session first stamps a row.
    usage_adjust: std::sync::Mutex<Option<Arc<usage_adjust::UsageAdjust>>>,
}

/// The dictionary a substring `FIND usages` searches, with its trigram tier.
///
/// `tokens` holds the usage tokens carrying a character outside
/// `[A-Za-z0-9_]` — the whole-text ones, such as include paths — because those
/// are the only tokens a substring query can reach; `trigrams` maps a trigram
/// to the indices of the tokens containing it. The tier is a *prefilter*: its
/// answer is a superset that the caller verifies with a real `contains` before
/// searching for the token.
struct SubstringIndex {
    /// Distinct non-identifier usage tokens, sorted and deduplicated.
    tokens: Vec<String>,

    /// Trigram postings over `tokens`, keyed by index into it.
    trigrams: crate::ast::trigram::TrigramIndex,
}

impl ColumnarStorage {
    /// Create a `ColumnarStorage` over an already-open shared entry.
    ///
    /// This is the path [`Self::warm_or_open`] uses, and the only one that
    /// shares: the entry is taken whole rather than unpacked, and holding it is
    /// what keeps this commit's decode available to the next session on it.
    #[must_use]
    pub fn from_shared(
        worktree_root: PathBuf,
        shared: Arc<SharedOpen>,
        lang_registry: Arc<LanguageRegistry>,
    ) -> Self {
        let staging_dir = worktree_root.join(super::STAGING_DIR_NAME);
        let delta_path = worktree_root.join(super::DELTA_FILE_NAME);
        let stats = IndexStats {
            rows: shared.overlay.row_count() as usize,
            ..IndexStats::default()
        };
        Self {
            worktree_root,
            shared,
            dirty: DirtyOverlay::new(),
            master_commit: String::new(),
            staging_dir,
            lang_registry,
            delta_path,
            stats,
            pending_reindex: Vec::new(),
            substring_index: std::sync::OnceLock::new(),
            usage_adjust: std::sync::Mutex::new(None),
        }
    }

    /// Create a `ColumnarStorage` over a decode this caller owns outright.
    ///
    /// The overlay and readers are wrapped in an entry of their own — one no
    /// cache published and nothing else can find. A session built this way
    /// therefore shares with nobody, and so does the next one built the same
    /// way. That is correct for tests and benchmarks that assemble an overlay
    /// by hand, and wrong for anything on a real session's path, which must
    /// come through [`Self::warm_or_open`]. `segments` **must** be in the same
    /// order as `overlay.segments()`.
    #[must_use]
    pub fn new_unshared(
        worktree_root: PathBuf,
        segments: Vec<Arc<SegmentReader>>,
        overlay: Arc<Overlay>,
        lang_registry: Arc<LanguageRegistry>,
    ) -> Self {
        Self::from_shared(
            worktree_root,
            Arc::new(SharedOpen { overlay, segments }),
            lang_registry,
        )
    }

    /// The workspace overlay for this session's commit.
    fn overlay(&self) -> &Arc<Overlay> {
        &self.shared.overlay
    }

    /// Per-segment readers, in the same order as `overlay().segments()`.
    fn segments(&self) -> &[Arc<SegmentReader>] {
        &self.shared.segments
    }

    /// The shared cache entry backing this session.
    ///
    /// Exposed so a test can prove two sessions on one commit were handed the
    /// same decode rather than equal copies of one — the property this type
    /// exists to provide and the one a green suite is least likely to notice
    /// losing.
    #[must_use]
    pub const fn shared_entry(&self) -> &Arc<SharedOpen> {
        &self.shared
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
