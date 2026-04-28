/// Scope and storage class enrichment — extracted from `collect_nodes()`.
///
/// For declaration nodes, adds:
/// - `scope`: `"file"` (top-level) or `"local"` (inside a function/block)
/// - `storage`: `"static"`, `"extern"`, etc. when a storage class specifier is present
/// - `binding_kind`: `"function"` if declarator contains a `function_declarator`, else `"variable"`
/// - `is_exported`: `"true"` for file-scope declarations without `static` storage
///
/// For function nodes (e.g. Rust `pub fn`), adds:
/// - `is_exported`: `"true"` when the function has a `pub` visibility modifier
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::enrich::data_flow_utils::has_descendant_kind;
use crate::ast::index::node_text;

/// Enricher that computes `scope` and `storage` fields for declarations,
/// and `is_exported` for both declarations and functions.
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
        let is_decl = ctx.language_config.is_declaration_kind(ctx.node.kind());
        let is_func = ctx.language_config.is_function_kind(ctx.node.kind());

        if !is_decl && !is_func {
            return;
        }

        // For declarations: full scope/storage/binding_kind enrichment.
        if is_decl {
            let scope = if ctx
                .node
                .parent()
                .is_some_and(|p| ctx.language_config.is_root_kind(p.kind()))
            {
                "file"
            } else {
                "local"
            };
            drop(fields.insert("scope".to_string(), scope.to_string()));

            // Extract storage class specifier (static, extern, etc.) if present.
            let mut is_static = false;
            let mut cursor = ctx.node.walk();
            if let Some(child) = ctx
                .node
                .named_children(&mut cursor)
                .find(|c| ctx.language_config.is_modifier_node_kind(c.kind()))
            {
                let text = node_text(ctx.source, child);
                if !text.is_empty() {
                    if text == "static" {
                        is_static = true;
                    }
                    drop(fields.insert("storage".to_string(), text));
                }
            }

            // binding_kind: function vs variable
            let has_func_decl =
                has_descendant_kind(ctx.node, ctx.language_config.function_declarator());
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

        // For functions (e.g. Rust pub fn): check for pub visibility modifier.
        if is_func && !fields.contains_key("is_exported") {
            let mut cursor = ctx.node.walk();
            let is_pub = ctx
                .node
                .named_children(&mut cursor)
                .filter(|c| ctx.language_config.is_modifier_node_kind(c.kind()))
                .map(|c| node_text(ctx.source, c))
                .any(|text| text == "pub" || text.starts_with("pub("));
            if is_pub {
                drop(fields.insert("is_exported".to_string(), "true".to_string()));
            }
        }
    }
}
