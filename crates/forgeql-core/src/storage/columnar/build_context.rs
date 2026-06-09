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
    #[allow(
        clippy::too_many_lines,
        reason = "mirrors reindex_files / ShadowWriter::run"
    )]
    pub fn make_inline_ctx(&self) -> (crate::ast::index::SegmentBuildCtx, Arc<InlineCtxState>) {
        use super::bytes_to_hex;
        use super::segment_builder::{RowId, SegmentBuilder, SymbolRow, is_valid_segment};
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

                // (ordinal, row_id, parent_ordinal) for the nav post-pass. Keeping
                // this in sync with ShadowWriter::run and reindex_files is essential:
                // omitting set_parent_ordinal + the nav pass here wrote flat
                // (parent_ordinal = MAX) segments from the inline build path, which
                // mismatched the nested rows reindex writes — the root of BUG-010.
                let mut ordinal_row: Vec<(u32, u32, u32)> = Vec::new();
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
                        builder.set_parent_ordinal(row_id, row.parent_ordinal);
                        builder.set_rev(row_id, row.rev);
                        ordinal_row.push((ordinal, row_id.0, row.parent_ordinal));
                    }
                    for (key, value) in table.resolve_fields(&row.fields) {
                        if key == "parent_ordinal" {
                            continue; // now a typed column
                        }
                        let _ = local_cols.insert(key.clone());
                        builder.set_field(row_id, &key, value);
                    }
                }

                // Nav post-pass: fill first_child, next_sibling, prev_sibling.
                // Group addressable rows by parent_ordinal, sort by ordinal (= DFS order).
                {
                    let mut by_parent: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
                    let mut ord_to_row: HashMap<u32, u32> = HashMap::new();
                    for &(ord, rid, parent) in &ordinal_row {
                        by_parent.entry(parent).or_default().push((ord, rid));
                        let _ = ord_to_row.insert(ord, rid);
                    }
                    for (parent_ord, mut children) in by_parent {
                        children.sort_unstable_by_key(|&(ord, _)| ord);
                        if let Some(&parent_rid) = ord_to_row.get(&parent_ord)
                            && let Some(&(first_ord, _)) = children.first()
                        {
                            builder.set_first_child_ordinal(RowId(parent_rid), first_ord);
                        }
                        for i in 0..children.len() {
                            let (_, this_rid) = children[i];
                            if i > 0 {
                                builder
                                    .set_prev_sibling_ordinal(RowId(this_rid), children[i - 1].0);
                            }
                            if i + 1 < children.len() {
                                builder
                                    .set_next_sibling_ordinal(RowId(this_rid), children[i + 1].0);
                            }
                        }
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use crate::ast::index::{IndexRow, SymbolTable};
    use crate::storage::columnar::SegmentReader;

    fn identity_hash(b: &[u8]) -> Vec<u8> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        b.hash(&mut h);
        h.finish().to_le_bytes().to_vec()
    }

    /// Regression for BUG-010. The inline build path must persist a node's
    /// `parent_ordinal` exactly like `ShadowWriter::run` and `reindex_files`.
    /// Before the fix it wrote only `ordinal`, leaving `parent_ordinal = MAX`
    /// (flat) in from-scratch segments; that mismatched the nested rows reindex
    /// produces, so the ordinal remapper failed every match and renumbered
    /// every node on the first reindex.
    #[test]
    fn inline_emit_persists_parent_ordinal() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let file_path = tmp.path().join("doc.md");

        // Two addressable rows for one file: a root section (ordinal 0, no
        // parent) and a nested heading (ordinal 1) whose parent is the section.
        let mut table = SymbolTable::default();
        let (n0, k0, f0, l0, p0) = table
            .strings
            .intern_row("Title", "section", "section", "markdown", &file_path);
        let fields0 = table.strings.intern_fields(HashMap::new());
        table.push_row(IndexRow {
            byte_range: 0..7,
            line: 1,
            usages_count: 0,
            ordinal: Some(0),
            parent_ordinal: u32::MAX,
            rev: 0,
            fields: fields0,
            name_id: n0,
            node_kind_id: k0,
            fql_kind_id: f0,
            language_id: l0,
            path_id: p0,
        });
        let (n1, k1, f1, l1, p1) = table
            .strings
            .intern_row("Section", "heading", "heading", "markdown", &file_path);
        let fields1 = table.strings.intern_fields(HashMap::new());
        table.push_row(IndexRow {
            byte_range: 9..19,
            line: 3,
            usages_count: 0,
            ordinal: Some(1),
            parent_ordinal: 0,
            rev: 0,
            fields: fields1,
            name_id: n1,
            node_kind_id: k1,
            fql_kind_id: f1,
            language_id: l1,
            path_id: p1,
        });

        let segments_dir = tmp.path().join("segments");
        let overlays_dir = tmp.path().join("overlays");
        let hash_fn: HashFn = Arc::new(|b: &[u8]| identity_hash(b));
        let cbc = ColumnarBuildContext::new(segments_dir.clone(), overlays_dir, "test", hash_fn);
        let (ctx, _state) = cbc.make_inline_ctx();

        let content_id = identity_hash(b"# Title\n\n## Section\n");
        (ctx.emit_fn)(&content_id, &table, 0);

        // Read the written segment back and confirm the heading's parent_ordinal
        // is the section's ordinal (0) — nested — not u32::MAX (flat).
        let provider_dir =
            segments_dir.join(format!("test-v{}", crate::storage::columnar::ENRICH_VER));
        let prefix_dir = std::fs::read_dir(&provider_dir)?
            .next()
            .ok_or_else(|| anyhow::anyhow!("no prefix shard dir"))??
            .path();
        let seg_path = std::fs::read_dir(&prefix_dir)?
            .next()
            .ok_or_else(|| anyhow::anyhow!("no segment file"))??
            .path();
        let reader = SegmentReader::open(&seg_path)?;

        let mut checked = false;
        for row in 0..reader.row_count {
            if reader.ordinal_of(row) == Some(1) {
                assert_eq!(
                    reader.parent_ordinal_of(row),
                    0,
                    "inline build must persist nested parent_ordinal (BUG-010)"
                );
                checked = true;
            }
        }
        assert!(checked, "heading row (ordinal 1) present in segment");
        Ok(())
    }
}
