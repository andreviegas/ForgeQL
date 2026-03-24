/// Shadow enrichment — detects functions where an inner scope
/// redeclares a variable name that already exists in an outer scope.
///
/// `enrich_row()` adds to `function_definition` rows:
/// - `has_shadow`:   `"true"` if any shadowed variable was detected.
/// - `shadow_count`: number of distinct shadowed variable names.
/// - `shadow_vars`:  comma-separated names of shadowed variables.
///
/// A "shadow" occurs when a declaration in a nested `compound_statement`
/// (or language-equivalent block) uses the same identifier as a
/// declaration in an enclosing scope of the same function.  Parameters
/// are treated as the outermost scope.
///
/// **Language-agnostic:** uses `block_raw_kind`, `declaration_raw_kinds`,
/// `function_raw_kinds`, `parameter_raw_kind`, and
/// `parameter_list_raw_kind` from [`LanguageConfig`].
use std::collections::{BTreeSet, HashMap};

use super::data_flow_utils::{extract_declarator_name, find_leaf_identifier};
use super::{EnrichContext, NodeEnricher};
use crate::ast::lang::LanguageConfig;

/// Enricher for variable shadowing detection.
pub struct ShadowEnricher;

impl NodeEnricher for ShadowEnricher {
    fn name(&self) -> &'static str {
        "shadow"
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

        let source = ctx.source;
        let func = ctx.node;

        // Collect parameter names as the outermost scope.
        let params = collect_parameter_names(func, source, config);

        // Walk the function body looking for nested scope shadows.
        let Some(body) = func.child_by_field_name("body") else {
            return;
        };

        let mut shadowed: BTreeSet<String> = BTreeSet::new();
        // Start with parameter names as the initial outer scope.
        let mut scope_stack: Vec<BTreeSet<String>> = vec![params];

        walk_scopes(body, source, config, &mut scope_stack, &mut shadowed);

        if !shadowed.is_empty() {
            drop(fields.insert("has_shadow".into(), "true".into()));
            drop(fields.insert("shadow_count".into(), shadowed.len().to_string()));
            let vars: Vec<&str> = shadowed.iter().map(String::as_str).collect();
            drop(fields.insert("shadow_vars".into(), vars.join(",")));
        }
    }
}

/// Collect all parameter names from a function's parameter list.
fn collect_parameter_names(
    func: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();

    // The parameter list may be nested inside a function_declarator
    // (e.g. in C++: function_definition → function_declarator → parameter_list),
    // so search recursively instead of looking at direct children only.
    let Some(param_list) = find_descendant_by_kind(func, config.parameter_list_raw_kind) else {
        return names;
    };

    for i in 0..param_list.child_count() {
        if let Some(child) = param_list.child(i)
            && config.is_parameter_kind(child.kind())
        {
            // Try to extract the parameter name from the declarator.
            if let Some(decl) = child.child_by_field_name(config.declarator_field())
                && let Some(name) = find_leaf_identifier(decl, source, config)
            {
                let _ = names.insert(name);
            }
        }
    }
    names
}

/// Recursively walk a scope (block node), collecting declarations and
/// detecting shadows against outer scopes.
fn walk_scopes(
    block: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    scope_stack: &mut Vec<BTreeSet<String>>,
    shadowed: &mut BTreeSet<String>,
) {
    // Declarations in THIS scope.
    let mut current_scope = BTreeSet::new();

    for i in 0..block.child_count() {
        let Some(child) = block.child(i) else {
            continue;
        };

        let kind = child.kind();

        // If this child is a declaration, extract its name.
        if config.is_declaration_kind(kind) {
            if let Some(name) = extract_declarator_name(child, source, config) {
                // Check against all outer scopes.
                if is_in_outer_scope(&name, scope_stack) {
                    let _ = shadowed.insert(name.clone());
                }
                let _ = current_scope.insert(name);
            }
        } else {
            // If this child contains a nested scope, recurse.
            // Many statements (if, for, while, etc.) contain compound_statement
            // children that form a new scope.
            visit_nested_scopes(child, source, config, scope_stack, &current_scope, shadowed);
        }
    }
}

/// Check if a name exists in any of the outer scopes in the stack.
fn is_in_outer_scope(name: &str, scope_stack: &[BTreeSet<String>]) -> bool {
    scope_stack.iter().any(|scope| scope.contains(name))
}

/// Find a descendant node of the given kind (BFS, stops at first match).
fn find_descendant_by_kind<'a>(
    root: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = root.walk();
    let mut visit = true;
    loop {
        if visit && cursor.node().kind() == kind && cursor.node() != root {
            return Some(cursor.node());
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
                return None;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Visit all nested scopes inside a node.
///
/// Handles:
/// - `compound_statement` blocks → recurse as a new scope
/// - Declarations inside non-block contexts (for-loop initializers) →
///   check for shadowing against outer + current scopes
/// - Other nodes → recurse into children
fn visit_nested_scopes(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    scope_stack: &mut Vec<BTreeSet<String>>,
    current_scope: &BTreeSet<String>,
    shadowed: &mut BTreeSet<String>,
) {
    // If the node itself is a block, recurse as a new scope.
    if config.is_block_kind(node.kind()) {
        scope_stack.push(current_scope.clone());
        walk_scopes(node, source, config, scope_stack, shadowed);
        drop(scope_stack.pop());
        return;
    }

    // Declarations inside non-block parents (e.g. for-loop initializer:
    // `for (int i = 0; ...)` — the `declaration` is a child of `for_statement`,
    // not of a compound_statement).  These are in an implicit nested scope.
    if config.is_declaration_kind(node.kind())
        && let Some(name) = extract_declarator_name(node, source, config)
    {
        if is_in_outer_scope(&name, scope_stack) || current_scope.contains(&name) {
            let _ = shadowed.insert(name);
        }
        return;
    }

    // Recurse into children.
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            visit_nested_scopes(child, source, config, scope_stack, current_scope, shadowed);
        }
    }
}
