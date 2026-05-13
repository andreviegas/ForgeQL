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
/// Explicit fallthrough annotations are recognised and suppress the flag:
/// - `__fallthrough;`  (Zephyr / GCC / Clang)
/// - `__fallthrough__;`
/// - `[[fallthrough]]` (C++17)
/// - `__attribute__((fallthrough))` (GCC attribute syntax)
///
/// Comments like `/* FALLTHROUGH */` are intentionally **not** suppressed —
/// they are author intent, not a language construct, and ForgeQL reports
/// structural facts only. Agents can filter annotated cases downstream.
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
        if !config.is_function_kind(ctx.node.kind()) {
            return;
        }

        // Short-circuit: language has no case statements.
        if !config.has_case_statement() {
            return;
        }

        let func = ctx.node;

        // Walk the function body looking for switch statements.
        let Some(body) = func.child_by_field_name("body") else {
            return;
        };

        let mut count = 0u32;
        collect_fallthroughs(body, config, ctx.source, &mut count);

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
    source: &[u8],
    count: &mut u32,
) {
    if config.is_switch_kind(node.kind()) {
        check_switch_cases(node, config, source, count);
        // Don't return — there might be nested switches inside cases.
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_fallthroughs(child, config, source, count);
    }
}

/// Check all `case_statement` children of a switch for fallthrough.
fn check_switch_cases(
    switch_node: tree_sitter::Node<'_>,
    config: &LanguageConfig,
    source: &[u8],
    count: &mut u32,
) {
    // The switch body is typically a compound_statement containing case_statements.
    let Some(body) = switch_node.child_by_field_name("body") else {
        return;
    };

    // Collect all direct children of the switch body.
    let children: Vec<tree_sitter::Node<'_>> = {
        let mut cursor = body.walk();
        body.children(&mut cursor).collect()
    };

    // Find child indices of case_statements.
    let case_indices: Vec<usize> = children
        .iter()
        .enumerate()
        .filter(|(_, c)| config.is_case_statement_kind(c.kind()))
        .map(|(i, _)| i)
        .collect();

    // Check each case except the last (last case can't fall through).
    for (pos, &idx) in case_indices.iter().enumerate() {
        if pos + 1 >= case_indices.len() {
            break;
        }
        let case = children[idx];
        if !is_fallthrough(case, config, source) {
            continue;
        }
        *count += 1;
    }
}

/// Determine if a `case_statement` falls through.
///
/// A case falls through if it has statement children (non-empty) but the
/// last statement is not a terminator (`break`, `return`) and there is no
/// explicit fallthrough annotation (`__fallthrough;`, `[[fallthrough]]`,
/// `/* FALLTHROUGH */`, etc.).
fn is_fallthrough(
    case_node: tree_sitter::Node<'_>,
    config: &LanguageConfig,
    source: &[u8],
) -> bool {
    // Collect statement children (skip labels, colons, and value literals).
    // In tree-sitter-cpp, case_statement named children include:
    // - the case value (number_literal, identifier, etc.)
    // - statements following the colon
    // We look for the last child that is a statement (ends with _statement or
    // _expression, or is a compound_statement, etc.).
    let mut last_statement: Option<tree_sitter::Node<'_>> = None;
    let mut has_statements = false;
    // Track whether any child is an explicit fallthrough annotation (comment or
    // annotation statement that appears after real statements).
    let mut has_explicit_annotation = false;

    for i in 0..case_node.child_count() {
        let Some(child) = case_node.child(i) else {
            continue;
        };
        let kind = child.kind();

        // Skip label tokens: `case`, `default`, `:`, value literals.
        if kind == "case" || kind == "default" || kind == ":" {
            continue;
        }

        // Skip comments — they are intent, not structure.
        if kind == "comment" {
            continue;
        }

        // Skip the case value (first named child after `case` keyword).
        // Values are typically number_literal, identifier, etc.
        // Statements have kinds ending in _statement or _expression,
        // or are compound_statement/expression_statement, etc.
        if config.is_statement_boundary_kind(kind) || config.is_block_kind(kind) {
            // Check if this statement itself is an annotation like __fallthrough;
            if is_fallthrough_statement(child, source) {
                has_explicit_annotation = true;
            }
            has_statements = true;
            last_statement = Some(child);
        }
    }

    // Empty case (no statements) — intentional fallthrough, don't flag.
    if !has_statements {
        return false;
    }

    // Explicit annotation found anywhere in this case — intentional, don't flag.
    if has_explicit_annotation {
        return false;
    }

    // Check if last statement is a terminator.
    if let Some(last) = last_statement {
        let kind = last.kind();
        if config.is_break_statement_kind(kind) || config.is_return_statement_kind(kind) {
            return false;
        }
        // Also check if the last thing inside a compound_statement is terminated.
        if config.is_block_kind(kind) {
            return !block_ends_with_terminator(last, config, source);
        }
    }

    true
}

/// Returns `true` if a statement node is an explicit fallthrough annotation:
/// - `__fallthrough;`  (Zephyr / GCC / Clang)
/// - `__fallthrough__;`
/// - `[[fallthrough]]` (C++17 `attributed_statement`)
/// - `__attribute__((fallthrough))`
fn is_fallthrough_statement(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let text = node
        .utf8_text(source)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // Strip trailing semicolon for comparison.
    let stripped = text.trim_end_matches(';').trim();
    matches!(
        stripped,
        "__fallthrough" | "__fallthrough__" | "[[fallthrough]]" | "__attribute__((fallthrough))"
    )
}

/// Check if the last statement in a block is a terminator.
fn block_ends_with_terminator(
    block: tree_sitter::Node<'_>,
    config: &LanguageConfig,
    source: &[u8],
) -> bool {
    // Walk children backwards to find the last statement.
    for i in (0..block.child_count()).rev() {
        if let Some(child) = block.child(i) {
            let kind = child.kind();
            if config.is_break_statement_kind(kind) || config.is_return_statement_kind(kind) {
                return true;
            }
            // A trailing annotation in a block also counts as intentional.
            if (config.is_statement_boundary_kind(kind) || config.is_block_kind(kind))
                && is_fallthrough_statement(child, source)
            {
                return true;
            }
            // Skip closing braces, whitespace tokens, and comments.
            if kind == "}" || kind == "{" {
                continue;
            }
            // Found a non-terminator statement.
            return false;
        }
    }
    false
}
