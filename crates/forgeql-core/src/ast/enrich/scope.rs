/// Scope and storage class enrichment — extracted from `collect_nodes()`.
///
/// For declaration nodes, adds:
/// - `scope`: `"file"` (top-level) or `"local"` (inside a function/block)
/// - `storage`: `"static"`, `"extern"`, etc. when a storage class specifier is present
/// - `binding_kind`: `"function"` if declarator contains a `function_declarator`, else `"variable"`
/// - `is_exported`: `"true"` for file-scope declarations without `static` storage
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::node_text;

/// Enricher that computes `scope` and `storage` fields for C++ declarations.
pub struct ScopeEnricher;

impl NodeEnricher for ScopeEnricher {
    fn name(&self) -> &'static str {
        "scope"
    }

    fn enrich_row(
        &self,
        ctx: &EnrichContext<'_>,
        _name: &str,
        fields: &mut HashMap<String, String>,
    ) {
        if !ctx
            .language_config
            .declaration_raw_kinds
            .contains(&ctx.node.kind())
        {
            return;
        }

        let scope = if ctx
            .node
            .parent()
            .is_some_and(|p| p.kind() == ctx.language_config.root_node_kind)
        {
            "file"
        } else {
            "local"
        };
        drop(fields.insert("scope".to_string(), scope.to_string()));

        // Extract storage class specifier (static, extern, etc.) if present.
        let mut is_static = false;
        for i in 0..ctx.node.named_child_count() {
            if let Some(child) = ctx.node.named_child(i)
                && ctx
                    .language_config
                    .modifier_node_kinds
                    .contains(&child.kind())
            {
                let text = node_text(ctx.source, child);
                if !text.is_empty() {
                    if text == "static" {
                        is_static = true;
                    }
                    drop(fields.insert("storage".to_string(), text));
                }
                break;
            }
        }

        // binding_kind: function vs variable
        let has_func_decl =
            has_descendant_kind(ctx.node, ctx.language_config.function_declarator_kind);
        let binding = if has_func_decl {
            "function"
        } else {
            "variable"
        };
        drop(fields.insert("binding_kind".to_string(), binding.to_string()));

        // is_exported: file-scope and not static
        if scope == "file" && !is_static {
            drop(fields.insert("is_exported".to_string(), "true".to_string()));
        }
    }
}

/// Check whether `node` or any descendant has kind `target`.
fn has_descendant_kind(node: tree_sitter::Node<'_>, target: &str) -> bool {
    if node.kind() == target {
        return true;
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i)
            && has_descendant_kind(child, target)
        {
            return true;
        }
    }
    false
}
