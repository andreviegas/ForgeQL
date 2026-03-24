/// Recursion enrichment — detects functions that call themselves
/// (direct recursion).
///
/// `enrich_row()` adds to `function_definition` rows:
/// - `is_recursive`: `"true"` if the function calls itself.
/// - `recursion_count`: number of self-call sites in the body.
///
/// Only detects **direct** (single-function) recursion.  Mutual recursion
/// (A→B→A) is not detected by this enricher.
///
/// **Language-agnostic:** uses `function_raw_kinds`,
/// `call_expression_raw_kind`, `identifier_raw_kind` from
/// [`LanguageConfig`].
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::node_text;
use crate::ast::lang::LanguageConfig;

/// Enricher for direct recursion detection.
pub struct RecursionEnricher;

impl NodeEnricher for RecursionEnricher {
    fn name(&self) -> &'static str {
        "recursion"
    }

    fn enrich_row(
        &self,
        ctx: &EnrichContext<'_>,
        name: &str,
        fields: &mut HashMap<String, String>,
    ) {
        let config = ctx.language_config;
        if !config.is_function_kind(ctx.node.kind()) {
            return;
        }

        // Short-circuit: language has no call expression kind.
        if !config.has_call_expression() {
            return;
        }

        let Some(body) = ctx.node.child_by_field_name("body") else {
            return;
        };

        let mut count = 0u32;
        count_self_calls(body, name, ctx.source, config, &mut count);

        if count > 0 {
            drop(fields.insert("is_recursive".into(), "true".into()));
            drop(fields.insert("recursion_count".into(), count.to_string()));
        }
    }
}

/// Walk a subtree counting call expressions whose callee matches `func_name`.
fn count_self_calls(
    node: tree_sitter::Node<'_>,
    func_name: &str,
    source: &[u8],
    config: &LanguageConfig,
    count: &mut u32,
) {
    if config.is_call_expression_kind(node.kind()) {
        // In tree-sitter, call_expression typically has a `function` field
        // pointing to the callee.  We extract its text and compare.
        if let Some(callee) = node.child_by_field_name("function") {
            let callee_name = extract_callee_name(callee, source, config);
            if callee_name.as_deref() == Some(func_name) {
                *count += 1;
            }
        }
    }

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            count_self_calls(child, func_name, source, config, count);
        }
    }
}

/// Extract the simple name from a call expression's function/callee node.
///
/// Handles:
/// - Simple identifiers: `foo()`  → `"foo"`
/// - Qualified names: `ns::foo()` → `"foo"` (last identifier)
///
/// Does NOT match method calls (`obj.method()`) as self-calls.
fn extract_callee_name(
    callee: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> Option<String> {
    // Direct identifier: `foo()`
    if config.is_identifier_kind(callee.kind()) {
        let text = node_text(source, callee);
        if !text.is_empty() {
            return Some(text);
        }
    }

    // Qualified identifier: `ns::foo()` — extract the rightmost identifier.
    // In tree-sitter-cpp this is `qualified_identifier` with a `name` field.
    if let Some(name_node) = callee
        .child_by_field_name("name")
        .filter(|n| config.is_identifier_kind(n.kind()))
    {
        let text = node_text(source, name_node);
        if !text.is_empty() {
            return Some(text);
        }
    }

    None
}
