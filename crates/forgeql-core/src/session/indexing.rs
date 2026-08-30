//! The session's index, and the storage backends that serve it.
//!
//! A session owns one index of the tree checked out in its worktree. This is
//! where that index is built, persisted, resumed from disk when the commit
//! still matches, re-indexed file-by-file after a mutation, and dropped — and
//! where the columnar backend is installed and the legacy one retired.
//!
//! Persisting the index is a cache, not a correctness requirement:
//! `resume_index` rebuilds from scratch on a miss. What it buys is latency,
//! because a session outlives the process that created it and a memory-only
//! index would be rebuilt on every reconnect. On a columnar session it is
//! skipped altogether — the overlay and the transaction delta carry that state
//! instead, so `build_index` writes no `.forgeql-index` and `flush_if_dirty`
//! has nothing to flush.

use std::path::PathBuf;

use anyhow::Result;
use tracing::{debug, info};

use crate::ast::index::SymbolTable;
use crate::session::Session;
use crate::storage::StorageEngine;
use crate::workspace::Workspace;

impl Session {
    /// Configure columnar shadow-write.
    ///
    /// Must be called **before** `build_index` / `resume_index`.  When set,
    /// each full build writes one segment per source file to
    /// `<segments_dir>/<provider_id>/<content-hex>/` and builds an overlay
    /// at `<overlays_dir>/<provider_id>/<commit>.bin`.
    pub fn set_columnar_build(&mut self, ctx: crate::storage::ColumnarBuildContext) {
        self.columnar_build = Some(ctx);
    }

    /// Columnar build context, if shadow-write was enabled at session creation.
    #[must_use]
    pub const fn columnar_build(&self) -> Option<&crate::storage::ColumnarBuildContext> {
        self.columnar_build.as_ref()
    }

    /// Parse all source files in the worktree and build a fresh `SymbolTable`.
    ///
    /// The resulting index is persisted to `<worktree>/.forgeql-index` for
    /// future `resume_index` calls.
    ///
    /// # Errors
    /// Returns `Err` if:
    /// - the workspace cannot be created (e.g. path does not exist)
    /// - tree-sitter parsing fails fatally
    /// - the cache file cannot be written
    pub fn build_index(&mut self) -> Result<()> {
        info!(
            session = %self.id,
            path = %self.worktree_path.display(),
            "building symbol index"
        );

        let workspace = Workspace::new(&self.worktree_path)?;
        // PhaseFT5: build and persist always operate on the legacy backend
        // explicitly; after the route-flip `default_engine_mut()` returns
        // columnar (which has no `build` or `persist_to_cache` semantics).
        let legacy = self
            .backends
            .legacy_storage_mut()
            .ok_or_else(|| anyhow::anyhow!("no legacy backend"))?;

        // Columnar inline fast-path: build a SegmentBuildCtx so SymbolTable::build
        // writes segments inline per-file (with per-file post_pass), skipping the
        // 2-minute sequential merge. Passed to build_with_seg_ctx (not stored on legacy).
        let worktree_root = self.worktree_path.clone();
        let (seg_ctx, inline_state) = self.columnar_build.as_ref().map_or_else(
            || (None, None),
            |ctx| {
                let (sc, state) = ctx.make_inline_ctx(&worktree_root);
                (Some(sc), Some(state))
            },
        );

        legacy.build_with_seg_ctx(&workspace, seg_ctx.as_ref())?;

        // After build (all rayon threads done), extract the inline segment_map
        // and store it on the Session for warm_or_open to consume.
        if let Some(state) = inline_state {
            let map = std::sync::Arc::try_unwrap(state).map_or_else(
                |arc| {
                    arc.segment_map
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                },
                |s| s.segment_map.into_inner().unwrap_or_default(),
            );
            self.prebuilt_segment_map = Some(map);
        }

        // When columnar is configured, the legacy SymbolTable is a transient
        // build artefact used only to shadow-write segments and build the
        // overlay. It is freed by `drop_legacy_index()` immediately after
        // `warm_or_open` completes, so writing it to `.forgeql-index` wastes
        // I/O and produces a file that is never read on future sessions
        // (the warm path skips `resume_index()` when an overlay exists).
        // Record the commit this index was built from on BOTH paths.
        // `cached_commit` is what the USE resume path compares against the
        // bare repo's branch head to decide whether an in-memory session is
        // stale after a REFRESH; leaving it unset on the columnar path made
        // that check silently pass (`None` compares as "not stale") and a
        // re-USE handed back a session based on the pre-REFRESH commit.
        let commit_hash = Self::get_head_oid(&self.worktree_path).unwrap_or_default();
        if self.columnar_build.is_none() {
            legacy.persist_to_cache(&self.worktree_path, &commit_hash, &self.source_name)?;
            debug!(
                session = %self.id,
                commit = %commit_hash,
                "index built and saved"
            );
        } else {
            debug!(
                session = %self.id,
                commit = %commit_hash,
                "index built in-memory (columnar configured — skipping .forgeql-index write)"
            );
        }
        self.cached_commit = Some(commit_hash);

        self.index_dirty = false;
        Ok(())
    }

    /// Load the index from disk if it is fresh, otherwise rebuild from scratch.
    ///
    /// "Fresh" means the cached commit hash equals the current HEAD of the
    /// worktree's repository. This is an O(1) check (one `git rev-parse`).
    ///
    /// # Errors
    /// Propagates errors from `build_index` if a rebuild is needed.
    pub fn resume_index(&mut self) -> Result<()> {
        let head_oid = Self::get_head_oid(&self.worktree_path).unwrap_or_default();

        // PhaseFT5: legacy must be loaded explicitly; `default_engine_mut()`
        // now returns columnar once installed, which has no cache semantics.
        let loaded = self
            .backends
            .legacy_storage_mut()
            .map(|l| l.load_from_cache(&self.worktree_path, &head_oid, &self.source_name))
            .transpose()?
            .unwrap_or(false);

        if loaded {
            debug!(
                session = %self.id,
                commit = %head_oid,
                "cache hit — restoring index from disk"
            );
            self.cached_commit = Some(head_oid);
            self.index_dirty = false;
        } else {
            debug!(
                session = %self.id,
                "cache miss — building fresh index"
            );
            self.build_index()?;
        }

        Ok(())
    }

    /// Build the in-memory index for a session whose columnar open failed.
    ///
    /// [`Self::build_index`] hands `SymbolTable::build` the inline segment
    /// context whenever columnar is configured, and that path returns an
    /// EMPTY table by design — the columnar engine never reads it after
    /// build. On the fallback path that table is the whole answer surface,
    /// so it has to be built again without the inline context, whatever the
    /// cost: a session that answers slowly is a fallback, a session that
    /// answers zero rows with a success status is a defect.
    ///
    /// # Errors
    /// Returns `Err` when the worktree cannot be walked, when the session has
    /// no legacy backend, or when parsing fails hard enough that no table can
    /// be built.
    pub fn build_fallback_index(&mut self) -> Result<()> {
        let workspace = Workspace::new(&self.worktree_path)?;
        let legacy = self
            .backends
            .legacy_storage_mut()
            .ok_or_else(|| anyhow::anyhow!("no legacy backend"))?;
        legacy.build_with_seg_ctx(&workspace, None)?;
        self.cached_commit = Some(Self::get_head_oid(&self.worktree_path).unwrap_or_default());
        self.index_dirty = false;
        Ok(())
    }

    /// Return a reference to the legacy `SymbolTable`, if the engine holds one.
    /// Provided for SHOW / exec paths that still work directly with the table.
    /// Returns `None` for non-legacy backends, or before the index is built.
    #[must_use]
    pub fn index(&self) -> Option<&SymbolTable> {
        self.backends.legacy_storage().and_then(|l| l.table())
    }

    /// `true` when an index has been built (or loaded from cache) for this
    /// session.  Used by callers that need to distinguish "no index yet"
    /// from "empty index" — e.g. ROLLBACK's smart-rollback path.
    #[must_use]
    pub fn has_index(&self) -> bool {
        self.backends.default_engine().has_index()
    }

    /// Return a reference to the legacy backend, if present.
    ///
    /// Used by call sites that need `&SymbolTable` directly (e.g. on-demand
    /// overlay builds in `exec_source`).  Returns `None` in Phase 09+ when
    /// the default backend is no longer legacy.
    #[must_use]
    pub const fn legacy_storage(&self) -> Option<&crate::storage::LegacyMemoryStorage> {
        self.backends.legacy_storage()
    }

    /// The commit hash the current index was built from, if available.
    #[must_use]
    pub fn cached_commit(&self) -> Option<&str> {
        self.cached_commit.as_deref()
    }

    /// Record which commit the active index snapshot corresponds to.
    ///
    /// The build and cache-resume paths record this themselves; the columnar
    /// WARM path installs an overlay read straight from disk without touching
    /// either, so the engine calls this after a successful install. The value
    /// feeds the USE resume staleness check and the resumed-USE `base_commit`
    /// echo.
    pub(crate) fn note_index_commit(&mut self, commit: String) {
        self.cached_commit = Some(commit);
    }

    /// Return a reference to the default (legacy) storage engine.
    #[must_use]
    pub fn engine(&self) -> &dyn StorageEngine {
        self.backends.default_engine()
    }

    /// Return a mutable reference to the default (legacy) storage engine.
    #[must_use]
    pub fn engine_mut(&mut self) -> &mut dyn StorageEngine {
        self.backends.default_engine_mut()
    }

    /// Return a reference to the storage engine to use for a given backend selector.
    ///
    /// - [`Backend::Default`] → the session's default engine: the columnar one
    ///   when one is installed, the legacy in-memory one otherwise.
    /// - [`Backend::Legacy`] → the legacy in-memory engine, always.
    /// - [`Backend::Columnar`] → the columnar engine, if one is installed.
    ///   Returns an error when no columnar engine has been installed.
    ///
    /// # Errors
    /// Returns `Err` if `backend` is [`Backend::Columnar`] and no columnar engine
    /// has been installed for this session.
    pub fn engine_for(&self, backend: &crate::ir::Backend) -> Result<&dyn StorageEngine> {
        self.backends.engine_for(backend)
    }

    /// Returns `true` if a columnar backend is installed on this session.
    #[must_use]
    pub fn has_columnar(&self) -> bool {
        self.backends.has_columnar()
    }

    /// Install (or replace) the columnar storage backend.
    ///
    /// In production this is called by `exec_source` when an overlay file is
    /// found on disk. In tests it can be called directly via
    /// [`ForgeQLEngine::install_columnar_for_session`].
    pub fn install_columnar(&mut self, columnar: Box<dyn StorageEngine>) {
        self.backends.set_columnar(columnar);
    }

    /// Free the legacy `SymbolTable` from memory.
    ///
    /// Called immediately after `install_columnar` (`PhaseFT5`) so that the
    /// legacy RAM is released once columnar is the default engine.
    pub fn drop_legacy_index(&mut self) {
        // Timed because freeing is work, and on a large corpus it is enough of
        // it to look like a hang: the macro table alone holds one heap
        // allocation per macro definition — over six million of them on the
        // Linux kernel — and returning them is a single-threaded walk that no
        // other line accounts for.
        let t_drop = std::time::Instant::now();
        if let Some(legacy) = self.backends.legacy_storage_mut() {
            legacy.drop_stored_index();
        }
        info!(
            ms = t_drop.elapsed().as_millis(),
            mem = %crate::mem::snapshot(),
            "TIMING drop_legacy_index: free the build-time table",
        );
    }

    /// Incrementally re-index the given files after a mutation.
    ///
    /// Each path is purged (all stale entries removed) then re-parsed.
    /// Deleted files are purged only.
    ///
    /// # Errors
    /// Returns `Err` if the index has not been built yet, or if tree-sitter
    /// parsing fails.
    pub fn reindex_files(&mut self, paths: &[PathBuf]) -> Result<()> {
        if let Err(e) = self.reindex_files_reporting(paths) {
            tracing::warn!("columnar reindex_files failed (non-fatal): {e}");
        }
        Ok(())
    }

    /// As [`Self::reindex_files`], but the columnar backend's failure is
    /// RETURNED rather than logged. The freshness gate refuses a command whose
    /// file it could not bring up to date, and a refusal that cannot name its
    /// cause states one it never checked. The legacy backend's failure stays
    /// non-fatal and logged either way: it may have no table at all after
    /// `drop_legacy_index()`, which is not a failure to re-index anything.
    ///
    /// # Errors
    /// Returns the columnar backend's error when re-indexing `paths` into it
    /// failed. The index is marked dirty regardless, since a partial write is
    /// still a write.
    pub fn reindex_files_reporting(&mut self, paths: &[PathBuf]) -> Result<()> {
        // A node-removal verb may have staged tombstones for this reindex; take
        // them (so they never leak into a later mutation) and hand them to both
        // backends. The tombstoned root ordinals stop a byte-identical
        // surviving sibling from adopting a just-deleted node's handle.
        let tombstones = std::mem::take(&mut self.pending_tombstones);
        // PhaseFT5: target both backends explicitly.
        // Legacy may have no table after `drop_legacy_index()` — treat as non-fatal.
        if let Some(legacy) = self.backends.legacy_storage_mut()
            && let Err(e) = legacy.reindex_files_tombstoned(paths, &tombstones)
        {
            tracing::warn!("legacy reindex_files (non-fatal): {e}");
        }
        let outcome = self.backends.columnar_engine_mut().map_or_else(
            || Ok(()),
            |columnar| columnar.reindex_files_tombstoned(paths, &tombstones),
        );
        self.index_dirty = true;
        outcome
    }

    /// Persist the current in-memory index to `.forgeql-index`.
    ///
    /// # Errors
    /// Returns `Err` if no index has been built yet, or if serialisation /
    /// I/O fails.
    pub fn save_index(&mut self) -> Result<()> {
        let commit_hash = Self::get_head_oid(&self.worktree_path).unwrap_or_default();
        // PhaseFT5: persist explicitly via legacy; `default_engine_mut()` now
        // returns columnar when installed.
        if let Some(legacy) = self.backends.legacy_storage_mut() {
            legacy.persist_to_cache(&self.worktree_path, &commit_hash, &self.source_name)?;
        }
        debug!(
            session = %self.id,
            commit = %commit_hash,
            "index saved to disk"
        );
        self.cached_commit = Some(commit_hash);
        self.index_dirty = false;
        Ok(())
    }

    /// Save the index to disk if it has been modified since the last save.
    ///
    /// Cheap no-op when `index_dirty` is `false`.
    ///
    /// # Errors
    /// Propagates `save_index` errors when a flush actually happens.
    pub fn flush_if_dirty(&mut self) -> Result<()> {
        if self.index_dirty {
            if self.backends.has_columnar() {
                // PhaseFT5: columnar sessions manage their delta file at
                // BEGIN TRANSACTION time (git-tracked).  Nothing to flush here.
            } else {
                self.save_index()?;
            }
        }
        Ok(())
    }

    /// Mark the in-memory index as having diverged from the on-disk cache.
    pub const fn mark_index_dirty(&mut self) {
        self.index_dirty = true;
    }

    /// Drop the in-memory index without saving.  Used by `ROLLBACK` so
    /// the next `resume_index` reads the freshly-restored
    /// `.forgeql-index` from disk instead of keeping a stale view.
    pub fn drop_index(&mut self) {
        self.backends.default_engine_mut().drop_stored_index();
        self.cached_commit = None;
        self.index_dirty = false;
    }

    /// Mutable access to the columnar storage backend, if installed.
    ///
    /// Returns `None` when the columnar backend is not enabled for this session.
    pub fn columnar_storage_mut(&mut self) -> Option<&mut dyn crate::storage::StorageEngine> {
        self.backends.columnar_engine_mut()
    }
}
