#![allow(clippy::must_use_candidate)]
/// Shared data-flow utilities for enrichers that inspect local variable
/// declarations, identifier usage, and write/read patterns within function
/// bodies.
///
/// Used by both `DeclDistanceEnricher` and `EscapeEnricher`.
use std::collections::HashSet;

use super::EnrichContext;
use crate::ast::index::node_text;
use crate::ast::lang::LanguageConfig;

/// A local variable: (name, 1-based declaration line).
pub type LocalDecl = (String, usize);

/// Collect all local variable declarations inside a function body.
///
/// Walks the function's direct body to find `declaration` nodes, extracts
/// the declarator name, and records its 1-based line.  Skips:
/// - Parameters (identified by `parameter_raw_kind`)
/// - Field declarations (member variables)
/// - Declarations that contain a function declarator (function pointer decls)
pub fn collect_local_declarations(ctx: &EnrichContext<'_>) -> Vec<LocalDecl> {
    let config = ctx.language_config;
    let func = ctx.node;
    let source = ctx.source;

    let mut locals = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = func.walk();
    let mut visit = true;

    loop {
        if visit {
            let node = cursor.node();
            let kind = node.kind();

            // Skip the function node itself.
            if node != func
                && config.is_declaration_kind(kind)
                && !is_inside_parameter_list(node, config)
                && let Some(name) = extract_declarator_name(node, source, config)
            {
                // First-seen-per-name: treat the first assignment as the
                // declaration point.  Subsequent assignments to the same
                // name are mutations, not new locals.
                if seen.insert(name.clone()) {
                    let line = node.start_position().row + 1; // 1-based
                    locals.push((name, line));
                }
            }
        }

        // DFS: descend, sibling, or ascend.
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
                return locals;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Check if a node is inside a parameter list (i.e. it IS a parameter).
pub fn is_inside_parameter_list(node: tree_sitter::Node<'_>, config: &LanguageConfig) -> bool {
    let mut parent = node.parent();
    while let Some(p) = parent {
        if config.is_parameter_list_kind(p.kind()) || config.is_parameter_kind(p.kind()) {
            return true;
        }
        // Stop at function boundary — don't walk above.
        if config.is_function_kind(p.kind()) {
            return false;
        }
        parent = p.parent();
    }
    false
}

/// Extract the identifier name from a declaration's declarator subtree.
///
/// Handles `int x`, `int x = ...`, `int *x`, `const int& x = ...` etc.
/// Returns `None` for function-declarator patterns (function pointer decls).
pub fn extract_declarator_name(
    decl_node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> Option<String> {
    // Resolve the name-carrying child of the declaration/assignment node.
    // C++ uses "declarator"; Rust uses "pattern"; Python uses "left".
    // When the language-specific field is empty, fall through common fields.
    let declarator = if !config.declarator_field().is_empty() {
        decl_node.child_by_field_name(config.declarator_field())
    } else {
        decl_node
            .child_by_field_name("pattern")
            .or_else(|| decl_node.child_by_field_name("left"))
            .or_else(|| decl_node.child_by_field_name("name"))
    }?;

    // Skip function pointer declarations.
    if !config.function_declarator().is_empty()
        && contains_kind(declarator, config.function_declarator())
    {
        return None;
    }

    // Drill down through nested declarators (init_declarator, pointer_declarator,
    // reference_declarator, etc.) to find the leaf identifier.
    find_leaf_identifier(declarator, source, config)
}

/// Recursively drill through declarator wrappers to find the leaf identifier.
pub fn find_leaf_identifier(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> Option<String> {
    // If this node is itself an identifier, return it.
    if config.is_identifier_kind(node.kind()) {
        let text = node_text(source, node);
        if !text.is_empty() {
            return Some(text);
        }
    }

    // Try the declarator field first (init_declarator, pointer_declarator, etc.).
    if let Some(child) = node.child_by_field_name(config.declarator_field()) {
        return find_leaf_identifier(child, source, config);
    }

    // Fallback: look for an identifier among direct children.
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i)
            && config.is_identifier_kind(child.kind())
        {
            let text = node_text(source, child);
            if !text.is_empty() {
                return Some(text);
            }
        }
    }

    None
}

/// Check if a subtree contains a node of the given kind.
pub fn contains_kind(node: tree_sitter::Node<'_>, target_kind: &str) -> bool {
    if node.kind() == target_kind {
        return true;
    }
    let mut cursor = node.walk();
    let mut visit = true;
    loop {
        if visit && cursor.node().kind() == target_kind && cursor.node() != node {
            return true;
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
                return false;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Check if `needle` is a descendant of (or equal to) `haystack`.
pub fn node_is_descendant_of(
    needle: tree_sitter::Node<'_>,
    haystack: tree_sitter::Node<'_>,
) -> bool {
    let nr = needle.byte_range();
    let hr = haystack.byte_range();
    nr.start >= hr.start && nr.end <= hr.end
}

/// Check if an identifier node is part of a declaration (i.e. the declarator
/// itself, not a reference).
pub fn is_in_declaration(node: tree_sitter::Node<'_>, config: &LanguageConfig) -> bool {
    let mut parent = node.parent();
    while let Some(p) = parent {
        let kind = p.kind();
        if config.is_declaration_kind(kind)
            || config.is_init_declarator_kind(kind)
            || config.is_parameter_kind(kind)
        {
            // Check if the identifier is on the declarator/LHS side, not the value side.
            let decl_child = if !config.declarator_field().is_empty() {
                p.child_by_field_name(config.declarator_field())
            } else {
                p.child_by_field_name("pattern")
                    .or_else(|| p.child_by_field_name("left"))
                    .or_else(|| p.child_by_field_name("name"))
            };
            if let Some(dc) = decl_child {
                return node_is_descendant_of(node, dc);
            }
            return true;
        }
        // Stop at statement/block boundaries.
        if config.is_statement_boundary_kind(kind) || config.is_block_kind(kind) {
            return false;
        }
        parent = p.parent();
    }
    false
}

/// Check if an identifier is in a write context (left side of `=`).
pub fn is_write_context(node: tree_sitter::Node<'_>, config: &LanguageConfig) -> bool {
    if let Some(parent) = node.parent() {
        let pk = parent.kind();

        // Simple assignment: `x = expr`
        if config.is_assignment_kind(pk)
            && let Some(left) = parent.child_by_field_name("left")
        {
            return left.id() == node.id();
        }

        // update_expression: `++x` or `x++`
        if config.is_update_kind(pk) {
            return true;
        }
    }
    false
}

/// Check if an identifier is in a compound-assignment (`+=`, `-=`, etc.)
/// or update expression (`++`, `--`) — these are reads AND writes.
pub fn is_compound_assign_or_update(node: tree_sitter::Node<'_>, config: &LanguageConfig) -> bool {
    if let Some(parent) = node.parent() {
        let pk = parent.kind();

        if config.is_update_kind(pk) {
            return true;
        }

        if config.is_assignment_kind(pk)
            && let Some(left) = parent.child_by_field_name("left")
            && left.id() == node.id()
            && let Some(op) = parent.child_by_field_name("operator")
        {
            // Compound if operator is not plain `=` (byte length > 1).
            let op_range = op.byte_range();
            return op_range.end - op_range.start > 1;
        }
    }
    false
}
