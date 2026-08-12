//! Overlay orchestration, dirty/delta helpers, and commit logic for [`super::ColumnarStorage`].
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use tracing::{debug, info};

use super::super::build_context::BuildInput;
use crate::ast::lang::LanguageRegistry;

use super::super::build_context::ColumnarBuildContext;
use super::super::delta_file::DeltaFile;
use super::super::dirty_overlay::DirtyOverlay;
use super::super::open_cache::{self, SharedOpen};
use super::super::overlay::Overlay;
use super::super::overlay_builder::OverlayBuilder;
use super::super::overlay_lock::OverlayLock;
use super::super::segment_reader::SegmentReader;
use super::super::shadow_writer::ShadowWriter;
use super::ColumnarStorage;

/// An overlay that opened cleanly while naming a segment the disk no longer
/// holds, where no rebuild reachable from here writes that segment back.
///
/// The two ways this can end are told apart by what the caller brought with it,
/// not by the fault. Shadow-writing from a merged symbol table can write a
/// segment that is not there, so where one is available the rebuild is allowed
/// to run and the index is usually repaired. It is only *usually*: the rebuild
/// skips a segment whose magic bytes are intact even when opening it fails, and
/// a failed flush leaves the path in the map with nothing behind it. So the
/// caller checks afterwards and raises this if the segment is still not there —
/// a rebuild running is not the same as a rebuild working, and the difference
/// between them would be an index one file smaller that never says so.
///
/// The other end is a rebuild that could only drop the segment — assembling
/// from the segments already on disk, which is what runs whenever the caller
/// carries an inline segment map — or no rebuild at all. Those refuse without
/// running anything, and the assembly itself refuses too when a segment it
/// names will not open, so a route that reaches it without passing here (a
/// COMMIT merging the base overlay with dirty segments) fails with the same
/// shape instead of writing a smaller overlay.
///
/// A readable overlay is not deleted on the strength of a missing segment in
/// either direction: removing one is destructive and cannot be undone, and the
/// repairing rebuild replaces it atomically without needing it gone.
#[derive(Debug)]
struct IncompleteIndex {
    /// Worktree-relative path of the file whose segment would not open.
    source_path: PathBuf,
    /// Where the open looked for that segment.
    segment_path: PathBuf,
    /// The overlay naming it, which is the other end of both repairs.
    overlay_path: PathBuf,
    /// Why it would not open.
    cause: String,
}

impl std::fmt::Display for IncompleteIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "index incomplete: {} names a segment for {} that is missing or \
             unreadable at {} ({}) — an index missing a segment cannot answer \
             completely, and no rebuild available here would write it back. Restore \
             that file, or index this source from scratch",
            self.overlay_path.display(),
            self.source_path.display(),
            self.segment_path.display(),
            self.cause
        )
    }
}

impl std::error::Error for IncompleteIndex {}

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
    /// Returns `Err` for hard failures: lock file I/O, the final
    /// `Overlay::open` after a successful build, and a segment the overlay
    /// lists that will not open and that no rebuild reachable from `input`
    /// writes back — including one a rebuild was allowed to try for and did not
    /// produce. The overlay is never deleted in that case; see
    /// [`Self::rebuild_or_refuse`]. Shadow-write failures are otherwise treated
    /// as non-fatal and logged.
    pub fn warm_or_open(
        ctx: &crate::storage::ColumnarBuildContext,
        input: BuildInput<'_>,
        worktree_path: PathBuf,
        commit_sha: &str,
        lang_registry: Arc<LanguageRegistry>,
    ) -> Result<Self> {
        let overlay_path = ctx.overlay_path_for(commit_sha);

        // A refusal deferred while the rebuild below is given a chance to write
        // the missing segment back. It is raised after that rebuild unless the
        // segment is really there: authorising a rebuild is not the same as the
        // rebuild succeeding, and the difference between the two is a smaller
        // index that never says it is smaller.
        let mut deferred_refusal: Option<IncompleteIndex> = None;

        // Fast path: overlay already on disk and readable.
        if overlay_path.exists() {
            match Self::shared_open(ctx, &overlay_path) {
                Ok(shared) => {
                    debug!(%commit_sha, "columnar warm_or_open: overlay found, fast-path load");
                    return Ok(Self::finish_open(
                        worktree_path,
                        shared,
                        lang_registry,
                        commit_sha,
                    ));
                }
                Err(e) => {
                    deferred_refusal = Self::rebuild_or_refuse(
                        e,
                        &overlay_path,
                        commit_sha,
                        Self::rebuild_regenerates_segments(&input),
                    )?;
                }
            }
        }

        // Slow path: build under lock.
        match OverlayLock::acquire(&overlay_path) {
            Err(e) => {
                return Err(anyhow!("overlay lock acquire failed for {commit_sha}: {e}"));
            }
            Ok(_lock) => {
                // Re-check: a peer may have built the overlay while we waited.
                if overlay_path.exists() {
                    match Self::shared_open(ctx, &overlay_path) {
                        Ok(shared) => {
                            debug!(%commit_sha, "columnar warm_or_open: peer built overlay under lock");
                            return Ok(Self::finish_open(
                                worktree_path,
                                shared,
                                Arc::clone(&lang_registry),
                                commit_sha,
                            ));
                        }
                        Err(e) => {
                            deferred_refusal = Self::rebuild_or_refuse(
                                e,
                                &overlay_path,
                                commit_sha,
                                Self::rebuild_regenerates_segments(&input),
                            )?;
                        }
                    }
                }

                Self::build_overlay(ctx, input, &worktree_path, &overlay_path, commit_sha);
                // _lock dropped here — releases OS lock.
            }
        }

        // Open whatever we built (or what was there before — best-effort).
        let shared = Self::shared_open(ctx, &overlay_path)
            .map_err(|e| anyhow!("overlay open failed for {commit_sha}: {e}"))?;

        // The rebuild was allowed to run because it *might* write the missing
        // segment back. Check that it did. Every way it can fail to — a segment
        // whose magic bytes survive so shadow-write calls it already valid, a
        // flush that failed, a source file that could not be read — now ends in
        // the assembly refusing and the old overlay staying put, so the open
        // above fails typed rather than succeeding over a smaller index. This
        // check stays as the second, independent proof that the segment really
        // came back: a rebuild running is not a rebuild working.
        if let Some(pending) = deferred_refusal
            && !shared
                .overlay
                .segments()
                .iter()
                .any(|meta| meta.source_path == pending.source_path)
        {
            return Err(anyhow::Error::new(IncompleteIndex {
                cause: format!("{}; a rebuild ran and did not write it back", pending.cause),
                ..pending
            }));
        }
        Ok(Self::finish_open(
            worktree_path,
            shared,
            lang_registry,
            commit_sha,
        ))
    }

    /// Opens the committed half of the index at `overlay_path`, reusing the
    /// copy another session already holds when there is one.
    ///
    /// The overlay and its segment readers are immutable once opened and every
    /// file behind them is content-addressed, so one key names one content —
    /// see [`open_cache`] for why that makes sharing sound and what it does
    /// *not* cover.
    pub fn shared_open(
        ctx: &crate::storage::ColumnarBuildContext,
        overlay_path: &Path,
    ) -> Result<Arc<SharedOpen>> {
        open_cache::shared_open(&Self::open_key(ctx, overlay_path), || {
            let overlay = Overlay::open(overlay_path)?;
            let segments = Self::open_segments_from_overlay(ctx, &overlay, overlay_path)?;
            // `open_segments_from_overlay` fails rather than dropping a reader
            // it cannot open, so `segments` is positionally aligned with the
            // overlay's own segment table by the time it reaches here. The
            // completeness guard stays: it is what stops a misaligned vector
            // becoming permanent for a commit, and it should go on holding that
            // line if some later producer of a `SharedOpen` is less strict than
            // this one.
            let complete = segments.len() == overlay.segments().len();
            Ok(open_cache::Opened {
                value: SharedOpen { overlay, segments },
                shareable: complete,
            })
        })
    }

    /// The cache key naming one commit's committed half.
    ///
    /// Both halves are required. A build context keeps its overlay directory
    /// and its segment directory as independent settings, so the overlay path
    /// alone does not say where the readers were opened from, and keying on it
    /// alone would serve one context's readers to another's session.
    #[must_use]
    pub fn open_key(
        ctx: &crate::storage::ColumnarBuildContext,
        overlay_path: &Path,
    ) -> open_cache::OpenKey {
        (overlay_path.to_path_buf(), ctx.versioned_segments_root())
    }

    /// Wrap already-open shared state in a session's own `ColumnarStorage`:
    /// take a handle on the shared overlay and segment readers, then
    /// best-effort load this session's dirty delta on top.
    /// Shared by all three return paths of `warm_or_open`.
    fn finish_open(
        worktree_path: PathBuf,
        shared: Arc<SharedOpen>,
        lang_registry: Arc<LanguageRegistry>,
        commit_sha: &str,
    ) -> Self {
        let mut storage = Self::from_shared(worktree_path, shared, lang_registry);
        if let Err(e) = storage.load_delta() {
            tracing::warn!(%commit_sha, "columnar warm_or_open: delta load failed (non-fatal): {e}");
        }
        storage
    }

    /// Whether rebuilding from `input` would regenerate a segment that is
    /// missing, rather than drop it.
    ///
    /// This mirrors the branch order in [`Self::build_overlay`] directly below,
    /// and has to keep mirroring it. The inline prebuilt map wins when both are
    /// present, and that path builds the overlay from the segments already on
    /// disk, so one it cannot read is dropped and the index quietly shrinks.
    /// Shadow-writing from a merged symbol table is the only path that writes a
    /// segment which is not already valid, and that is what makes it a repair
    /// rather than a loss.
    ///
    /// Two tests hold the two answers apart, so drift here surfaces as a
    /// failure instead of as a wrong answer.
    const fn rebuild_regenerates_segments(input: &BuildInput<'_>) -> bool {
        input.prebuilt_segment_map.is_none() && input.table.is_some()
    }
    /// Build segments + overlay under the held lock. Prefers the inline fast-path
    /// (segments already written per-file during `build_index`) and falls back to
    /// shadow-writing the merged `SymbolTable`. Best-effort: failures are logged.
    fn build_overlay(
        ctx: &ColumnarBuildContext,
        input: BuildInput<'_>,
        worktree_path: &Path,
        overlay_path: &Path,
        commit_sha: &str,
    ) {
        // Prefer the inline fast-path when segments were already written per-file
        // during build_index.
        if let Some(segment_map) = input.prebuilt_segment_map {
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
                worktree_path.to_path_buf(),
                segment_map,
            );
            if let Err(e) = builder.build_and_persist(overlay_path) {
                tracing::warn!(%commit_sha, "columnar warm_or_open: overlay build failed: {e}");
            } else {
                debug!(%commit_sha, "columnar warm_or_open: overlay built (inline path)");
            }
        } else if let Some(table) = input.table {
            // Legacy path: shadow-write from the merged SymbolTable.
            let writer = ShadowWriter::new(
                table,
                &ctx.segments_dir,
                &ctx.provider_id,
                ctx.hash_fn.as_ref(),
                HashMap::new(),
                worktree_path,
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
                        worktree_path.to_path_buf(),
                        result.segment_map,
                    );
                    if let Err(e) = builder.build_and_persist(overlay_path) {
                        tracing::warn!(%commit_sha, "columnar warm_or_open: overlay build failed: {e}");
                    } else {
                        debug!(%commit_sha, "columnar warm_or_open: overlay built");
                    }
                }
                Err(e) => {
                    tracing::warn!(%commit_sha, "columnar warm_or_open: shadow-write failed: {e}");
                }
            }
        }
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
        input: BuildInput<'_>,
        worktree_path: PathBuf,
        commit_sha: &str,
    ) -> Result<()> {
        // Background warming never calls reindex_files; use an empty registry.
        let registry = Arc::new(LanguageRegistry::new(vec![]));
        let _ = Self::warm_or_open(ctx, input, worktree_path, commit_sha, registry)?;
        Ok(())
    }

    /// Decide what a failed open of an existing overlay means, for both places
    /// that try one.
    ///
    /// `Ok(None)` means the overlay is unusable and has been removed, so the
    /// caller should rebuild from scratch. `Ok(Some(..))` means the overlay is
    /// fine, a segment it names is gone, and `can_regenerate` says the rebuild
    /// the caller is about to run may write that segment again — the caller
    /// must then check that it actually did, and raise the returned refusal if
    /// it did not. `Err` is the remaining case: a readable overlay over a
    /// missing segment with no rebuild here that would write it back, which is
    /// refused outright. See [`IncompleteIndex`].
    ///
    /// A readable overlay is never removed here. Removing one is destructive and
    /// cannot be undone, and the repairing rebuild does not need it gone: it
    /// replaces the file atomically.
    ///
    /// Both call sites go through here so the two cannot drift apart, and so one
    /// test covers both.
    ///
    /// # Errors
    /// Returns the original error for an [`IncompleteIndex`] no rebuild
    /// available here would write back.
    fn rebuild_or_refuse(
        e: anyhow::Error,
        overlay_path: &Path,
        commit_sha: &str,
        can_regenerate: bool,
    ) -> Result<Option<IncompleteIndex>> {
        let incomplete = match e.downcast::<IncompleteIndex>() {
            Ok(incomplete) => incomplete,
            Err(other) => {
                // Corrupt / schema mismatch — remove and rebuild.
                debug!(%commit_sha, "columnar warm_or_open: overlay unreadable, will rebuild");
                drop(other);
                let _ = std::fs::remove_file(overlay_path);
                return Ok(None);
            }
        };
        if !can_regenerate {
            return Err(anyhow::Error::new(incomplete));
        }
        // A merged symbol table is available, so the rebuild below may write the
        // segment that is gone. It is handed back rather than dropped because
        // "may" is the most that can be said here: shadow-write skips a segment
        // whose magic bytes are intact even when opening it fails, and a flush
        // that fails leaves the path in the map with nothing behind it. The
        // assembly step that follows now refuses a segment it cannot open, and
        // the caller still checks afterwards that the segment is really there —
        // two independent reasons a rebuild that ran is not a rebuild that
        // worked.
        debug!(%commit_sha, "columnar warm_or_open: segment missing, rebuilding to regenerate it");
        Ok(Some(incomplete))
    }

    /// Open one reader per segment the overlay lists, in the overlay's own order.
    ///
    /// # Errors
    /// Returns `Err` if any listed segment will not open. The returned vector is
    /// indexed positionally by the overlay's own `segment_idx`, so a dropped
    /// reader is not a smaller correct answer: it shifts every later index, and
    /// rows are then served against a different file's reader — right name and
    /// line, wrong path and content, and a node handle addressing a file the
    /// query never named. A partial index cannot answer authoritatively, so it
    /// is reported rather than read on from; what the caller does with that —
    /// regenerate the segment or refuse — is decided in
    /// [`Self::rebuild_or_refuse`].
    fn open_segments_from_overlay(
        ctx: &crate::storage::ColumnarBuildContext,
        overlay: &Arc<Overlay>,
        overlay_path: &Path,
    ) -> Result<Vec<Arc<SegmentReader>>> {
        overlay
            .segments()
            .iter()
            .map(|meta| {
                let path = ctx.segment_path_for(&meta.source_path, &meta.hex_content_id);
                match SegmentReader::open(&path) {
                    Ok(reader) => Ok(Arc::new(reader)),
                    Err(e) => Err(anyhow::Error::new(IncompleteIndex {
                        source_path: meta.source_path.clone(),
                        segment_path: path,
                        overlay_path: overlay_path.to_path_buf(),
                        cause: e.to_string(),
                    })),
                }
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
        self.overlay()
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
    ///
    /// Staged entries the loader could not restore (previous-generation delta,
    /// or a missing staging segment) are queued in `pending_reindex`; the
    /// session layer MUST drain them via [`Self::take_pending_reindex_paths`]
    /// and re-index those files, or their pre-edit base rows stay visible.
    pub fn load_delta(&mut self) -> Result<()> {
        if self.delta_path.exists() {
            match DeltaFile::load(&self.delta_path, &self.staging_dir) {
                Ok((dirty, needs_reindex)) => {
                    self.dirty = dirty;
                    self.pending_reindex = needs_reindex;
                }
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
    /// Reads the valid staged segment names from the restored delta file, GCs
    /// any orphaned staging files, then reloads the dirty overlay from the delta.
    pub fn reload_delta_after_rollback(&mut self) -> Result<()> {
        let valid_names = DeltaFile::read_valid_segment_names(&self.delta_path);
        DeltaFile::gc_orphaned_staging(&valid_names, &self.staging_dir);
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
        //    Idempotent: skips any (path, content) key already present.
        for ds in &self.dirty.added {
            let hex = ds.reader.content_id_hex();
            let src = crate::storage::columnar::delta_file::staged_segment_path(
                &self.staging_dir,
                &ds.source_path,
                &hex,
            );
            let dst = ctx.segment_path_for(&ds.source_path, &hex);
            promote_segment(&src, &dst)?;
        }

        // 2. Build new overlay = merge(persistent, dirty).
        //    All segments are re-opened fresh from the bare repo after promotion.
        let new_overlay_path = ctx.overlay_path_for(new_commit_oid);
        let builder =
            OverlayBuilder::from_merge(self.overlay(), &self.dirty, ctx, &self.worktree_root);
        builder.build_and_persist(&new_overlay_path)?;

        // 3. Swap to the new overlay. Routed through the shared cache rather
        //    than opening directly: this commit is the one every later session
        //    will attach to, so an uncached decode here is the one most likely
        //    to be paid for twice.
        let opened = Self::shared_open(ctx, &new_overlay_path)
            .with_context(|| format!("open new overlay at {}", new_overlay_path.display()))?;
        // The substring dictionary was built from the overlay being replaced,
        // and the committed tokens have just left `dirty` too — without this a
        // partial substring query would silently under-report them.
        self.substring_index = std::sync::OnceLock::new();
        self.stats.rows = opened.overlay.row_count() as usize;
        self.shared = opened;

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
