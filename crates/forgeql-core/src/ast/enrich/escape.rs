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
    collect_local_declarations, contains_kind, is_in_declaration, is_inside_parameter_list,
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
        if !config.function_raw_kinds.contains(&ctx.node.kind()) {
            return;
        }

        // Short-circuit: language has no address-of operator → no escape possible.
        if config.address_of_operator.is_empty() {
            return;
        }

        // Phase 1: Collect local variable declarations.
        let locals = collect_local_declarations(ctx);
        if locals.is_empty() {
            return;
        }

        let local_names: HashSet<&str> = locals.iter().map(|(n, _)| n.as_str()).collect();

        // Phase 2: Identify which locals are arrays.
        let array_locals = collect_array_locals(ctx, &local_names);

        // Phase 3: Identify which locals are static (safe — exclude them).
        let static_locals = collect_static_locals(ctx);

        // Phase 4: Build alias map (Tier 3): track `ptr = &local` assignments.
        let alias_map = build_alias_map(ctx, &local_names, &static_locals);

        // Phase 5: Walk all return statements and detect escaping patterns.
        let mut escaping: Vec<String> = Vec::new();
        let mut best_tier: u8 = 0;
        let mut kinds_seen: HashSet<&str> = HashSet::new();

        walk_dfs(ctx.node, |node| {
            if node.kind() != config.return_statement_raw_kind {
                return;
            }

            // Find the returned expression (first non-keyword child, or named child).
            let Some(ret_expr) = find_return_expr(node) else {
                return;
            };

            // Recurse into the expression to find escaping patterns even
            // inside ternary/conditional expressions, casts, parenthesised
            // expressions, etc.
            check_expr_escape(
                ret_expr,
                ctx,
                &local_names,
                &array_locals,
                &static_locals,
                &alias_map,
                &mut escaping,
                &mut best_tier,
                &mut kinds_seen,
            );
        });

        if escaping.is_empty() {
            return;
        }

        // Deduplicate while preserving order.
        let mut seen = HashSet::new();
        escaping.retain(|v| seen.insert(v.clone()));

        drop(fields.insert("has_escape".to_string(), "true".to_string()));
        drop(fields.insert("escape_tier".to_string(), best_tier.to_string()));
        drop(fields.insert("escape_vars".to_string(), escaping.join(",")));
        drop(fields.insert("escape_count".to_string(), escaping.len().to_string()));

        // Build sorted, deterministic escape_kinds.
        let mut kinds: Vec<&str> = kinds_seen.into_iter().collect();
        kinds.sort_unstable();
        drop(fields.insert("escape_kinds".to_string(), kinds.join(",")));
    }
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// Recursively check an expression for escaping patterns.
#[allow(clippy::too_many_arguments, clippy::collapsible_if)]
fn check_expr_escape(
    node: tree_sitter::Node<'_>,
    ctx: &EnrichContext<'_>,
    local_names: &HashSet<&str>,
    array_locals: &HashSet<String>,
    static_locals: &HashSet<String>,
    alias_map: &HashMap<String, String>,
    escaping: &mut Vec<String>,
    best_tier: &mut u8,
    kinds_seen: &mut HashSet<&str>,
) {
    let config = ctx.language_config;
    let source = ctx.source;

    // Tier 1: direct address-of  →  return &local
    if node.kind() == config.address_of_expression_raw_kind {
        if let Some(op_child) = node.child(0) {
            let op_text = node_text(source, op_child);
            if op_text == config.address_of_operator {
                if let Some(operand) = node.child(1) {
                    let name = resolve_identifier(operand, source, config);
                    if let Some(name) = name {
                        if local_names.contains(name.as_str()) && !static_locals.contains(&name) {
                            escaping.push(name);
                            let _ = kinds_seen.insert("address_of");
                            if *best_tier == 0 || *best_tier > 1 {
                                *best_tier = 1;
                            }
                            return;
                        }
                    }
                }
            }
        }
    }

    // Tier 2: array decay  →  return local_array
    if node.kind() == config.identifier_raw_kind {
        let name = node_text(source, node);
        if array_locals.contains(&name)
            && !static_locals.contains(&name)
            && !is_in_declaration(node, config)
        {
            escaping.push(name);
            let _ = kinds_seen.insert("array_decay");
            if *best_tier == 0 || *best_tier > 2 {
                *best_tier = 2;
            }
            return;
        }
    }

    // Tier 3: indirect alias  →  return ptr  where ptr was assigned &local
    if node.kind() == config.identifier_raw_kind {
        let name = node_text(source, node);
        if let Some(target) = alias_map.get(&name) {
            if !is_in_declaration(node, config) {
                escaping.push(target.clone());
                let _ = kinds_seen.insert("alias");
                if *best_tier == 0 {
                    *best_tier = 3;
                }
                return;
            }
        }
    }

    // Recurse into child expressions (ternary, casts, parenthesised, etc.)
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            check_expr_escape(
                child,
                ctx,
                local_names,
                array_locals,
                static_locals,
                alias_map,
                escaping,
                best_tier,
                kinds_seen,
            );
        }
    }
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
    if node.kind() == config.identifier_raw_kind {
        let text = node_text(source, node);
        if !text.is_empty() {
            return Some(text);
        }
    }
    // Parenthesised: `return &(local)` — recurse.
    if !config.parenthesized_expression_raw_kind.is_empty()
        && node.kind() == config.parenthesized_expression_raw_kind
    {
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
    if node.kind() != config.address_of_expression_raw_kind {
        return None;
    }
    let op_child = node.child(0)?;
    let op_text = node_text(source, op_child);
    if op_text != config.address_of_operator {
        return None;
    }
    let operand = node.child(1)?;
    resolve_identifier(operand, source, config)
}

/// Collect the names of local variables declared with array declarators.
#[allow(clippy::collapsible_if)]
fn collect_array_locals(ctx: &EnrichContext<'_>, local_names: &HashSet<&str>) -> HashSet<String> {
    let config = ctx.language_config;
    if config.array_declarator_raw_kind.is_empty() {
        return HashSet::new();
    }

    let mut arrays = HashSet::new();
    let mut cursor = ctx.node.walk();
    let mut visit = true;

    loop {
        if visit {
            let node = cursor.node();
            let kind = node.kind();

            if node != ctx.node
                && config.declaration_raw_kinds.contains(&kind)
                && !is_inside_parameter_list(node, config)
            {
                // Check if the declarator subtree contains an array_declarator.
                if let Some(decl) = node.child_by_field_name(config.declarator_field_name) {
                    if contains_kind(decl, config.array_declarator_raw_kind) {
                        // Extract the name of this declaration.
                        if let Some(name) = super::data_flow_utils::extract_declarator_name(
                            node, ctx.source, config,
                        ) {
                            if local_names.contains(name.as_str()) {
                                let _ = arrays.insert(name);
                            }
                        }
                    }
                }
            }
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
                return arrays;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Collect the names of local variables declared with `static` storage class.
#[allow(clippy::collapsible_if)]
fn collect_static_locals(ctx: &EnrichContext<'_>) -> HashSet<String> {
    let config = ctx.language_config;
    if config.static_storage_keywords.is_empty() {
        return HashSet::new();
    }

    let mut statics = HashSet::new();
    let mut cursor = ctx.node.walk();
    let mut visit = true;

    loop {
        if visit {
            let node = cursor.node();
            let kind = node.kind();

            if node != ctx.node
                && config.declaration_raw_kinds.contains(&kind)
                && !is_inside_parameter_list(node, config)
            {
                if has_static_specifier(node, ctx.source, config) {
                    if let Some(name) =
                        super::data_flow_utils::extract_declarator_name(node, ctx.source, config)
                    {
                        let _ = statics.insert(name);
                    }
                }
            }
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
                return statics;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
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
            if config.static_storage_keywords.contains(&text.as_str()) {
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
        if config.assignment_raw_kinds.contains(&kind) {
            let Some(left) = node.child_by_field_name("left") else {
                return;
            };
            let Some(right) = node.child_by_field_name("right") else {
                return;
            };

            // Left side must be a simple identifier.
            if left.kind() != config.identifier_raw_kind {
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
        if !config.init_declarator_raw_kind.is_empty() && kind == config.init_declarator_raw_kind {
            // Extract the declared name from the declarator subtree.
            let Some(decl_child) = node.child_by_field_name(config.declarator_field_name) else {
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
