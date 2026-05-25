//! Overlay orchestration, dirty/delta helpers, and commit logic for [`super::ColumnarStorage`].
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use tracing::{debug, info};

use crate::ast::lang::LanguageRegistry;
use crate::storage::LegacyMemoryStorage;

use super::super::build_context::ColumnarBuildContext;
use super::super::delta_file::DeltaFile;
use super::super::dirty_overlay::DirtyOverlay;
use super::super::overlay::Overlay;
use super::super::overlay_builder::OverlayBuilder;
use super::super::overlay_lock::OverlayLock;
use super::super::segment_reader::SegmentReader;
use super::super::shadow_writer::ShadowWriter;
use super::ColumnarStorage;
impl ColumnarStorage {
    /// Open the overlay for `commit_sha`, building it via shadow-write if absent.
    ///
    /// # Steps
    /// 1. Compute overlay path from `ctx`.
    /// 2. If the overlay opens cleanly → fast path: return immediately.
    /// 3. Otherwise acquire [`OverlayLock`], re-check inside the lock, and
    ///    build via [`ShadowWriter`] + [`OverlayBuilder`].
    /// 4. Construct and return a ready-to-query `ColumnarStorage`.
    ///
    /// `legacy` is read-only; only its [`SymbolTable`] is passed to
    /// `ShadowWriter`. Both this method and the caller accept `None` for
    /// `legacy` — if `None` the slow-path build is skipped (non-fatal).
    ///
    /// # Errors
    /// Returns `Err` only for hard failures (lock file I/O, final
    /// `Overlay::open` after a successful build). Shadow-write failures
    /// are treated as non-fatal and logged.
    #[expect(
        clippy::too_many_lines,
        reason = "Three phases: fast overlay open, lock-guarded slow-path build, and final open; collapsing phases would obscure the retry/lock logic"
    )]
    pub fn warm_or_open(
        ctx: &crate::storage::ColumnarBuildContext,
        legacy: Option<&LegacyMemoryStorage>,
        worktree_path: PathBuf,
        commit_sha: &str,
        lang_registry: Arc<LanguageRegistry>,
    ) -> Result<Self> {
        let overlay_path = ctx.overlay_path_for(commit_sha);

        // Fast path: overlay already on disk and readable.
        if overlay_path.exists() {
            if let Ok(overlay) = Overlay::open(&overlay_path) {
                debug!(%commit_sha, "columnar warm_or_open: overlay found, fast-path load");
                let segments = Self::open_segments_from_overlay(ctx, &overlay);
                let mut storage = Self::new(worktree_path, segments, overlay, lang_registry);
                if let Err(e) = storage.load_delta() {
                    tracing::warn!(%commit_sha, "columnar warm_or_open: delta load failed (non-fatal): {e}");
                }
                return Ok(storage);
            }
            // Corrupt / schema mismatch — remove and rebuild below.
            debug!(%commit_sha, "columnar warm_or_open: overlay unreadable, will rebuild");
            let _ = std::fs::remove_file(&overlay_path);
        }

        // Slow path: build under lock.
        match OverlayLock::acquire(&overlay_path) {
            Err(e) => {
                return Err(anyhow!("overlay lock acquire failed for {commit_sha}: {e}"));
            }
            Ok(_lock) => {
                // Re-check: a peer may have built the overlay while we waited.
                if overlay_path.exists() {
                    if let Ok(overlay) = Overlay::open(&overlay_path) {
                        debug!(%commit_sha, "columnar warm_or_open: peer built overlay under lock");
                        let segments = Self::open_segments_from_overlay(ctx, &overlay);
                        let mut storage =
                            Self::new(worktree_path, segments, overlay, Arc::clone(&lang_registry));
                        if let Err(e) = storage.load_delta() {
                            tracing::warn!(%commit_sha, "columnar warm_or_open: delta load failed (non-fatal): {e}");
                        }
                        return Ok(storage);
                    }
                    let _ = std::fs::remove_file(&overlay_path);
                }

                // Build segments + overlay. Prefer the inline fast-path when
                // segments were already written per-file during build_index.
                let segment_map_opt = legacy.and_then(|l| l.prebuilt_segment_map.clone());

                if let Some(segment_map) = segment_map_opt {
                    // Fast-path: segments written inline — skip ShadowWriter.
                    let t_sw = std::time::Instant::now();
                    info!(
                        ms = t_sw.elapsed().as_millis(),
                        %commit_sha,
                        segments = segment_map.len(),
                        "TIMING warm_or_open: inline segments (no shadow-write)"
                    );
                    let builder = OverlayBuilder::new(
                        &ctx.provider_id,
                        ctx.segments_dir.clone(),
                        worktree_path.clone(),
                        segment_map,
                    );
                    if let Err(e) = builder.build_and_persist(&overlay_path) {
                        tracing::warn!(
                            %commit_sha,
                            "columnar warm_or_open: overlay build failed: {e}"
                        );
                    } else {
                        debug!(%commit_sha, "columnar warm_or_open: overlay built (inline path)");
                    }
                } else if let Some(legacy) = legacy
                    && let Some(table) = legacy.table()
                {
                    // Legacy path: shadow-write from the merged SymbolTable.
                    let writer = ShadowWriter::new(
                        table,
                        &ctx.segments_dir,
                        &ctx.provider_id,
                        ctx.hash_fn.as_ref(),
                        HashMap::new(),
                    );
                    let t_sw = std::time::Instant::now();
                    match writer.run() {
                        Ok(result) => {
                            info!(
                                ms = t_sw.elapsed().as_millis(),
                                %commit_sha,
                                segments = result.count,
                                "TIMING warm_or_open: shadow-write"
                            );
                            let builder = OverlayBuilder::new(
                                &ctx.provider_id,
                                ctx.segments_dir.clone(),
                                worktree_path.clone(),
                                result.segment_map,
                            );
                            if let Err(e) = builder.build_and_persist(&overlay_path) {
                                tracing::warn!(
                                    %commit_sha,
                                    "columnar warm_or_open: overlay build failed: {e}"
                                );
                            } else {
                                debug!(%commit_sha, "columnar warm_or_open: overlay built");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                %commit_sha,
                                "columnar warm_or_open: shadow-write failed: {e}"
                            );
                        }
                    }
                }
                // _lock dropped here — releases OS lock.
            }
        }

        // Open whatever we built (or what was there before — best-effort).
        let overlay = Overlay::open(&overlay_path)
            .map_err(|e| anyhow!("overlay open failed for {commit_sha}: {e}"))?;
        let segments = Self::open_segments_from_overlay(ctx, &overlay);
        let mut storage = Self::new(worktree_path, segments, overlay, lang_registry);
        if let Err(e) = storage.load_delta() {
            tracing::warn!(%commit_sha, "columnar warm_or_open: delta load failed (non-fatal): {e}");
        }
        Ok(storage)
    }

    /// Build segments + overlay for `commit_sha` without returning a
    /// `ColumnarStorage`.
    ///
    /// Convenience wrapper around [`warm_or_open`] used by background
    /// warming where the result is discarded immediately.
    ///
    /// [`warm_or_open`]: Self::warm_or_open
    ///
    /// # Errors
    /// Propagates errors from `warm_or_open`.
    pub fn warm(
        ctx: &crate::storage::ColumnarBuildContext,
        legacy: Option<&LegacyMemoryStorage>,
        worktree_path: PathBuf,
        commit_sha: &str,
    ) -> Result<()> {
        // Background warming never calls reindex_files; use an empty registry.
        let registry = Arc::new(LanguageRegistry::new(vec![]));
        let _ = Self::warm_or_open(ctx, legacy, worktree_path, commit_sha, registry)?;
        Ok(())
    }

    /// Open all segment readers referenced by `overlay`.
    ///
    /// Segments that cannot be opened are silently skipped — the overlay
    /// is still usable for queries that target other segments.
    fn open_segments_from_overlay(
        ctx: &crate::storage::ColumnarBuildContext,
        overlay: &Arc<Overlay>,
    ) -> Vec<Arc<SegmentReader>> {
        overlay
            .segments()
            .iter()
            .filter_map(|meta| {
                let dir = ctx.segment_path_for(&meta.hex_content_id);
                SegmentReader::open(&dir).ok().map(Arc::new)
            })
            .collect()
    }
}

impl ColumnarStorage {
    /// Mutable access to the per-session dirty overlay.
    ///
    /// Used by PhaseFT2 `reindex_files` and PhaseFT3 delta-file loading.
    pub const fn dirty_mut(&mut self) -> &mut DirtyOverlay {
        &mut self.dirty
    }

    /// Read-only access to the per-session dirty overlay.
    #[must_use]
    pub const fn dirty(&self) -> &DirtyOverlay {
        &self.dirty
    }

    /// Look up the `hex_content_id` of the persistent overlay segment for a
    /// given worktree-relative path, if one exists.
    pub(super) fn path_to_hex_content_id(&self, rel_path: &Path) -> Option<String> {
        self.overlay
            .segments()
            .iter()
            .find(|m| m.source_path == rel_path)
            .map(|m| m.hex_content_id.clone())
    }

    // ─────────────────────────────────────────────────────────────────────
    // PhaseFT3: delta file helpers
    // ─────────────────────────────────────────────────────────────────────

    /// Serialize the current dirty overlay to `.forgeql-columnar-delta`.
    ///
    /// Delegates to [`DeltaFile::save`].  Called at the end of every
    /// `reindex_files` / `purge_file` and at the start of `BEGIN TRANSACTION`
    /// so the overlay state survives server restarts and `ROLLBACK`.
    pub(super) fn save_delta(&self) -> Result<()> {
        DeltaFile::save(&self.dirty, &self.delta_path)
    }

    /// Load the delta file and restore the dirty overlay.
    ///
    /// No-op when `.forgeql-columnar-delta` does not exist (empty session).
    /// Called from `warm_or_open` (reconnect) and `reload_delta_after_rollback`.
    pub fn load_delta(&mut self) -> Result<()> {
        if self.delta_path.exists() {
            match DeltaFile::load(&self.delta_path, &self.staging_dir) {
                Ok(dirty) => self.dirty = dirty,
                Err(e) => {
                    tracing::warn!(
                        path = %self.delta_path.display(),
                        "columnar delta load failed, resetting dirty overlay: {e}"
                    );
                    self.dirty = DirtyOverlay::new();
                    let valid: &[String] = &[];
                    DeltaFile::gc_orphaned_staging(valid, &self.staging_dir);
                }
            }
        }
        Ok(())
    }

    /// Called by `ROLLBACK` after `git reset --hard` restores the worktree.
    ///
    /// Reads the valid hex IDs from the restored delta file, GCs any orphaned
    /// staging directories, then reloads the dirty overlay from the delta.
    pub fn reload_delta_after_rollback(&mut self) -> Result<()> {
        let valid_hexes = DeltaFile::read_valid_hexes(&self.delta_path);
        DeltaFile::gc_orphaned_staging(&valid_hexes, &self.staging_dir);
        self.dirty = DirtyOverlay::new();
        self.load_delta()
    }

    // ─────────────────────────────────────────────────────────────────────
    // PhaseFT4: commit_dirty — promote staging segments + build new overlay
    // ─────────────────────────────────────────────────────────────────────

    /// Called from `exec_commit` after the git commit succeeds.
    ///
    /// Promotes all staging segments to the bare-repo segment store, builds a
    /// new overlay for `new_commit_oid` by merging the persistent overlay with
    /// the dirty overlay, then swaps the session to the new overlay and clears
    /// all dirty state.
    ///
    /// # Errors
    /// Returns `Err` when segment promotion, overlay build/open, or staging-dir
    /// cleanup fails.  `exec_commit` treats this as non-fatal: the session falls
    /// back to its stale overlay; the next `USE` will rebuild from legacy.
    pub(super) fn commit_dirty_inner(
        &mut self,
        new_commit_oid: &str,
        ctx: &ColumnarBuildContext,
    ) -> Result<()> {
        // 1. Promote staging segments → bare-repo segment store.
        //    Idempotent: skips any hex that is already there.
        for ds in &self.dirty.added {
            let hex = ds.reader.content_id_hex();
            let src = self.staging_dir.join(format!("{hex}.fqsf"));
            let dst = ctx.segment_path_for(&hex);
            promote_segment(&src, &dst)?;
        }

        // 2. Build new overlay = merge(persistent, dirty).
        //    All segments are re-opened fresh from the bare repo after promotion.
        let new_overlay_path = ctx.overlay_path_for(new_commit_oid);
        let builder =
            OverlayBuilder::from_merge(&self.overlay, &self.dirty, ctx, &self.worktree_root);
        builder.build_and_persist(&new_overlay_path)?;

        // 3. Swap to the new overlay (Overlay::open returns Arc<Overlay>).
        let new_overlay = Overlay::open(&new_overlay_path)
            .with_context(|| format!("open new overlay at {}", new_overlay_path.display()))?;
        let new_segments = Self::open_segments_from_overlay(ctx, &new_overlay);
        self.overlay = new_overlay;
        self.segments = new_segments;
        self.stats.rows = self.overlay.row_count() as usize;

        // 4. Clear dirty state and staging directory.
        self.dirty = DirtyOverlay::new();
        clear_staging_dir(&self.staging_dir)?;

        // 5. Remove the delta file — no pending changes after commit.
        let _ = std::fs::remove_file(&self.delta_path);

        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PhaseFT4: private filesystem helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Promote a staging `.fqsf` segment file to the bare-repo segment store.
///
/// Prefers `rename(2)` for an atomic, zero-copy move on the same filesystem.
/// Falls back to `fs::copy` when the rename fails (cross-device or lost race).
/// The `dst.exists()` guard makes promotion idempotent.
fn promote_segment(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Ok(()); // already promoted — idempotent
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create segment parent dir {}", parent.display()))?;
    }
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    // Rename failed: cross-device or concurrent promotion won the race.
    if dst.exists() {
        return Ok(()); // lost race — peer already promoted
    }
    // True cross-device: copy the single .fqsf file.
    std::fs::copy(src, dst)
        .with_context(|| format!("copy segment {} → {}", src.display(), dst.display()))
        .map(|_| ())
}

/// Delete all entries inside the staging directory without removing the
/// directory itself (avoids a `create_dir_all` on the next `reindex_files`).
fn clear_staging_dir(staging_dir: &Path) -> Result<()> {
    if !staging_dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(staging_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("remove staging subdir {}", path.display()))?;
        } else {
            std::fs::remove_file(&path)
                .with_context(|| format!("remove staging file {}", path.display()))?;
        }
    }
    Ok(())
}
