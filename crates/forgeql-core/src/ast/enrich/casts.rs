/// Cast style enrichment — indexes cast expressions.
///
/// Creates a new [`IndexRow`] for each cast expression with:
/// - `cast_style`: from `LanguageConfig::cast_kinds`
/// - `cast_target_type`: the target type text
/// - `cast_safety`: `"safe"` / `"moderate"` / `"unsafe"` from config
///
/// `enrich_row()` also adds to `function` rows:
/// - `has_cast`: `"true"` if the function body contains any cast expressions.
/// - `cast_count`: number of cast expressions in the body.
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::{IndexRow, node_text};
use crate::ast::lang::LanguageConfig;

/// Enricher that indexes cast expressions with style metadata.
pub struct CastEnricher;

impl NodeEnricher for CastEnricher {
    fn name(&self) -> &'static str {
        "casts"
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

        let Some(body) = ctx.node.child_by_field_name("body") else {
            return;
        };

        let mut count = 0u32;
        count_casts(body, config, &mut count);

        if count > 0 {
            drop(fields.insert("has_cast".into(), "true".into()));
            drop(fields.insert("cast_count".into(), count.to_string()));
        }
    }
    fn extra_rows(&self, ctx: &EnrichContext<'_>) -> Vec<IndexRow> {
        let kind = ctx.node.kind();
        let config = ctx.language_config;

        // Look up the cast style from config
        let Some((cast_style, safety)) = config.cast_info(kind) else {
            return vec![];
        };

        let target_type = if cast_style == "c_style" {
            // C-style cast: `(Type)expr`
            ctx.node
                .child_by_field_name("type")
                .map(|n| node_text(ctx.source, n))
                .unwrap_or_default()
        } else {
            // Named casts: `static_cast<Type>(expr)` etc.
            extract_template_type(ctx.node, ctx.source, ctx.language_config)
        };

        let mut fields = HashMap::new();
        drop(fields.insert("cast_style".to_string(), cast_style.to_string()));
        drop(fields.insert("cast_safety".to_string(), safety.to_string()));
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
            fql_kind: ctx
                .language_support
                .map_kind(kind)
                .unwrap_or("")
                .to_string(),
            language: ctx.language_name.to_string(),
            path: ctx.path.to_path_buf(),
            byte_range: ctx.node.byte_range(),
            line: ctx.node.start_position().row + 1,
            usages_count: 0,
            fields,
            ..Default::default()
        }]
    }
}

/// Extract the type from a C++ named cast's template argument.
fn extract_template_type(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &crate::ast::lang::LanguageConfig,
) -> String {
    // Try "type" field first (some grammars expose it)
    if let Some(type_node) = node.child_by_field_name("type") {
        return node_text(source, type_node);
    }

    // Fallback: look for template_argument_list or type_descriptor child
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            let kind = child.kind();
            if config.is_type_descriptor_kind(kind) || config.is_template_argument_list_kind(kind) {
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

/// Walk a subtree counting cast expressions.
fn count_casts(node: tree_sitter::Node<'_>, config: &LanguageConfig, count: &mut u32) {
    if config.cast_info(node.kind()).is_some() {
        *count += 1;
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            count_casts(child, config, count);
        }
    }
}
