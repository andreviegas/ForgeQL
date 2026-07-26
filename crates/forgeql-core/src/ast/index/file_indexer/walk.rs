//! The tree walk — one depth-first pass over a parsed file.
//!
//! `collect_nodes` drives a tree-sitter cursor and hands each named node to row
//! emission. It owns the state that only means anything mid-walk: the
//! parent/ordinal stacks that tell a node who its parent is, the guard stack
//! recording which conditional a node sits under, and the block run currently
//! being spanned.
//!
//! The traversal order is load-bearing. A newly seen node takes the next value
//! from the per-file counter as the walk reaches it — only nodes the remapper
//! can match to a prior hint keep an existing ordinal — so changing the order
//! of this walk renumbers every handle it has not seen before.

use crate::ast::enrich::guard_utils::{GuardFrame, build_env_guard_frame, build_guard_frame};
use crate::ast::index::node_text;
use crate::ast::lang::{LanguageConfig, LanguageSupport};

use super::IndexContext;
use super::blocks::{ActiveBlock, BlockTag, block_group_key, emit_block_row, scan_block_run};
use super::ordinals::OrdinalRemapper;
use super::rows::process_node_rows;

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
pub(super) fn collect_nodes(
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
        .map_or(0, OrdinalRemapper::next_ordinal);
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
