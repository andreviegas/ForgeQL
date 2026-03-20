/// Scope and storage class enrichment — extracted from `collect_nodes()`.
///
/// For C++ `declaration` nodes, adds:
/// - `scope`: `"file"` (top-level) or `"local"` (inside a function/block)
/// - `storage`: `"static"`, `"extern"`, etc. when a storage class specifier is present
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
        if ctx.language_name != "cpp" || ctx.node.kind() != "declaration" {
            return;
        }

        let scope = if ctx
            .node
            .parent()
            .is_some_and(|p| p.kind() == "translation_unit")
        {
            "file"
        } else {
            "local"
        };
        drop(fields.insert("scope".to_string(), scope.to_string()));

        // Extract storage class specifier (static, extern, etc.) if present.
        for i in 0..ctx.node.named_child_count() {
            if let Some(child) = ctx.node.named_child(i)
                && child.kind() == "storage_class_specifier"
            {
                let text = node_text(ctx.source, child);
                if !text.is_empty() {
                    drop(fields.insert("storage".to_string(), text));
                }
                break;
            }
        }
    }
}
