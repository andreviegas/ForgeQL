//! Core query execution for [`super::ColumnarStorage`].
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::ast::enrich::default_enrichers;
use crate::ast::index::{
    IndexContext, IndexStats, OrdinalHint, OrdinalRemapper, SymbolTable, index_file,
};
use crate::ir::Clauses;
use crate::result::{FindNodeResult, SymbolMatch};
use crate::workspace::Workspace;

use super::super::bytes_to_hex;
use super::super::segment_builder::{RowId, SegmentBuilder, SymbolRow, is_valid_segment};
use super::super::segment_reader::SegmentReader;
use super::ColumnarStorage;
use crate::storage::git_sha1_provider::git_blob_sha1;
use crate::storage::{StorageEngine, SymbolLocation};

mod find;

mod resolve;

mod node_lookup;

mod outline;
// ─────────────────────────────────────────────────────────────────────────────
// StorageEngine implementation
// ─────────────────────────────────────────────────────────────────────────────

impl StorageEngine for ColumnarStorage {
    fn backend_name(&self) -> &'static str {
        "columnar"
    }

    fn find_node(&self, node_id: &str, root: &Path) -> Result<Option<FindNodeResult>> {
        self.find_node_impl(node_id, root)
    }

    fn find_node_id_at_line(&self, rel_path: &str, line: usize) -> Option<String> {
        self.find_node_id_at_line_impl(rel_path, line)
    }

    fn innermost_nodes_for_lines(
        &self,
        rel_path: &str,
        root: &Path,
        start: usize,
        end: usize,
    ) -> Vec<Option<(String, usize)>> {
        self.innermost_nodes_for_lines_impl(rel_path, root, start, end)
    }

    fn find_symbols(&self, clauses: &Clauses, root: &Path) -> Result<Vec<SymbolMatch>> {
        Ok(self.find_symbols_impl(clauses, root))
    }

    fn find_usages(&self, name: &str, clauses: &Clauses, root: &Path) -> Result<Vec<SymbolMatch>> {
        Ok(self.find_usages_impl(name, clauses, root))
    }

    fn indexed_files(&self) -> Option<Vec<crate::result::FileEntry>> {
        Some(self.indexed_files_impl())
    }

    fn resolve_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        Ok(self.resolve_impl(name, clauses, root, None))
    }

    fn resolve_type_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        // Prefer struct / class / enum / union / type_alias / trait / interface rows
        // that have members — mirrors the legacy resolver's type-preference scan.
        const TYPE_KINDS: &[&str] = &[
            "class",
            "struct",
            "enum",
            "union",
            "type_alias",
            "trait",
            "interface",
        ];
        Ok(self.resolve_impl(name, clauses, root, Some(TYPE_KINDS)))
    }

    fn resolve_body_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>> {
        // Only consider function-like kinds for SHOW body.  Using the preference
        // list still allows the fallback to any row when the function-kind rows
        // are filtered out by clauses, but we check the returned kind below so
        // that non-function symbols produce an actionable error instead of
        // silently resolving to whatever enclosing function happens to contain
        // the last type reference.
        const BODY_KINDS: &[&str] = &["function", "method", "constructor", "destructor"]; // macros excluded: C preprocessor macros have no function_definition in the AST

        let Some(loc) = self.resolve_impl(name, clauses, root, Some(BODY_KINDS)) else {
            return Ok(None);
        };

        // `resolve_impl` uses BODY_KINDS as a *preference*, not a hard filter:
        // if no function-kind rows exist it falls back to the last row with any
        // non-empty fql_kind.  Guard against that by checking the resolved kind.
        // Exception: allow member declarations (fql_kind outside BODY_KINDS)
        // that carry a `body_symbol` redirect — they are C++ method stubs
        // created by MemberEnricher and should follow the redirect below.
        if !BODY_KINDS.contains(&loc.node_kind.as_str())
            && !loc.enrichment.contains_key("body_symbol")
        {
            anyhow::bail!(
                "'{name}' is not a function (found fql_kind: [{}]). \
                 Use FIND symbols WHERE name = '{name}' to locate the definition, \
                 then SHOW LINES n-m OF 'file' to read it.",
                loc.node_kind
            );
        }

        // Follow the `body_symbol` redirect for C++ out-of-line definitions.
        // The redirect is resolved without user clauses — matches legacy behaviour.
        if let Some(target) = loc.enrichment.get("body_symbol").cloned()
            && let Some(redirected) =
                self.resolve_impl(&target, &Clauses::default(), root, Some(BODY_KINDS))
        {
            return Ok(Some(redirected));
        }
        Ok(Some(loc))
    }

    fn index_stats(&self) -> Option<&IndexStats> {
        Some(&self.stats)
    }

    fn locate_definition(&self, name: &str) -> Option<(PathBuf, usize)> {
        self.resolve_impl(
            name,
            &crate::ir::Clauses::default(),
            &self.worktree_root,
            None,
        )
        .map(|loc| (loc.path, loc.line))
    }

    fn build(&mut self, _workspace: &Workspace) -> Result<()> {
        // The columnar backend is populated by ShadowWriter during the legacy
        // build; it does not expose its own independent build path.
        Err(anyhow::anyhow!(
            "ColumnarStorage::build is not callable directly; \
             use shadow_write via LegacyMemoryStorage"
        ))
    }

    #[allow(clippy::too_many_lines)]
    fn reindex_files(&mut self, paths: &[PathBuf]) -> Result<()> {
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
            let hints: Vec<OrdinalHint> = {
                let seg: Option<&SegmentReader> = self
                    .dirty
                    .added
                    .iter()
                    .find(|ds| ds.source_path == rel_path)
                    .map(|ds| ds.reader.as_ref())
                    .or_else(|| {
                        self.overlay
                            .segments()
                            .iter()
                            .enumerate()
                            .find(|(_, m)| m.source_path == rel_path)
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
                                guard_branch: seg
                                    .extra_field_str("guard_branch", row)
                                    .map(str::to_owned),
                                first_body_statement_fingerprint: seg
                                    .extra_field_str("first_body_statement_fingerprint", row)
                                    .map(str::to_owned),
                                content_hash: seg
                                    .extra_field_str("content_hash", row)
                                    .map(str::to_owned),
                                ordinal,
                            })
                        })
                        .collect()
                })
            };
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
                // Nav post-pass: fill first_child/next_sibling/prev_sibling and the
                // typed parent_ordinal/rev columns so reindexed segments carry the
                // same navigation + identity data as the initial shadow_writer build.
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

                builder.flush(&seg_path)?;
            }

            let seg_reader = SegmentReader::open(&seg_path)?;
            self.dirty
                .add_segment(Arc::new(seg_reader), rel_path, old_hex);
        }
        self.save_delta()?;
        Ok(())
    }

    fn is_path_fresh(&self, rel_path: &Path, _root: &Path) -> bool {
        // A dirty segment for this path was built from the current file on
        // disk, so its line/byte data is authoritative — always fresh.
        if self.dirty.added.iter().any(|ds| ds.source_path == rel_path) {
            return true;
        }
        // No committed segment → nothing indexed for this path; there is no
        // stale absolute line data to serve.
        let Some(stored_hex) = self.path_to_hex_content_id(rel_path) else {
            return true;
        };
        // Compare the committed segment's content hash against the live file.
        // The committed overlay is git-sha1 content-addressed (see reindex_files
        // and the shadow-write hash_fn), so any divergence — HEAD advanced past
        // the cached overlay, a file reverted while git-clean, or an edit made
        // outside ForgeQL — surfaces here as a hash mismatch.
        let abs = self.worktree_root.join(rel_path);
        let Ok(bytes) = std::fs::read(&abs) else {
            // Unreadable (e.g. deleted): don't force a reindex loop — let the
            // normal mutation/query path surface the I/O error.
            return true;
        };
        bytes_to_hex(&git_blob_sha1(&bytes)) == stored_hex
    }

    fn purge_file(&mut self, path: &Path) -> Result<()> {
        let rel_path = path
            .strip_prefix(&self.worktree_root)
            .unwrap_or(path)
            .to_path_buf();
        if let Some(old_hex) = self.path_to_hex_content_id(&rel_path) {
            self.dirty.remove_hex(old_hex);
        }
        drop(self.dirty.remove_stale_for_path(&rel_path));
        self.save_delta()?;
        Ok(())
    }

    fn persist_to_cache(
        &mut self,
        _worktree_path: &Path,
        _commit_hash: &str,
        _source_name: &str,
    ) -> Result<()> {
        // Overlays are already on disk — no separate cache step needed.
        Ok(())
    }

    fn load_from_cache(
        &mut self,
        _worktree_path: &Path,
        _head_oid: &str,
        _source_name: &str,
    ) -> Result<bool> {
        // The overlay is opened by `use_source`; not loaded here.
        Ok(false)
    }

    fn drop_stored_index(&mut self) {
        // Nothing to drop — the overlay is on disk.
    }

    fn has_index(&self) -> bool {
        true
    }

    fn flush_delta(&mut self) -> Result<()> {
        self.save_delta()
    }

    fn reload_dirty_from_delta(&mut self) -> Result<()> {
        // Always GC first — safe for both ROLLBACK (removes orphaned staging
        // dirs from after the checkpoint) and reconnect (no-op when delta+staging
        // are already in sync).
        self.reload_delta_after_rollback()
    }

    fn commit_dirty(
        &mut self,
        new_commit_oid: &str,
        ctx: &crate::storage::ColumnarBuildContext,
    ) -> Result<()> {
        // Delegate to the inherent method which has access to all private fields.
        Self::commit_dirty_inner(self, new_commit_oid, ctx)
    }

    fn show_outline_for_file(
        &self,
        workspace: &Workspace,
        file: &str,
        all: bool,
    ) -> Result<serde_json::Value> {
        Ok(self.show_outline_for_file_impl(workspace, file, all))
    }
}
