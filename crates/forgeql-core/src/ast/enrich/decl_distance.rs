/// Declaration distance and dead-store enrichment.
///
/// `enrich_row()` adds to `function_definition` rows:
/// - `decl_distance`: sum of (`first_use` − `decl`) for locals with distance ≥ 2.
/// - `decl_far_count`: number of locals with distance ≥ 2.
/// - `decl_far_conditional`: `"true"` if any far-declared variable (`decl_distance ≥ 2`)
///   has its declaration or first use inside a branch/loop (conditionally
///   executed), so the distance measurement is unreliable.
/// - `has_unused_reassign`: `"true"` if any local is written twice without a
///   read in between at branch depth 0 (unconditional dead store).
/// - `dead_store_conditional`: `"true"` if a conditional dead store exists
///   (write-over-write where at least one write is inside a branch/loop).
/// - `has_unused_param`: `"true"` if any parameter is never referenced in
///   the body.
/// - `unused_param_count`: number of unused parameters.
/// - `unused_params`: comma-separated names of unused parameters.
use std::collections::{HashMap, HashSet};

use super::data_flow_utils::{
    LocalDecl, collect_local_declarations, collect_parameter_names, is_compound_assign_or_update,
    is_in_declaration, is_inside_parameter_list, is_write_context,
};
use super::guard_utils::{
    GuardFrame, GuardInfo, are_guards_exclusive, build_guard_frame, guard_info_from_stack,
};
use super::{EnrichContext, NodeEnricher};
use crate::ast::lang::LanguageConfig;

const FAR_THRESHOLD: usize = 2;

pub struct DeclDistanceEnricher;

impl NodeEnricher for DeclDistanceEnricher {
    fn name(&self) -> &'static str {
        "decl_distance"
    }

    fn enrich_row(
        &self,
        ctx: &EnrichContext<'_>,
        _name: &str,
        fields: &mut HashMap<String, String>,
    ) {
        let config = ctx.language_config;
        if !config.is_function_kind(ctx.node.kind()) {
            return;
        }

        let locals = collect_local_declarations(ctx);

        let AnalyseResult {
            first_uses,
            first_use_depths,
            has_dead_store,
            has_dead_store_conditional,
            seen_identifiers,
        } = analyse_uses(ctx, &locals);

        let mut total_distance: usize = 0;
        let mut far_count: usize = 0;
        let mut far_conditional_count: usize = 0;

        for local in &locals {
            if let Some(&first_use_line) = first_uses.get(local.name.as_str()) {
                let dist = first_use_line.saturating_sub(local.line);
                if dist >= FAR_THRESHOLD {
                    total_distance += dist;
                    far_count += 1;
                    let use_depth = first_use_depths
                        .get(local.name.as_str())
                        .copied()
                        .unwrap_or(0);
                    if local.branch_depth > 0 || use_depth > 0 {
                        far_conditional_count += 1;
                    }
                }
            }
        }

        drop(fields.insert("decl_distance".to_string(), total_distance.to_string()));
        drop(fields.insert("decl_far_count".to_string(), far_count.to_string()));
        drop(fields.insert(
            "decl_far_conditional".to_string(),
            (far_conditional_count > 0).to_string(),
        ));
        drop(fields.insert(
            "has_unused_reassign".to_string(),
            has_dead_store.to_string(),
        ));
        drop(fields.insert(
            "dead_store_conditional".to_string(),
            has_dead_store_conditional.to_string(),
        ));

        emit_unused_params(ctx.node, ctx.source, config, &seen_identifiers, fields);
    }
}

fn emit_unused_params(
    func: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    seen_identifiers: &HashSet<String>,
    fields: &mut HashMap<String, String>,
) {
    let params = collect_parameter_names(func, source, config);
    if params.is_empty() {
        return;
    }
    let mut unused: Vec<&str> = params
        .iter()
        .filter(|p| !seen_identifiers.contains(p.as_str()))
        .map(String::as_str)
        .collect();
    unused.sort_unstable();
    if !unused.is_empty() {
        drop(fields.insert("has_unused_param".into(), "true".into()));
        drop(fields.insert("unused_param_count".into(), unused.len().to_string()));
        drop(fields.insert("unused_params".into(), unused.join(",")));
    }
}

struct AnalyseResult<'a> {
    first_uses: HashMap<&'a str, usize>,
    first_use_depths: HashMap<&'a str, u32>,
    has_dead_store: bool,
    has_dead_store_conditional: bool,
    seen_identifiers: HashSet<String>,
}

/// Walk all identifier nodes in a function body to find first uses, detect
/// dead stores, and collect all referenced names.
///
/// Branch depth is tracked via a `depth_stack` maintained alongside the
/// cursor DFS — each entry records whether the level was entered by crossing
/// a branch/loop node.  No recursion is used.
#[allow(clippy::too_many_lines)] // guard stack management adds necessary lines
fn analyse_uses<'a>(ctx: &EnrichContext<'a>, locals: &[LocalDecl]) -> AnalyseResult<'a> {
    let source = ctx.source;
    let func = ctx.node;
    let config = ctx.language_config;

    let local_names: HashMap<&str, usize> =
        locals.iter().map(|d| (d.name.as_str(), d.line)).collect();

    let mut first_uses: HashMap<&str, usize> = HashMap::new();
    let mut first_use_depths: HashMap<&str, u32> = HashMap::new();
    let mut seen_identifiers: HashSet<String> = HashSet::new();
    // `(written, guard)` — tracks whether the last op was a write (not yet read),
    // and the guard in effect when the write occurred.  Guard info is used to
    // suppress false-positive dead-store reports for writes in exclusive branches.
    let mut written_not_read: HashMap<&str, (bool, Option<GuardInfo>)> = HashMap::new();
    let mut has_dead_store = false;
    let mut has_dead_store_conditional = false;

    // Only seed unconditional declarations that carry an initializer value
    // as "initially written".  Rules:
    //   1. branch_depth == 0: the declaration always executes — a conditional
    //      declaration may not run, so its "write" is not guaranteed.
    //   2. has_initializer: only a declaration *with* an initial value
    //      (e.g. `int x = 0;`) can produce a dead store if immediately
    //      overwritten.  A bare uninitialized declaration (e.g. `int x;` or
    //      `let x;`) has no value to preserve — the first write after it is
    //      always valid and must NOT be flagged as a dead store.
    for local in locals {
        if local.branch_depth == 0 && local.has_initializer {
            // Unconditional declarations have no guard (None).
            let _ = written_not_read.insert(local.name.as_str(), (true, None));
        }
    }

    let mut cursor = func.walk();
    let mut visit = true;
    // depth_stack[i] = true if crossing into level i required a branch/loop node.
    let mut depth_stack: Vec<bool> = Vec::new();
    let mut branch_depth: u32 = 0;
    // Mini guard stack: updated in lock-step with the cursor walk.
    let mut mini_guard_stack: Vec<GuardFrame> = Vec::new();
    loop {
        if visit {
            let node = cursor.node();
            let kind = node.kind();

            // --- Mini guard stack management ---
            if config.has_guard_support() {
                while let Some(top) = mini_guard_stack.last() {
                    if node.start_byte() >= top.guard_byte_range.end {
                        drop(mini_guard_stack.pop());
                    } else {
                        break;
                    }
                }
                if config.is_block_guard_kind(kind)
                    || config.is_elif_kind(kind)
                    || config.is_else_kind(kind)
                {
                    let frame = build_guard_frame(node, source, config, &mini_guard_stack);
                    mini_guard_stack.push(frame);
                }
            }

            // Macro expansion: when a call_expression matches a macro in
            // the table, expand it and register contained identifiers as
            // reads.  This prevents false dead-store positives for patterns
            // like `err = fn(); __ASSERT(err == 0);`.
            if let Some(table) = ctx.macro_table {
                let call_kind = config.call_expression_kind();
                if !call_kind.is_empty()
                    && kind == call_kind
                    && let Some(func_node) = node.child_by_field_name("function")
                {
                    let func_name =
                        std::str::from_utf8(&source[func_node.byte_range()]).unwrap_or("");
                    if let Some(expander) = ctx.language_support.macro_expander() {
                        let args = expander.extract_args(node, source);
                        let mut budget = super::macro_resolve::ExpansionBudget {
                            max_depth: 1,
                            max_steps: 1,
                            steps_remaining: 1,
                        };
                        if let Some(result) = super::macro_resolve::resolve_macro(
                            table,
                            func_name,
                            &args,
                            expander,
                            &mut budget,
                            0,
                        ) {
                            // Scan expanded text for local variable reads.
                            for &local_name in local_names.keys() {
                                if contains_word(&result.expanded, local_name) {
                                    let _ = seen_identifiers.insert(local_name.to_owned());
                                    // Mark as read — clears any pending dead-store flag.
                                    let _ = written_not_read.insert(local_name, (false, None));
                                }
                            }
                        }
                    }
                }
            }
            if config.is_identifier_kind(kind)
                && node != func
                && !is_inside_parameter_list(node, config)
            {
                let text = std::str::from_utf8(&source[node.byte_range()]).unwrap_or("");
                if !text.is_empty() {
                    let _ = seen_identifiers.insert(text.to_owned());

                    if let Some(&decl_line) = local_names.get(text) {
                        let use_line = node.start_position().row + 1;

                        // Skip the identifier that IS the declaration itself.
                        if use_line != decl_line || !is_in_declaration(node, config) {
                            // Record first use with its branch depth.
                            if let std::collections::hash_map::Entry::Vacant(e) =
                                first_uses.entry(text)
                            {
                                let _ = e.insert(use_line);
                                let _ = first_use_depths.insert(text, branch_depth);
                            }

                            let is_write = is_write_context(node, config);
                            let is_compound = is_compound_assign_or_update(node, config);

                            if is_compound {
                                // Compound assign / ++ / -- : read AND write.
                                let _ = written_not_read.insert(text, (false, None));
                            } else if is_write {
                                let prev = written_not_read.get(text).copied();
                                if let Some((true, prev_guard)) = prev {
                                    let write_guard = guard_info_from_stack(&mini_guard_stack);
                                    // Exclusive guard branches → not a dead store.
                                    if !guards_exclusive_opts(
                                        prev_guard.as_ref(),
                                        write_guard.as_ref(),
                                    ) {
                                        if branch_depth == 0 {
                                            has_dead_store = true;
                                        } else {
                                            has_dead_store_conditional = true;
                                        }
                                    }
                                }
                                let write_guard = guard_info_from_stack(&mini_guard_stack);
                                let _ = written_not_read.insert(text, (true, write_guard));
                            } else {
                                let _ = written_not_read.insert(text, (false, None));
                            }
                        }
                    }
                }
            }

            // Try to descend.  Record whether we are crossing a branch/loop boundary.
            let is_branch = config.is_branch_kind(kind) || config.is_loop_kind(kind);
            if cursor.goto_first_child() {
                depth_stack.push(is_branch);
                if is_branch {
                    branch_depth += 1;
                }
                continue;
            }
        }

        if cursor.goto_next_sibling() {
            visit = true;
            continue;
        }

        loop {
            if !cursor.goto_parent() {
                return AnalyseResult {
                    first_uses,
                    first_use_depths,
                    has_dead_store,
                    has_dead_store_conditional,
                    seen_identifiers,
                };
            }
            if let Some(was_branch) = depth_stack.pop()
                && was_branch
            {
                branch_depth = branch_depth.saturating_sub(1);
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Returns `true` if `a` and `b` are in structurally exclusive guard branches.
/// Returns `false` if either is unconditional (no guard).
#[inline]
fn guards_exclusive_opts(a: Option<&GuardInfo>, b: Option<&GuardInfo>) -> bool {
    match (a, b) {
        (Some(ga), Some(gb)) => are_guards_exclusive(ga, gb),
        _ => false,
    }
}

/// Word-boundary check: returns `true` if `haystack` contains `word` as a
/// whole word (not as a substring of a longer identifier).
fn contains_word(haystack: &str, word: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(word) {
        let abs = start + pos;
        let before_ok = abs == 0
            || !haystack.as_bytes()[abs - 1].is_ascii_alphanumeric()
                && haystack.as_bytes()[abs - 1] != b'_';
        let end = abs + word.len();
        let after_ok = end >= haystack.len()
            || !haystack.as_bytes()[end].is_ascii_alphanumeric()
                && haystack.as_bytes()[end] != b'_';
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}
