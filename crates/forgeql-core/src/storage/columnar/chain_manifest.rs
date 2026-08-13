//! [`ChainManifest`] — the per-commit record of "master overlay + changes".
//!
//! A commit whose overlay was never built still has a complete index: the
//! overlay of the commit it grew from (the *master*) plus the per-file
//! segments its changes promoted into the content-addressed store. The
//! manifest names exactly that: the master commit, the changed files with
//! their segment content IDs, the shadowed paths, and the non-indexed files
//! the chain added. An attacher that finds a manifest opens the master
//! overlay and seeds its session dirty overlay from the entries instead of
//! paying a full corpus merge.
//!
//! One manifest names one master and one change set — never a chain of
//! deltas. The session dirty overlay is cumulative across commits, so the
//! manifest written at each commit already folds everything since the
//! master into a single layer.
//!
//! ## On-disk format
//!
//! Four magic bytes (`FQCM`) followed by the `bincode`-encoded struct.
//! `version` leads the struct for the same reason it leads [`DeltaFile`]:
//! bincode carries no field names, so an older layout must be recognised by
//! number, never inferred from a decode that happens to succeed.
//!
//! Written atomically (temp file + fsync + rename) at commit time, beside
//! the overlay the commit would otherwise have. A manifest that is missing,
//! unreadable, or from another index generation is never an error the user
//! sees: the attacher falls back to the full build, which is slower and
//! complete — a bad manifest can cost time, never rows.
//!
//! [`DeltaFile`]: super::delta_file::DeltaFile

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::dirty_overlay::DirtyOverlay;

/// Magic bytes at the start of every chain manifest.
pub(crate) const CHAIN_MAGIC: [u8; 4] = *b"FQCM";

/// Current chain manifest format version. Bump on any layout change.
pub const CHAIN_FORMAT_VERSION: u32 = 1;

/// Reads the leading `version` field without decoding the whole struct, so
/// a layout this build does not know is reported as such rather than as a
/// bincode decode error indistinguishable from corruption.
#[derive(Debug, Deserialize)]
struct ChainHeader {
    version: u32,
}

/// One changed file: its workspace-relative path, the content ID of its
/// segment in the store, and the content ID of the master segment it
/// replaces (empty for a file the chain created).
///
/// Mirrors [`StagedEntry`] field-for-field so that seeding a session dirty
/// overlay from a manifest is the same mechanical walk as restoring one
/// from a delta file.
///
/// [`StagedEntry`]: super::delta_file::StagedEntry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainEntry {
    pub source_path: PathBuf,
    pub hex_content_id: String,
    pub replaces_hex: String,
}

/// The per-commit "master + changes" record. See the module doc.
#[derive(Debug, Serialize, Deserialize)]
pub struct ChainManifest {
    /// Format version of this file — must lead the struct.
    pub version: u32,
    /// The `ENRICH_VER` the entries' segments were produced under. A
    /// mismatch means the store partition this manifest points into is not
    /// the one the running binary reads; the attacher falls back to a full
    /// build rather than assembling an index from another generation.
    pub enrich_ver: u32,
    /// The commit whose overlay this chain grows from. Always a commit with
    /// a real overlay on disk, never another chained commit: the writer
    /// inherits its own master rather than pointing at itself.
    pub master_commit: String,
    /// The changed files, one entry per segment to seed.
    pub entries: Vec<ChainEntry>,
    /// Source paths of master segments hidden from queries — replaced paths
    /// and deleted paths both. Keyed by path, never content hash: two files
    /// can share bytes, and shadowing the changed one must not blank out
    /// the unchanged twin.
    pub removed_paths: Vec<PathBuf>,
    /// Workspace-relative paths of files no language plugin claims that the
    /// chain added — the `FIND files` universe beyond the indexed one.
    pub added_paths: Vec<PathBuf>,
}

impl ChainManifest {
    /// Builds the manifest for a commit from the session state that made it:
    /// the master commit the session serves from and its cumulative dirty
    /// overlay. Pure — no filesystem access.
    #[must_use]
    pub fn from_dirty(master_commit: &str, dirty: &DirtyOverlay) -> Self {
        let entries = dirty
            .added
            .iter()
            .map(|ds| ChainEntry {
                source_path: ds.source_path.clone(),
                hex_content_id: ds.reader.content_id_hex(),
                replaces_hex: ds.replaces_hex.clone(),
            })
            .collect();
        let mut removed_paths: Vec<PathBuf> = dirty.removed_paths.iter().cloned().collect();
        removed_paths.sort_unstable();
        let mut added_paths: Vec<PathBuf> = dirty.added_paths.iter().cloned().collect();
        added_paths.sort_unstable();
        Self {
            version: CHAIN_FORMAT_VERSION,
            enrich_ver: super::ENRICH_VER,
            master_commit: master_commit.to_owned(),
            entries,
            removed_paths,
            added_paths,
        }
    }

    /// Writes the manifest atomically: temp file in the target directory,
    /// fsync, rename. A crash mid-write leaves the old file or none.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating manifest dir {}", parent.display()))?;
        }
        let tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or_else(|| Path::new(".")))
            .context("creating temp chain manifest")?;
        {
            let mut f = std::io::BufWriter::new(tmp.as_file());
            f.write_all(&CHAIN_MAGIC)
                .context("writing chain manifest magic")?;
            bincode::serialize_into(&mut f, self).context("encoding chain manifest")?;
            f.flush().context("flushing chain manifest")?;
            tmp.as_file()
                .sync_all()
                .context("fsyncing chain manifest")?;
        }
        let _ = tmp
            .persist(path)
            .with_context(|| format!("persisting chain manifest to {}", path.display()))?;
        Ok(())
    }

    /// Reads and validates a manifest. Every failure is an `Err` naming the
    /// reason; the caller decides whether that means "fall back to a full
    /// build" (attach) or something louder.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading chain manifest {}", path.display()))?;
        let Some(payload) = bytes.strip_prefix(&CHAIN_MAGIC) else {
            anyhow::bail!(
                "chain manifest at {} does not start with FQCM",
                path.display()
            );
        };
        let header: ChainHeader = bincode::deserialize_from(payload).map_err(|e| {
            anyhow::anyhow!("chain manifest at {} is unreadable: {e}", path.display())
        })?;
        if header.version != CHAIN_FORMAT_VERSION {
            anyhow::bail!(
                "chain manifest at {} has format version {} (expected {})",
                path.display(),
                header.version,
                CHAIN_FORMAT_VERSION
            );
        }
        let manifest: Self = bincode::deserialize(payload).map_err(|e| {
            anyhow::anyhow!(
                "chain manifest at {} claims format version {} but does not decode as one: {e}",
                path.display(),
                header.version
            )
        })?;
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ChainManifest {
        ChainManifest {
            version: CHAIN_FORMAT_VERSION,
            enrich_ver: crate::storage::columnar::ENRICH_VER,
            master_commit: "abc123".to_owned(),
            entries: vec![ChainEntry {
                source_path: PathBuf::from("src/a.rs"),
                hex_content_id: "deadbeef".to_owned(),
                replaces_hex: "cafebabe".to_owned(),
            }],
            removed_paths: vec![PathBuf::from("src/a.rs"), PathBuf::from("src/gone.rs")],
            added_paths: vec![PathBuf::from("assets/logo.png")],
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.chain");
        sample().save(&path).unwrap();
        let back = ChainManifest::load(&path).unwrap();
        assert_eq!(back.master_commit, "abc123");
        assert_eq!(back.entries.len(), 1);
        assert_eq!(back.entries[0].replaces_hex, "cafebabe");
        assert_eq!(back.removed_paths.len(), 2);
        assert_eq!(back.added_paths.len(), 1);
    }

    #[test]
    fn wrong_magic_is_refused_not_decoded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.chain");
        std::fs::write(&path, b"NOPE----junk").unwrap();
        let err = ChainManifest::load(&path).unwrap_err().to_string();
        assert!(err.contains("FQCM"), "unexpected error: {err}");
    }

    #[test]
    fn a_newer_format_version_is_refused_by_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.chain");
        let mut m = sample();
        m.version = CHAIN_FORMAT_VERSION + 1;
        m.save(&path).unwrap();
        let err = ChainManifest::load(&path).unwrap_err().to_string();
        assert!(
            err.contains("format version") && err.contains("expected"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_truncated_file_is_an_error_not_a_partial_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.chain");
        sample().save(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();
        assert!(ChainManifest::load(&path).is_err());
    }
}
