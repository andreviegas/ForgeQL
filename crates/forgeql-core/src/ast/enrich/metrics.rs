/// Code metrics enrichment — lines, parameters, members, qualifiers, visibility.
///
/// `enrich_row()` adds to existing rows:
/// - `lines`: body line count for functions/structs/classes/enums
/// - `param_count`: number of parameters for functions
/// - `member_count`: number of fields/enumerators for structs/classes/enums
/// - `is_const`, `is_volatile`, `is_static`, `is_inline`, etc.: qualifier flags (config-driven)
/// - `visibility`: `"public"` / `"private"` / `"protected"` for class members
///
/// `post_pass()` aggregates on function rows:
/// - `return_count`, `goto_count`, `string_count`, `throw_count`
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::{SymbolTable, node_text};
use crate::ast::lang::LanguageConfig;

/// Enricher for code size and structure metrics.
pub struct MetricsEnricher;

impl NodeEnricher for MetricsEnricher {
    fn name(&self) -> &'static str {
        "metrics"
    }

    fn enrich_row(
        &self,
        ctx: &EnrichContext<'_>,
        _name: &str,
        fields: &mut HashMap<String, String>,
    ) {
        let kind = ctx.node.kind();
        let config = ctx.language_config;

        // Lines: body span for definitions
        if config.is_definition_kind(kind) {
            let lines = ctx.node.end_position().row - ctx.node.start_position().row + 1;
            drop(fields.insert("lines".to_string(), lines.to_string()));
        }

        // Parameter count for functions
        if config.is_function_kind(kind) {
            let param_count = count_params(ctx.node, config);
            drop(fields.insert("param_count".to_string(), param_count.to_string()));
            // Aggregate counts that require subtree walk
            let return_count = count_descendants_by_kind(ctx.node, config.return_statement_kind());
            drop(fields.insert("return_count".to_string(), return_count.to_string()));

            let goto_count = count_descendants_by_kind(ctx.node, config.goto_statement_kind());
            drop(fields.insert("goto_count".to_string(), goto_count.to_string()));

            let string_count = count_descendants_by_kinds(ctx.node, config.string_literal_kinds());
            drop(fields.insert("string_count".to_string(), string_count.to_string()));

            let throw_count = count_descendants_by_kind(ctx.node, config.throw_statement_kind());
            drop(fields.insert("throw_count".to_string(), throw_count.to_string()));
        }

        // Member count for type definitions (struct/class/enum)
        if config.is_type_kind(kind) {
            let count = count_direct_members(ctx.node, config);
            drop(fields.insert("member_count".to_string(), count.to_string()));
        }

        // Modifier flags from config (const, static, virtual, inline, etc.)
        if config.is_declaration_kind(kind) || config.is_function_kind(kind) {
            check_modifiers(ctx.node, ctx.source, config, fields);
        }

        // Visibility for field_declaration inside classes
        if config.is_field_kind(kind)
            && let Some(vis) = detect_visibility(ctx.node, ctx.source, config)
        {
            drop(fields.insert("visibility".to_string(), vis.to_string()));
        }
    }

    fn post_pass(
        &self,
        _table: &mut SymbolTable,
        _scope: Option<&std::collections::HashSet<std::path::PathBuf>>,
    ) {
        // return_count, goto_count, string_count are now computed in
        // enrich_row() during the tree walk, so no post_pass needed.
    }
}

/// Count all descendants of a specific kind within the node's subtree.
fn count_descendants_by_kind(node: tree_sitter::Node<'_>, target_kind: &str) -> usize {
    let mut count = 0;
    let mut cursor = node.walk();
    let mut visit = true;

    loop {
        if visit && cursor.node().kind() == target_kind && cursor.node() != node {
            count += 1;
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
                return count;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Count all descendants matching any of the given kinds within the node's subtree.
fn count_descendants_by_kinds(node: tree_sitter::Node<'_>, target_kinds: &[String]) -> usize {
    let mut count = 0;
    let mut cursor = node.walk();
    let mut visit = true;

    loop {
        if visit && target_kinds.iter().any(|s| s == cursor.node().kind()) && cursor.node() != node
        {
            count += 1;
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
                return count;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Count parameters for a function node.
///
/// For languages where parameters have a distinct single node kind (e.g. Rust's
/// `parameter`, C++'s `parameter_declaration`), counts all descendants of that
/// kind.  For languages where parameters are represented by multiple kinds with
/// no common wrapper (e.g. Python where `identifier`, `default_parameter`, and
/// `typed_parameter` all appear directly inside `parameters`), counts instead
/// the named children of the parameter-list container node.
fn count_params(node: tree_sitter::Node<'_>, config: &LanguageConfig) -> usize {
    let param_kind = config.parameter_kind();
    if !param_kind.is_empty() {
        return count_descendants_by_kind(node, param_kind);
    }

    // Fallback: count named children of the parameter-list node directly.
    let list_kind = config.parameter_list_kind();
    if list_kind.is_empty() {
        return 0;
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i)
            && child.kind() == list_kind
        {
            return child.named_child_count();
        }
    }
    0
}
/// Count direct members of a struct/class body (one level deep).
///
/// If the node has a `member_body_raw_kind` child, counts member kinds
/// within it (including inside access-specifier sections).  Otherwise
/// falls back to counting all named children of the first list child
/// (for enums whose body kind differs).
fn count_direct_members(node: tree_sitter::Node<'_>, config: &LanguageConfig) -> usize {
    let is_member = |k: &str| {
        config.is_member_kind(k)
            || config.is_field_kind(k)
            || config.is_function_kind(k)
            || config.is_declaration_kind(k)
    };

    // Struct/class path: look for the config-driven body kind
    if let Some(body) = node
        .children(&mut node.walk())
        .find(|c| config.is_member_body_kind(c.kind()))
    {
        let mut count = 0;
        for child in body.children(&mut body.walk()) {
            if is_member(child.kind()) {
                count += 1;
            } else {
                // Access-specifier sections may wrap members.
                for inner in child.children(&mut child.walk()) {
                    if is_member(inner.kind()) {
                        count += 1;
                    }
                }
            }
        }
        return count;
    }

    // Enum path: count named children of the first list-like child
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i)
            && child.named_child_count() > 0
            && child.kind().contains("list")
        {
            return child.named_child_count();
        }
    }
    0
}

/// Check modifier flags from config (const, static, inline, virtual, etc.).
fn check_modifiers(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    fields: &mut HashMap<String, String>,
) {
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i)
            && config.is_modifier_node_kind(child.kind())
        {
            let text = node_text(source, child);
            if let Some(field_name) = config.modifier_field_for(&text) {
                drop(fields.insert(field_name.to_string(), "true".to_string()));
            }
        }
    }
}

/// Detect visibility context of a member within a type body.
fn detect_visibility<'a>(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &'a LanguageConfig,
) -> Option<&'a str> {
    // Walk backwards through siblings to find the governing access specifier
    let mut sibling = node.prev_named_sibling();
    while let Some(sib) = sibling {
        let text = node_text(source, sib);
        if let Some(vis) = config.visibility_for_text(&text) {
            return Some(vis);
        }
        sibling = sib.prev_named_sibling();
    }

    // Default: check parent container type against config defaults
    let parent = node.parent()?;
    let grandparent = parent.parent()?;
    let gp_kind = grandparent.kind();
    config.default_visibility_for_type(gp_kind)
}
