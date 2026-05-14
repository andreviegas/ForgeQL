use std::path::PathBuf;

use super::HashFn;

/// Per-session columnar build configuration.
///
/// Populated at session creation when columnar shadow-write is enabled,
/// then consumed by the shadow-write and overlay-build paths.
///
/// Replaces the four flat `columnar_*` fields previously on [`Session`]:
/// `columnar_segments_dir`, `columnar_provider_id`, `columnar_hash_fn`,
/// and `columnar_overlays_dir`.
///
/// [`Session`]: crate::session::Session
#[derive(Clone)]
pub struct ColumnarBuildContext {
    /// Workspace-private segments directory (typically `<bare>/forgeql/segments`).
    pub segments_dir: PathBuf,
    /// Workspace-private overlays directory (typically `<bare>/forgeql/overlays`).
    pub overlays_dir: PathBuf,
    /// Source-provider identifier, e.g. `"git-sha1"`. Used as a path component.
    pub provider_id: String,
    /// Hash function selected by the provider.
    pub hash_fn: HashFn,
}

impl ColumnarBuildContext {
    /// Construct a context from explicit values.
    pub fn new(
        segments_dir: PathBuf,
        overlays_dir: PathBuf,
        provider_id: impl Into<String>,
        hash_fn: HashFn,
    ) -> Self {
        Self {
            segments_dir,
            overlays_dir,
            provider_id: provider_id.into(),
            hash_fn,
        }
    }

    /// Versioned provider directory name: `"<provider_id>-v<ENRICH_VER>"`.
    ///
    /// Used as the first path component under both `segments/` and `overlays/`.
    /// Bumping `ENRICH_VER` produces a new namespace; old dirs are orphaned.
    #[must_use]
    pub fn versioned_provider(&self) -> String {
        format!("{}-v{}", self.provider_id, super::ENRICH_VER)
    }

    /// Path to the segment directory for a given hex content ID.
    ///
    /// Returns `<segments_dir>/<provider_id>-v<N>/<hex[0..2]>/<hex[2..]>`
    /// (git-style 2-char fan-out to avoid flat directories on large repos).
    #[must_use]
    pub fn segment_dir_for(&self, hex_content_id: &str) -> PathBuf {
        self.segments_dir
            .join(self.versioned_provider())
            .join(&hex_content_id[..2])
            .join(&hex_content_id[2..])
    }

    /// Path to the overlay file for a given snapshot hex (e.g. commit SHA).
    ///
    /// Returns `<overlays_dir>/<provider_id>-v<N>/<hex[0..2]>/<hex[2..]>.bin`.
    #[must_use]
    pub fn overlay_path_for(&self, snapshot_hex: &str) -> PathBuf {
        self.overlays_dir
            .join(self.versioned_provider())
            .join(&snapshot_hex[..2])
            .join(format!("{}.bin", &snapshot_hex[2..]))
    }

    /// Path to the versioned manifest file.
    ///
    /// Returns `<forgeql_dir>/manifest-<provider_id>-v<ENRICH_VER>.json`
    /// where `<forgeql_dir>` is the parent of `segments_dir`.
    #[must_use]
    pub fn manifest_path(&self) -> PathBuf {
        let forgeql_dir = self
            .segments_dir
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        forgeql_dir.join(format!(
            "manifest-{}-v{}.json",
            self.provider_id,
            super::ENRICH_VER
        ))
    }
}
