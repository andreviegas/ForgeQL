/// Code metrics enrichment — lines, parameters, members, qualifiers, visibility.
///
/// `enrich_row()` adds to existing rows:
/// - `lines`: body line count for functions/structs/classes/enums
/// - `param_count`: number of parameters for functions
/// - `member_count`: number of fields/enumerators for structs/classes/enums
/// - `is_const`, `is_volatile`, `is_static`, `is_inline`: qualifier flags
/// - `visibility`: `"public"` / `"private"` / `"protected"` for class members
///
/// `post_pass()` aggregates on function rows:
/// - `return_count`, `goto_count`, `string_count`
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::{SymbolTable, node_text};

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

        // Lines: body span for definitions
        if matches!(
            kind,
            "function_definition" | "struct_specifier" | "class_specifier" | "enum_specifier"
        ) {
            let lines = ctx.node.end_position().row - ctx.node.start_position().row + 1;
            drop(fields.insert("lines".to_string(), lines.to_string()));
        }

        // Parameter count for functions
        if kind == "function_definition" {
            let param_count = count_descendants_by_kind(ctx.node, "parameter_declaration");
            drop(fields.insert("param_count".to_string(), param_count.to_string()));

            // Aggregate counts that require subtree walk
            let return_count = count_descendants_by_kind(ctx.node, "return_statement");
            drop(fields.insert("return_count".to_string(), return_count.to_string()));

            let goto_count = count_descendants_by_kind(ctx.node, "goto_statement");
            drop(fields.insert("goto_count".to_string(), goto_count.to_string()));

            let string_count = count_descendants_by_kind(ctx.node, "string_literal");
            drop(fields.insert("string_count".to_string(), string_count.to_string()));
        }

        // Member count for structs/classes/enums
        if matches!(
            kind,
            "struct_specifier" | "class_specifier" | "enum_specifier"
        ) {
            let target = if kind == "enum_specifier" {
                "enumerator"
            } else {
                "field_declaration"
            };
            let count = count_descendants_by_kind(ctx.node, target);
            drop(fields.insert("member_count".to_string(), count.to_string()));
        }

        // Qualifier flags for declarations
        if kind == "declaration" {
            check_qualifier_with_source(ctx.node, ctx.source, "const", "is_const", fields);
            check_qualifier_with_source(ctx.node, ctx.source, "volatile", "is_volatile", fields);
            check_specifier_with_source(ctx.node, ctx.source, "static", "is_static", fields);
        }

        // Inline specifier for functions
        if kind == "function_definition" {
            check_specifier_with_source(ctx.node, ctx.source, "inline", "is_inline", fields);
        }

        // Visibility for field_declaration inside classes
        if kind == "field_declaration"
            && let Some(vis) = detect_visibility(ctx.node, ctx.source)
        {
            drop(fields.insert("visibility".to_string(), vis.to_string()));
        }
    }

    fn post_pass(&self, _table: &mut SymbolTable) {
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

/// Check if a specific type qualifier (const, volatile) appears in children.
fn check_qualifier_with_source(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    qualifier: &str,
    field_name: &str,
    fields: &mut HashMap<String, String>,
) {
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i)
            && child.kind() == "type_qualifier"
        {
            let text = node_text(source, child);
            if text == qualifier {
                drop(fields.insert(field_name.to_string(), "true".to_string()));
                return;
            }
        }
    }
}

/// Check if a specific storage class / function specifier appears in children.
fn check_specifier_with_source(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    specifier: &str,
    field_name: &str,
    fields: &mut HashMap<String, String>,
) {
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i)
            && matches!(
                child.kind(),
                "storage_class_specifier" | "function_specifier" | "virtual_function_specifier"
            )
        {
            let text = node_text(source, child);
            if text == specifier {
                drop(fields.insert(field_name.to_string(), "true".to_string()));
                return;
            }
        }
    }
}

/// Detect visibility context of a `field_declaration` within a class.
fn detect_visibility(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<&'static str> {
    // Walk backwards through siblings to find the governing access_specifier
    let mut sibling = node.prev_named_sibling();
    while let Some(sib) = sibling {
        if sib.kind() == "access_specifier" {
            let text = node_text(source, sib);
            if text.contains("public") {
                return Some("public");
            } else if text.contains("protected") {
                return Some("protected");
            } else if text.contains("private") {
                return Some("private");
            }
        }
        sibling = sib.prev_named_sibling();
    }

    // Default: check parent container type
    let parent = node.parent()?;
    let grandparent = parent.parent()?;
    match grandparent.kind() {
        "class_specifier" => Some("private"),
        "struct_specifier" => Some("public"),
        _ => None,
    }
}
