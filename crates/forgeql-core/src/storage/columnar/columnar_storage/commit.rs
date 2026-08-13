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

        // Chain path: no overlay for this commit, but a manifest names the
        // master overlay it grew from and the per-file segments its changes
        // promoted. Seeding the session dirty overlay from those serves the
        // same rows the full build would, through the read path every
        // editing session already exercises — at the cost of the dirty-side
        // fast-path declines, never of correctness. Any failure here falls
        // through to the full build below: a bad manifest can cost time,
        // never rows.
        let manifest_path = ctx.chain_manifest_path_for(commit_sha);
        if manifest_path.exists() {
            match Self::open_via_chain(
                ctx,
                &manifest_path,
                worktree_path.clone(),
                commit_sha,
                Arc::clone(&lang_registry),
            ) {
                Ok(storage) => return Ok(storage),
                Err(e) => {
                    tracing::warn!(
                        %commit_sha,
                        "columnar warm_or_open: chain attach failed — falling back \
                         to a full build: {e}"
                    );
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
            // Timed separately from the overlay build even though a cold index
            // runs both back to back: this opens every segment a second time,
            // the build having opened and dropped its own set, and without a
            // line of its own that second open is invisible — it lands after
            // the build's "total" and before the session is announced, so it
            // reads as time nothing accounts for.
            let t_open = std::time::Instant::now();
            let overlay = Overlay::open(overlay_path)?;
            let t_overlay = t_open.elapsed();
            let segments = Self::open_segments_from_overlay(ctx, &overlay, overlay_path)?;
            info!(
                ms = t_open.elapsed().as_millis(),
                overlay_ms = t_overlay.as_millis(),
                n = segments.len(),
                mem = %crate::mem::snapshot(),
                "TIMING shared_open: open overlay + segments",
            );
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
        commit_sha.clone_into(&mut storage.master_commit);
        if let Err(e) = storage.load_delta() {
            tracing::warn!(%commit_sha, "columnar warm_or_open: delta load failed (non-fatal): {e}");
        }
        storage
    }

    /// Opens a chained commit: the master overlay plus a dirty overlay
    /// seeded from the commit's chain manifest.
    ///
    /// Every failure is an error the caller turns into a full-build
    /// fallback: an unreadable manifest, another index generation, a master
    /// overlay not on disk, a named segment that does not open, or an entry
    /// that shadows a master path without recording the replacement.
    /// Nothing on this path may produce a smaller index silently — refusal
    /// and fallback are the only exits besides a complete one.
    fn open_via_chain(
        ctx: &ColumnarBuildContext,
        manifest_path: &Path,
        worktree_path: PathBuf,
        commit_sha: &str,
        lang_registry: Arc<LanguageRegistry>,
    ) -> Result<Self> {
        use super::super::chain_manifest::ChainManifest;

        let manifest = ChainManifest::load(manifest_path)?;
        if manifest.enrich_ver != super::super::ENRICH_VER {
            anyhow::bail!(
                "chain manifest for {commit_sha} is from index generation {} (running {})",
                manifest.enrich_ver,
                super::super::ENRICH_VER
            );
        }
        let master_path = ctx.overlay_path_for(&manifest.master_commit);
        if !master_path.exists() {
            anyhow::bail!(
                "chain manifest for {commit_sha} names master {} whose overlay is not on disk",
                manifest.master_commit
            );
        }
        let shared = Self::shared_open(ctx, &master_path)
            .with_context(|| format!("open master overlay for chained {commit_sha}"))?;
        let mut storage = Self::finish_open(worktree_path, shared, lang_registry, commit_sha);
        storage.master_commit.clone_from(&manifest.master_commit);
        // A restored session delta already holds the seeded chain state plus
        // any edits made on top of it; seeding again would duplicate rows.
        if storage.dirty.is_empty() && storage.pending_reindex.is_empty() {
            storage.seed_dirty_from_chain(&manifest, ctx)?;
        }
        info!(
            %commit_sha,
            master = %manifest.master_commit,
            entries = manifest.entries.len(),
            shadowed = manifest.removed_paths.len(),
            "columnar warm_or_open: chain attach — master overlay + seeded changes"
        );
        Ok(storage)
    }

    /// Seeds the session dirty overlay from a chain manifest: one dirty
    /// segment per entry, opened from the content-addressed store and
    /// hard-linked into this session's staging directory so the delta file
    /// restores it after a restart exactly like a session-built segment.
    ///
    /// Ownership is verified while seeding: an entry whose path the master
    /// also holds must record the replacement, or one path would answer
    /// from two layers at once. That inconsistency is a refusal, never a
    /// repair.
    fn seed_dirty_from_chain(
        &mut self,
        manifest: &super::super::chain_manifest::ChainManifest,
        ctx: &ColumnarBuildContext,
    ) -> Result<()> {
        use super::super::delta_file::staged_segment_path;
        use super::super::dirty_overlay::DirtySegment;

        let master_paths: std::collections::HashSet<&Path> = self
            .shared
            .overlay
            .segments()
            .iter()
            .map(|m| m.source_path.as_path())
            .collect();
        for entry in &manifest.entries {
            if master_paths.contains(entry.source_path.as_path()) && entry.replaces_hex.is_empty() {
                anyhow::bail!(
                    "chain entry {} shadows a master segment without recording the \
                     replacement — one path would answer from two layers",
                    entry.source_path.display()
                );
            }
            let store = ctx.segment_path_for(&entry.source_path, &entry.hex_content_id);
            let staged =
                staged_segment_path(&self.staging_dir, &entry.source_path, &entry.hex_content_id);
            promote_segment(&store, &staged).with_context(|| {
                format!("staging chain segment for {}", entry.source_path.display())
            })?;
            let reader = SegmentReader::open(&staged).with_context(|| {
                format!(
                    "opening chain segment {} for {}",
                    entry.hex_content_id,
                    entry.source_path.display()
                )
            })?;
            self.dirty.added.push(DirtySegment {
                reader: Arc::new(reader),
                source_path: entry.source_path.clone(),
                replaces_hex: entry.replaces_hex.clone(),
            });
        }
        self.dirty
            .removed_paths
            .extend(manifest.removed_paths.iter().cloned());
        self.dirty
            .added_paths
            .extend(manifest.added_paths.iter().cloned());
        self.save_delta()?;
        Ok(())
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
                mem = %crate::mem::snapshot(), "TIMING warm_or_open: inline segments (no shadow-write)"
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
                        mem = %crate::mem::snapshot(), "TIMING warm_or_open: shadow-write"
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

    /// Refuse when the base overlay names a segment the store no longer holds.
    ///
    /// The rows of a committed file live in its segment; the overlay is an
    /// index over them. So a segment reclaimed from under a live session — by
    /// a `VACUUM`, or by a store rebuilt at a different version — means the
    /// index that any later attach builds would come out self-consistent and
    /// silently smaller, dropping that file from every answer on this commit.
    ///
    /// The build refuses on exactly this, and that refusal is the one that
    /// protects the answer. This check exists because a deferred build happens
    /// after the session that could act on the news has gone: it reports the
    /// same fault at the commit, while the agent that made it is still here.
    /// Nothing in a commit reads these segments, so this is a guard and not a
    /// dependency — a stat each, no open, and no bytes read.
    fn refuse_if_base_segments_vanished(&self, ctx: &ColumnarBuildContext) -> Result<()> {
        const LISTED_CAP: usize = 10;
        let missing: Vec<&Path> = self
            .overlay()
            .segments()
            .iter()
            .filter(|meta| {
                // A path the session has shadowed is one whose replacement is
                // staged here: its base segment is on its way out of the answer
                // anyway, so its absence costs nothing.
                !self.dirty.removed_paths.contains(&meta.source_path)
                    && !ctx
                        .segment_path_for(&meta.source_path, &meta.hex_content_id)
                        .exists()
            })
            .map(|meta| meta.source_path.as_path())
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let listed = missing
            .iter()
            .take(LISTED_CAP)
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("; ");
        let more = missing.len().saturating_sub(LISTED_CAP);
        let tail = if more > 0 {
            format!("; … and {more} more")
        } else {
            String::new()
        };
        anyhow::bail!(
            "overlay build incomplete: {} of {} segments this commit's index is \
             built from are missing from {} — {listed}{tail}. Committing would \
             leave an index that silently drops those files from every answer \
             on this commit; restore the segment files or re-index this source",
            missing.len(),
            self.overlay().segments().len(),
            ctx.versioned_segments_root().display(),
        );
    }

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
        &self,
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

        // 2. Do NOT build an overlay for this commit. It is a cache over the
        //    segments promoted above, and the segments are what make the
        //    commit's rows durable — so leaving it unbuilt loses nothing but
        //    the fast path, and building it costs a full merge of every name
        //    and row in the corpus. Measured on the Linux kernel, committing
        //    one edited comment rebuilt an index over 6,877,345 names and
        //    29,864,281 rows, none of which changed: 296 seconds, of which
        //    92% was the name index and the enrichment bitmaps. A worktree
        //    that commits ten times paid that ten times and threw nine of the
        //    results away, because only the last commit is ever attached to.
        //
        //    Whoever attaches to this commit no longer builds it either: the
        //    chain manifest written in step 5 below names the master overlay
        //    and these promoted segments, so a later `USE` of this commit
        //    seeds a dirty overlay from the manifest and answers at once.
        //    The full build survives only as the fallback for a missing or
        //    unusable manifest — and as compaction, once a chain grows past
        //    what the dirty-side read path serves well.
        //
        // 3. The session keeps answering from its base overlay plus the dirty
        //    one, which is what it was doing a moment ago and is still the
        //    same set of rows: a commit moves changes into git, it does not
        //    change which rows the session should return. So `dirty` is kept
        //    rather than cleared, the staging segments it reads stay where
        //    they are, and the delta file that restores it after a restart is
        //    left on disk and rewritten to match.
        //
        //    The rows are correct either way; what grows is the work of
        //    reading them, since dirty rows sit outside the base overlay's
        //    name index and posting lists. That is the trade — a slower read
        //    path for as long as the worktree lives, against not rebuilding
        //    the corpus once per commit.
        // 4. Check the base segments are still on disk, even though nothing
        //    here reads them.
        //
        //    Deferring the build moves the moment a vanished base segment is
        //    noticed from the commit to whoever builds the overlay next, and
        //    that is the wrong end: by then the session that could still act
        //    on it has handed over. The build's own refusal stays where it is
        //    and is what actually protects the answer — this only brings the
        //    news forward. It is a stat per segment and no open, which on a
        //    corpus of 80,426 segments is a fraction of a second against the
        //    296 the build used to cost.
        self.refuse_if_base_segments_vanished(ctx)?;

        self.save_delta()?;

        // 5. Write the chain manifest for the new commit: master overlay +
        //    this session's cumulative changes. The next attacher opens the
        //    master and seeds these instead of paying the full merge. The
        //    dirty overlay is cumulative against the master, so the manifest
        //    always names one master and one change set — never a chain of
        //    chains. Best-effort like the deferred build it replaces: a
        //    failed write costs the attacher time (full build), never rows.
        if self.master_commit.is_empty() {
            tracing::warn!(
                commit = %new_commit_oid,
                "commit: no master commit recorded — chain manifest not written"
            );
        } else {
            let manifest = super::super::chain_manifest::ChainManifest::from_dirty(
                &self.master_commit,
                &self.dirty,
            );
            let manifest_path = ctx.chain_manifest_path_for(new_commit_oid);
            match manifest.save(&manifest_path) {
                Ok(()) => info!(
                    commit = %new_commit_oid,
                    staged = self.dirty.added.len(),
                    shadowed = self.dirty.removed_paths.len(),
                    "commit: chain manifest written; overlay build deferred to compaction"
                ),
                Err(e) => tracing::warn!(
                    commit = %new_commit_oid,
                    "commit: chain manifest write failed (next attach falls back \
                     to a full build): {e}"
                ),
            }
        }
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
    // Link rather than move, so the staging copy outlives the promotion.
    //
    // A commit no longer rebuilds the overlay, so the session goes on
    // answering from its base overlay plus the dirty one — and the dirty
    // one is restored after a restart by reading the delta file against
    // the staging directory. Moving the file out of staging would leave
    // that reload with nothing to open, and the rows of every committed
    // file would silently revert to their pre-edit versions on the next
    // reconnect. A hard link costs no space: both names address one inode.
    if std::fs::hard_link(src, dst).is_ok() {
        return Ok(());
    }
    if dst.exists() {
        return Ok(()); // lost race — peer already promoted
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
