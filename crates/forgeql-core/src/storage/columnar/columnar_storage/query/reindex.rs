//! Per-session file reindexing for [`ColumnarStorage`] (the `reindex_files` staging build).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use crate::ast::enrich::default_enrichers;
use crate::ast::index::{IndexContext, OrdinalHint, OrdinalRemapper, SymbolTable, index_file};
use crate::storage::columnar::bytes_to_hex;
use crate::storage::columnar::columnar_storage::ColumnarStorage;
use crate::storage::columnar::segment_builder::{
    RowId, SegmentBuilder, SymbolRow, is_valid_segment,
};
use crate::storage::columnar::segment_reader::SegmentReader;
use crate::storage::git_sha1_provider::git_blob_sha1;

impl ColumnarStorage {
    pub(super) fn reindex_files_impl(&mut self, paths: &[PathBuf]) -> Result<()> {
        // Run the per-file parse+enrich on the big-stack indexing pool: `index_file`
        // walks the AST recursively and a single deeply-nested edited file would
        // otherwise overflow rayon's default ~2 MiB stack. The full build already
        // does this (see `SymbolTable::indexing_pool`); reindex needs it too.
        SymbolTable::indexing_pool().install(|| self.reindex_files_on_pool(paths))
    }

    #[allow(clippy::too_many_lines)]
    fn reindex_files_on_pool(&mut self, paths: &[PathBuf]) -> Result<()> {
        std::fs::create_dir_all(&self.staging_dir)?;
        let mut parser = tree_sitter::Parser::new();
        let enrichers = default_enrichers();

        for path in paths {
            let rel_path = path
                .strip_prefix(&self.worktree_root)
                .unwrap_or(path)
                .to_path_buf();

            // Build ordinal hints from the most-recent version of this segment:
            // prefer an existing dirty entry (re-edit within a transaction) over
            // the committed segment (first edit).  This keeps node_ids stable
            // across every reindex — including the one triggered by COMMIT MESSAGE
            // when dirty segments are promoted to committed.
            let hints = self.build_ordinal_hints(&rel_path);
            if crate::debug_log::is_enabled() {
                let base = hints
                    .iter()
                    .map(|h| h.ordinal)
                    .max()
                    .map_or(0, |m| m.saturating_add(1));
                crate::debug_log!(
                    "reindex file={} prior_hints={} next_ordinal_base={}",
                    rel_path.display(),
                    hints.len(),
                    base
                );
            }
            let remapper = OrdinalRemapper::from_previous(hints);

            // Shadow the persistent segment and evict any stale dirty entry
            // AFTER capturing hints so the remapper references valid data.
            let old_hex = self.path_to_hex_content_id(&rel_path).unwrap_or_default();
            if !old_hex.is_empty() {
                self.dirty.remove_hex(old_hex.clone());
            }
            drop(self.dirty.remove_stale_for_path(&rel_path));

            if !path.exists() {
                continue;
            }

            let Some(lang) = self.lang_registry.language_for_path(path) else {
                continue;
            };

            let bytes = std::fs::read(path)?;
            let content_id_bytes = git_blob_sha1(&bytes);
            let hex_content_id = bytes_to_hex(&content_id_bytes);

            let seg_path = self.staging_dir.join(format!("{hex_content_id}.fqsf"));

            if !is_valid_segment(&seg_path) {
                parser
                    .set_language(&lang.tree_sitter_language())
                    .map_err(|e| anyhow::anyhow!("tree-sitter language error: {e}"))?;

                let mut table = SymbolTable::default();
                {
                    let mut ctx = IndexContext {
                        path,
                        language: lang.as_ref(),
                        enrichers: &enrichers,
                        macro_table: None,
                        ordinal_remapper: Some(remapper),
                        table: &mut table,
                    };
                    let _ = index_file(&mut parser, &mut ctx, None);
                }
                for enricher in &enrichers {
                    enricher.post_pass(&mut table, None);
                }

                let mut builder = SegmentBuilder::new("git-sha1", &content_id_bytes);

                populate_builder(&mut builder, &table);

                builder.flush(&seg_path)?;
            }

            let seg_reader = SegmentReader::open(&seg_path)?;
            self.dirty
                .add_segment(Arc::new(seg_reader), rel_path, old_hex);
        }
        self.save_delta()?;
        Ok(())
    }
}

impl ColumnarStorage {
    /// Build ordinal hints for `rel_path` from the most-recent version of its
    /// segment — preferring a dirty entry (re-edit within a transaction) over
    /// the committed segment (first edit). Keeps node_ids stable across every
    /// reindex, including the one COMMIT triggers when dirty segments promote.
    fn build_ordinal_hints(&self, rel_path: &std::path::Path) -> Vec<OrdinalHint> {
        let seg: Option<&SegmentReader> = self
            .dirty
            .added
            .iter()
            .find(|ds| ds.source_path == *rel_path)
            .map(|ds| ds.reader.as_ref())
            .or_else(|| {
                self.overlay
                    .segments()
                    .iter()
                    .enumerate()
                    .find(|(_, m)| m.source_path == *rel_path)
                    .and_then(|(idx, _)| self.segments.get(idx).map(Arc::as_ref))
            });
        seg.map_or_else(Vec::new, |seg| {
            (0..seg.row_count)
                .filter_map(|row| {
                    let ordinal = seg.ordinal_of(row)?;
                    Some(OrdinalHint {
                        name: seg.name_of(row).to_owned(),
                        fql_kind: seg.fql_kind_of(row).to_owned(),
                        parent_ordinal: seg.parent_ordinal_of(row),
                        guard_group_id: seg
                            .extra_field_str("guard_group_id", row)
                            .map(str::to_owned),
                        guard_branch: seg.extra_field_str("guard_branch", row).map(str::to_owned),
                        first_body_statement_fingerprint: seg
                            .extra_field_str("first_body_statement_fingerprint", row)
                            .map(str::to_owned),
                        content_hash: seg.extra_field_str("content_hash", row).map(str::to_owned),
                        ordinal,
                    })
                })
                .collect()
        })
    }
}

/// Emit every row of `table` into `builder`, then run the navigation post-pass
/// (first_child / prev_sibling / next_sibling, grouped by parent ordinal and
/// ordered by ordinal = DFS order) so the reindexed segment carries the same
/// navigation + identity data as the initial shadow-writer build.
fn populate_builder(builder: &mut SegmentBuilder, table: &SymbolTable) {
    let mut ordinal_row: Vec<(u32, u32, u32)> = Vec::new();
    for row in &table.rows {
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
        for (key, val) in table.resolve_fields(&row.fields) {
            if key == "parent_ordinal" {
                continue;
            }
            builder.set_field(row_id, &key, val.as_str());
        }
    }

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
                builder.set_prev_sibling_ordinal(RowId(this_rid), children[i - 1].0);
            }
            if i + 1 < children.len() {
                builder.set_next_sibling_ordinal(RowId(this_rid), children[i + 1].0);
            }
        }
    }
}
