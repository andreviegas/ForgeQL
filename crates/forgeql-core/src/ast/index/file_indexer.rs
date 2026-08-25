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
    ADDRESSABLE_FQL_KINDS.contains(&fql_kind)
}

/// The kinds a row gets an ordinal and a `node_id` for.
///
/// A list rather than a `matches!` chain so it can be compared with
/// [`crate::field_tiers::FQL_KIND_VALUES`], the set a clause value is refused
/// against: a kind addressable here and absent there would make
/// `WHERE fql_kind = '<it>'` refuse rows that demonstrably exist. The test
/// below is what ties the two. The value universe is deliberately the wider of
/// the two — a number, a cast or an operator row is queryable without being
/// addressable by handle — so the relation asserted is subset, not equality.
const ADDRESSABLE_FQL_KINDS: &[&str] = &[
    "function",
    "struct",
    "union",
    "class",
    "interface",
    "enum",
    "enumerator",
    "field",
    "method",
    "import",
    "namespace",
    "type_alias",
    "macro",
    "include_group",
    "variable",
    "global_variable",
    "local_declaration",
    "if",
    "for",
    "while",
    "switch",
    "do",
    "do_while",
    "call_statement",
    "return_expression",
    "comment",
    "comment_block",
    "section",
    "heading",
    "list_item",
    "paragraph",
    "code_block",
    "table",
    "block_quote",
    "pair",
    "object",
    "array",
    "macro_call",
    // `#ifdef` / `#if` / `#elif`. Addressable so a config flag's guard
    // sites answer to `fql_kind = 'guard'` and can be read and edited
    // by handle, the same as any other construct. Note `guard` is also
    // an enrichment FIELD name — see the doc table.
    "guard",
    // A region the parser could not parse.  Addressable ON PURPOSE:
    // mapping the damage was only half the contract — an agent must be
    // able to `SHOW NODE` it and `CHANGE NODE` it to repair it.  The
    // engine still never repairs it itself (P1).
    //
    // This CONSUMES ordinals, so node_ids shift in every file that holds
    // an error.  That is why no golden case may hardcode a node_id: they
    // all capture handles from a FIND at run time now.
    "error",
];

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
mod addressable_kinds_are_in_the_value_universe {
    /// Every addressable kind is a kind the value universe accepts.
    ///
    /// Two hardcoded kind lists live in core now — this one and
    /// [`crate::field_tiers::FQL_KIND_VALUES`], which is what
    /// `filter::reject_unknown_enum_values` refuses against. Nothing else ties
    /// them, and the direction that hurts is a kind added HERE and not there:
    /// rows of it would exist, be addressable, and `WHERE fql_kind = '<new>'`
    /// would be refused — the exact failure the refusal was built to prevent,
    /// arriving through the door it opened.
    ///
    /// Subset, not equality: the universe is deliberately wider, because a
    /// number, a cast or an operator row is queryable without being addressable
    /// by handle.
    #[test]
    fn every_addressable_kind_is_a_value_the_universe_accepts() {
        use super::{ADDRESSABLE_FQL_KINDS, is_addressable_fql_kind};
        let missing: Vec<&str> = ADDRESSABLE_FQL_KINDS
            .iter()
            .copied()
            .filter(|k| !crate::field_tiers::FQL_KIND_VALUES.contains(k))
            .collect();
        assert!(
            missing.is_empty(),
            "addressable kinds absent from FQL_KIND_VALUES, so a query naming one \
             would be refused although rows of it exist: {missing:?}"
        );
        // The list this walks must be the list the predicate answers from, or
        // the assertion is about a copy nothing consults.
        for kind in ADDRESSABLE_FQL_KINDS {
            assert!(
                is_addressable_fql_kind(kind),
                "{kind} is enumerated here but is_addressable_fql_kind says no"
            );
        }
    }
}
