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
        if let Some(callee) = node.child_by_field_name("function")
            && let Some((callee_text, is_qualified)) = extract_callee_name(callee, source, config)
        {
            let is_self_call = if is_qualified {
                // Qualified call: `Foo::bar()` — must match the full
                // function name exactly (covers `A::B::C::method`
                // at any nesting depth).
                callee_text == func_name
            } else {
                // Unqualified call: `bar()` — matches if func_name is
                // bare `bar` or qualified `Foo::bar` (C++ out-of-line).
                func_name == callee_text || func_name.ends_with(&format!("::{callee_text}"))
            };
            if is_self_call {
                *count += 1;
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count_self_calls(child, func_name, source, config, count);
    }
}

/// Extract the callee name from a call expression's function node.
///
/// Returns `(text, is_qualified)` where:
/// - Simple identifiers: `foo()`            → `("foo", false)`
/// - Qualified names:    `A::B::method()`   → `("A::B::method", true)`
///
/// Does NOT match method calls (`obj.method()`) as self-calls.
fn extract_callee_name(
    callee: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> Option<(String, bool)> {
    // Direct identifier: `foo()`
    if config.is_identifier_kind(callee.kind()) {
        let text = node_text(source, callee);
        if !text.is_empty() {
            return Some((text, false));
        }
    }

    // Qualified identifier: `ns::foo()` or `A::B::C::method()`.
    // Return the full qualified text so the caller can compare against
    // the (possibly qualified) function name.
    if callee
        .child_by_field_name("name")
        .filter(|n| config.is_identifier_kind(n.kind()))
        .is_some()
    {
        let full_text = node_text(source, callee);
        if !full_text.is_empty() {
            return Some((full_text, true));
        }
    }

    None
}
