/// Cast style enrichment — indexes cast expressions.
///
/// Creates a new [`IndexRow`] for each cast expression with:
/// - `cast_style`: `"c_style"` / `"static_cast"` / `"reinterpret_cast"` /
///   `"const_cast"` / `"dynamic_cast"`
/// - `cast_target_type`: the target type text
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::{IndexRow, node_text};
use crate::ast::lang::{CppLanguageInline, LanguageSupport};

/// Enricher that indexes cast expressions with style metadata.
pub struct CastEnricher;

impl NodeEnricher for CastEnricher {
    fn name(&self) -> &'static str {
        "casts"
    }

    fn extra_rows(&self, ctx: &EnrichContext<'_>) -> Vec<IndexRow> {
        let kind = ctx.node.kind();

        let (cast_style, target_type) = match kind {
            // C-style cast: `(Type)expr`
            "cast_expression" => {
                let type_text = ctx
                    .node
                    .child_by_field_name("type")
                    .map(|n| node_text(ctx.source, n))
                    .unwrap_or_default();
                ("c_style", type_text)
            }

            // C++ named casts: `static_cast<Type>(expr)`
            "static_cast_expression" => {
                let type_text = extract_template_type(ctx.node, ctx.source);
                ("static_cast", type_text)
            }
            "reinterpret_cast_expression" => {
                let type_text = extract_template_type(ctx.node, ctx.source);
                ("reinterpret_cast", type_text)
            }
            "const_cast_expression" => {
                let type_text = extract_template_type(ctx.node, ctx.source);
                ("const_cast", type_text)
            }
            "dynamic_cast_expression" => {
                let type_text = extract_template_type(ctx.node, ctx.source);
                ("dynamic_cast", type_text)
            }

            _ => return vec![],
        };

        let mut fields = HashMap::new();
        drop(fields.insert("cast_style".to_string(), cast_style.to_string()));
        if !target_type.is_empty() {
            drop(fields.insert("cast_target_type".to_string(), target_type.clone()));
        }

        let name = format!(
            "{cast_style}<{}>",
            if target_type.is_empty() {
                "?"
            } else {
                &target_type
            }
        );

        vec![IndexRow {
            name,
            node_kind: kind.to_string(),
            fql_kind: CppLanguageInline.map_kind(kind).unwrap_or("").to_string(),
            language: "cpp".to_string(),
            path: ctx.path.to_path_buf(),
            byte_range: ctx.node.byte_range(),
            line: ctx.node.start_position().row + 1,
            fields,
        }]
    }
}

/// Extract the type from a C++ named cast's template argument.
fn extract_template_type(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    // Try "type" field first (some grammars expose it)
    if let Some(type_node) = node.child_by_field_name("type") {
        return node_text(source, type_node);
    }

    // Fallback: look for template_argument_list or type_descriptor child
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            let kind = child.kind();
            if kind == "type_descriptor" || kind == "template_argument_list" {
                let text = node_text(source, child);
                // Strip surrounding < > if present
                return text
                    .strip_prefix('<')
                    .and_then(|s| s.strip_suffix('>'))
                    .unwrap_or(&text)
                    .to_string();
            }
        }
    }

    String::new()
}
