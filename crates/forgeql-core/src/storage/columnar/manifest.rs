//! [`Manifest`] — per-source metadata for the columnar storage engine.
//!
//! Written as `<bare-repo>/forgeql/manifest.json` after every successful
//! shadow-write build.  Records the provider ID and the set of enrichment
//! column names ever observed across all segment directories.
//!
//! Phase 05 and Phase 08 read the manifest to discover which enrichment
//! columns exist, enabling `WHERE param_count > 2` and similar predicates
//! against the columnar backend.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Current manifest schema version.  Bump when the format changes in a
/// backwards-incompatible way.
const SCHEMA_VERSION: u32 = 1;

/// Metadata file for a ForgeQL columnar segment store.
///
/// Stored as `<bare-repo>/forgeql/manifest.json`.  The file is written
/// atomically (temp-file + rename) so a concurrent `USE` cannot observe a
/// partially-written manifest.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    /// Manifest format version — bump when the schema changes.
    pub schema_version: u32,
    /// Provider ID string (e.g. `"git-sha1"`) that matches the sub-directory
    /// under `segments/` where segment data lives.
    pub provider_id: String,
    /// Enrichment field names ever observed across all segment directories.
    ///
    /// Used by Phase 04+ read paths to discover which optional columns exist
    /// and can be queried via `WHERE field_name = value`.
    pub column_registry: BTreeSet<String>,
    /// Cumulative count of segments successfully written across all builds.
    pub segment_count: u64,
}

impl Manifest {
    /// Load a manifest from `path`, or return a zeroed manifest if the file
    /// does not yet exist.
    ///
    /// # Errors
    /// Returns `Err` if the file exists but cannot be read or contains invalid
    /// JSON.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing manifest at {}", path.display()))
    }

    /// Atomically write the manifest to `path` using a `.tmp` sibling file.
    ///
    /// # Errors
    /// Returns `Err` on I/O or serialisation failure.
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).context("serialising manifest")?;
        // Atomic write: write temp file next to target, then rename.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming manifest to {}", path.display()))?;
        Ok(())
    }

    /// Load → merge → save in one call.
    ///
    /// Reads the existing manifest at `path` (or starts fresh), merges in
    /// `new_columns` and adds `segments_written` to the cumulative counter,
    /// then saves atomically.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read, parsed, or written.
    pub fn update(
        path: &Path,
        provider_id: &str,
        new_columns: &BTreeSet<String>,
        segments_written: u64,
    ) -> Result<()> {
        let mut m = Self::load(path)?;
        m.schema_version = SCHEMA_VERSION;
        provider_id.clone_into(&mut m.provider_id);
        m.column_registry.extend(new_columns.iter().cloned());
        m.segment_count = m.segment_count.saturating_add(segments_written);
        m.save(path)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");

        let m = Manifest {
            schema_version: 1,
            provider_id: "git-sha1".to_owned(),
            column_registry: BTreeSet::new(),
            segment_count: 0,
        };
        m.save(&path).unwrap();

        let loaded = Manifest::load(&path).unwrap();
        assert_eq!(loaded.provider_id, "git-sha1");
        assert_eq!(loaded.segment_count, 0);
        assert!(loaded.column_registry.is_empty());
    }

    #[test]
    fn update_merges_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");

        let cols1: BTreeSet<String> = ["param_count".to_owned(), "is_const".to_owned()]
            .into_iter()
            .collect();
        Manifest::update(&path, "git-sha1", &cols1, 3).unwrap();

        let cols2: BTreeSet<String> = ["is_const".to_owned(), "naming".to_owned()]
            .into_iter()
            .collect();
        Manifest::update(&path, "git-sha1", &cols2, 2).unwrap();

        let m = Manifest::load(&path).unwrap();
        assert_eq!(m.segment_count, 5);
        assert!(m.column_registry.contains("param_count"));
        assert!(m.column_registry.contains("is_const"));
        assert!(m.column_registry.contains("naming"));
    }

    #[test]
    fn load_missing_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");
        let m = Manifest::load(&path).unwrap();
        assert_eq!(m.segment_count, 0);
        assert!(m.provider_id.is_empty());
    }
}
