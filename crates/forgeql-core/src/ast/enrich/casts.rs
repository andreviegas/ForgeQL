/// Cast style enrichment — indexes cast expressions.
///
/// Creates a new [`IndexRow`] for each cast expression with:
/// - `cast_style`: from `LanguageConfig::cast_kinds` or `named_cast_keywords`
/// - `cast_target_type`: the target type text
/// - `cast_safety`: `"safe"` / `"moderate"` / `"unsafe"` from config
///
/// For C++ (tree-sitter-cpp 0.23+), named casts (`static_cast<T>()`,
/// `reinterpret_cast<T>()`, etc.) are parsed as `call_expression` nodes
/// with a `template_function(identifier)` child.  `CastEnricher` detects
/// these via `LanguageConfig::named_cast_keywords` and classifies them
/// correctly: `static_cast`/`dynamic_cast` → `"safe"`, `const_cast` →
/// `"moderate"`, `reinterpret_cast` → `"unsafe"`.
///
/// `enrich_row()` also adds to `function` rows:
/// - `has_cast`: `"true"` if the function body contains any cast expressions.
/// - `cast_count`: number of cast expressions in the body.
use std::collections::HashMap;

use super::{EnrichContext, ExtraRow, NodeEnricher};
use crate::ast::index::node_text;
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
        count_casts(body, ctx.source, config, &mut count);

        if count > 0 {
            drop(fields.insert("has_cast".into(), "true".into()));
            drop(fields.insert("cast_count".into(), count.to_string()));
        }
    }
    fn extra_rows(&self, ctx: &EnrichContext<'_>) -> Vec<ExtraRow> {
        let kind = ctx.node.kind();
        let config = ctx.language_config;

        // --- Path 1: direct cast node kind (C-style cast, Rust `as`-cast) ---
        if let Some((cast_style, safety)) = config.cast_info(kind) {
            let target_type = if cast_style == "c_style" {
                // C-style cast: `(Type)expr`
                ctx.node
                    .child_by_field_name("type")
                    .map(|n| node_text(ctx.source, n))
                    .unwrap_or_default()
            } else {
                // Named casts (non-call_expression variants): `static_cast<Type>(expr)` etc.
                extract_template_type(ctx.node, ctx.source, ctx.language_config)
            };

            return build_cast_row(ctx, cast_style, safety, &target_type);
        }

        // --- Path 2: named cast via call_expression + template_function ---
        // In tree-sitter-cpp 0.23, `static_cast<T>(x)` etc. parse as:
        //   call_expression
        //     function: template_function
        //       name: identifier  ("static_cast")
        //       arguments: template_argument_list  ("<T>")
        //     arguments: argument_list  ("(x)")
        if !config.named_cast_keywords.is_empty()
            && config.is_call_expression_kind(kind)
            && let Some(row) = detect_named_cast_row(ctx)
        {
            return vec![row];
        }

        vec![]
    }
}

/// Try to detect a named C++ cast inside a `call_expression` node.
///
/// Returns `Some(ExtraRow)` when the call's function is a `template_function`
/// whose `name` identifier matches a configured named-cast keyword.
fn detect_named_cast_row(ctx: &EnrichContext<'_>) -> Option<ExtraRow> {
    let config = ctx.language_config;

    let fn_node = ctx.node.child_by_field_name("function")?;
    let name_node = fn_node.child_by_field_name("name")?;
    let keyword = node_text(ctx.source, name_node);
    let (cast_style, safety) = config.named_cast_info(&keyword)?;

    // Target type lives in the template argument list of the template_function.
    let target_type = fn_node
        .child_by_field_name("arguments")
        .map(|args| {
            let text = node_text(ctx.source, args);
            text.strip_prefix('<')
                .and_then(|s| s.strip_suffix('>'))
                .map_or(text.as_str(), str::trim)
                .to_string()
        })
        .unwrap_or_default();

    let rows = build_cast_row(ctx, cast_style, safety, &target_type);
    rows.into_iter().next()
}

/// Build the `ExtraRow` for a detected cast.
fn build_cast_row(
    ctx: &EnrichContext<'_>,
    cast_style: &str,
    safety: &str,
    target_type: &str,
) -> Vec<ExtraRow> {
    let mut fields = HashMap::new();
    drop(fields.insert("cast_style".to_string(), cast_style.to_string()));
    drop(fields.insert("cast_safety".to_string(), safety.to_string()));
    if !target_type.is_empty() {
        drop(fields.insert("cast_target_type".to_string(), target_type.to_string()));
    }

    let name = format!(
        "{cast_style}<{}>",
        if target_type.is_empty() {
            "?"
        } else {
            target_type
        }
    );

    vec![ExtraRow {
        name,
        node_kind: ctx.node.kind().to_string(),
        // Always "cast" — this is a synthetic cast row regardless of the
        // raw node kind (e.g. `call_expression` for C++ named casts).
        fql_kind: "cast".to_string(),
        byte_range: ctx.node.byte_range(),
        line: ctx.node.start_position().row + 1,
        fields,
        path_override: None,
        is_self_row: true,
    }]
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

/// Walk a subtree counting cast expressions (both direct-kind and named casts).
fn count_casts(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    count: &mut u32,
) {
    if config.cast_info(node.kind()).is_some() {
        *count += 1;
    } else if !config.named_cast_keywords.is_empty() && config.is_call_expression_kind(node.kind())
    {
        // Check for named cast: call_expression whose function is a
        // template_function with a name matching a configured keyword.
        if let Some(fn_node) = node.child_by_field_name("function")
            && let Some(name_node) = fn_node.child_by_field_name("name")
        {
            let keyword = node_text(source, name_node);
            if config.named_cast_info(&keyword).is_some() {
                *count += 1;
            }
        }
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            count_casts(child, source, config, count);
        }
    }
}
