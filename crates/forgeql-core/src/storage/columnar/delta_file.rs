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

#![cfg_attr(test, allow(clippy::unwrap_used))]

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

/// File name of a staged reindex segment:
/// `{path_hex}-{content_hex}-v{ENRICH_VER}.fqsf`.
///
/// The path fingerprint is part of the key because node ordinals are
/// file-history-dependent identity, not content-derived data: two files with
/// identical bytes must not share a staged segment, or one file's reindex
/// would silently adopt the other file's node ids (and skip the tombstoned
/// ordinal remap that a removal requires).
///
/// `ENRICH_VER` is part of it for the same reason committed segments live under
/// a versioned directory: a staged segment holds index output. Without it an
/// uncommitted transaction staged before an upgrade survives the restart, the
/// re-index skips the file because a segment already exists, and COMMIT
/// promotes the previous generation's rows into the new store — silently, and
/// with every gate green.
pub(crate) fn staged_segment_file_name(source_path: &Path, hex_content_id: &str) -> String {
    let path_hex = crate::node_id::hex_prefix(
        &crate::node_id::sha256_of_path(&source_path.to_string_lossy()),
        12,
    );
    format!("{path_hex}-{hex_content_id}-v{}.fqsf", super::ENRICH_VER)
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
/// replaced the content-hash removal set with source paths. Version 3 added
/// the `enrich_ver` stamp so a delta written by a previous index-output
/// generation is detected as a whole and its files re-indexed, instead of
/// each staged entry silently failing its (version-suffixed) file-name lookup.
/// Version 4 added `added_paths`, the non-indexed files this session created.
const DELTA_FORMAT_VERSION: u32 = 4;

/// The head of a [`DeltaFile`], decodable on its own.
///
/// bincode writes fields in declaration order, so the leading `version` can be
/// read without the rest — which is what lets a delta from an older layout be
/// reported as an old format rather than as a decode failure.
#[derive(Debug, Deserialize)]
struct DeltaHeader {
    version: u32,
}

/// `bincode`-serialized snapshot of a [`DirtyOverlay`].
///
/// `DirtyOverlay` is not serialized directly — its in-memory indexes are
/// rebuilt from the staging segment files on load.  Only the content-ID list,
/// the removed-path set and the non-indexed paths need to persist.
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
    /// The `ENRICH_VER` this delta's staged segments were produced under.
    ///
    /// Staged segments hold index output, so a delta from an earlier
    /// generation must not be reassembled after an upgrade. The per-segment
    /// file names already embed the version (a stale entry simply fails its
    /// lookup), but that failure is per-entry and silent — recording the
    /// generation here lets the loader recognise the situation as a whole,
    /// keep the version-independent parts (`removed_paths`), and hand the
    /// affected source paths back to the caller for a fresh re-index.
    pub enrich_ver: u32,
    /// One entry per dirty segment held in `DirtyOverlay::added`.
    /// Also the authoritative list of valid staging directories.
    pub staged: Vec<StagedEntry>,
    /// Source paths of persistent overlay segments hidden from queries.
    /// Corresponds to `DirtyOverlay::removed_paths`.
    pub removed_paths: Vec<PathBuf>,
    /// Workspace-relative paths of files this session touched that no language
    /// plugin claims. Version-independent, like `removed_paths`: no index
    /// output is involved, so a delta from an earlier generation keeps them.
    pub added_paths: Vec<PathBuf>,
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
            enrich_ver: super::ENRICH_VER,
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
            added_paths: dirty.added_paths.iter().cloned().collect(),
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
    ///
    /// The second return value lists source paths whose staged index state
    /// could NOT be restored and must be re-indexed from the worktree:
    /// - every staged path, when the delta was written under a different
    ///   `ENRICH_VER` (its segments are a previous generation's index output —
    ///   the version-suffixed file names would each miss anyway, but the file
    ///   stamp makes it one loud decision instead of N silent skips);
    /// - any entry whose staging segment file is missing or unreadable.
    ///
    /// A dropped staged entry also drops the shadowing it re-derived
    /// (`replaces_hex` → `removed_paths`), which would resurface the base
    /// segment's pre-edit rows for that path — reindexing the returned paths
    /// is what restores correctness, so callers must not ignore them.
    /// `removed_paths` (deleted files) and `added_paths` (files this session
    /// created that no plugin claims) hold no index output, so they are
    /// version-independent and always kept.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read or bincode decoding fails.
    pub fn load(path: &Path, staging_dir: &Path) -> Result<(DirtyOverlay, Vec<PathBuf>)> {
        use super::dirty_overlay::DirtySegment;

        let bytes = std::fs::read(path)?;
        // Two reasons the version has to be checked, and one reason it is read
        // on its own. A delta written before the removal set became path-keyed
        // decodes cleanly — bincode encodes `PathBuf` exactly like `String` —
        // with content hashes misread as paths, which shadows nothing and
        // silently resurrects a deleted file's symbols. And a delta written
        // before a field was added fails with a bincode end-of-input, an error
        // indistinguishable from a corrupt file, so the caller would reset the
        // overlay rather than report a format this build no longer reads.
        // Decoding the whole struct first would turn the second case into the
        // wrong error; `version` leads the struct precisely so it need not be.
        let header: DeltaHeader = bincode::deserialize_from(&bytes[..]).map_err(|e| {
            anyhow::anyhow!("columnar delta at {} is unreadable: {e}", path.display())
        })?;
        if header.version != DELTA_FORMAT_VERSION {
            anyhow::bail!(
                "columnar delta at {} has format version {} (expected {})",
                path.display(),
                header.version,
                DELTA_FORMAT_VERSION
            );
        }
        let file: Self = bincode::deserialize(&bytes)?;

        let mut dirty = DirtyOverlay::new();
        let mut needs_reindex: Vec<PathBuf> = Vec::new();
        // Shadowed paths come from two places. The recorded set is authoritative
        // for deleted files, which have no staged replacement to infer from; the
        // staged entries re-derive the changed-file half, since a staged segment
        // that replaces a base segment always shadows its own path.
        dirty.removed_paths = file.removed_paths.into_iter().collect();
        dirty.added_paths = file.added_paths.into_iter().collect();

        if file.enrich_ver != super::ENRICH_VER {
            tracing::info!(
                delta_ver = file.enrich_ver,
                current_ver = super::ENRICH_VER,
                staged = file.staged.len(),
                "columnar delta from a previous index generation — staged \
                 segments discarded, files queued for re-index"
            );
            needs_reindex.extend(file.staged.iter().map(|e| e.source_path.clone()));
            return Ok((dirty, needs_reindex));
        }

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
                        path = %entry.source_path.display(),
                        "columnar delta: staging segment missing/unreadable — \
                         file queued for re-index: {e}"
                    );
                    needs_reindex.push(entry.source_path.clone());
                }
            }
        }

        Ok((dirty, needs_reindex))
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
            .filter(|f| f.version == DELTA_FORMAT_VERSION && f.enrich_ver == super::ENRICH_VER)
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn write_delta(dir: &Path, file: &DeltaFile) -> PathBuf {
        let path = dir.join(".forgeql-columnar-delta");
        std::fs::write(&path, bincode::serialize(file).unwrap()).unwrap();
        path
    }

    fn entry(path: &str, hex: &str, replaces: &str) -> StagedEntry {
        StagedEntry {
            hex_content_id: hex.to_string(),
            source_path: PathBuf::from(path),
            replaces_hex: replaces.to_string(),
        }
    }

    /// A delta stamped with a previous `ENRICH_VER` keeps its (version-
    /// independent) deletions but drops every staged entry, reporting all of
    /// their source paths for re-index.
    #[test]
    fn previous_generation_delta_queues_all_staged_paths_for_reindex() {
        let tmp = tempfile::tempdir().unwrap();
        let file = DeltaFile {
            version: DELTA_FORMAT_VERSION,
            enrich_ver: super::super::ENRICH_VER - 1,
            staged: vec![
                entry("src/a.rs", "aaaa", "beef"),
                entry("src/b.rs", "bbbb", ""),
            ],
            removed_paths: vec![PathBuf::from("src/deleted.rs")],
            added_paths: vec![PathBuf::from("build.conf")],
        };
        let path = write_delta(tmp.path(), &file);

        let (dirty, needs) = DeltaFile::load(&path, tmp.path()).unwrap();
        assert!(
            dirty.added.is_empty(),
            "no previous-generation segment may load"
        );
        assert!(
            dirty
                .removed_paths
                .contains(&PathBuf::from("src/deleted.rs")),
            "deletions are version-independent and must survive"
        );
        assert!(
            dirty.added_paths.contains(&PathBuf::from("build.conf")),
            "a non-indexed path holds no index output, so a generation change \
             cannot invalidate it — restoring it only after the mismatch check \
             would lose it and the file would stop being searched"
        );
        assert_eq!(
            needs,
            vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")],
            "every staged path must be queued for re-index"
        );
    }

    /// A current-generation delta whose staging segment file is missing skips
    /// the entry AND reports its path — a silent skip would leave the file's
    /// pre-edit base rows visible.
    #[test]
    fn missing_staging_segment_queues_its_path_for_reindex() {
        let tmp = tempfile::tempdir().unwrap();
        let file = DeltaFile {
            version: DELTA_FORMAT_VERSION,
            enrich_ver: super::super::ENRICH_VER,
            staged: vec![entry("src/gone.rs", "cccc", "beef")],
            removed_paths: vec![],
            added_paths: vec![],
        };
        let path = write_delta(tmp.path(), &file);

        let (dirty, needs) = DeltaFile::load(&path, tmp.path()).unwrap();
        assert!(dirty.added.is_empty());
        assert_eq!(needs, vec![PathBuf::from("src/gone.rs")]);
    }

    /// A pre-stamp (format v2) delta is refused outright — bincode would
    /// misalign the fields.
    #[test]
    fn older_format_version_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let file = DeltaFile {
            version: DELTA_FORMAT_VERSION - 1,
            enrich_ver: super::super::ENRICH_VER,
            staged: vec![],
            removed_paths: vec![],
            added_paths: vec![],
        };
        let path = write_delta(tmp.path(), &file);
        assert!(DeltaFile::load(&path, tmp.path()).is_err());
    }

    /// The test above writes a current-shaped body with an old version number,
    /// which the version check catches on its own. A *genuinely* older delta
    /// has a shorter body too, and decoding the struct before the version
    /// would fail on end-of-input instead — an error the caller cannot tell
    /// from a corrupt file, so it resets the overlay rather than reporting an
    /// unreadable format. Adding one field is enough to cause that, so this
    /// pins the version being read from the head of the payload.
    #[test]
    fn a_delta_from_the_previous_layout_is_refused_by_version_not_by_eof() {
        /// The v3 shape: no `added_paths`.
        #[derive(serde::Serialize)]
        struct DeltaV3 {
            version: u32,
            enrich_ver: u32,
            staged: Vec<StagedEntry>,
            removed_paths: Vec<PathBuf>,
        }

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".forgeql-columnar-delta");
        let old = DeltaV3 {
            version: DELTA_FORMAT_VERSION - 1,
            enrich_ver: super::super::ENRICH_VER,
            staged: vec![entry("src/a.rs", "aaaa", "")],
            removed_paths: vec![PathBuf::from("src/deleted.rs")],
        };
        std::fs::write(&path, bincode::serialize(&old).unwrap()).unwrap();

        let Err(err) = DeltaFile::load(&path, tmp.path()) else {
            panic!("a previous layout must not load");
        };
        let err = err.to_string();
        assert!(
            err.contains("format version"),
            "the refusal must name the format, not surface a decode error: {err}"
        );
    }

    /// `read_valid_segment_names` treats a previous-generation delta as
    /// having no valid names, so its staging files are GC'd rather than kept.
    #[test]
    fn previous_generation_delta_has_no_valid_segment_names() {
        let tmp = tempfile::tempdir().unwrap();
        let file = DeltaFile {
            version: DELTA_FORMAT_VERSION,
            enrich_ver: super::super::ENRICH_VER - 1,
            staged: vec![entry("src/a.rs", "aaaa", "")],
            removed_paths: vec![],
            added_paths: vec![],
        };
        let path = write_delta(tmp.path(), &file);
        assert!(DeltaFile::read_valid_segment_names(&path).is_empty());
    }
}
