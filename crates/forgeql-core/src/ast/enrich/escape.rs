/// Escape enrichment — detects functions that return addresses of
/// stack-local variables (dangling pointer risk).
///
/// `enrich_row()` adds to `function_definition` rows:
/// - `has_escape`: `"true"` if any escaping local address was detected.
/// - `escape_tier`: highest-certainty tier triggered:
///   `1` = direct `return &local`,
///   `2` = array decay (`return local_array`),
///   `3` = indirect alias (`ptr = &local; return ptr`).
/// - `escape_vars`: comma-separated names of escaping variables.
///
/// **Exclusions:** `static` locals (they have static storage duration and are
/// safe to return), parameters, globals.
///
/// **Tier certainty:**
/// - Tier 1 (direct address-of) — 100% certain.
/// - Tier 2 (array decay) — 100% certain.
/// - Tier 3 (indirect alias) — heuristic, intra-procedural only.
use std::collections::{HashMap, HashSet};

use super::data_flow_utils::{
    LocalDecl, contains_kind, for_each_local_declaration, is_in_declaration, local_decl_from,
};
use super::{EnrichContext, NodeEnricher};
use crate::ast::index::node_text;
use crate::ast::lang::LanguageConfig;

/// Enricher for escaping local address detection.
pub struct EscapeEnricher;

impl NodeEnricher for EscapeEnricher {
    fn name(&self) -> &'static str {
        "escape"
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

        // Short-circuit: language has no address-of operator → no escape possible.
        if !config.has_address_of() {
            return;
        }

        // Phases 1-3 in ONE declaration walk: the locals, the array-typed
        // locals and the static locals were three traversals with the same
        // node filter, so they are now gathered together.
        let scan = scan_declarations(ctx);
        if scan.locals.is_empty() {
            return;
        }
        let locals = scan.locals;
        let local_names: HashSet<&str> = locals.iter().map(|d| d.name.as_str()).collect();
        let static_locals = scan.statics;
        // An array candidate is by construction also a local, but keeping the
        // restriction preserves the original set exactly.
        let array_locals: HashSet<String> = scan
            .array_candidates
            .into_iter()
            .filter(|n| local_names.contains(n.as_str()))
            .collect();

        // Phase 4: the alias map needs the complete local and static sets, and
        // a later assignment removes an alias an earlier one added, so this
        // stays a separate ordered pass.
        let alias_map = build_alias_map(ctx, &local_names, &static_locals);

        // Phase 5: scan return statements; Phase 5b: scan macro expansions.
        let escape_locals = EscapeLocals {
            local_names: &local_names,
            array_locals: &array_locals,
            static_locals: &static_locals,
            alias_map: &alias_map,
        };
        let mut acc = EscapeAccumulator::new();
        scan_return_escapes(ctx, config, &escape_locals, &mut acc);
        scan_macro_escapes(ctx, config, &local_names, &mut acc);

        if acc.escaping.is_empty() {
            return;
        }

        // Deduplicate while preserving order.
        let mut seen = HashSet::new();
        acc.escaping.retain(|v| seen.insert(v.clone()));

        drop(fields.insert("has_escape".to_string(), "true".to_string()));
        drop(fields.insert("escape_tier".to_string(), acc.best_tier.to_string()));
        drop(fields.insert("escape_vars".to_string(), acc.escaping.join(",")));
        drop(fields.insert("escape_count".to_string(), acc.escaping.len().to_string()));

        // Build sorted, deterministic escape_kinds.
        let mut kinds: Vec<&str> = acc.kinds_seen.iter().copied().collect();
        kinds.sort_unstable();
        drop(fields.insert("escape_kinds".to_string(), kinds.join(",")));
    }
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// The four read-only input sets shared across all nodes in one escape-check walk.
struct EscapeLocals<'a> {
    local_names: &'a HashSet<&'a str>,
    array_locals: &'a HashSet<String>,
    static_locals: &'a HashSet<String>,
    alias_map: &'a HashMap<String, String>,
}

/// Mutable accumulator that collects escape detections during the walk.
struct EscapeAccumulator {
    escaping: Vec<String>,
    best_tier: u8,
    kinds_seen: HashSet<&'static str>,
}

impl EscapeAccumulator {
    fn new() -> Self {
        Self {
            escaping: Vec::new(),
            best_tier: 0,
            kinds_seen: HashSet::new(),
        }
    }
}

/// Recursively check an expression for escaping patterns.
#[allow(clippy::collapsible_if)]
fn check_expr_escape(
    node: tree_sitter::Node<'_>,
    ctx: &EnrichContext<'_>,
    locals: &EscapeLocals<'_>,
    acc: &mut EscapeAccumulator,
) {
    let config = ctx.language_config;
    let source = ctx.source;

    // Tier 1: direct address-of  →  return &local
    if config.is_address_of_expression_kind(node.kind()) {
        if let Some(op_child) = node.child(0) {
            let op_text = node_text(source, op_child);
            if op_text == config.address_of_op() {
                if let Some(operand) = node.child(1) {
                    let name = resolve_identifier(operand, source, config);
                    if let Some(name) = name {
                        if locals.local_names.contains(name.as_str())
                            && !locals.static_locals.contains(&name)
                        {
                            acc.escaping.push(name);
                            let _ = acc.kinds_seen.insert("address_of");
                            if acc.best_tier == 0 || acc.best_tier > 1 {
                                acc.best_tier = 1;
                            }
                            return;
                        }
                    }
                }
            }
        }
    }

    // Tier 2: array decay  →  return local_array
    if config.is_identifier_kind(node.kind()) {
        let name = node_text(source, node);
        if locals.array_locals.contains(&name)
            && !locals.static_locals.contains(&name)
            && !is_in_declaration(node, config)
        {
            acc.escaping.push(name);
            let _ = acc.kinds_seen.insert("array_decay");
            if acc.best_tier == 0 || acc.best_tier > 2 {
                acc.best_tier = 2;
            }
            return;
        }
    }

    // Tier 3: indirect alias  →  return ptr  where ptr was assigned &local
    if config.is_identifier_kind(node.kind()) {
        let name = node_text(source, node);
        if let Some(target) = locals.alias_map.get(&name) {
            if !is_in_declaration(node, config) {
                acc.escaping.push(target.clone());
                let _ = acc.kinds_seen.insert("alias");
                if acc.best_tier == 0 {
                    acc.best_tier = 3;
                }
                return;
            }
        }
    }

    // Recurse into child expressions (ternary, casts, parenthesised, etc.)
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            check_expr_escape(child, ctx, locals, acc);
        }
    }
}

/// Phase 5: walk every `return` statement and flag locals that escape through
/// the returned expression (including via casts, ternaries, parentheses).
fn scan_return_escapes(
    ctx: &EnrichContext<'_>,
    config: &LanguageConfig,
    locals: &EscapeLocals<'_>,
    acc: &mut EscapeAccumulator,
) {
    walk_dfs(ctx.node, |node| {
        if !config.is_return_statement_kind(node.kind()) {
            return;
        }
        // Find the returned expression (first non-keyword child, or named child).
        let Some(ret_expr) = find_return_expr(node) else {
            return;
        };
        check_expr_escape(ret_expr, ctx, locals, acc);
    });
}

/// Phase 5b: detect locals whose address escapes through a macro expansion — a
/// macro call in the body that expands to text containing `&<local>`.
fn scan_macro_escapes(
    ctx: &EnrichContext<'_>,
    config: &LanguageConfig,
    local_names: &HashSet<&str>,
    acc: &mut EscapeAccumulator,
) {
    let Some(table) = ctx.macro_table else {
        return;
    };
    let Some(expander) = ctx.language_support.macro_expander() else {
        return;
    };
    let call_kind = config.call_expression_kind();
    if call_kind.is_empty() {
        return;
    }
    // Sorted once, outside the walk: `escape_vars` is emitted in discovery
    // order, so iterating the set directly spelled the same names in a
    // different order on every run — a HashSet is seeded per process. The
    // sibling `escape_kinds` below is already sorted for this reason.
    let mut locals_sorted: Vec<&str> = local_names.iter().copied().collect();
    locals_sorted.sort_unstable();

    walk_dfs(ctx.node, |node| {
        if node.kind() != call_kind {
            return;
        }
        let Some(func_node) = node.child_by_field_name("function") else {
            return;
        };
        let func_name = node_text(ctx.source, func_node);
        let args = expander.extract_args(node, ctx.source);
        let mut budget = super::macro_resolve::ExpansionBudget {
            max_depth: 1,
            max_steps: 1,
            steps_remaining: 1,
        };
        if let Some(result) =
            super::macro_resolve::resolve_macro(table, &func_name, &args, expander, &mut budget, 0)
        {
            for &local_name in &locals_sorted {
                let pattern = format!("&{local_name}");
                if result.expanded.contains(&pattern) {
                    acc.escaping.push(local_name.to_string());
                    let _ = acc.kinds_seen.insert("address_of");
                    if acc.best_tier == 0 || acc.best_tier > 2 {
                        acc.best_tier = 2;
                    }
                }
            }
        }
    });
}

/// Find the first named child of a return statement that is the returned
/// expression (skip keyword tokens like `return`).
fn find_return_expr(ret_node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    for i in 0..ret_node.named_child_count() {
        if let Some(child) = ret_node.named_child(i) {
            return Some(child);
        }
    }
    None
}

/// Resolve an expression to an identifier name.  If the node is already an
/// identifier, return its text.  If it's a parenthesised expression, recurse.
fn resolve_identifier(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> Option<String> {
    if config.is_identifier_kind(node.kind()) {
        let text = node_text(source, node);
        if !text.is_empty() {
            return Some(text);
        }
    }
    // Parenthesised: `return &(local)` — recurse.
    if config.is_parenthesized_expression_kind(node.kind()) {
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                return resolve_identifier(child, source, config);
            }
        }
    }
    None
}

/// If `node` is an address-of expression (`&ident`), return the identifier name.
fn extract_address_of_target(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> Option<String> {
    if !config.is_address_of_expression_kind(node.kind()) {
        return None;
    }
    let op_child = node.child(0)?;
    let op_text = node_text(source, op_child);
    if op_text != config.address_of_op() {
        return None;
    }
    let operand = node.child(1)?;
    resolve_identifier(operand, source, config)
}

/// Everything one declaration walk of a function body yields.
struct DeclScan {
    /// Local declarations, first-seen per name, in DFS order.
    locals: Vec<LocalDecl>,
    /// Names whose declarator subtree contains an array declarator.
    array_candidates: HashSet<String>,
    /// Names declared with a static storage specifier.
    statics: HashSet<String>,
}

/// Collect the locals, the array-typed locals and the static locals in ONE
/// traversal of the function body.
///
/// These were three separate walks — `collect_local_declarations`,
/// `collect_array_locals` and `collect_static_locals` — over the same subtree
/// with the same node filter (`is_declaration_kind`, not the function node
/// itself, not inside a parameter list). Merging them is behaviour-preserving:
/// every per-node test below is the one its own walk applied, and the two
/// language gates that used to skip a whole walk now skip that walk's per-node
/// work instead, yielding the same empty set.
fn scan_declarations(ctx: &EnrichContext<'_>) -> DeclScan {
    let config = ctx.language_config;
    let want_arrays = config.has_array_declarator();
    let want_statics = config.has_static_storage();

    let mut locals = Vec::new();
    let mut seen = HashSet::new();
    let mut array_candidates = HashSet::new();
    let mut statics = HashSet::new();

    for_each_local_declaration(ctx, |node, name| {
        if seen.insert(name.to_string()) {
            locals.push(local_decl_from(ctx, node, name));
        }

        if want_arrays
            && let Some(decl) = node.child_by_field_name(config.declarator_field())
            && contains_kind(decl, config.array_declarator_kind())
        {
            let _ = array_candidates.insert(name.to_string());
        }

        if want_statics && has_static_specifier(node, ctx.source, config) {
            let _ = statics.insert(name.to_string());
        }
    });

    DeclScan {
        locals,
        array_candidates,
        statics,
    }
}

/// Check if a declaration node has a `static` storage class specifier.
fn has_static_specifier(
    decl_node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> bool {
    for i in 0..decl_node.child_count() {
        if let Some(child) = decl_node.child(i) {
            let text = node_text(source, child);
            if config.is_static_storage_keyword(&text) {
                return true;
            }
        }
    }
    false
}

/// Build an alias map: identifier → local variable name, for assignments
/// of the form `ptr = &local_var`.
#[allow(clippy::collapsible_if)]
fn build_alias_map(
    ctx: &EnrichContext<'_>,
    local_names: &HashSet<&str>,
    static_locals: &HashSet<String>,
) -> HashMap<String, String> {
    let config = ctx.language_config;
    let source = ctx.source;
    let mut aliases: HashMap<String, String> = HashMap::new();

    walk_dfs(ctx.node, |node| {
        let kind = node.kind();

        // Case 1: assignment_expression  →  `ptr = &local`
        if config.is_assignment_kind(kind) {
            let Some(left) = node.child_by_field_name("left") else {
                return;
            };
            let Some(right) = node.child_by_field_name("right") else {
                return;
            };

            // Left side must be a simple identifier.
            if !config.is_identifier_kind(left.kind()) {
                return;
            }
            let lhs_name = node_text(source, left);

            // Check if this is a simple `=` (not compound).
            if let Some(op) = node.child_by_field_name("operator") {
                let op_range = op.byte_range();
                if op_range.end - op_range.start > 1 {
                    return; // Compound assign, skip.
                }
            }

            if let Some(target) = extract_address_of_target(right, source, config) {
                if local_names.contains(target.as_str()) && !static_locals.contains(&target) {
                    drop(aliases.insert(lhs_name, target));
                    return;
                }
            }

            // If RHS is something else, kill the alias (reassigned away).
            drop(aliases.remove(&lhs_name));
            return;
        }

        // Case 2: init_declarator  →  `int *p = &local`
        if config.is_init_declarator_kind(kind) {
            // Extract the declared name from the declarator subtree.
            let Some(decl_child) = node.child_by_field_name(config.declarator_field()) else {
                return;
            };
            let Some(name) =
                super::data_flow_utils::find_leaf_identifier(decl_child, source, config)
            else {
                return;
            };

            // Check if the value child is an address-of expression.
            let Some(value) = node.child_by_field_name("value") else {
                return;
            };
            if let Some(target) = extract_address_of_target(value, source, config) {
                if local_names.contains(target.as_str()) && !static_locals.contains(&target) {
                    drop(aliases.insert(name, target));
                }
            }
        }
    });

    aliases
}

/// Walk all nodes in the subtree rooted at `root` in DFS pre-order,
/// calling `f` for each node.
fn walk_dfs(root: tree_sitter::Node<'_>, mut f: impl FnMut(tree_sitter::Node<'_>)) {
    let mut cursor = root.walk();
    let mut visit = true;

    loop {
        if visit {
            f(cursor.node());
        }

        if visit && cursor.goto_first_child() {
            visit = true;
            continue;
        }
        if cursor.goto_next_sibling() {
            visit = true;
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}
