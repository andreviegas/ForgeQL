//! [`ShadowWriter`] — drives per-file shadow-write from a built [`SymbolTable`].
//!
//! After the legacy index build completes, `ShadowWriter::run` iterates
//! every source file present in the symbol table, computes its git blob
//! SHA-1 content hash, and writes the corresponding columnar segment to
//! `<segments_base>/git-sha1/<hex>/`.
//!
//! Shadow-write is **idempotent**: if a segment directory already exists and
//! contains a valid `header.bin`, it is skipped.
//!
//! Errors on individual files are logged as warnings and skipped — shadow-
//! write must never abort the primary legacy-index build.
//!
//! [`SymbolTable`]: crate::ast::index::SymbolTable

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use tracing::{debug, warn};

use crate::ast::index::SymbolTable;
use crate::storage::git_sha1_provider::git_blob_sha1;

use super::bytes_to_hex;
use super::segment_builder::{SegmentBuilder, is_valid_segment};

/// Iterates a [`SymbolTable`] and writes one columnar segment per source file.
pub struct ShadowWriter<'a> {
    table: &'a SymbolTable,
    workspace_path: &'a Path,
    segments_base: &'a Path,
}

impl<'a> ShadowWriter<'a> {
    /// Create a shadow writer.
    ///
    /// - `table`: fully-built symbol table.
    /// - `workspace_path`: absolute path to the working directory (used to
    ///   read source file bytes for hashing).
    /// - `segments_base`: path to `<bare-repo>/forgeql/segments/`.
    ///   Segments are written under `<segments_base>/git-sha1/<hex>/`.
    #[must_use]
    pub const fn new(
        table: &'a SymbolTable,
        workspace_path: &'a Path,
        segments_base: &'a Path,
    ) -> Self {
        Self {
            table,
            workspace_path,
            segments_base,
        }
    }

    /// Write one columnar segment per source file in the symbol table.
    ///
    /// Returns the count of **newly written** segments (already-valid
    /// segments are skipped and not counted).
    ///
    /// # Errors
    /// Returns `Err` only for fatal infrastructure failures (e.g. unable to
    /// create the `segments/git-sha1/` directory).  Per-file errors are
    /// logged as warnings and skipped.
    pub fn run(&self) -> Result<usize> {
        // Group row indices by path_id so each file is processed once.
        let mut by_path: HashMap<u32, Vec<usize>> = HashMap::new();
        for (idx, row) in self.table.rows.iter().enumerate() {
            by_path.entry(row.path_id).or_default().push(idx);
        }

        if by_path.is_empty() {
            return Ok(0);
        }

        // Ensure the provider directory exists.
        let provider_dir = self.segments_base.join("git-sha1");
        std::fs::create_dir_all(&provider_dir)?;

        let mut written: usize = 0;

        for row_indices in by_path.values() {
            // `row_indices` is non-empty by construction.
            let first_row = &self.table.rows[row_indices[0]];
            let rel_path = self.table.path_of(first_row);
            let abs_path = self.workspace_path.join(rel_path);

            // Read source bytes for content hashing.
            let bytes = match std::fs::read(&abs_path) {
                Ok(b) => b,
                Err(e) => {
                    warn!(
                        path = %abs_path.display(),
                        "shadow-write: skipping unreadable source file: {e}"
                    );
                    continue;
                }
            };

            let content_id = git_blob_sha1(&bytes);
            let hex = bytes_to_hex(&content_id);
            let target_dir = provider_dir.join(&hex);

            // Idempotent: skip already-valid segments.
            if is_valid_segment(&target_dir) {
                debug!(
                    path = %rel_path.display(),
                    hex = %hex,
                    "shadow-write: segment already valid, skipping"
                );
                continue;
            }

            // Build segment.
            let mut builder = SegmentBuilder::new("git-sha1", &content_id);
            for &idx in row_indices {
                let row = &self.table.rows[idx];
                #[allow(clippy::cast_possible_truncation)]
                builder.add_row(
                    self.table.name_of(row),
                    self.table.fql_kind_of(row),
                    self.table.language_of(row),
                    row.line as u32,
                    row.byte_range.start as u32,
                    row.byte_range.end as u32,
                    row.usages_count,
                );
            }

            match builder.flush(&target_dir) {
                Ok(()) => {
                    debug!(
                        path = %rel_path.display(),
                        hex = %hex,
                        "shadow-write: segment written"
                    );
                    written += 1;
                }
                Err(e) => {
                    warn!(
                        path = %rel_path.display(),
                        hex = %hex,
                        "shadow-write: flush failed: {e}"
                    );
                }
            }
        }

        Ok(written)
    }
}
