//! Core query execution for [`super::ColumnarStorage`].
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::ast::index::IndexStats;
use crate::ir::Clauses;
use crate::result::{FindNodeResult, SymbolMatch};
use crate::workspace::Workspace;

use super::super::bytes_to_hex;
use super::ColumnarStorage;
use crate::storage::git_sha1_provider::git_blob_sha1;
use crate::storage::{StorageEngine, SymbolLocation};

mod reindex;

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

    fn find_node_id_at_byte(&self, rel_path: &str, byte: usize) -> Option<String> {
        self.find_node_id_at_byte_impl(rel_path, byte)
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
        self.find_symbols_impl(clauses, root)
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

    fn reindex_files(&mut self, paths: &[PathBuf]) -> Result<()> {
        self.reindex_files_impl(paths)
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
