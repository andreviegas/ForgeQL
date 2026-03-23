/// Declaration distance enrichment — measures how far local variables are
/// declared from their first use.
///
/// `enrich_row()` adds to `function_definition` rows:
/// - `decl_distance`: sum of (`first_use_line` − `declaration_line`) for all
///   local variables whose distance ≥ 2.  Higher values indicate more
///   "scattered" declarations.
/// - `decl_far_count`: number of local variables with distance ≥ 2.
/// - `has_unused_reassign`: `"true"` if any local variable is reassigned
///   before its previous value was read (dead store).
///
/// **Exclusions:** parameters, globals, member accesses (`this->x`, `m_x`).
/// **Inclusions:** all locals including loop variables, const locals, nested
/// scope locals.
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::node_text;
use crate::ast::lang::LanguageConfig;

/// Minimum distance threshold — locals closer than this are considered fine.
const FAR_THRESHOLD: usize = 2;

/// Enricher for variable declaration distance and dead-store detection.
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
        if !config.function_raw_kinds.contains(&ctx.node.kind()) {
            return;
        }

        // 1. Collect local variable declarations (name → declaration line).
        let locals = collect_local_declarations(ctx);
        if locals.is_empty() {
            drop(fields.insert("decl_distance".to_string(), "0".to_string()));
            drop(fields.insert("decl_far_count".to_string(), "0".to_string()));
            drop(fields.insert("has_unused_reassign".to_string(), "false".to_string()));
            return;
        }

        // 2. Walk identifiers to find first use of each local & detect dead stores.
        let (first_uses, has_dead_store) = analyse_uses(ctx, &locals);

        // 3. Compute distance metrics.
        let mut total_distance: usize = 0;
        let mut far_count: usize = 0;

        for (name, decl_line) in &locals {
            if let Some(&first_use_line) = first_uses.get(name.as_str()) {
                let dist = first_use_line.saturating_sub(*decl_line);
                if dist >= FAR_THRESHOLD {
                    total_distance += dist;
                    far_count += 1;
                }
            }
            // If no use found, the variable is unused — decl_distance doesn't
            // count it (other linters catch unused variables).
        }

        drop(fields.insert("decl_distance".to_string(), total_distance.to_string()));
        drop(fields.insert("decl_far_count".to_string(), far_count.to_string()));
        drop(fields.insert(
            "has_unused_reassign".to_string(),
            has_dead_store.to_string(),
        ));
    }
}

/// A local variable: (name, 1-based declaration line).
type LocalDecl = (String, usize);

/// Collect all local variable declarations inside a function body.
///
/// Walks the function's direct body to find `declaration` nodes, extracts
/// the declarator name, and records its 1-based line.  Skips:
/// - Parameters (identified by `parameter_raw_kind`)
/// - Field declarations (member variables)
/// - Declarations that contain a function declarator (function pointer decls)
fn collect_local_declarations(ctx: &EnrichContext<'_>) -> Vec<LocalDecl> {
    let config = ctx.language_config;
    let func = ctx.node;
    let source = ctx.source;

    let mut locals = Vec::new();
    let mut cursor = func.walk();
    let mut visit = true;

    loop {
        if visit {
            let node = cursor.node();
            let kind = node.kind();

            // Skip the function node itself.
            if node != func
                && config.declaration_raw_kinds.contains(&kind)
                && !is_inside_parameter_list(node, config)
                && let Some(name) = extract_declarator_name(node, source, config)
            {
                let line = node.start_position().row + 1; // 1-based
                locals.push((name, line));
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
fn is_inside_parameter_list(node: tree_sitter::Node<'_>, config: &LanguageConfig) -> bool {
    let mut parent = node.parent();
    while let Some(p) = parent {
        if p.kind() == config.parameter_list_raw_kind || p.kind() == config.parameter_raw_kind {
            return true;
        }
        // Stop at function boundary — don't walk above.
        if config.function_raw_kinds.contains(&p.kind()) {
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
fn extract_declarator_name(
    decl_node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> Option<String> {
    let declarator = decl_node.child_by_field_name(config.declarator_field_name)?;

    // Skip function pointer declarations.
    if contains_kind(declarator, config.function_declarator_kind) {
        return None;
    }

    // Drill down through nested declarators (init_declarator, pointer_declarator,
    // reference_declarator, etc.) to find the leaf identifier.
    find_leaf_identifier(declarator, source, config)
}

/// Recursively drill through declarator wrappers to find the leaf identifier.
fn find_leaf_identifier(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> Option<String> {
    // If this node is itself an identifier, return it.
    if node.kind() == config.identifier_raw_kind {
        let text = node_text(source, node);
        if !text.is_empty() {
            return Some(text);
        }
    }

    // Try the declarator field first (init_declarator, pointer_declarator, etc.).
    if let Some(child) = node.child_by_field_name(config.declarator_field_name) {
        return find_leaf_identifier(child, source, config);
    }

    // Fallback: look for an identifier among direct children.
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i)
            && child.kind() == config.identifier_raw_kind
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
fn contains_kind(node: tree_sitter::Node<'_>, target_kind: &str) -> bool {
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

/// Analyse identifier uses within a function body.
///
/// Returns:
/// - `first_uses`: map of local name → first-use 1-based line
/// - `has_dead_store`: whether any local was written twice without a read
///   in between (excludes compound assignments like `+=`)
fn analyse_uses<'a>(
    ctx: &EnrichContext<'a>,
    locals: &[LocalDecl],
) -> (HashMap<&'a str, usize>, bool) {
    let source = ctx.source;
    let func = ctx.node;
    let config = ctx.language_config;

    // Build a set of local names for quick lookup.
    // We store the name as owned strings and use a HashSet for O(1) membership.
    let local_names: HashMap<&str, usize> = locals
        .iter()
        .map(|(name, line)| (name.as_str(), *line))
        .collect();

    // first_uses: name → first-use line (1-based)
    let mut first_uses: HashMap<&str, usize> = HashMap::new();

    // Dead-store tracking: name → state (true = written-not-yet-read)
    let mut written_not_read: HashMap<&str, bool> = HashMap::new();
    let mut has_dead_store = false;

    // Seed: declarations with initializers (int x = expr;) count as an
    // initial write.  Uninitialized declarations (int x;) don't.
    for (name, _) in locals {
        // Check if the declaration has a "value" child in the init_declarator,
        // indicating it's initialized.  We conservatively assume initialized
        // (most C++ locals have initializers).
        let _ = written_not_read.insert(name.as_str(), true);
    }

    // Walk all nodes in source order.
    let mut cursor = func.walk();
    let mut visit = true;

    loop {
        if visit {
            let node = cursor.node();
            let kind = node.kind();

            // Only interested in identifier nodes that could be local variable refs.
            if kind == config.identifier_raw_kind && node != func {
                let text = std::str::from_utf8(&source[node.byte_range()]).unwrap_or("");

                if let Some(&decl_line) = local_names.get(text) {
                    let use_line = node.start_position().row + 1;

                    // Skip the identifier that IS the declaration itself.
                    if use_line != decl_line || !is_in_declaration(node, config) {
                        // Track first use.
                        let _ = first_uses.entry(text).or_insert(use_line);

                        // Dead-store tracking.
                        let is_write = is_write_context(node, config);
                        let is_compound = is_compound_assign_or_update(node, config);

                        if is_compound {
                            // Compound assign (+=, etc.) or ++/-- reads AND writes.
                            // This counts as a read, so clear the written flag.
                            let _ = written_not_read.insert(text, true);
                        } else if is_write {
                            // Pure write (simple =).
                            if written_not_read.get(text) == Some(&true) {
                                has_dead_store = true;
                            }
                            let _ = written_not_read.insert(text, true);
                        } else {
                            // Read.
                            let _ = written_not_read.insert(text, false);
                        }
                    }
                }
            }
        }

        // DFS traversal
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
                return (first_uses, has_dead_store);
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Check if an identifier node is part of a declaration (i.e. the declarator
/// itself, not a reference).
fn is_in_declaration(node: tree_sitter::Node<'_>, config: &LanguageConfig) -> bool {
    let mut parent = node.parent();
    while let Some(p) = parent {
        let kind = p.kind();
        if config.declaration_raw_kinds.contains(&kind)
            || kind == config.init_declarator_raw_kind
            || kind == config.parameter_raw_kind
        {
            // Check if the identifier is on the declarator side, not the value side.
            if let Some(decl_child) = p.child_by_field_name(config.declarator_field_name) {
                return node_is_descendant_of(node, decl_child);
            }
            return true;
        }
        // Stop at statement/block boundaries.
        if kind.ends_with("_statement") || kind == config.block_raw_kind {
            return false;
        }
        parent = p.parent();
    }
    false
}

/// Check if `needle` is a descendant of (or equal to) `haystack`.
fn node_is_descendant_of(needle: tree_sitter::Node<'_>, haystack: tree_sitter::Node<'_>) -> bool {
    let nr = needle.byte_range();
    let hr = haystack.byte_range();
    nr.start >= hr.start && nr.end <= hr.end
}

/// Check if an identifier is in a write context (left side of `=`).
fn is_write_context(node: tree_sitter::Node<'_>, config: &LanguageConfig) -> bool {
    if let Some(parent) = node.parent() {
        let pk = parent.kind();

        // Simple assignment: `x = expr`
        if config.assignment_raw_kinds.contains(&pk)
            && let Some(left) = parent.child_by_field_name("left")
        {
            return left.id() == node.id();
        }

        // update_expression: `++x` or `x++`
        if config.update_raw_kinds.contains(&pk) {
            return true;
        }
    }
    false
}

/// Check if an identifier is in a compound-assignment (`+=`, `-=`, etc.)
/// or update expression (`++`, `--`) — these are reads AND writes.
fn is_compound_assign_or_update(node: tree_sitter::Node<'_>, config: &LanguageConfig) -> bool {
    if let Some(parent) = node.parent() {
        let pk = parent.kind();

        if config.update_raw_kinds.contains(&pk) {
            return true;
        }

        if config.assignment_raw_kinds.contains(&pk)
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
