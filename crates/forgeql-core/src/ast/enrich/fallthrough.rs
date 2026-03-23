/// Fallthrough enrichment — detects switch/case statements that fall
/// through to the next case without a `break` or `return`.
///
/// `enrich_row()` adds to `function_definition` rows:
/// - `has_fallthrough`:   `"true"` if any case falls through.
/// - `fallthrough_count`: number of non-empty cases missing a terminator.
///
/// Empty cases (just a label with no statements, used for intentional
/// grouping like `case 1: case 2: ...`) are NOT flagged.
///
/// **Language-agnostic:** uses `function_raw_kinds`, `switch_raw_kinds`,
/// `case_statement_raw_kind`, `break_statement_raw_kind`,
/// `return_statement_raw_kind`, `block_raw_kind` from [`LanguageConfig`].
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::lang::LanguageConfig;

/// Enricher for switch-case fallthrough detection.
pub struct FallthroughEnricher;

impl NodeEnricher for FallthroughEnricher {
    fn name(&self) -> &'static str {
        "fallthrough"
    }

    fn enrich_row(
        &self,
        ctx: &EnrichContext<'_>,
        _name: &str,
        fields: &mut HashMap<String, String>,
    ) {
        let config = ctx.language_config;
        if !config.function_raw_kinds.contains(&ctx.node.kind()) {
            return;
        }

        // Short-circuit: language has no case statements.
        if config.case_statement_raw_kind.is_empty() {
            return;
        }

        let func = ctx.node;

        // Walk the function body looking for switch statements.
        let Some(body) = func.child_by_field_name("body") else {
            return;
        };

        let mut count = 0u32;
        collect_fallthroughs(body, config, &mut count);

        if count > 0 {
            drop(fields.insert("has_fallthrough".into(), "true".into()));
            drop(fields.insert("fallthrough_count".into(), count.to_string()));
        }
    }
}

/// Walk a subtree looking for switch statements, then check their cases.
fn collect_fallthroughs(
    node: tree_sitter::Node<'_>,
    config: &LanguageConfig,
    count: &mut u32,
) {
    if config.switch_raw_kinds.contains(&node.kind()) {
        check_switch_cases(node, config, count);
        // Don't return — there might be nested switches inside cases.
    }

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_fallthroughs(child, config, count);
        }
    }
}

/// Check all `case_statement` children of a switch for fallthrough.
fn check_switch_cases(
    switch_node: tree_sitter::Node<'_>,
    config: &LanguageConfig,
    count: &mut u32,
) {
    // The switch body is typically a compound_statement containing case_statements.
    let Some(body) = switch_node.child_by_field_name("body") else {
        return;
    };

    // Collect all case_statement children.
    let mut cases: Vec<tree_sitter::Node<'_>> = Vec::new();
    for i in 0..body.child_count() {
        if let Some(child) = body.child(i)
            && child.kind() == config.case_statement_raw_kind
        {
            cases.push(child);
        }
    }

    // Check each case except the last (last case can't fall through).
    for case in cases.iter().take(cases.len().saturating_sub(1)) {
        if is_fallthrough(*case, config) {
            *count += 1;
        }
    }
}

/// Determine if a `case_statement` falls through.
///
/// A case falls through if it has statement children (non-empty) but the
/// last statement is not a terminator (`break`, `return`).
fn is_fallthrough(case_node: tree_sitter::Node<'_>, config: &LanguageConfig) -> bool {
    // Collect statement children (skip labels, colons, and value literals).
    // In tree-sitter-cpp, case_statement named children include:
    // - the case value (number_literal, identifier, etc.)
    // - statements following the colon
    // We look for the last child that is a statement (ends with _statement or
    // _expression, or is a compound_statement, etc.).
    let mut last_statement: Option<tree_sitter::Node<'_>> = None;
    let mut has_statements = false;

    for i in 0..case_node.child_count() {
        let Some(child) = case_node.child(i) else {
            continue;
        };
        let kind = child.kind();

        // Skip label tokens: `case`, `default`, `:`, value literals.
        if kind == "case" || kind == "default" || kind == ":" {
            continue;
        }

        // Skip the case value (first named child after `case` keyword).
        // Values are typically number_literal, identifier, etc.
        // Statements have kinds ending in _statement or _expression,
        // or are compound_statement/expression_statement, etc.
        if kind.ends_with("_statement") || kind.ends_with("_expression")
            || kind == config.block_raw_kind
        {
            has_statements = true;
            last_statement = Some(child);
        }
    }

    // Empty case (no statements) — intentional fallthrough, don't flag.
    if !has_statements {
        return false;
    }

    // Check if last statement is a terminator.
    if let Some(last) = last_statement {
        let kind = last.kind();
        if kind == config.break_statement_raw_kind
            || kind == config.return_statement_raw_kind
        {
            return false;
        }
        // Also check if the last thing inside a compound_statement is terminated.
        if kind == config.block_raw_kind {
            return !block_ends_with_terminator(last, config);
        }
    }

    true
}

/// Check if the last statement in a block is a terminator.
fn block_ends_with_terminator(block: tree_sitter::Node<'_>, config: &LanguageConfig) -> bool {
    // Walk children backwards to find the last statement.
    for i in (0..block.child_count()).rev() {
        if let Some(child) = block.child(i) {
            let kind = child.kind();
            if kind == config.break_statement_raw_kind
                || kind == config.return_statement_raw_kind
            {
                return true;
            }
            // Skip closing braces and whitespace tokens.
            if kind == "}" || kind == "{" {
                continue;
            }
            // Found a non-terminator statement.
            return false;
        }
    }
    false
}
