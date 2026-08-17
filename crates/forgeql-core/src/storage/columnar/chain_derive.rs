//! Deriving a chain manifest for a commit no ForgeQL session made.
//!
//! A COMMIT writes its own [`ChainManifest`] from the session's dirty
//! overlay. A commit that arrived from upstream — pushed by someone else and
//! pulled in by `REFRESH SOURCE` — has no such record, and until now every
//! attach to it paid a full corpus merge however few files had changed. The
//! same manifest can be constructed at attach time from what the attacher
//! already holds: the overlay of an ancestor commit (the *master*), and the
//! per-file segment map its own parse produced for the target commit. Both
//! name every indexed file by workspace-relative path and content id, so
//! the difference between them is exactly the change set — a path with a
//! new content id is a replaced file, a path only the target has is a new
//! one, a path only the master has is deleted, and a rename is a deletion
//! plus an addition. Nothing is inferred from the commit graph: the master
//! is chosen for being an ancestor because a nearer ancestor means a
//! smaller change set, but the change set itself is read off the two
//! indexes, so a stale master costs entries, never rows.
//!
//! The non-indexed files (`FIND files` beyond the indexed universe) come
//! from the same worktree walk a full build runs, compared against the
//! master's file-only entries by path and size — the two things a listing
//! reports.
//!
//! [`ChainManifest`]: super::chain_manifest::ChainManifest

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::chain_manifest::{ChainEntry, ChainManifest};
use super::overlay::Overlay;

/// Build the manifest that serves `target_segments` as `master` plus a
/// change set.
///
/// `target_segments` is the target commit's inline segment map — one entry
/// per indexed file, keyed by the path the parse saw (absolute under
/// `worktree_root`, or already relative) with its content id — and
/// `worktree_root` is the checkout of the target commit, walked for the
/// files no plugin claims. `master` is the ancestor overlay the chain grows
/// from.
///
/// # Errors
/// Refuses a path that is an indexed file in the master and a non-indexed
/// one in the target: the dirty overlay records a path as either shadowed
/// or added, and such a file would need both at once. It cannot arise from
/// a change of content — which extensions are indexed is fixed by the
/// running binary — so it is a refusal, and the caller falls back to the
/// full build.
pub(super) fn derive_upstream_manifest(
    master_commit: &str,
    master: &Overlay,
    target_segments: &HashMap<PathBuf, Vec<u8>>,
    worktree_root: &Path,
) -> Result<ChainManifest> {
    let master_segs: HashMap<&Path, &str> = master
        .segments()
        .iter()
        .map(|m| (m.source_path.as_path(), m.hex_content_id.as_str()))
        .collect();

    // Indexed files: replaced, added, deleted.
    let mut entries = Vec::new();
    let mut removed: HashSet<PathBuf> = HashSet::new();
    let mut target_rel: HashSet<PathBuf> = HashSet::with_capacity(target_segments.len());
    for (path, cid) in target_segments {
        let rel = super::segment_source_rel(path, worktree_root);
        let hex = super::bytes_to_hex(cid);
        let _ = target_rel.insert(rel.to_path_buf());
        match master_segs.get(rel) {
            Some(master_hex) if *master_hex == hex => {}
            Some(master_hex) => {
                let _ = removed.insert(rel.to_path_buf());
                entries.push(ChainEntry {
                    source_path: rel.to_path_buf(),
                    hex_content_id: hex,
                    replaces_hex: (*master_hex).to_owned(),
                });
            }
            None => entries.push(ChainEntry {
                source_path: rel.to_path_buf(),
                hex_content_id: hex,
                replaces_hex: String::new(),
            }),
        }
    }
    for path in master_segs.keys() {
        if !target_rel.contains(*path) {
            let _ = removed.insert((*path).to_path_buf());
        }
    }

    // Non-indexed files: the target's walk against the master's listing.
    let master_file_only: HashMap<&Path, u32> = master
        .file_entries()
        .iter()
        .map(|(p, size)| (p.as_path(), *size))
        .collect();
    let mut added: Vec<PathBuf> = Vec::new();
    let mut target_file_only: HashSet<PathBuf> = HashSet::new();
    for (rel, _) in super::overlay_builder::collect_file_only(worktree_root, &target_rel) {
        if master_segs.contains_key(rel.as_path()) {
            anyhow::bail!(
                "{} is an indexed file in master {master_commit} and a non-indexed \
                 one in the target commit; a chain cannot serve both",
                rel.display()
            );
        }
        // Size as the listing reports it — the same read `FIND files` does
        // for a file it has no segment for.
        let size = std::fs::metadata(worktree_root.join(&rel))
            .map(|m| u32::try_from(m.len()).unwrap_or(u32::MAX))
            .unwrap_or(0);
        if master_file_only.get(rel.as_path()) != Some(&size) {
            added.push(rel.clone());
        }
        let _ = target_file_only.insert(rel);
    }
    for path in master_file_only.keys() {
        if !target_file_only.contains(*path) {
            let _ = removed.insert((*path).to_path_buf());
        }
    }

    entries.sort_by(|a, b| a.source_path.cmp(&b.source_path));
    let mut removed_paths: Vec<PathBuf> = removed.into_iter().collect();
    removed_paths.sort_unstable();
    added.sort_unstable();
    Ok(ChainManifest {
        version: super::chain_manifest::CHAIN_FORMAT_VERSION,
        enrich_ver: super::ENRICH_VER,
        master_commit: master_commit.to_owned(),
        entries,
        removed_paths,
        added_paths: added,
    })
}
