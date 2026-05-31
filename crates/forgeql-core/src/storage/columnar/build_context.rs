use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tracing::warn;

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
    /// Returns `<segments_dir>/<provider_id>-v<N>/<hex[0..2]>/<hex[2..]>.fqsf`
    /// (git-style 2-char fan-out to avoid flat directories on large repos).
    #[must_use]
    pub fn segment_path_for(&self, hex_content_id: &str) -> PathBuf {
        self.segments_dir
            .join(self.versioned_provider())
            .join(&hex_content_id[..2])
            .join(format!("{}.fqsf", &hex_content_id[2..]))
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

    /// Create a [`SegmentBuildCtx`] that writes segments **inline** per-file
    /// during the parallel parse, and an [`InlineCtxState`] for extracting the
    /// results after [`SymbolTable::build`] completes.
    ///
    /// The returned `emit_fn` closure:
    /// 1. Hashes source bytes → content-ID (already done by caller, passed in).
    /// 2. Writes the per-file segment to `segments_dir` (idempotent).
    /// 3. Accumulates `(abs_path, content_id)` in `InlineCtxState::segment_map`.
    /// 4. Accumulates enrichment column names in `InlineCtxState::all_columns`.
    ///
    /// [`SymbolTable::build`]: crate::ast::index::SymbolTable::build
    #[must_use]
    pub fn make_inline_ctx(&self) -> (crate::ast::index::SegmentBuildCtx, Arc<InlineCtxState>) {
        use super::bytes_to_hex;
        use super::segment_builder::{SegmentBuilder, SymbolRow, is_valid_segment};
        use crate::ast::index::{SegEmitFn, SegmentBuildCtx};

        let state = Arc::new(InlineCtxState {
            segment_map: Mutex::new(HashMap::new()),
            all_columns: Mutex::new(BTreeSet::new()),
        });

        let segments_dir = self.segments_dir.clone();
        let provider_id = self.provider_id.clone();
        let enrich_ver = super::ENRICH_VER;
        let state_ref = Arc::clone(&state);

        let emit_fn: SegEmitFn = Arc::new(
            move |content_id: &[u8], table: &crate::ast::index::SymbolTable, rows_start: usize| {
                let Some(first_row) = table.rows.get(rows_start) else {
                    return;
                };
                let abs_path = table.path_of(first_row).to_path_buf();

                let hex = bytes_to_hex(content_id);
                let provider_ver_dir = segments_dir.join(format!("{provider_id}-v{enrich_ver}"));
                let target_path = provider_ver_dir
                    .join(&hex[..2])
                    .join(format!("{}.fqsf", &hex[2..]));

                // Always register in segment_map, even for already-written segments.
                {
                    let mut map = state_ref
                        .segment_map
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let _ = map.insert(abs_path, content_id.to_vec());
                }

                if is_valid_segment(&target_path) {
                    return; // Idempotent: segment already written on a prior run.
                }

                if let Err(e) = std::fs::create_dir_all(&provider_ver_dir) {
                    warn!(path = %provider_ver_dir.display(), "inline emit: failed to create provider dir: {e}");
                    return;
                }

                let mut builder = SegmentBuilder::new(&provider_id, content_id);
                let mut local_cols: BTreeSet<String> = BTreeSet::new();

                for row in &table.rows[rows_start..] {
                    let row_id = builder.emit_row(SymbolRow {
                        name: table.name_of(row),
                        fql_kind: table.fql_kind_of(row),
                        language: table.language_of(row),
                        line: u32::try_from(row.line).unwrap_or(u32::MAX),
                        byte_start: u32::try_from(row.byte_range.start).unwrap_or(u32::MAX),
                        byte_end: u32::try_from(row.byte_range.end).unwrap_or(u32::MAX),
                        usages_count: row.usages_count,
                    });
                    if let Some(ordinal) = row.ordinal {
                        builder.set_ordinal(row_id, ordinal);
                    }
                    for (key, value) in table.resolve_fields(&row.fields) {
                        let _ = local_cols.insert(key.clone());
                        builder.set_field(row_id, &key, value);
                    }
                }

                match builder.flush(&target_path) {
                    Ok(()) => {
                        let mut cols = state_ref
                            .all_columns
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        cols.extend(local_cols);
                    }
                    Err(e) => {
                        warn!(target = %target_path.display(), "inline emit: flush failed: {e}");
                    }
                }
            },
        );

        let ctx = SegmentBuildCtx {
            provider_id: self.provider_id.clone(),
            hash_fn: Arc::clone(&self.hash_fn),
            emit_fn,
        };

        (ctx, state)
    }
}

// ---------------------------------------------------------------------------
// InlineCtxState — shared mutable state for make_inline_ctx
// ---------------------------------------------------------------------------

/// Shared state populated by the inline-emit closure during
/// [`ColumnarBuildContext::make_inline_ctx`].
///
/// After [`SymbolTable::build`] returns (all rayon threads finished), the
/// caller can extract the final results via [`InlineCtxState::take`].
///
/// [`SymbolTable::build`]: crate::ast::index::SymbolTable::build
pub struct InlineCtxState {
    /// Absolute source path → raw content-ID bytes, one entry per processed file.
    pub segment_map: Mutex<HashMap<PathBuf, Vec<u8>>>,
    /// Enrichment column names seen across all files.
    pub all_columns: Mutex<BTreeSet<String>>,
}
