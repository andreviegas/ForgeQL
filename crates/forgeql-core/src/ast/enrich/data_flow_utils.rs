#![allow(clippy::must_use_candidate)]
/// Shared data-flow utilities for enrichers that inspect local variable
/// declarations, identifier usage, and write/read patterns within function
/// bodies.
///
/// Used by `DeclDistanceEnricher`, `EscapeEnricher`, and `ShadowEnricher`.
use std::collections::HashSet;

use super::EnrichContext;
use crate::ast::index::node_text;
use crate::ast::lang::LanguageConfig;

/// A local variable declaration annotated with branch depth.
pub struct LocalDecl {
    /// Variable name.
    pub name: String,
    /// 1-based declaration line.
    pub line: usize,
    /// Number of branch/loop ancestor nodes between this declaration and the
    /// enclosing function body.  Zero means the declaration is unconditional
    /// (always executed on every call).
    pub branch_depth: u32,
}

/// Collect all local variable declarations inside a function body.
///
/// Walks the function's entire subtree to find `declaration` nodes, extracts
/// the declarator name, and records its 1-based line and branch depth.
/// Skips:
/// - Parameters (inside the parameter list)
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

            if node != func
                && config.is_declaration_kind(kind)
                && !is_inside_parameter_list(node, config)
                && let Some(name) = extract_declarator_name(node, source, config)
            {
                // First-seen-per-name: the first assignment is the declaration.
                // Later assignments to the same name are mutations, not new locals.
                if seen.insert(name.clone()) {
                    let line = node.start_position().row + 1;
                    let branch_depth = count_node_branch_depth(node, func, config);
                    locals.push(LocalDecl {
                        name,
                        line,
                        branch_depth,
                    });
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
                return locals;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Count the number of `branch_kind` or `loop_kind` ancestor nodes between
/// `node` and `ancestor` (exclusive on both ends).  Iterative — no recursion.
pub fn count_node_branch_depth(
    node: tree_sitter::Node<'_>,
    ancestor: tree_sitter::Node<'_>,
    config: &LanguageConfig,
) -> u32 {
    let mut depth = 0u32;
    let mut parent = node.parent();
    while let Some(p) = parent {
        if p.id() == ancestor.id() {
            break;
        }
        if config.is_branch_kind(p.kind()) || config.is_loop_kind(p.kind()) {
            depth += 1;
        }
        parent = p.parent();
    }
    depth
}

/// Collect all parameter names from a function's parameter list.
///
/// Shared by `ShadowEnricher` and `DeclDistanceEnricher`
/// (unused-param detection).
pub fn collect_parameter_names(
    func: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> Vec<String> {
    let mut names = Vec::new();
    let Some(param_list) = find_descendant_by_kind(func, config.parameter_list_kind()) else {
        return names;
    };

    let has_param_kind = !config.parameter_kind().is_empty();
    for i in 0..param_list.child_count() {
        let Some(child) = param_list.child(i) else {
            continue;
        };
        if has_param_kind {
            if config.is_parameter_kind(child.kind()) {
                if let Some(decl) = child.child_by_field_name(config.declarator_field())
                    && let Some(name) = find_leaf_identifier(decl, source, config)
                {
                    names.push(name);
                } else if let Some(name) = find_leaf_identifier(child, source, config) {
                    names.push(name);
                }
            }
        } else {
            // Python-style: no dedicated parameter kind; each named child is a param.
            if let Some(name) = find_leaf_identifier(child, source, config) {
                names.push(name);
            }
        }
    }
    names
}

/// Iterative BFS search for the first descendant node of the given kind.
pub fn find_descendant_by_kind<'a>(
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

/// Check if a node is inside a parameter list (i.e. it IS a parameter).
pub fn is_inside_parameter_list(node: tree_sitter::Node<'_>, config: &LanguageConfig) -> bool {
    let mut parent = node.parent();
    while let Some(p) = parent {
        if config.is_parameter_list_kind(p.kind()) || config.is_parameter_kind(p.kind()) {
            return true;
        }
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
    // C++ uses "declarator"; Rust uses "pattern"; Python uses "left".
    let declarator = if config.declarator_field().is_empty() {
        decl_node
            .child_by_field_name("pattern")
            .or_else(|| decl_node.child_by_field_name("left"))
            .or_else(|| decl_node.child_by_field_name("name"))
    } else {
        decl_node.child_by_field_name(config.declarator_field())
    }?;

    if !config.function_declarator().is_empty()
        && contains_kind(declarator, config.function_declarator())
    {
        return None;
    }

    find_leaf_identifier(declarator, source, config)
}

/// Drill through declarator wrappers to find the leaf identifier.
/// Note: this is recursive but bounded to declarator chain depth (typically 1-3 levels).
pub fn find_leaf_identifier(
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

    if let Some(child) = node.child_by_field_name(config.declarator_field()) {
        return find_leaf_identifier(child, source, config);
    }

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

/// Check if an identifier node is part of a declaration (the declarator side,
/// not the value side).
pub fn is_in_declaration(node: tree_sitter::Node<'_>, config: &LanguageConfig) -> bool {
    let mut parent = node.parent();
    while let Some(p) = parent {
        let kind = p.kind();
        if config.is_declaration_kind(kind)
            || config.is_init_declarator_kind(kind)
            || config.is_parameter_kind(kind)
        {
            let decl_child = if config.declarator_field().is_empty() {
                p.child_by_field_name("pattern")
                    .or_else(|| p.child_by_field_name("left"))
                    .or_else(|| p.child_by_field_name("name"))
            } else {
                p.child_by_field_name(config.declarator_field())
            };
            if let Some(dc) = decl_child {
                return node_is_descendant_of(node, dc);
            }
            return true;
        }
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

        if config.is_assignment_kind(pk)
            && let Some(left) = parent.child_by_field_name("left")
        {
            return left.id() == node.id();
        }

        if config.is_update_kind(pk) {
            return true;
        }
    }
    false
}

/// Check if an identifier is in a compound-assignment or update expression —
/// these are simultaneous reads AND writes.
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
            let op_range = op.byte_range();
            return op_range.end - op_range.start > 1;
        }
    }
    false
}
