#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
//! Storage engine abstraction for ForgeQL.
//!
//! This module defines the [`StorageEngine`] trait — a MySQL-handler–style
//! abstraction that decouples all `exec_*` query paths from the concrete
//! `SymbolTable` type. Every backend (legacy in-RAM, future columnar disk
//! store) implements this trait.
//!
//! Also contains the [`SourceProvider`] trait (see [`source_provider`]) that
//! decouples the storage engine from git internals.
//!
//! # Phase 05.4 scope
//!
//! In this phase:
//! - [`LegacyMemoryStorage`] wraps the existing `SymbolTable` and is the only
//!   active backend. All live queries are served by it.
//! - [`StubColumnarStorage`] is a throwaway empty implementation used to validate
//!   the trait shape compiles for a non-legacy backend.
//! - `SHOW` paths reach the legacy table via `Session::index()` which calls
//!   `BackendSet::legacy_storage()`. The `StorageEngine` trait contains no
//!   legacy-specific escape hatches as of Phase 05.4.

pub mod backend_set;
pub mod columnar;
pub mod git_sha1_provider;
pub mod legacy;
pub mod mock_provider;
pub mod source_provider;
pub mod stub;

/// Whole-path handles: `n<hex>` with no ordinal addresses a **file or
/// directory**, not a node inside one.
///
/// This is deliberately backend-independent. A path handle is the fingerprint of
/// a path plus what is on disk — there is no index in it, nothing is stored, and
/// so no `ENRICH_VER` bump is possible or needed. Every backend answers these,
/// and a backend with an index (the columnar one) only uses its catalogs to skip
/// the worktree walk.
pub mod path_node {
    use anyhow::{Result, anyhow};
    use std::path::{Path, PathBuf};

    use crate::result::FindNodeResult;

    /// Minimum hex chars after `n`. Matches `shortest_prefix_len`'s floor:
    /// below it, an ordinary all-hex symbol name (`nadd`, `nbeef`) would parse
    /// as a file handle wherever a name and a node_id are both accepted.
    const MIN_HEX: usize = 12;

    /// The `<hex>` of a bare handle — `None` when the id carries an ordinal and
    /// so addresses a node inside a file.
    #[must_use]
    pub fn bare_hex(node_id: &str) -> Option<&str> {
        let stripped = node_id.strip_prefix('n')?;
        if stripped.contains('.') {
            return None;
        }
        Some(stripped)
    }

    /// Is this string a whole-path handle, as opposed to a path?
    ///
    /// Stricter than [`bare_hex`], which only asks "no ordinal?" of something
    /// already known to be a node id. This one is asked of an argument that
    /// could be *either* — a `TO` destination — so `notes/` must not read as a
    /// handle merely because it starts with `n`.
    #[must_use]
    pub fn is_handle(value: &str) -> bool {
        bare_hex(value).is_some_and(|hex| {
            hex.len() >= MIN_HEX
                && hex.len() <= 64
                && hex.len().is_multiple_of(2)
                && hex.bytes().all(|b| b.is_ascii_hexdigit())
        })
    }

    /// Normalize and check a bare hex, or say why it is not a handle.
    pub fn validate_hex(node_id: &str, hex: &str) -> Result<String> {
        let hex = hex.to_ascii_lowercase();
        if hex.len() < MIN_HEX
            || hex.len() > 64
            || !hex.len().is_multiple_of(2)
            || !hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(anyhow!("invalid node_id format: {node_id}"));
        }
        Ok(hex)
    }

    /// Does `rel` fingerprint to something starting with `hex`?
    #[must_use]
    pub fn path_matches_hex(rel: &Path, hex: &str) -> bool {
        let full =
            crate::node_id::hex_prefix(&crate::node_id::sha256_of_path(&rel.to_string_lossy()), 64);
        full.starts_with(hex)
    }

    /// Every file in the worktree, workspace-relative — the same membership
    /// `FIND files` reports, so a directory rev folds exactly the files an agent
    /// can see listed.
    #[must_use]
    pub fn worktree_files(root: &Path) -> Vec<PathBuf> {
        let Ok(workspace) = crate::workspace::Workspace::new(root) else {
            return Vec::new();
        };
        workspace
            .files()
            .filter(|p| !crate::result::FileEntry::is_runtime_artifact(p))
            .map(|abs| abs.strip_prefix(root).unwrap_or(&abs).to_path_buf())
            .collect()
    }

    /// Resolve a bare handle against the worktree itself.
    ///
    /// This is the only place a directory can be found (no catalog lists them,
    /// and an empty one is implied by no file path), and it is also where a file
    /// created this session — before the overlay was rebuilt — turns up.
    pub fn resolve_in_worktree(node_id: &str, hex: &str, root: &Path) -> Result<FindNodeResult> {
        let files = worktree_files(root);
        let mut hits: Vec<(PathBuf, bool)> = files
            .iter()
            .filter(|p| path_matches_hex(p, hex))
            .map(|p| (p.clone(), false))
            .collect();

        if let Ok(workspace) = crate::workspace::Workspace::new(root) {
            hits.extend(
                workspace
                    .dirs()
                    .into_iter()
                    .map(|abs| abs.strip_prefix(root).unwrap_or(&abs).to_path_buf())
                    .filter(|p| path_matches_hex(p, hex))
                    .map(|p| (p, true)),
            );
        }
        hits.sort();
        hits.dedup();

        match hits.len() {
            0 => Err(anyhow!("node_id not found: {node_id}")),
            1 => {
                let (rel, is_dir) = &hits[0];
                if *is_dir {
                    Ok(dir_node(node_id, rel, root, &files))
                } else {
                    file_node(node_id, rel, root)
                }
            }
            // Never guess: the caller may be about to delete it.
            n => Err(anyhow!(
                "ambiguous node_id {node_id}: prefix matches {n} paths: {}",
                hits.iter()
                    .map(|(p, _)| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// Lines in a byte buffer: a trailing newline does not open a new line.
    fn count_lines(bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }
        // split() yields runs between newlines: run count - 1 == newline count.
        let newlines = bytes.split(|&b| b == b'\n').count() - 1;
        if bytes.last() == Some(&b'\n') {
            newlines
        } else {
            newlines + 1
        }
    }

    /// Synthesize the node for a whole file.
    pub fn file_node(node_id: &str, rel: &Path, root: &Path) -> Result<FindNodeResult> {
        let abs = root.join(rel);
        let bytes = std::fs::read(&abs).map_err(|e| {
            anyhow!(
                "node_id {node_id} resolves to {} which cannot be read: {e}",
                rel.display()
            )
        })?;
        Ok(FindNodeResult {
            node_id: node_id.to_owned(),
            fql_kind: "file".to_owned(),
            name: rel
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            path: abs,
            line: 1,
            // An empty file still spans line 1: INSERT BEFORE/AFTER needs a line
            // to land against, and that is the create-then-write bootstrap.
            end_line: count_lines(&bytes).max(1),
            rev: crate::node_id::format_rev(crate::node_id::rev_of_bytes(&bytes)),
            parent_node_id: None,
            first_child_node_id: None,
            next_sibling_node_id: None,
            prev_sibling_node_id: None,
        })
    }

    /// Synthesize the node for a whole directory.
    ///
    /// A directory has no bytes, so its rev is a membership XOR over the paths
    /// of every file underneath it: it moves when a file is added, removed,
    /// renamed or moved anywhere in the subtree, and deliberately does not move
    /// when file content changes. That is what a recursive delete has to be
    /// gated on — that the agent saw the current membership, not that it read
    /// every byte. (Content staleness is the per-file rev's job.)
    #[must_use]
    pub fn dir_node(node_id: &str, rel: &Path, root: &Path, files: &[PathBuf]) -> FindNodeResult {
        let rev = files
            .iter()
            .filter(|f| f.starts_with(rel))
            .fold(0u64, |acc, f| {
                crate::node_id::fold_path_rev(acc, &f.to_string_lossy())
            });
        FindNodeResult {
            node_id: node_id.to_owned(),
            fql_kind: "dir".to_owned(),
            name: format!(
                "{}/",
                rel.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            path: root.join(rel),
            line: 1,
            // A directory spans no lines. `offset_lines` refuses an `(n-m)`
            // suffix on it rather than underflowing.
            end_line: 0,
            rev: crate::node_id::format_rev_exact(rev),
            parent_node_id: None,
            first_child_node_id: None,
            next_sibling_node_id: None,
            prev_sibling_node_id: None,
        }
    }
}

pub use backend_set::BackendSet;
pub use columnar::overlay::Overlay;
pub use columnar::shadow_writer::ShadowWriteResult;
pub use columnar::{
    ColumnarBuildContext, ColumnarStorage, HashFn, OverlayBuilder, SegmentReader, ShadowWriter,
};
pub use legacy::LegacyMemoryStorage;
pub use source_provider::SourceProvider;
pub use stub::StubColumnarStorage;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::ast::index::{IndexRow, IndexStats, SymbolTable};
use crate::ir::Clauses;
use crate::result::{FileEntry, FindNodeResult, SymbolMatch};
use crate::workspace::Workspace;

// -----------------------------------------------------------------------
// SymbolLocation — lightweight symbol reference for SHOW resolution
// -----------------------------------------------------------------------

/// Identifies the on-disk location of a single symbol definition.
///
/// Returned by [`StorageEngine::resolve_symbol`] and its variants.
/// Contains enough information to re-read and re-parse the source file
/// without retaining any reference into the storage backend.
///
/// The `exec_show` path obtains a `SymbolLocation`, reads the source bytes
/// from disk, and feeds the row info into the tree-sitter re-parser.
#[derive(Debug, Clone)]
pub struct SymbolLocation {
    /// Absolute path to the source file containing the symbol.
    pub path: PathBuf,
    /// Byte range of the symbol node within the source file.
    pub byte_range: std::ops::Range<usize>,
    /// 1-based start line number of the symbol.
    pub line: usize,
    /// Interned language ID (backend-specific).
    pub language_id: u32,
    /// Raw tree-sitter node kind (e.g. `"function_definition"`).
    /// Used by `show_signature` to determine whether to look for a body node.
    pub node_kind: String,
    /// Pre-resolved enrichment fields for this symbol.
    /// Populated by `row_to_location`; empty for non-legacy backends.
    pub enrichment: HashMap<String, String>,
    /// Content SHA-1 of the source file at resolve time, when known.
    ///
    /// Populated by the columnar backend from `SegmentMeta::content_id`.
    /// The legacy backend always leaves this as `None`.
    /// When `Some`, `get_or_parse_for_show` can skip `read_bytes` on a cache
    /// Content SHA-1 of the source file at resolve time, when known.
    ///
    /// Populated by the columnar backend from `SegmentMeta::content_id`.
    /// The legacy backend always leaves this as `None`.
    /// When `Some`, `get_or_parse_for_show` can skip `read_bytes` on a cache
    /// hit and skip `sha1_of_bytes` on a miss.
    pub blob_sha: Option<[u8; 20]>,
    /// Per-file DFS ordinal used to build `node_id` handles.
    ///
    /// `None` for legacy segments and rows without an assigned ordinal.
    pub ordinal: Option<u32>,
}

// -----------------------------------------------------------------------
// -----------------------------------------------------------------------
// StorageEngine trait
// -----------------------------------------------------------------------

/// The central abstraction over all ForgeQL storage backends.
///
/// All `exec_*` query paths go through this trait. The concrete
/// [`LegacyMemoryStorage`] is the default implementation for Phase 01;
/// a columnar disk-backed engine will be added in later phases.
///
/// Implementors must be `Send + Sync` so sessions can be held in a
/// `HashMap` inside `Arc<Mutex<ForgeQLEngine>>`.
pub trait StorageEngine: Send + Sync + 'static {
    /// Short identifier for the backend, e.g. `"legacy"`, `"columnar"`.
    fn backend_name(&self) -> &'static str;

    // -------- read-only queries ----------------------------------------

    /// Execute a `FIND symbols` query.
    ///
    /// Applies all fast-path index shortcuts, predicate evaluation, ORDER BY,
    /// and GROUP BY internally. The caller is responsible for DEFAULT_QUERY_LIMIT
    /// truncation and result formatting.
    ///
    /// Returns the full result set (no truncation).
    fn find_symbols(&self, clauses: &Clauses, root: &Path) -> Result<Vec<SymbolMatch>>;

    /// Execute a `FIND usages OF 'name'` query.
    ///
    /// Applies glob filtering and the remaining clause pipeline internally.
    /// Returns the full result set (no truncation).
    fn find_usages(&self, name: &str, clauses: &Clauses, root: &Path) -> Result<Vec<SymbolMatch>>;

    /// Execute a FIND NODE id query.
    ///
    /// Resolves a node_id to its current location, rev, and nav links.
    /// Returns `None` when the node cannot be matched (deleted or renamed).
    fn find_node(&self, node_id: &str, root: &Path) -> Result<Option<FindNodeResult>> {
        // A bare `n<hex>` handle addresses a whole file or directory. That needs
        // no index — only the path fingerprint and the worktree — so it is
        // answered here, for every backend, rather than in one of them. A
        // backend that does have catalogs (columnar) overrides this to skip the
        // walk; the answer is the same either way.
        if let Some(hex) = path_node::bare_hex(node_id) {
            let hex = path_node::validate_hex(node_id, hex)?;
            return path_node::resolve_in_worktree(node_id, &hex, root).map(Some);
        }
        Ok(None)
    }

    /// Find the node_id of the first symbol that starts at the given source line.
    ///
    /// Used to locate newly inserted symbols after `INSERT BEFORE|AFTER NODE`.
    /// Returns `None` when no addressable symbol starts at that line, or when
    /// this backend does not maintain a columnar index.
    fn find_node_id_at_line(&self, rel_path: &str, line: usize) -> Option<String> {
        let _ = (rel_path, line);
        None
    }

    /// The innermost indexed node whose byte span contains `byte`.
    ///
    /// `SHOW members` reads its rows from the AST, not the index, and the two do
    /// not agree on a node's *line*: the indexed `field` node starts at the
    /// attribute or doc line above the declaration. Containment is the relation
    /// that actually holds between them — the member's first byte lies inside the
    /// indexed node — so handles are attached by span, not by a fuzzy line match.
    ///
    /// Backends without byte spans return `None`; the row then simply carries no
    /// handle, exactly as before.
    fn find_node_id_at_byte(&self, rel_path: &str, byte: usize) -> Option<String> {
        let _ = (rel_path, byte);
        None
    }

    /// For each 1-based source line in `start..=end`, the innermost indexed node
    /// that *contains* it, as `(node_id, node_start_line)` (`None` for a line
    /// covered by no indexed node). An empty `Vec` means this backend keeps no
    /// usable columnar index, so callers fall back to absolute line numbers.
    /// Drives `SHOW LINES` node-relative offset rendering, where a line's offset
    /// is `line - node_start_line + 1`.
    fn innermost_nodes_for_lines(
        &self,
        rel_path: &str,
        root: &Path,
        start: usize,
        end: usize,
    ) -> Vec<Option<(String, usize)>> {
        let _ = (rel_path, root, start, end);
        Vec::new()
    }

    /// Whether the indexed segment for `rel_path` still matches the file on
    /// disk (content-addressed freshness check).
    ///
    /// Returns `true` when this backend keeps no content-addressed index — it
    /// has no stale absolute line data to serve — or when the stored segment
    /// hash equals the live file's hash. A `false` result tells the caller to
    /// reindex `rel_path` before trusting any line/byte offset for it, which is
    /// what stops a stale committed segment from corrupting a file on
    /// `CHANGE NODE` (BUG-001) or misresolving `FIND NODE` (BUG-002).
    fn is_path_fresh(&self, _rel_path: &Path, _root: &Path) -> bool {
        true
    }

    /// Return all indexed source files as typed [`FileEntry`] rows.
    ///
    /// When `Some` is returned, `FIND files` skips the filesystem walk and
    /// feeds the entries directly into `filter::apply_clauses`.  The paths
    /// are **relative** to the worktree root; the `depth` field is
    /// pre-populated from `path.components().count()`.
    ///
    /// Returns `None` when this backend does not maintain an indexed file
    /// list — the caller falls back to a workspace filesystem walk.
    fn indexed_files(&self) -> Option<Vec<FileEntry>> {
        None
    }

    // -------- symbol resolution (for SHOW) --------------------------------

    /// Resolve a symbol name to its on-disk location.
    ///
    /// Applies `IN`/`EXCLUDE` and `WHERE` clauses to disambiguate when
    /// multiple candidates exist. Returns `Ok(None)` when the symbol is not
    /// found (the caller may emit a friendly error).
    fn resolve_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>>;

    /// Like [`resolve_symbol`] but prefers type definitions with members.
    fn resolve_type_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>>;

    /// Like [`resolve_symbol`] but follows the `body_symbol` redirect field.
    fn resolve_body_symbol(
        &self,
        name: &str,
        clauses: &Clauses,
        root: &Path,
    ) -> Result<Option<SymbolLocation>>;

    // -------- aggregates --------------------------------------------------

    /// Return a reference to the pre-aggregated [`IndexStats`], if the index
    /// has been built.
    fn index_stats(&self) -> Option<&IndexStats>;

    // -------- lifecycle ---------------------------------------------------

    /// Build a fresh index from all files in `workspace`.
    ///
    /// After a successful call `has_index()` returns `true`.
    fn build(&mut self, workspace: &Workspace) -> Result<()>;

    /// Incrementally re-index the given paths after a mutation.
    ///
    /// Deleted files are purged; modified files are re-parsed.
    fn reindex_files(&mut self, paths: &[PathBuf]) -> Result<()>;

    /// Remove all rows originating from a single source file.
    fn purge_file(&mut self, path: &Path) -> Result<()>;

    /// Persist the in-memory index to `<worktree_path>/.forgeql-index`.
    ///
    /// `commit_hash` and `source_name` are stored in the cache header so
    /// that `load_from_cache` can validate freshness on the next resume.
    fn persist_to_cache(
        &mut self,
        worktree_path: &Path,
        commit_hash: &str,
        source_name: &str,
    ) -> Result<()>;

    /// Attempt to load the index from `<worktree_path>/.forgeql-index`.
    ///
    /// Returns `true` on a cache hit (the cached commit matches `head_oid`
    /// and the source name matches), `false` when the cache is absent or
    /// stale (caller should call `build` instead).
    fn load_from_cache(
        &mut self,
        worktree_path: &Path,
        head_oid: &str,
        source_name: &str,
    ) -> Result<bool>;

    /// Drop the in-memory index without saving.
    ///
    /// Used by `ROLLBACK` so the next `resume_index` reads the freshly
    /// restored `.forgeql-index` from disk.
    fn drop_stored_index(&mut self);

    /// Return `true` when an index has been built or loaded from cache.
    /// Return `true` when an index has been built or loaded from cache.
    fn has_index(&self) -> bool;

    /// Flush the dirty overlay to the on-disk `.forgeql-columnar-delta` file.
    ///
    /// Called by `BEGIN TRANSACTION` before `git::stage_and_commit` so the
    /// checkpoint snapshot includes an up-to-date delta file.
    ///
    /// The default no-op is correct for the legacy backend.  `ColumnarStorage`
    /// overrides this to call `DeltaFile::save`.
    fn flush_delta(&mut self) -> Result<()> {
        Ok(())
    }

    /// Reload the dirty overlay from the on-disk `.forgeql-columnar-delta` file.
    ///
    /// Called after `ROLLBACK` (the delta is restored by `git reset --hard`)
    /// and on session reconnect (via `warm_or_open`).
    ///
    /// The default no-op is correct for the legacy backend.  `ColumnarStorage`
    /// overrides this to call `DeltaFile::load`.
    fn reload_dirty_from_delta(&mut self) -> Result<()> {
        Ok(())
    }

    /// Promote staging segments and build a new overlay for `new_commit_oid`.
    ///
    /// Called by `exec_commit` after the git commit succeeds.  The default
    /// no-op is correct for the legacy backend.  `ColumnarStorage` overrides
    /// this to promote staged segments to the bare-repo store and rebuild the
    /// overlay via `OverlayBuilder::from_merge`.
    fn commit_dirty(&mut self, _new_commit_oid: &str, _ctx: &ColumnarBuildContext) -> Result<()> {
        Ok(())
    }
    // -------- SHOW helpers ------------------------------------------------

    /// Locate a symbol definition by name, returning its file path and line.
    ///
    /// Used by `show_callees` to annotate each callee with its definition
    /// location. Returns `None` when the name is not found or the backend
    /// does not support definition lookup.
    fn locate_definition(&self, _name: &str) -> Option<(PathBuf, usize)> {
        None
    }

    /// Render `SHOW outline OF file` as a JSON value.
    ///
    /// Delegates to the backend symbol rows so `exec_show` does not need to
    /// hold a `&SymbolTable` reference and can work across all backends.
    /// `all = false` returns only structural declarations (functions, types,
    /// namespaces, …); `all = true` returns every node. A `node_id` passed as
    /// `file` scopes the outline to that node's subtree.
    fn show_outline_for_file(
        &self,
        workspace: &Workspace,
        file: &str,
        all: bool,
    ) -> Result<serde_json::Value>;
}

pub(crate) fn row_to_location(row: &IndexRow, table: &SymbolTable) -> SymbolLocation {
    SymbolLocation {
        path: table.path_of(row).to_path_buf(),
        byte_range: row.byte_range.clone(),
        line: row.line,
        language_id: row.language_id,
        node_kind: table.node_kind_of(row).to_string(),
        enrichment: table.strings.resolve_fields(&row.fields),
        blob_sha: None,
        ordinal: row.ordinal,
    }
}
// -----------------------------------------------------------------------
// Phase 01 integration tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{StorageEngine, SymbolLocation};
    use crate::{
        ir::Clauses,
        storage::{
            mock_provider::MockProvider,
            source_provider::{ContentId, SourceProvider},
            stub::StubColumnarStorage,
        },
    };

    // --- StubColumnarStorage (trait shape) ---

    #[test]
    fn stub_backend_name() {
        let s = StubColumnarStorage;
        assert_eq!(s.backend_name(), "stub");
    }

    #[test]
    fn stub_has_no_index() {
        let s = StubColumnarStorage;
        assert!(!s.has_index());
    }

    #[test]
    fn stub_find_symbols_returns_empty() {
        let s = StubColumnarStorage;
        let clauses = Clauses::default();
        let root = Path::new("/tmp");
        let results = s.find_symbols(&clauses, root).expect("should not error");
        assert!(results.is_empty());
    }

    #[test]
    fn stub_find_usages_returns_empty() {
        let s = StubColumnarStorage;
        let clauses = Clauses::default();
        let root = Path::new("/tmp");
        let results = s
            .find_usages("foo", &clauses, root)
            .expect("should not error");
        assert!(results.is_empty());
    }

    #[test]
    fn stub_resolve_symbol_returns_none() {
        let s = StubColumnarStorage;
        let clauses = Clauses::default();
        let root = Path::new("/tmp");
        let loc: Option<SymbolLocation> = s
            .resolve_symbol("foo", &clauses, root)
            .expect("should not error");
        assert!(loc.is_none());
    }

    #[test]
    fn stub_persist_and_load_are_noops() {
        let mut s = StubColumnarStorage;
        s.persist_to_cache(Path::new("/tmp"), "abc123", "test")
            .expect("persist noop");
        let loaded = s
            .load_from_cache(Path::new("/tmp"), "abc123", "test")
            .expect("load noop");
        assert!(!loaded, "stub always returns false for load");
    }

    // --- MockProvider (SourceProvider shape) ---

    #[test]
    fn mock_provider_insert_and_read() {
        let mut p = MockProvider::default();
        let id = p.insert(b"hello");
        let bytes = p.read_content(&id).expect("blob must exist");
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn mock_provider_hash_is_deterministic() {
        let p = MockProvider::default();
        let id1 = p.hash_content(b"world");
        let id2 = p.hash_content(b"world");
        assert_eq!(id1.hex(), id2.hex(), "same bytes must produce same id");
    }

    #[test]
    fn mock_provider_walk_snapshot() {
        let mut p = MockProvider::default();
        let id = p.insert(b"fn foo() {}");
        p.add_snapshot("snap-a", vec![(PathBuf::from("src/foo.rs"), id.clone())]);
        p.set_current("snap-a");

        let snap = p
            .current_snapshot(Path::new("/repo"))
            .expect("current snap");
        let entries: Vec<_> = p
            .walk_snapshot(&snap)
            .expect("walk ok")
            .map(|r| r.expect("entry ok"))
            .collect();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, PathBuf::from("src/foo.rs"));
        assert_eq!(entries[0].1.hex(), id.hex());
    }

    #[test]
    fn mock_provider_changed_paths() {
        let mut p = MockProvider::default();
        let id_a = p.insert(b"v1");
        let id_b = p.insert(b"v2");
        let id_c = p.insert(b"new");

        p.add_snapshot(
            "snap-1",
            vec![
                (PathBuf::from("a.rs"), id_a.clone()),
                (PathBuf::from("b.rs"), id_b),
            ],
        );
        p.add_snapshot(
            "snap-2",
            vec![
                (PathBuf::from("a.rs"), id_a),
                (PathBuf::from("b.rs"), id_c.clone()),
                (PathBuf::from("c.rs"), id_c),
            ],
        );

        let from = MockProvider::mock_snapshot("snap-1");
        let to = MockProvider::mock_snapshot("snap-2");

        let changed = p.changed_paths(&from, &to).expect("changed paths ok");
        assert!(changed.contains(&PathBuf::from("b.rs")));
        assert!(changed.contains(&PathBuf::from("c.rs")));
        assert!(!changed.contains(&PathBuf::from("a.rs")));
    }

    #[test]
    fn stub_satisfies_dyn_trait_bound() {
        let _: Box<dyn StorageEngine> = Box::new(StubColumnarStorage);
    }
}
