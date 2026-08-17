//! Attaching to an upstream commit through a chain it never wrote.
//!
//! A commit made by a ForgeQL session leaves a chain manifest behind, and
//! `warm_or_open` serves the next attach from it. A commit that arrived by
//! `REFRESH SOURCE` — the normal way a shared corpus moves — leaves nothing,
//! so every attach to it merged the whole corpus again to absorb the few
//! files an upstream push usually changes. This is where that attach
//! instead derives the manifest it was never given: from the nearest
//! ancestor overlay on disk and the segment map the attach's own parse
//! just produced. See [`chain_derive`] for how the two are compared.
//!
//! [`chain_derive`]: super::super::chain_derive

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use tracing::{debug, info};

use super::super::build_context::{BuildInput, ColumnarBuildContext};
use super::ColumnarStorage;
use crate::ast::lang::LanguageRegistry;

/// How far back the ancestor search walks before giving up. A base further
/// away than this carries a change set the compaction threshold would
/// refuse to seed on any corpus this has been measured on, and the walk is
/// the one cost here that grows with history rather than with the change.
const ANCESTOR_MAX_WALK: usize = 50_000;

impl ColumnarStorage {
    /// Serve `commit_sha` as a chain over the nearest ancestor that has an
    /// overlay, when its change set is small enough to seed. `Ok(None)`
    /// means the full build should run: no segment map to diff, no
    /// ancestor with an overlay within reach, or a change set past the
    /// compaction threshold — where seeding would cost more than the merge
    /// it replaces and every query afterwards would pay the dirty-side
    /// declines for nothing.
    ///
    /// On success the derived manifest is on disk beside the overlay the
    /// commit does not have, so the next attacher opens it directly and a
    /// COMMIT from this session inherits its master. The master overlay is
    /// held open across the whole derivation and attach so the shared-open
    /// cache hands the chain path the decode this already paid for.
    ///
    /// # Errors
    /// Every failure that is not one of the three `Ok(None)` cases: the
    /// repository or the master overlay could not be opened, the change set
    /// could not be derived, or the chain refused to seed. The caller logs
    /// it and falls back to the full build — a derived manifest that fails
    /// is removed so no later attach retries it.
    pub(super) fn attach_via_derived_chain(
        ctx: &ColumnarBuildContext,
        input: &BuildInput<'_>,
        worktree_path: PathBuf,
        commit_sha: &str,
        lang_registry: Arc<LanguageRegistry>,
    ) -> Result<Option<Self>> {
        let Some(segment_map) = input.prebuilt_segment_map.as_ref() else {
            return Ok(None);
        };
        let candidates = ctx.overlay_commits();
        if candidates.is_empty() {
            return Ok(None);
        }
        let t_all = std::time::Instant::now();
        let Some(base) = crate::git::ancestry::nearest_ancestor(
            &worktree_path,
            commit_sha,
            &candidates,
            ANCESTOR_MAX_WALK,
        )?
        else {
            debug!(
                %commit_sha,
                overlays = candidates.len(),
                "columnar warm_or_open: no ancestor with an overlay — full build"
            );
            return Ok(None);
        };
        let t_ancestor = t_all.elapsed();

        let master_path = ctx.overlay_path_for(&base.commit);
        let master = Self::shared_open(ctx, &master_path)
            .with_context(|| format!("open ancestor overlay {} for {commit_sha}", base.commit))?;
        let t_derive = std::time::Instant::now();
        let manifest = super::super::chain_derive::derive_upstream_manifest(
            &base.commit,
            &master.overlay,
            segment_map,
            &worktree_path,
        )?;
        let effective_paths = manifest
            .entries
            .iter()
            .map(|e| e.source_path.as_path())
            .chain(manifest.removed_paths.iter().map(PathBuf::as_path))
            .collect::<HashSet<_>>()
            .len();
        info!(
            ms = t_derive.elapsed().as_millis(),
            ancestor_ms = t_ancestor.as_millis(),
            %commit_sha,
            master = %base.commit,
            distance = base.distance,
            entries = manifest.entries.len(),
            removed = manifest.removed_paths.len(),
            added_paths = manifest.added_paths.len(),
            effective_paths,
            mem = %crate::mem::snapshot(),
            "TIMING warm_or_open: derived chain manifest"
        );
        if effective_paths >= Self::chain_compact_threshold() {
            info!(
                %commit_sha,
                effective_paths,
                threshold = Self::chain_compact_threshold(),
                "columnar warm_or_open: upstream change set past the chain threshold — full build"
            );
            return Ok(None);
        }

        let manifest_path = ctx.chain_manifest_path_for(commit_sha);
        manifest.save(&manifest_path)?;
        let attached = Self::open_via_chain(
            ctx,
            &manifest_path,
            worktree_path,
            commit_sha,
            lang_registry,
            true,
        );
        drop(master);
        match attached {
            Ok(storage) => {
                info!(
                    ms = t_all.elapsed().as_millis(),
                    %commit_sha,
                    "TIMING warm_or_open: derived chain attach"
                );
                Ok(Some(storage))
            }
            Err(e) => {
                let _ = std::fs::remove_file(&manifest_path);
                Err(e)
            }
        }
    }
}

/// The manifests that can serve a commit without an overlay of its own,
/// tried in order: one a COMMIT wrote, then one derived from an ancestor.
impl ColumnarStorage {
    /// The whole chain path of `warm_or_open`: a written manifest first,
    /// then a derived one when the commit still has no overlay. `None` means
    /// neither served it and the full build should run; every failure on the
    /// way is logged and treated the same, because a chain can cost time and
    /// never rows.
    pub(super) fn chain_or_fall_through(
        ctx: &ColumnarBuildContext,
        input: &BuildInput<'_>,
        overlay_path: &Path,
        worktree_path: PathBuf,
        commit_sha: &str,
        lang_registry: Arc<LanguageRegistry>,
    ) -> Option<Self> {
        let manifest_path = ctx.chain_manifest_path_for(commit_sha);
        if manifest_path.exists() {
            match Self::open_via_chain(
                ctx,
                &manifest_path,
                worktree_path.clone(),
                commit_sha,
                Arc::clone(&lang_registry),
                false,
            ) {
                Ok(storage) => return Some(storage),
                Err(e) => tracing::warn!(
                    %commit_sha,
                    "columnar warm_or_open: chain attach failed — falling back \
                     to a full build: {e}"
                ),
            }
        }
        if overlay_path.exists() {
            return None;
        }
        match Self::attach_via_derived_chain(ctx, input, worktree_path, commit_sha, lang_registry) {
            Ok(storage) => storage,
            Err(e) => {
                tracing::warn!(
                    %commit_sha,
                    "columnar warm_or_open: derived chain attach failed — falling \
                     back to a full build: {e}"
                );
                None
            }
        }
    }
}
