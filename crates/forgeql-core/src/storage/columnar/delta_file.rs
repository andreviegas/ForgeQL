//! Serialization format for the `DirtyOverlay` delta file.
//!
//! `.forgeql-columnar-delta` persists the per-session dirty overlay state so it
//! survives server restarts and `ROLLBACK` operations.
//!
//! ## On-disk format
//!
//! `bincode`-encoded `DeltaFile` struct (same codec as `.forgeql-index`).
//! Binary format keeps the file compact and write-fast for the hot path
//! (`reindex_files` / `purge_file` call `DeltaFile::save` on every mutation).
//!
//! ## Lifecycle
//!
//! | Event               | Action                                         |
//! |---------------------|------------------------------------------------|
//! | `reindex_files`     | Write / overwrite delta                        |
//! | `purge_file`        | Write / overwrite delta                        |
//! | `BEGIN TRANSACTION` | Explicit `save` + delta committed in checkpoint|
//! | `COMMIT MESSAGE`    | Delta excluded from user-facing commit         |
//! | `ROLLBACK`          | `git reset --hard` restores delta; re-load + GC|
//! | Session reconnect   | `load` → restore `DirtyOverlay` in RAM         |

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::dirty_overlay::DirtyOverlay;
use super::segment_reader::SegmentReader;

// ─────────────────────────────────────────────────────────────────────────────
// StagedEntry  (per-segment metadata stored in the file)
// ─────────────────────────────────────────────────────────────────────────────

/// Metadata for one staged segment serialized inside [`DeltaFile`].
#[derive(Debug, Serialize, Deserialize)]
pub struct StagedEntry {
    /// Hex content ID of the staged segment (one half of its file name).
    pub hex_content_id: String,
    /// Workspace-relative source path for the file this segment covers.
    pub source_path: PathBuf,
    /// Hex content ID of the persistent overlay segment being replaced,
    /// or an empty string when the file had no prior persistent entry.
    pub replaces_hex: String,
}

/// File name of a staged reindex segment: `{path_hex}-{content_hex}.fqsf`.
///
/// The path fingerprint is part of the key because node ordinals are
/// file-history-dependent identity, not content-derived data: two files with
/// identical bytes must not share a staged segment, or one file's reindex
/// would silently adopt the other file's node ids (and skip the tombstoned
/// ordinal remap that a removal requires).
pub(crate) fn staged_segment_file_name(source_path: &Path, hex_content_id: &str) -> String {
    let path_hex = crate::node_id::hex_prefix(
        &crate::node_id::sha256_of_path(&source_path.to_string_lossy()),
        12,
    );
    format!("{path_hex}-{hex_content_id}.fqsf")
}

/// On-disk path of a staged segment.
///
/// Named by (path, content) only. A segment staged before that naming carried a
/// content-only name, and resolving one here would hand a file the segment of a
/// byte-identical file with a different path or language — the defect this
/// naming exists to prevent — so pre-0.121 staged segments are deliberately not
/// found. They are orphaned and garbage-collected; the file is reindexed from
/// the worktree, which still holds its uncommitted bytes.
pub(crate) fn staged_segment_path(
    staging_dir: &Path,
    source_path: &Path,
    hex_content_id: &str,
) -> PathBuf {
    staging_dir.join(staged_segment_file_name(source_path, hex_content_id))
}

// ─────────────────────────────────────────────────────────────────────────────
// DeltaFile  (on-disk struct)
// ─────────────────────────────────────────────────────────────────────────────

/// Current [`DeltaFile`] format version.
///
/// Bumped when the meaning or layout of the persisted fields changes. Version 2
/// replaced the content-hash removal set with source paths.
const DELTA_FORMAT_VERSION: u32 = 2;

/// `bincode`-serialized snapshot of a [`DirtyOverlay`].
///
/// `DirtyOverlay` is not serialized directly — its in-memory indexes are
/// rebuilt from the staging segment files on load.  Only the content-ID list
/// and the removed-path set need to persist.
///
/// [`DirtyOverlay`]: super::dirty_overlay::DirtyOverlay
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DeltaFile {
    /// Format version of this file — must lead the struct so a delta written by
    /// an older engine is rejected rather than silently reinterpreted.
    ///
    /// `removed_paths` once held content hashes as `Vec<String>`, and bincode
    /// encodes `PathBuf` exactly like `String`, so an unversioned older file
    /// decodes cleanly with hashes misread as paths.
    pub version: u32,
    /// One entry per dirty segment held in `DirtyOverlay::added`.
    /// Also the authoritative list of valid staging directories.
    pub staged: Vec<StagedEntry>,
    /// Source paths of persistent overlay segments hidden from queries.
    /// Corresponds to `DirtyOverlay::removed_paths`.
    pub removed_paths: Vec<PathBuf>,
}

impl DeltaFile {
    // ── serialization ────────────────────────────────────────────────────────

    /// Serialize `dirty` and write to `path` (atomic write-then-rename).
    ///
    /// # Errors
    /// Returns `Err` on bincode encoding failure or file I/O error.
    pub fn save(dirty: &DirtyOverlay, path: &Path) -> Result<()> {
        let file = Self {
            version: DELTA_FORMAT_VERSION,
            staged: dirty
                .added
                .iter()
                .map(|ds| StagedEntry {
                    hex_content_id: ds.reader.content_id_hex(),
                    source_path: ds.source_path.clone(),
                    replaces_hex: ds.replaces_hex.clone(),
                })
                .collect(),
            removed_paths: dirty.removed_paths.iter().cloned().collect(),
        };
        let bytes = bincode::serialize(&file)?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let tmp = parent.join(".forgeql-columnar-delta.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Deserialize from `path` and rebuild a `DirtyOverlay`.
    ///
    /// For each staged entry, opens the matching `SegmentReader` from
    /// `staging_dir` (file name derived via [`staged_segment_file_name`]).
    /// Entries whose segment file is missing or unreadable are silently
    /// skipped (non-fatal).
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read or bincode decoding fails.
    pub fn load(path: &Path, staging_dir: &Path) -> Result<DirtyOverlay> {
        use super::dirty_overlay::DirtySegment;

        let bytes = std::fs::read(path)?;
        let file: Self = bincode::deserialize(&bytes)?;
        // A delta written before the removal set became path-keyed decodes
        // cleanly — bincode encodes `PathBuf` exactly like `String` — with
        // content hashes misread as paths, which shadows nothing and silently
        // resurrects a deleted file's symbols. Refuse it instead.
        if file.version != DELTA_FORMAT_VERSION {
            anyhow::bail!(
                "columnar delta at {} has format version {} (expected {})",
                path.display(),
                file.version,
                DELTA_FORMAT_VERSION
            );
        }

        let mut dirty = DirtyOverlay::new();
        // Shadowed paths come from two places. The recorded set is authoritative
        // for deleted files, which have no staged replacement to infer from; the
        // staged entries re-derive the changed-file half, since a staged segment
        // that replaces a base segment always shadows its own path.
        dirty.removed_paths = file.removed_paths.into_iter().collect();

        for entry in &file.staged {
            let seg_path =
                staged_segment_path(staging_dir, &entry.source_path, &entry.hex_content_id);
            match SegmentReader::open(&seg_path) {
                Ok(reader) => {
                    dirty.added.push(DirtySegment {
                        reader: Arc::new(reader),
                        source_path: entry.source_path.clone(),
                        replaces_hex: entry.replaces_hex.clone(),
                    });
                    if !entry.replaces_hex.is_empty() {
                        let _ = dirty.removed_paths.insert(entry.source_path.clone());
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        hex = %entry.hex_content_id,
                        "columnar delta: staging segment missing/unreadable (skipping): {e}"
                    );
                }
            }
        }

        Ok(dirty)
    }

    // ── GC helpers ───────────────────────────────────────────────────────────

    /// Return the staged segment file names recorded in the delta file at
    /// `path`, without fully loading the overlay.
    ///
    /// Returns an empty `Vec` if the file is absent or unreadable (non-fatal).
    #[must_use]
    pub fn read_valid_segment_names(path: &Path) -> Vec<String> {
        let Ok(bytes) = std::fs::read(path) else {
            return Vec::new();
        };
        bincode::deserialize::<Self>(&bytes)
            .ok()
            .filter(|f| f.version == DELTA_FORMAT_VERSION)
            .map(|f| {
                f.staged
                    .into_iter()
                    .map(|e| staged_segment_file_name(&e.source_path, &e.hex_content_id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Delete staged segment files not listed in `valid_names`.
    ///
    /// Called after `git reset --hard` restores an older delta file, so
    /// segments written after the checkpoint are garbage-collected.
    /// Errors from individual deletions are silently ignored.
    pub fn gc_orphaned_staging(valid_names: &[String], staging_dir: &Path) {
        let Ok(entries) = std::fs::read_dir(staging_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if valid_names.contains(&name) {
                continue;
            }
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
