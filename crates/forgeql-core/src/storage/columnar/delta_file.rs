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
    /// Hex content ID — name of the staging subdir: `.forgeql-staging/<hex>/`.
    pub hex_content_id: String,
    /// Workspace-relative source path for the file this segment covers.
    pub source_path: PathBuf,
    /// Hex content ID of the persistent overlay segment being replaced,
    /// or an empty string when the file had no prior persistent entry.
    pub replaces_hex: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// DeltaFile  (on-disk struct)
// ─────────────────────────────────────────────────────────────────────────────

/// `bincode`-serialized snapshot of a [`DirtyOverlay`].
///
/// `DirtyOverlay` is not serialized directly — its in-memory indexes are
/// rebuilt from the staging segment files on load.  Only the content-ID list
/// and the removed-blob set need to persist.
///
/// [`DirtyOverlay`]: super::dirty_overlay::DirtyOverlay
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DeltaFile {
    /// One entry per dirty segment held in `DirtyOverlay::added`.
    /// Also the authoritative list of valid staging directories.
    pub staged: Vec<StagedEntry>,
    /// Hex content IDs of persistent overlay segments hidden from queries.
    /// Corresponds to `DirtyOverlay::removed_hex_ids`.
    pub removed_hex_ids: Vec<String>,
}

impl DeltaFile {
    // ── serialization ────────────────────────────────────────────────────────

    /// Serialize `dirty` and write to `path` (atomic write-then-rename).
    ///
    /// # Errors
    /// Returns `Err` on bincode encoding failure or file I/O error.
    pub fn save(dirty: &DirtyOverlay, path: &Path) -> Result<()> {
        let file = Self {
            staged: dirty
                .added
                .iter()
                .map(|ds| StagedEntry {
                    hex_content_id: ds.reader.content_id_hex(),
                    source_path: ds.source_path.clone(),
                    replaces_hex: ds.replaces_hex.clone(),
                })
                .collect(),
            removed_hex_ids: dirty.removed_hex_ids.iter().cloned().collect(),
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
    /// `staging_dir/<hex>/`.  Entries whose segment directory is missing or
    /// unreadable are silently skipped (non-fatal).
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read or bincode decoding fails.
    pub fn load(path: &Path, staging_dir: &Path) -> Result<DirtyOverlay> {
        use super::dirty_overlay::DirtySegment;

        let bytes = std::fs::read(path)?;
        let file: Self = bincode::deserialize(&bytes)?;

        let mut dirty = DirtyOverlay::new();
        dirty.removed_hex_ids = file.removed_hex_ids.into_iter().collect();

        for entry in &file.staged {
            let seg_path = staging_dir.join(format!("{}.fqsf", &entry.hex_content_id));
            match SegmentReader::open(&seg_path) {
                Ok(reader) => {
                    dirty.added.push(DirtySegment {
                        reader: Arc::new(reader),
                        source_path: entry.source_path.clone(),
                        replaces_hex: entry.replaces_hex.clone(),
                    });
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

    /// Return the set of staging hex IDs recorded in the delta file at `path`,
    /// without fully loading the overlay.
    ///
    /// Returns an empty `Vec` if the file is absent or unreadable (non-fatal).
    #[must_use]
    pub fn read_valid_hexes(path: &Path) -> Vec<String> {
        let Ok(bytes) = std::fs::read(path) else {
            return Vec::new();
        };
        bincode::deserialize::<Self>(&bytes)
            .map(|f| f.staged.into_iter().map(|e| e.hex_content_id).collect())
            .unwrap_or_default()
    }

    /// Delete staging segment directories not listed in `valid_hexes`.
    ///
    /// Called after `git reset --hard` restores an older delta file, so
    /// segments written after the checkpoint are garbage-collected.
    /// Errors from individual deletions are silently ignored.
    pub fn gc_orphaned_staging(valid_hexes: &[String], staging_dir: &Path) {
        let Ok(entries) = std::fs::read_dir(staging_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // Staging files are now `<hex>.fqsf`; strip extension to get hex.
            let hex = name.strip_suffix(".fqsf").unwrap_or(&name);
            if !valid_hexes.contains(&hex.to_owned()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}
