//! Per-file parse pass: macro collection, node traversal, row emission.
use std::path::Path;

use anyhow::Result;

use crate::ast::enrich::NodeEnricher;
use crate::ast::enrich::guard_utils::{GuardFrame, build_env_guard_frame, build_guard_frame};
use crate::ast::enrich::macro_table::MacroTable;
use crate::ast::lang::{LanguageConfig, LanguageSupport};
use crate::error::ForgeError;

use super::{SegmentBuildCtx, SymbolTable, node_text};

mod blocks;
mod hash;
mod ordinals;
mod rows;

use blocks::{ActiveBlock, BlockTag, block_group_key, emit_block_row, scan_block_run};
use rows::process_node_rows;

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
            | "preprocessor_region"
            | "preprocessor_directive"
            | "macro_call"
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

// -----------------------------------------------------------------------
// Generic node collector
// -----------------------------------------------------------------------

/// Walk the AST and produce index rows for every named node.
///
/// A node is "interesting" if [`extract_name`] returns a name for it.
/// Identifier tokens are also indexed as usage sites regardless of kind.
///
/// `preproc_else` and `preproc_elif` subtrees are skipped entirely so that
/// only the primary (#if) branch is indexed.  Without this, tree-sitter's
/// full-source parse would create duplicate rows and usage sites for every
/// symbol that appears in both a `#if` branch and its `#else` counterpart.
///
/// Uses iterative depth-first traversal via `TreeCursor` navigation to
/// avoid stack overflow on large codebases (e.g. Zephyr RTOS).
#[allow(clippy::too_many_lines)]
fn collect_nodes(
    source: &[u8],
    ctx: &mut IndexContext<'_>,
    cursor: &mut tree_sitter::TreeCursor<'_>,
    ts_language: &tree_sitter::Language,
) {
    let config = ctx.language.config();
    let lang = ctx.language;
    // Block grouping: a language may declare runs of same-kind leaf nodes (e.g.
    // comments) to be spanned by a synthetic, childless "block" node. When the
    // language declares none, all per-node block work below is skipped.
    let block_groups_active = !config.block_groups().is_empty();
    // The block currently being spanned, carried across loop iterations; while
    // set, member nodes inside its span are tagged with the block address.
    let mut active_block: Option<ActiveBlock> = None;
    let mut guard_stack: Vec<GuardFrame> = Vec::new();
    // Tracks the kind of the parent node at each level of the DFS, updated
    // O(1) by the cursor navigation below.  Avoids calling node.parent()
    // inside enrichers (which is O(sibling_count) in tree-sitter 0.25).
    let mut parent_kind_stack: Vec<&'static str> = Vec::new();
    // Two independent depth counters exposed to enrichers via EnrichContext.
    // Using usize (not bool) because a string_literal can appear inside an
    // ERROR subtree, so each counter must track its own nesting depth.
    //
    // string_depth — incremented when descending into an opaque-string kind
    //   or comment node; decremented on ascent.
    //   → ctx.inside_string
    //
    // error_depth  — incremented when descending into a tree-sitter ERROR
    //   recovery node; decremented on ascent.
    //   → ctx.inside_error
    let mut string_depth: usize = 0;
    let mut error_depth: usize = 0;
    // Pre-compile env_guard_patterns once per file.
    let env_guard_regex: Option<regex::RegexSet> = if config.env_guard_patterns().is_empty() {
        None
    } else {
        regex::RegexSet::new(config.env_guard_patterns()).ok()
    };
    // Per-file DFS ordinal counter — each named row gets the next value so
    // callers can compute a stable node_id handle without re-parsing.
    let mut row_ordinal_counter: u32 = ctx
        .ordinal_remapper
        .as_ref()
        .map_or(0, ordinals::OrdinalRemapper::next_ordinal);
    // Parallel to parent_kind_stack: propagates the enclosing row's ordinal
    // to unnamed descendant nodes so they inherit their nearest named ancestor.
    let mut parent_ordinal_stack: Vec<u32> = Vec::new();
    loop {
        let node = cursor.node();

        // --- Guard stack management (pop stale frames, push new guard frames) ---
        update_guard_stack(
            node,
            source,
            config,
            env_guard_regex.as_ref(),
            ctx.language,
            &mut guard_stack,
        );

        // Skip alternate conditional-compilation branches entirely.
        let skip = config.is_skip_kind(node.kind());

        if !skip {
            let parent_ordinal = parent_ordinal_stack.last().copied().unwrap_or(u32::MAX);

            // Block grouping: if this node begins a run of >= min_run adjacent
            // same-key members, emit one childless block row spanning the whole
            // run. Members keep their own parent and node ids; only the block is
            // added. next_sibling() bridges blank lines (they are not tree nodes).
            if block_groups_active {
                if active_block
                    .as_ref()
                    .is_some_and(|ab| node.start_byte() >= ab.end_byte)
                {
                    active_block = None;
                }
                if active_block.is_none()
                    && let Some(spec) =
                        config.block_group_for_member(lang.map_kind(node.kind()).unwrap_or(""))
                {
                    let key = block_group_key(node, source, lang, spec);
                    let (count, end_byte) = scan_block_run(node, source, lang, spec, &key);
                    if count >= spec.min_run {
                        let first_text = node_text(source, node);
                        let snippet = crate::result::comment_snippet(&first_text);
                        let label = if snippet.chars().count() > 40 {
                            let short: String = snippet.chars().take(40).collect();
                            format!("{short}… (×{count})")
                        } else {
                            format!("{snippet} (×{count})")
                        };
                        let block_ordinal = emit_block_row(
                            ctx,
                            spec,
                            node.start_byte(),
                            end_byte,
                            node.start_position().row + 1,
                            parent_ordinal,
                            &mut row_ordinal_counter,
                            source,
                            &label,
                        );
                        active_block = Some(ActiveBlock {
                            ord_suffix: format!("{block_ordinal:04}"),
                            start_line: node.start_position().row + 1,
                            end_byte,
                            member_fql_kind: spec.member_fql_kind.clone(),
                        });
                    }
                }
            }

            // Stage 2: tag each member of an active block with the block ordinal
            // and the member's offset within it, so FIND/SHOW surface the member
            // as `block_id(offset)`.
            let block_tag = active_block.as_ref().and_then(|ab| {
                if lang.map_kind(node.kind()).unwrap_or("") == ab.member_fql_kind
                    && node.start_byte() < ab.end_byte
                {
                    let start = node.start_position().row + 1 - ab.start_line + 1;
                    // A doc (`///`) or block (`/* */`) comment span can include
                    // the trailing newline — its end_position is column 0 of the
                    // next line. Clamp to the last content line so a one-line
                    // comment surfaces as a single offset, not a 2-line range.
                    let end_pos = node.end_position();
                    let member_end =
                        if end_pos.column == 0 && end_pos.row > node.start_position().row {
                            end_pos.row
                        } else {
                            end_pos.row + 1
                        };
                    let end = member_end - ab.start_line + 1;
                    let off = if start == end {
                        start.to_string()
                    } else {
                        format!("{start}-{end}")
                    };
                    Some(BlockTag {
                        ord: ab.ord_suffix.clone(),
                        off,
                    })
                } else {
                    None
                }
            });
            let current_node_ordinal = process_node_rows(
                ctx,
                node,
                source,
                ts_language,
                &guard_stack,
                parent_kind_stack.last().copied().unwrap_or(""),
                parent_ordinal,
                string_depth > 0,
                error_depth > 0,
                &mut row_ordinal_counter,
                block_tag.as_ref(),
            );

            // Descend into children.
            if cursor.goto_first_child() {
                // Maintain two independent depth counters so enrichers can gate
                // on string/comment context and ERROR-recovery context separately.
                // See EnrichContext::inside_string / inside_error for rationale.
                if config.is_opaque_string_kind(node.kind()) || config.is_comment_kind(node.kind())
                {
                    string_depth += 1;
                }
                if node.is_error() {
                    error_depth += 1;
                }
                // Record this node as the parent for the child level; mirror
                // with the ordinal stack so unnamed descendants can inherit it.
                let parent_ord = current_node_ordinal
                    .unwrap_or_else(|| parent_ordinal_stack.last().copied().unwrap_or(u32::MAX));
                parent_ordinal_stack.push(parent_ord);
                parent_kind_stack.push(node.kind());
                continue;
            }
        }
        // When `skip` is true we never call goto_first_child(), so the
        // entire subtree is skipped — matches the old early-return behaviour.

        // Move to next sibling, or walk up until we find one.
        if cursor.goto_next_sibling() {
            continue;
        }
        if !ascend_to_next_sibling(
            cursor,
            config,
            &mut parent_ordinal_stack,
            &mut parent_kind_stack,
            &mut string_depth,
            &mut error_depth,
        ) {
            break;
        }
    }
}

/// Advance the `guard_stack` for `node`: pop frames whose byte scope we have
/// left, then push a block/elif/else guard frame and/or a heuristic env-guard
/// frame when `node` opens one. Extracted from the `collect_nodes` walk loop.
fn update_guard_stack(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    env_guard_regex: Option<&regex::RegexSet>,
    language: &dyn LanguageSupport,
    guard_stack: &mut Vec<GuardFrame>,
) {
    // Pop frames whose byte scope we've left.
    while let Some(frame) = guard_stack.last() {
        if node.start_byte() >= frame.guard_byte_range.end {
            drop(guard_stack.pop());
        } else {
            break;
        }
    }
    // Push a new frame when entering a block-guard-opening node.
    if config.has_guard_support()
        && (config.is_block_guard_kind(node.kind())
            || config.is_elif_kind(node.kind())
            || config.is_else_kind(node.kind()))
    {
        let frame = build_guard_frame(node, source, config, &*guard_stack);
        guard_stack.push(frame);
    }
    // Push a heuristic guard frame for env-guarded `if` nodes
    // (e.g. Python `if TYPE_CHECKING:` or `if sys.platform == "linux":`).
    if let Some(regex_set) = env_guard_regex
        && language.map_kind(node.kind()) == Some("if")
        && let Some(frame) = build_env_guard_frame(node, source, config, regex_set)
    {
        guard_stack.push(frame);
    }
}

/// Walk up the cursor until a node with an unvisited next sibling is found,
/// unwinding the parent/ordinal stacks and string/error depth counters on the
/// way. Returns `true` if a next sibling was reached, `false` at end of tree.
/// Extracted from the `collect_nodes` walk loop.
fn ascend_to_next_sibling(
    cursor: &mut tree_sitter::TreeCursor<'_>,
    config: &LanguageConfig,
    parent_ordinal_stack: &mut Vec<u32>,
    parent_kind_stack: &mut Vec<&'static str>,
    string_depth: &mut usize,
    error_depth: &mut usize,
) -> bool {
    while cursor.goto_parent() {
        let _ = parent_ordinal_stack.pop();
        if let Some(popped) = parent_kind_stack.pop() {
            if config.is_opaque_string_kind(popped) || config.is_comment_kind(popped) {
                *string_depth = string_depth.saturating_sub(1);
            }
            if popped == "ERROR" {
                *error_depth = error_depth.saturating_sub(1);
            }
        }
        if cursor.goto_next_sibling() {
            return true;
        }
    }
    false
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
