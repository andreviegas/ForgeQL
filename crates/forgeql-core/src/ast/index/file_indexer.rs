//! Per-file parse pass: macro collection, node traversal, row emission.
use std::path::Path;

use anyhow::Result;

use crate::ast::enrich::NodeEnricher;
use crate::ast::enrich::macro_table::MacroTable;
use crate::ast::lang::LanguageSupport;
use crate::error::ForgeError;

use super::{SegmentBuildCtx, SymbolTable};

mod blocks;
mod hash;
mod ordinals;
mod rows;
mod walk;

use walk::collect_nodes;

pub use ordinals::{OrdinalHint, OrdinalRemapper, OrdinalTombstones};
// First-pass macro collector
// -----------------------------------------------------------------------

/// Walk the AST of a single file and collect all macro definitions.
///
/// Returns an empty `Vec` when the language has no `macro_expander()`.
///
/// # Errors
/// Returns an error if the file cannot be read or tree-sitter parsing fails.
pub(super) fn collect_macro_defs_for_file(
    parser: &mut tree_sitter::Parser,
    path: &Path,
    language: &dyn LanguageSupport,
) -> Result<Vec<crate::ast::lang::MacroDef>> {
    let Some(expander) = language.macro_expander() else {
        return Ok(Vec::new());
    };
    let source = crate::workspace::file_io::read_bytes(path)?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| ForgeError::AstParse {
            path: path.to_path_buf(),
        })?;
    let config = language.config();
    let mut cursor = tree.root_node().walk();
    let mut defs = Vec::new();
    loop {
        let node = cursor.node();
        if config.macro_def_kinds().iter().any(|k| k == node.kind())
            && let Some(mut def) = expander.extract_def(node, &source, config)
        {
            def.file = path.to_path_buf();
            defs.push(def);
        }
        if !config.is_skip_kind(node.kind()) && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return Ok(defs);
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

// -----------------------------------------------------------------------
// Per-file indexing context
// -----------------------------------------------------------------------

/// Bundles the parameters shared between [`index_file`] and [`collect_nodes`]
/// to reduce their argument lists.
pub struct IndexContext<'a> {
    /// The file being indexed.
    pub path: &'a Path,
    /// Language-specific AST support.
    pub language: &'a dyn LanguageSupport,
    /// Active enrichers applied to every node.
    pub enrichers: &'a [Box<dyn NodeEnricher>],
    /// Macro definitions from the first pass, if available.
    pub macro_table: Option<&'a MacroTable>,
    /// Optional remapper used to preserve node ordinals across re-indexes.
    pub ordinal_remapper: Option<OrdinalRemapper>,
    /// The symbol table being populated.
    pub table: &'a mut SymbolTable,
    /// Root that row paths are relative to, when the caller knows it.
    ///
    /// Only guard group IDs read it, and only to strip the prefix before
    /// hashing: a group's stored ID must not change between two worktrees of
    /// the same commit. `None` means "hash the path as given" — right for
    /// tests and one-off indexing, wrong for anything whose rows are cached.
    pub workspace_root: Option<&'a Path>,
}

/// Returns true when a row kind should receive a stable node `ordinal/node_id`.
///
/// Phase A policy: only addressable semantic nodes get `node_ids`; analysis-only
/// fragments (number/cast/operators/etc.) must not.
fn is_addressable_fql_kind(fql_kind: &str) -> bool {
    matches!(
        fql_kind,
        "function"
            | "struct"
            | "union"
            | "class"
            | "interface"
            | "enum"
            | "enumerator"
            | "field"
            | "method"
            | "import"
            | "namespace"
            | "type_alias"
            | "macro"
            | "include_group"
            | "variable"
            | "global_variable"
            | "local_declaration"
            | "if"
            | "for"
            | "while"
            | "switch"
            | "do"
            | "do_while"
            | "call_statement"
            | "return_expression"
            | "comment"
            | "comment_block"
            | "section"
            | "heading"
            | "list_item"
            | "paragraph"
            | "code_block"
            | "table"
            | "block_quote"
            | "pair"
            | "object"
            | "array"
            | "macro_call"
            // `#ifdef` / `#if` / `#elif`. Addressable so a config flag's guard
            // sites answer to `fql_kind = 'guard'` and can be read and edited
            // by handle, the same as any other construct. Note `guard` is also
            // an enrichment FIELD name — see the doc table.
            | "guard"
            // A region the parser could not parse.  Addressable ON PURPOSE:
            // mapping the damage was only half the contract — an agent must be
            // able to `SHOW NODE` it and `CHANGE NODE` it to repair it.  The
            // engine still never repairs it itself (P1).
            //
            // This CONSUMES ordinals, so node_ids shift in every file that holds
            // an error.  That is why no golden case may hardcode a node_id: they
            // all capture handles from a FIND at run time now.
            | "error"
    )
}

// -----------------------------------------------------------------------
// Index one file (second pass)
// -----------------------------------------------------------------------
// Index one file (second pass)
// -----------------------------------------------------------------------

/// Index a single file, adding its rows to `table`.
///
/// `macro_table` — optional table of macro definitions built during the
/// first pass; passed through to [`EnrichContext`] for macro-aware enrichers.
///
/// # Errors
/// Returns an error if the file cannot be read or tree-sitter parsing fails.
pub fn index_file(
    parser: &mut tree_sitter::Parser,
    ctx: &mut IndexContext<'_>,
    seg_ctx: Option<&SegmentBuildCtx>,
) -> Result<usize> {
    let source = crate::workspace::file_io::read_bytes(ctx.path)?;
    index_file_from_source(parser, ctx, seg_ctx, &source)
}

/// [`index_file`] body operating on already-read source bytes.
///
/// Split out so callers that must read the file anyway (for example the
/// columnar fast-path's pre-parse segment-reuse check) can avoid a second
/// read of the same content.
///
/// # Errors
/// Returns an error if tree-sitter parsing fails.
pub(super) fn index_file_from_source(
    parser: &mut tree_sitter::Parser,
    ctx: &mut IndexContext<'_>,
    seg_ctx: Option<&SegmentBuildCtx>,
    source: &[u8],
) -> Result<usize> {
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| ForgeError::AstParse {
            path: ctx.path.to_path_buf(),
        })?;

    let ts_lang = ctx.language.tree_sitter_language();
    let before = ctx.table.rows.len();

    let mut cursor = tree.root_node().walk();
    collect_nodes(source, ctx, &mut cursor, &ts_lang);

    // Per-file columnar shadow-write: hash the already-read source bytes and
    // emit a SegmentBuilder for the rows added to this per-file table.
    // Runs inline so files are only read once.
    //
    // Run enricher post_pass on the per-file table BEFORE emitting.
    // ControlFlowEnricher and RedundancyEnricher both group rows by file path
    // and work entirely intra-file, so per-file post_pass produces identical
    // enrichment results to full-table post_pass — without the sequential merge.
    if let Some(seg) = seg_ctx {
        for enricher in ctx.enrichers {
            enricher.post_pass(ctx.table, None);
        }
        let content_id = (seg.hash_fn)(source);
        (seg.emit_fn)(&content_id, ctx.table, before);
    }

    Ok(ctx.table.rows.len() - before)
}

#[cfg(test)]
mod identical_sibling_tombstone_tests {
    use super::ordinals::OrdinalMatchKey;
    use super::*;

    // Two byte-identical same-parent siblings share every discriminator the
    // remapper keys on — name, kind, parent, guard, fingerprint, content_hash —
    // and differ only in ordinal. This is the one case `content_hash` cannot
    // resolve, so `assign` falls through to the min-ordinal tiebreak.
    fn twin_hint(ordinal: u32) -> OrdinalHint {
        OrdinalHint {
            name: "sep".to_string(),
            fql_kind: "comment".to_string(),
            parent_ordinal: 0,
            guard_group_id: None,
            guard_branch: None,
            first_body_statement_fingerprint: None,
            content_hash: Some("deadbeef".to_string()),
            ordinal,
        }
    }

    fn twin_key() -> OrdinalMatchKey<'static> {
        OrdinalMatchKey {
            name: "sep",
            fql_kind: "comment",
            parent_ordinal: 0,
            guard_group_id: None,
            guard_branch: None,
            first_body_statement_fingerprint: None,
            content_hash: Some("deadbeef"),
        }
    }

    #[test]
    fn without_tombstone_survivor_adopts_the_deleted_twins_ordinal() {
        // Reproduces the pre-fix behaviour: with both twins' hints live, the lone survivor
        // matches the *lower* ordinal — the deleted node's — and silently
        // re-keys to the stale handle.
        let mut remapper = OrdinalRemapper::from_previous(vec![twin_hint(1), twin_hint(5)]);
        assert_eq!(remapper.assign(&twin_key()), 1);
    }

    #[test]
    fn tombstone_keeps_the_surviving_twin_on_its_own_ordinal() {
        // The fix: tombstoning the deleted twin's ordinal hides that hint, so
        // the survivor matches only its own (5) and keeps its handle. The stale
        // handle to ordinal 1 then resolves to nothing (node_not_found).
        let mut remapper = OrdinalRemapper::from_previous(vec![twin_hint(1), twin_hint(5)]);
        remapper.tombstone(&[1]);
        assert_eq!(remapper.assign(&twin_key()), 5);
    }

    #[test]
    fn tombstone_does_not_reissue_the_retired_ordinal() {
        // The retired ordinal must never be handed to a different node:
        // `next_ordinal` stays max+1 because the tombstoned hint remains in
        // `previous`, still bounding the max.
        let mut remapper = OrdinalRemapper::from_previous(vec![twin_hint(1), twin_hint(5)]);
        remapper.tombstone(&[1]);
        assert_eq!(remapper.assign(&twin_key()), 5);
        let fresh = OrdinalMatchKey {
            name: "other",
            ..twin_key()
        };
        assert_eq!(remapper.assign(&fresh), 6);
    }

    fn fn_hint(ordinal: u32) -> OrdinalHint {
        OrdinalHint {
            name: "dup".to_string(),
            fql_kind: "function".to_string(),
            parent_ordinal: 0,
            guard_group_id: None,
            guard_branch: None,
            first_body_statement_fingerprint: Some("fp".to_string()),
            content_hash: Some("H".to_string()),
            ordinal,
        }
    }

    fn child_hint(ordinal: u32, parent_ordinal: u32) -> OrdinalHint {
        OrdinalHint {
            name: "x".to_string(),
            fql_kind: "variable".to_string(),
            parent_ordinal,
            guard_group_id: None,
            guard_branch: None,
            first_body_statement_fingerprint: None,
            content_hash: Some("C".to_string()),
            ordinal,
        }
    }

    fn fn_key() -> OrdinalMatchKey<'static> {
        OrdinalMatchKey {
            name: "dup",
            fql_kind: "function",
            parent_ordinal: 0,
            guard_group_id: None,
            guard_branch: None,
            first_body_statement_fingerprint: Some("fp"),
            content_hash: Some("H"),
        }
    }

    #[test]
    fn tombstoning_only_the_root_still_keeps_the_survivor_with_children() {
        // Twins WITH a child subtree: fn@0{child@1}, fn@2{child@3}. Deleting the
        // first stages ONLY the root ordinal 0 (current engine behaviour). If the
        // remapper handles this correctly the survivor keeps ordinal 2.
        let mut remapper = OrdinalRemapper::from_previous(vec![
            fn_hint(0),
            child_hint(1, 0),
            fn_hint(2),
            child_hint(3, 2),
        ]);
        remapper.tombstone(&[0]);
        assert_eq!(remapper.assign(&fn_key()), 2);
    }
}
