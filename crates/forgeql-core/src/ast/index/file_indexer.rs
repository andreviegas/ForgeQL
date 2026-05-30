//! Per-file parse pass: macro collection, node traversal, row emission.
use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use crate::ast::enrich::guard_utils::{
    GuardFrame, build_env_guard_frame, build_guard_frame, collect_attribute_guard_frames,
    inject_guard_fields,
};
use crate::ast::enrich::macro_table::MacroTable;
use crate::ast::enrich::{EnrichContext, NodeEnricher};
use crate::ast::lang::LanguageSupport;
use crate::error::ForgeError;

use super::{IndexRow, SegmentBuildCtx, SymbolTable, node_text};
// -----------------------------------------------------------------------
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
    /// The symbol table being populated.
    pub table: &'a mut SymbolTable,
}
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
    let lang_name = ctx.language.name();
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
    let mut row_ordinal_counter: u32 = 0;
    // Parallel to parent_kind_stack: propagates the enclosing row's ordinal
    // to unnamed descendant nodes so they inherit their nearest named ancestor.
    let mut parent_ordinal_stack: Vec<u32> = Vec::new();

    loop {
        let node = cursor.node();
        // Tracks whether this iteration produced a named row (and its ordinal),
        // so goto_first_child can push the correct parent ordinal.
        let mut current_node_ordinal: Option<u32> = None;

        // --- Guard stack management ---
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
            let frame = build_guard_frame(node, source, config, &guard_stack);
            guard_stack.push(frame);
        }
        // Push a heuristic guard frame for env-guarded `if` nodes
        // (e.g. Python `if TYPE_CHECKING:` or `if sys.platform == "linux":`).
        if let Some(regex_set) = &env_guard_regex
            && ctx.language.map_kind(node.kind()) == Some("if")
            && let Some(frame) = build_env_guard_frame(node, source, config, regex_set)
        {
            guard_stack.push(frame);
        }
        // --- End guard stack management ---

        // Skip alternate conditional-compilation branches entirely.
        let skip = config.is_skip_kind(node.kind());

        if !skip {
            // Build the enrichment context once for this node.
            let enrich_ctx = EnrichContext {
                node,
                source,
                path: ctx.path,
                language_name: lang_name,
                language_config: config,
                language_support: ctx.language,
                guard_stack: &guard_stack,
                macro_table: ctx.macro_table,
                parent_kind: parent_kind_stack.last().copied().unwrap_or(""),
                inside_string: string_depth > 0,
                inside_error: error_depth > 0,
            };

            // Every named node becomes a row.
            if let Some(name) = ctx.language.extract_name(node, source) {
                let mut fields = extract_fields(node, source, ts_language);

                // Inject guard fields from the current block-guard stack.
                if !guard_stack.is_empty() {
                    inject_guard_fields(&guard_stack, &mut fields);
                }

                // Inject item-level attribute guards (e.g. Rust `#[cfg(...)]`).
                let attr_guard_name = config.item_guard_attribute();
                if !attr_guard_name.is_empty() {
                    let attr_frames = collect_attribute_guard_frames(node, source, attr_guard_name);
                    if !attr_frames.is_empty() {
                        inject_guard_fields(&attr_frames, &mut fields);
                    }
                }

                // Run all enrichers on this row.
                for enricher in ctx.enrichers {
                    enricher.enrich_row(&enrich_ctx, &name, &mut fields);
                }

                let fql_kind_val = ctx.language.map_kind(node.kind()).unwrap_or("");
                let (name_id, node_kind_id, fql_kind_id, language_id, path_id) = ctx
                    .table
                    .strings
                    .intern_row(&name, node.kind(), fql_kind_val, lang_name, ctx.path);
                // Assign a stable per-file DFS ordinal for node addressing.
                // Stored as an enrichment field so it flows through the segment
                // pipeline automatically and is queryable as WHERE ordinal = 'N'.
                let ordinal = row_ordinal_counter;
                row_ordinal_counter += 1;
                current_node_ordinal = Some(ordinal);
                let _ = fields.insert("ordinal".to_string(), ordinal.to_string());
                // Intern field keys+values before storing — converts the temporary
                // HashMap<String,String> enricher buffer into HashMap<u32,u32>.
                let fields = ctx.table.strings.intern_fields(fields);
                ctx.table.push_row(IndexRow {
                    name_id,
                    node_kind_id,
                    fql_kind_id,
                    language_id,
                    path_id,
                    byte_range: node.byte_range(),
                    line: node.start_position().row + 1,
                    usages_count: 0,
                    fields,
                });
            } else if let Some(mtable) = ctx.macro_table {
                // Re-tag: tree-sitter-cpp parses C macro calls as
                // call_expression, not macro_invocation.  When extract_name
                // returns None for a call_expression whose function name is
                // in the MacroTable, emit a macro_call row.
                let call_kind = config.call_expression_kind();
                if !call_kind.is_empty()
                    && node.kind() == call_kind
                    && let Some(func_node) = node.child_by_field_name("function")
                {
                    let func_name = node_text(source, func_node);
                    if !func_name.is_empty() && mtable.contains(&func_name) {
                        let mut fields = extract_fields(node, source, ts_language);

                        if !guard_stack.is_empty() {
                            inject_guard_fields(&guard_stack, &mut fields);
                        }
                        let attr_guard_name = config.item_guard_attribute();
                        if !attr_guard_name.is_empty() {
                            let attr_frames =
                                collect_attribute_guard_frames(node, source, attr_guard_name);
                            if !attr_frames.is_empty() {
                                inject_guard_fields(&attr_frames, &mut fields);
                            }
                        }

                        for enricher in ctx.enrichers {
                            enricher.enrich_row(&enrich_ctx, &func_name, &mut fields);
                        }

                        let (name_id, node_kind_id, fql_kind_id, language_id, path_id) = ctx
                            .table
                            .strings
                            .intern_row(&func_name, node.kind(), "macro_call", lang_name, ctx.path);
                        let fields = ctx.table.strings.intern_fields(fields);
                        ctx.table.push_row(IndexRow {
                            name_id,
                            node_kind_id,
                            fql_kind_id,
                            language_id,
                            path_id,
                            byte_range: node.byte_range(),
                            line: node.start_position().row + 1,
                            usages_count: 0,
                            fields,
                        });
                    }
                }
            }
            // Run extra_rows() for every node (even if extract_name returned None).
            for enricher in ctx.enrichers {
                for extra in enricher.extra_rows(&enrich_ctx) {
                    let extra_path = extra.path_override.as_deref().unwrap_or(ctx.path);
                    let (eni, enk, enf, enl, enp) = ctx.table.strings.intern_row(
                        &extra.name,
                        &extra.node_kind,
                        &extra.fql_kind,
                        lang_name,
                        extra_path,
                    );
                    let fields = ctx.table.strings.intern_fields(extra.fields);
                    ctx.table.push_row(IndexRow {
                        name_id: eni,
                        node_kind_id: enk,
                        fql_kind_id: enf,
                        language_id: enl,
                        path_id: enp,
                        byte_range: extra.byte_range,
                        line: extra.line,
                        usages_count: 0,
                        fields,
                    });
                }
            }

            // All identifier tokens become usage sites.
            if config.is_usage_node_kind(node.kind()) {
                let name = node_text(source, node);
                if name.len() > 1 {
                    let line = node.start_position().row + 1;
                    ctx.table.add_usage(name, ctx.path, node.byte_range(), line);
                }
            }

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
        let mut found_sibling = false;
        while cursor.goto_parent() {
            let _ = parent_ordinal_stack.pop();
            if let Some(popped) = parent_kind_stack.pop() {
                if config.is_opaque_string_kind(popped) || config.is_comment_kind(popped) {
                    string_depth = string_depth.saturating_sub(1);
                }
                if popped == "ERROR" {
                    error_depth = error_depth.saturating_sub(1);
                }
            }
            if cursor.goto_next_sibling() {
                found_sibling = true;
                break;
            }
        }
        if !found_sibling {
            break;
        }
    }
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
