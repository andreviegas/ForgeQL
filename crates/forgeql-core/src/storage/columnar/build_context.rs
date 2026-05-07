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

    /// Path to the segment directory for a given hex content ID.
    ///
    /// Returns `<segments_dir>/<provider_id>/<hex_content_id>`.
    #[must_use]
    pub fn segment_dir_for(&self, hex_content_id: &str) -> PathBuf {
        self.segments_dir
            .join(&self.provider_id)
            .join(hex_content_id)
    }

    /// Path to the overlay file for a given snapshot hex (e.g. commit SHA).
    ///
    /// Returns `<overlays_dir>/<provider_id>/<snapshot_hex>.bin`.
    #[must_use]
    pub fn overlay_path_for(&self, snapshot_hex: &str) -> PathBuf {
        self.overlays_dir
            .join(&self.provider_id)
            .join(format!("{snapshot_hex}.bin"))
    }
}
