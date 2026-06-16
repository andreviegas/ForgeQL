//! Per-file parse pass: macro collection, node traversal, row emission.
use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use sha2::{Digest, Sha256};

use crate::ast::enrich::guard_utils::{
    GuardFrame, build_env_guard_frame, build_guard_frame, collect_attribute_guard_frames,
    inject_guard_fields,
};
use crate::ast::enrich::macro_table::MacroTable;
use crate::ast::enrich::{EnrichContext, NodeEnricher};
use crate::ast::lang::{BlockGroupSpec, LanguageConfig, LanguageSupport};
use crate::error::ForgeError;

use super::{IndexRow, SegmentBuildCtx, SymbolTable, node_text};
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

#[derive(Clone)]
pub struct OrdinalHint {
    pub name: String,
    pub fql_kind: String,
    pub parent_ordinal: u32,
    pub guard_group_id: Option<String>,
    pub guard_branch: Option<String>,
    pub first_body_statement_fingerprint: Option<String>,
    pub content_hash: Option<String>,
    pub ordinal: u32,
}

pub struct OrdinalRemapper {
    previous: Vec<OrdinalHint>,
    used: Vec<bool>,
    next_ordinal: u32,
}

struct OrdinalMatchKey<'a> {
    name: &'a str,
    fql_kind: &'a str,
    parent_ordinal: u32,
    guard_group_id: Option<&'a str>,
    guard_branch: Option<&'a str>,
    first_body_statement_fingerprint: Option<&'a str>,
    content_hash: Option<&'a str>,
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
            | "class"
            | "interface"
            | "enum"
            | "field"
            | "method"
            | "import"
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
    )
}

impl OrdinalRemapper {
    #[must_use]
    pub fn from_previous(previous: Vec<OrdinalHint>) -> Self {
        let next_ordinal = previous
            .iter()
            .map(|h| h.ordinal)
            .max()
            .map_or(0, |m| m.saturating_add(1));
        let used = vec![false; previous.len()];
        Self {
            previous,
            used,
            next_ordinal,
        }
    }

    fn primary_matches(
        hint: &OrdinalHint,
        name: &str,
        fql_kind: &str,
        parent_ordinal: u32,
    ) -> bool {
        hint.name == name && hint.fql_kind == fql_kind && hint.parent_ordinal == parent_ordinal
    }

    fn guard_matches(
        hint: &OrdinalHint,
        guard_group_id: Option<&str>,
        guard_branch: Option<&str>,
    ) -> bool {
        hint.guard_group_id.as_deref() == guard_group_id
            && hint.guard_branch.as_deref() == guard_branch
    }

    fn assign(&mut self, key: &OrdinalMatchKey<'_>) -> u32 {
        let mut candidates: Vec<usize> = self
            .previous
            .iter()
            .enumerate()
            .filter(|(idx, hint)| {
                !self.used[*idx]
                    && Self::primary_matches(hint, key.name, key.fql_kind, key.parent_ordinal)
            })
            .map(|(idx, _)| idx)
            .collect();

        if candidates.len() > 1 {
            let guard_filtered: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|idx| {
                    Self::guard_matches(&self.previous[*idx], key.guard_group_id, key.guard_branch)
                })
                .collect();
            if !guard_filtered.is_empty() {
                candidates = guard_filtered;
            }
        }

        if candidates.len() > 1 && key.first_body_statement_fingerprint.is_some() {
            let fp_filtered: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|idx| {
                    self.previous[*idx]
                        .first_body_statement_fingerprint
                        .as_deref()
                        == key.first_body_statement_fingerprint
                })
                .collect();
            if !fp_filtered.is_empty() {
                candidates = fp_filtered;
            }
        }

        if candidates.len() > 1 && key.content_hash.is_some() {
            let hash_filtered: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|idx| self.previous[*idx].content_hash.as_deref() == key.content_hash)
                .collect();
            if !hash_filtered.is_empty() {
                candidates = hash_filtered;
            }
        }

        if let Some(best_idx) = candidates
            .into_iter()
            .min_by_key(|idx| self.previous[*idx].ordinal)
        {
            self.used[best_idx] = true;
            let ordinal = self.previous[best_idx].ordinal;
            crate::debug_log!(
                "assign MATCH name={:?} kind={:?} parent_ord={} -> ord={} (reused)",
                key.name,
                key.fql_kind,
                key.parent_ordinal,
                ordinal
            );
            return ordinal;
        }

        let ordinal = self.next_ordinal;
        self.next_ordinal = self.next_ordinal.saturating_add(1);
        if crate::debug_log::is_enabled() {
            // On a fresh allocation, surface any prior hints that matched on
            // name+kind but were rejected — their parent_ordinal reveals whether
            // the miss is a structural (flat vs nested) mismatch.
            let rejected_parent_ords: Vec<u32> = self
                .previous
                .iter()
                .filter(|h| h.name == key.name && h.fql_kind == key.fql_kind)
                .map(|h| h.parent_ordinal)
                .collect();
            crate::debug_log!(
                "assign NEW   name={:?} kind={:?} parent_ord={} -> ord={} (name+kind priors={}, their parent_ords={:?})",
                key.name,
                key.fql_kind,
                key.parent_ordinal,
                ordinal,
                rejected_parent_ords.len(),
                rejected_parent_ords
            );
        }
        ordinal
    }
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

    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| ForgeError::AstParse {
            path: ctx.path.to_path_buf(),
        })?;

    let ts_lang = ctx.language.tree_sitter_language();
    let before = ctx.table.rows.len();

    let mut cursor = tree.root_node().walk();
    collect_nodes(&source, ctx, &mut cursor, &ts_lang);

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
        let content_id = (seg.hash_fn)(&source);
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
    let mut row_ordinal_counter: u32 = ctx.ordinal_remapper.as_ref().map_or(0, |r| r.next_ordinal);
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
                    let key = block_group_key(node, source, config, spec);
                    let (count, end_byte) = scan_block_run(node, source, config, lang, spec, &key);
                    if count >= spec.min_run {
                        let block_ordinal = emit_block_row(
                            ctx,
                            spec,
                            node.start_byte(),
                            end_byte,
                            node.start_position().row + 1,
                            parent_ordinal,
                            &mut row_ordinal_counter,
                            source,
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

/// Emit all symbol-table rows for a single (non-skipped) `node`: the named row
/// (or a re-tagged `macro_call` row), every enricher `extra_rows`, and any
/// usage site. Returns the named row's ordinal so the caller can propagate it
/// to descendant nodes. Does **not** descend into children — the caller owns
/// the cursor walk. Extracted from the `collect_nodes` walk loop.
#[allow(clippy::too_many_arguments)]
fn process_node_rows(
    ctx: &mut IndexContext<'_>,
    node: tree_sitter::Node<'_>,
    source: &[u8],
    ts_language: &tree_sitter::Language,
    guard_stack: &[GuardFrame],
    parent_kind: &'static str,
    parent_ordinal: u32,
    inside_string: bool,
    inside_error: bool,
    row_ordinal_counter: &mut u32,
    block_tag: Option<&BlockTag>,
) -> Option<u32> {
    let config = ctx.language.config();
    let lang_name = ctx.language.name();
    let mut current_node_ordinal: Option<u32> = None;

    // Build the enrichment context once for this node.
    let enrich_ctx = EnrichContext {
        node,
        source,
        path: ctx.path,
        language_name: lang_name,
        language_config: config,
        language_support: ctx.language,
        guard_stack,
        macro_table: ctx.macro_table,
        parent_kind,
        inside_string,
        inside_error,
    };

    // Every named node becomes a row.
    if let Some(name) = ctx.language.extract_name(node, source) {
        let fql_kind_val = ctx.language.map_kind(node.kind()).unwrap_or("");
        let mut sink = RowSink {
            table: ctx.table,
            enrichers: ctx.enrichers,
            remapper: ctx.ordinal_remapper.as_mut(),
            row_ordinal_counter,
        };
        current_node_ordinal = emit_addressable_row(
            &mut sink,
            &enrich_ctx,
            ts_language,
            &name,
            fql_kind_val,
            parent_ordinal,
            block_tag,
        );
    } else if let Some(mtable) = ctx.macro_table {
        // Re-tag: tree-sitter-cpp parses C macro calls as call_expression,
        // not macro_invocation.  When extract_name returns None for a
        // call_expression whose function name is in the MacroTable, emit a
        // macro_call row.
        let call_kind = config.call_expression_kind();
        if !call_kind.is_empty()
            && node.kind() == call_kind
            && let Some(func_node) = node.child_by_field_name("function")
        {
            let func_name = node_text(source, func_node);
            if !func_name.is_empty() && mtable.contains(&func_name) {
                let mut sink = RowSink {
                    table: ctx.table,
                    enrichers: ctx.enrichers,
                    remapper: ctx.ordinal_remapper.as_mut(),
                    row_ordinal_counter,
                };
                current_node_ordinal = emit_addressable_row(
                    &mut sink,
                    &enrich_ctx,
                    ts_language,
                    &func_name,
                    "macro_call",
                    parent_ordinal,
                    None,
                );
            }
        }
    }

    // Run extra_rows() for every node (even if extract_name returned None).
    let mut sink = RowSink {
        table: ctx.table,
        enrichers: ctx.enrichers,
        remapper: ctx.ordinal_remapper.as_mut(),
        row_ordinal_counter,
    };
    let extra_self = emit_extra_rows(&mut sink, &enrich_ctx, parent_ordinal);
    // §4.1: promote the nameless control-flow self-row so the body parents to it.
    if current_node_ordinal.is_none() && config.is_control_flow_kind(node.kind()) {
        current_node_ordinal = extra_self;
    }

    // All identifier tokens become usage sites.
    if config.is_usage_node_kind(node.kind()) {
        let name = node_text(source, node);
        if name.len() > 1 {
            let line = node.start_position().row + 1;
            ctx.table.add_usage(name, ctx.path, node.byte_range(), line);
        }
    }

    current_node_ordinal
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

/// Fields and identity metadata prepared for a single index row, shared by the
/// named-node and re-tagged macro-call emission paths.
struct PreparedRow {
    fields: HashMap<String, String>,
    content_hash: String,
    guard_group_id: Option<String>,
    guard_branch: Option<String>,
    first_body_statement_fingerprint: Option<String>,
}

/// Extract the raw fields of a node and inject its guard/attribute context,
/// returning the field map plus the identity metadata needed to compute a
/// stable ordinal.
///
/// Pure: derives the node, source, config, and guard stack from `ctx`, so the
/// named and macro-call paths prepare rows identically.
fn build_row_fields(ctx: &EnrichContext<'_>, ts_language: &tree_sitter::Language) -> PreparedRow {
    let node = ctx.node;
    let source = ctx.source;
    let mut fields = extract_fields(node, source, ts_language);

    // Inject guard fields from the current block-guard stack.
    if !ctx.guard_stack.is_empty() {
        inject_guard_fields(ctx.guard_stack, &mut fields);
    }

    // Inject item-level attribute guards (e.g. Rust `#[cfg(...)]`).
    let attr_guard_name = ctx.language_config.item_guard_attribute();
    if !attr_guard_name.is_empty() {
        let attr_frames = collect_attribute_guard_frames(node, source, attr_guard_name);
        if !attr_frames.is_empty() {
            inject_guard_fields(&attr_frames, &mut fields);
        }
    }

    let first_body_statement_fingerprint = first_body_statement_fingerprint(node, source);
    let content_hash = node_content_hash(node, source);
    let guard_group_id = fields.get("guard_group_id").cloned();
    let guard_branch = fields.get("guard_branch").cloned();
    if let Some(fp) = &first_body_statement_fingerprint {
        drop(fields.insert("first_body_statement_fingerprint".to_string(), fp.clone()));
    }
    drop(fields.insert("content_hash".to_string(), content_hash.clone()));

    PreparedRow {
        fields,
        content_hash,
        guard_group_id,
        guard_branch,
        first_body_statement_fingerprint,
    }
}

/// Assign a node ordinal: reuse a prior one via the remapper when available
/// (keeps `node_id` handles stable across re-indexes), otherwise hand out the
/// next value from the per-file counter.
fn assign_ordinal(
    remapper: Option<&mut OrdinalRemapper>,
    row_ordinal_counter: &mut u32,
    key: &OrdinalMatchKey<'_>,
) -> u32 {
    remapper.map_or_else(
        || {
            let next = *row_ordinal_counter;
            *row_ordinal_counter = row_ordinal_counter.saturating_add(1);
            next
        },
        |remapper| remapper.assign(key),
    )
}

/// Content revision for an addressable row: the first 8 bytes of the SHA-256 of
/// the node source. Non-addressable rows (ordinal `None`) get `0`.
fn row_rev(ordinal: Option<u32>, source: &[u8], byte_range: std::ops::Range<usize>) -> u64 {
    ordinal.map_or(0, |_| {
        let bytes = Sha256::digest(&source[byte_range]);
        u64::from_le_bytes(bytes[..8].try_into().unwrap_or([0u8; 8]))
    })
}

/// Borrowed sinks an addressable row is written into: the symbol table, the
/// active enrichers, the optional ordinal remapper, and the per-file ordinal
/// counter. Bundled so the row-emission helper stays under the argument limit.
struct RowSink<'a> {
    table: &'a mut SymbolTable,
    enrichers: &'a [Box<dyn NodeEnricher>],
    remapper: Option<&'a mut OrdinalRemapper>,
    row_ordinal_counter: &'a mut u32,
}

/// Emit one addressable index row for `node` under the given `name` and
/// `fql_kind`. Shared by the named-node path (`fql_kind` from `map_kind`) and
/// the re-tagged macro-call path (`fql_kind` = `"macro_call"`); the two differ
/// only in those two strings. Returns the assigned ordinal (or `None` when the
/// kind is not addressable) so the caller can propagate it to descendants.
fn emit_addressable_row(
    sink: &mut RowSink<'_>,
    ctx: &EnrichContext<'_>,
    ts_language: &tree_sitter::Language,
    name: &str,
    fql_kind: &str,
    parent_ordinal: u32,
    block_tag: Option<&BlockTag>,
) -> Option<u32> {
    let node = ctx.node;
    let source = ctx.source;
    let prepared = build_row_fields(ctx, ts_language);
    let mut fields = prepared.fields;

    // Run all enrichers on this row.
    for enricher in sink.enrichers {
        enricher.enrich_row(ctx, name, &mut fields);
    }

    // Stage 2: tag this row with its owning block address so FIND/SHOW can
    // surface the member node id as `block_id(offset)`.
    if let Some(tag) = block_tag {
        drop(fields.insert("block_ord".to_string(), tag.ord.clone()));
        drop(fields.insert("block_off".to_string(), tag.off.clone()));
    }

    let (name_id, node_kind_id, fql_kind_id, language_id, path_id) =
        sink.table
            .strings
            .intern_row(name, node.kind(), fql_kind, ctx.language_name, ctx.path);
    // Reuse prior ordinals when possible to keep node_id stable across re-indexes.
    let ordinal = if is_addressable_fql_kind(fql_kind) {
        Some(assign_ordinal(
            sink.remapper.as_deref_mut(),
            sink.row_ordinal_counter,
            &OrdinalMatchKey {
                name,
                fql_kind,
                parent_ordinal,
                guard_group_id: prepared.guard_group_id.as_deref(),
                guard_branch: prepared.guard_branch.as_deref(),
                first_body_statement_fingerprint: prepared
                    .first_body_statement_fingerprint
                    .as_deref(),
                content_hash: Some(prepared.content_hash.as_str()),
            },
        ))
    } else {
        None
    };
    // Fold leading `#[...]` attributes into the span (rev covers them); ordinal
    // matching keeps the unextended content_hash so attribute edits don't churn ids.
    let (start_byte, start_line) = attr_extended_start(node);
    let span = start_byte..node.byte_range().end;
    let rev = row_rev(ordinal, source, span.clone());
    let fields = sink.table.strings.intern_fields(fields);
    sink.table.push_row(IndexRow {
        name_id,
        node_kind_id,
        fql_kind_id,
        language_id,
        path_id,
        byte_range: span,
        line: start_line,
        usages_count: 0,
        ordinal,
        parent_ordinal,
        rev,
        fields,
    });
    ordinal
}

/// Grouping key for a block-group member. Members that share a key AND are
/// adjacent tree siblings coalesce into one block. For `split_on_attr =
/// "comment_style"` the key is the comment style, so `///` doc runs and `//`
/// line runs form separate blocks; otherwise every member of the kind shares
/// one key.
/// State for the block currently being spanned, carried across loop iterations
/// so each member of the run can be tagged with the block's address.
struct ActiveBlock {
    /// 4-digit ordinal suffix of the block node (matches the `node_id` format),
    /// e.g. `"0123"` for ordinal 123.
    ord_suffix: String,
    /// 1-based start line of the block (used to compute member offsets).
    start_line: usize,
    /// End byte of the block span; once a node starts at/after this, the block is
    /// closed.
    end_byte: usize,
    /// FQL kind of the run's members (only these nodes are tagged).
    member_fql_kind: String,
}

/// Per-member block address, written onto the member row as `block_ord` /
/// `block_off` fields so `FIND`/`SHOW` can surface the member as
/// `block_id(offset)` (Stage 2 alias).
struct BlockTag {
    /// 4-digit ordinal suffix of the owning block node.
    ord: String,
    /// 1-based offset (or `start-end` range) of the member within the block.
    off: String,
}
fn block_group_key(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    spec: &BlockGroupSpec,
) -> String {
    match spec.split_on_attr.as_deref() {
        Some("comment_style") => config
            .detect_comment_style(&node_text(source, node))
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Walk forward over tree siblings, extending a run while each sibling is the
/// same member kind and grouping key. Blank lines are bridged for free: blank
/// lines are not tree nodes, so two same-kind declarations separated only by
/// blank lines are adjacent siblings. A node of any other kind ends the run.
/// Returns `(member_count, run_end_byte)`.
fn scan_block_run(
    first: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    lang: &dyn LanguageSupport,
    spec: &BlockGroupSpec,
    key: &str,
) -> (usize, usize) {
    let mut count = 1usize;
    let mut end_byte = first.byte_range().end;
    let mut cursor = first;
    while let Some(sib) = cursor.next_sibling() {
        if lang.map_kind(sib.kind()).unwrap_or("") != spec.member_fql_kind {
            break;
        }
        if block_group_key(sib, source, config, spec) != key {
            break;
        }
        count += 1;
        end_byte = sib.byte_range().end;
        cursor = sib;
    }
    (count, end_byte)
}

/// Emit a synthetic, childless "block" row spanning a run of grouped members.
/// The block shares the `parent_ordinal` of its members — it is their sibling,
/// never their parent — and gives one addressable handle over the whole run.
/// The member rows are emitted normally and keep their own node ids.
#[allow(clippy::too_many_arguments)]
fn emit_block_row(
    ctx: &mut IndexContext<'_>,
    spec: &BlockGroupSpec,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    parent_ordinal: u32,
    row_ordinal_counter: &mut u32,
    source: &[u8],
) -> u32 {
    let span = start_byte..end_byte;
    let block_kind = spec.block_fql_kind.as_str();
    let content_hash = short_sha256_hex(source.get(span.clone()).unwrap_or_default());
    let path = ctx.path;
    let lang_name = ctx.language.name();
    let (name_id, node_kind_id, fql_kind_id, language_id, path_id) = ctx
        .table
        .strings
        .intern_row(block_kind, block_kind, block_kind, lang_name, path);
    let ordinal = assign_ordinal(
        ctx.ordinal_remapper.as_mut(),
        row_ordinal_counter,
        &OrdinalMatchKey {
            name: block_kind,
            fql_kind: block_kind,
            parent_ordinal,
            guard_group_id: None,
            guard_branch: None,
            first_body_statement_fingerprint: None,
            content_hash: Some(content_hash.as_str()),
        },
    );
    let rev = row_rev(Some(ordinal), source, span.clone());
    // Carry the content hash as a field so the reindex hint can disambiguate this
    // block from sibling blocks (which all share the constant `comment_block`
    // name) and keep its node id stable across edits to other blocks.
    let mut block_fields = HashMap::new();
    drop(block_fields.insert("content_hash".to_string(), content_hash.clone()));
    let fields = ctx.table.strings.intern_fields(block_fields);
    ctx.table.push_row(IndexRow {
        name_id,
        node_kind_id,
        fql_kind_id,
        language_id,
        path_id,
        byte_range: span,
        line: start_line,
        usages_count: 0,
        ordinal: Some(ordinal),
        parent_ordinal,
        rev,
        fields,
    });
    ordinal
}

/// Walk back over the contiguous run of leading attribute items (`#[...]`)
/// preceding `node` and return the `(start_byte, 1-based start_line)` of the
/// first attribute, so a node's span folds in its operational attributes.
/// Falls back to the node's own start when there are none.
///
/// Matches `collect_attribute_guard_frames`' detection (`attribute_item` via
/// `prev_named_sibling`), so today this only folds Rust attributes; other
/// languages' attribute kinds don't match and are left unchanged.
fn attr_extended_start(node: tree_sitter::Node<'_>) -> (usize, usize) {
    let mut start_byte = node.start_byte();
    let mut start_line = node.start_position().row + 1;
    let mut prev = node.prev_named_sibling();
    while let Some(sib) = prev {
        if sib.kind() != "attribute_item" {
            break;
        }
        start_byte = sib.start_byte();
        start_line = sib.start_position().row + 1;
        prev = sib.prev_named_sibling();
    }
    (start_byte, start_line)
}

/// Emit the synthetic rows contributed by the `extra_rows` of each enricher for
/// current node (e.g. usage sites, derived symbols). Runs for every node, even
/// when `extract_name` returned `None`. `parent_ordinal` is constant across the
/// extra rows of a node, so the caller computes it once.
fn emit_extra_rows(
    sink: &mut RowSink<'_>,
    ctx: &EnrichContext<'_>,
    parent_ordinal: u32,
) -> Option<u32> {
    let node = ctx.node;
    let source = ctx.source;
    let enrichers = sink.enrichers;
    let mut self_ordinal: Option<u32> = None;
    for enricher in enrichers {
        for extra in enricher.extra_rows(ctx) {
            let guard_group_id = extra.fields.get("guard_group_id").map(String::as_str);
            let guard_branch = extra.fields.get("guard_branch").map(String::as_str);
            let content_hash = node_content_hash(node, source);
            let extra_path = extra.path_override.as_deref().unwrap_or(ctx.path);
            let (eni, enk, enf, enl, enp) = sink.table.strings.intern_row(
                &extra.name,
                &extra.node_kind,
                &extra.fql_kind,
                ctx.language_name,
                extra_path,
            );
            let ordinal = if is_addressable_fql_kind(&extra.fql_kind) {
                Some(assign_ordinal(
                    sink.remapper.as_deref_mut(),
                    sink.row_ordinal_counter,
                    &OrdinalMatchKey {
                        name: &extra.name,
                        fql_kind: &extra.fql_kind,
                        parent_ordinal,
                        guard_group_id,
                        guard_branch,
                        first_body_statement_fingerprint: None,
                        content_hash: Some(content_hash.as_str()),
                    },
                ))
            } else {
                None
            };
            // `is_self_row` (set by the enricher that produced the row) marks
            // the row representing the visited node itself. Capture its ordinal so
            // a control-flow node can become the parent of its body — an explicit
            // flag instead of an implicit `byte_range == node.byte_range()` match.
            if self_ordinal.is_none() && extra.is_self_row && ordinal.is_some() {
                self_ordinal = ordinal;
            }
            let rev = row_rev(ordinal, source, extra.byte_range.clone());
            let fields = sink.table.strings.intern_fields(extra.fields);
            sink.table.push_row(IndexRow {
                name_id: eni,
                node_kind_id: enk,
                fql_kind_id: enf,
                language_id: enl,
                path_id: enp,
                byte_range: extra.byte_range,
                line: extra.line,
                usages_count: 0,
                ordinal,
                parent_ordinal,
                rev,
                fields,
            });
        }
    }
    self_ordinal
}

fn short_sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn first_body_statement_fingerprint(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let first = body.named_children(&mut cursor).next()?;
    let text = node_text(source, first);
    if text.is_empty() {
        return None;
    }
    Some(short_sha256_hex(text.as_bytes()))
}

fn node_content_hash(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    let range = node.byte_range();
    let slice = source.get(range).unwrap_or_default();
    short_sha256_hex(slice)
}

/// Extract all grammar fields from a tree-sitter node into a string map.
fn extract_fields(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    language: &tree_sitter::Language,
) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let field_count = language.field_count();

    for field_id in 1..=u16::try_from(field_count).unwrap_or(u16::MAX) {
        if let Some(child) = node.child_by_field_id(field_id)
            && let Some(field_name) = language.field_name_for_id(field_id)
        {
            let text = node_text(source, child);
            if !text.is_empty() {
                drop(fields.insert(field_name.to_string(), text));
            }
        }
    }

    fields
}
