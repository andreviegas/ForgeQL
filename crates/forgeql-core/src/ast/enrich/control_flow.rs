/// Control flow enrichment — indexes condition complexity for if/while/for/switch/do.
///
/// Creates a new [`IndexRow`] for each control-flow node with fields:
/// - `condition_tests`: count of comparison/logical operators in condition
/// - `paren_depth`: maximum nesting depth of parentheses
/// - `condition_text`: structural skeleton with sequential letter substitution
/// - `has_default`: (switch only) `"true"` if a default case exists
/// - `has_assignment_in_condition`: `"true"` if `=` (not `==`) in condition
/// - `mixed_logic`: `"true"` if both `&&` and `||` appear
///
/// Post-pass aggregates on `function_definition` rows:
/// - `max_condition_tests`, `max_paren_depth`, `branch_count`
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::{IndexRow, SymbolTable, node_text};

/// Enricher for control-flow statement complexity analysis.
pub struct ControlFlowEnricher;

const CONTROL_FLOW_KINDS: &[&str] = &[
    "if_statement",
    "while_statement",
    "for_statement",
    "switch_statement",
    "do_statement",
];

impl NodeEnricher for ControlFlowEnricher {
    fn name(&self) -> &'static str {
        "control_flow"
    }

    fn extra_rows(&self, ctx: &EnrichContext<'_>) -> Vec<IndexRow> {
        let kind = ctx.node.kind();
        if !CONTROL_FLOW_KINDS.contains(&kind) {
            return vec![];
        }

        let mut fields = HashMap::new();

        // Extract the condition subtree
        let condition_node = ctx.node.child_by_field_name("condition");
        let condition_text_raw = condition_node
            .map(|n| node_text(ctx.source, n))
            .unwrap_or_default();

        if let Some(cond) = condition_node {
            let tests = count_condition_tests(cond, ctx.source);
            drop(fields.insert("condition_tests".to_string(), tests.to_string()));

            let depth = max_paren_depth(&condition_text_raw);
            drop(fields.insert("paren_depth".to_string(), depth.to_string()));

            let skeleton = skeleton_condition(cond, ctx.source);
            drop(fields.insert("condition_text".to_string(), skeleton.clone()));

            let has_assign = has_assignment_in_condition(cond, ctx.source);
            drop(fields.insert(
                "has_assignment_in_condition".to_string(),
                has_assign.to_string(),
            ));

            let mixed = skeleton.contains("&&") && skeleton.contains("||");
            drop(fields.insert("mixed_logic".to_string(), mixed.to_string()));
        }

        // Switch: check for default case
        if kind == "switch_statement" {
            let has_default = has_default_case(ctx.node);
            drop(fields.insert("has_default".to_string(), has_default.to_string()));
        }

        // Name = the skeleton (or raw condition text if no condition)
        let name = fields
            .get("condition_text")
            .cloned()
            .unwrap_or_else(|| condition_text_raw.clone());

        vec![IndexRow {
            name,
            node_kind: kind.to_string(),
            path: ctx.path.to_path_buf(),
            byte_range: ctx.node.byte_range(),
            line: ctx.node.start_position().row + 1,
            fields,
        }]
    }

    fn post_pass(&self, table: &mut SymbolTable) {
        // Collect aggregated metrics per function
        let mut func_metrics: HashMap<usize, (i64, i64, i64)> = HashMap::new();

        // First, identify all function rows and their byte ranges
        let func_ranges: Vec<(usize, std::ops::Range<usize>, std::path::PathBuf)> = table
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.node_kind == "function_definition")
            .map(|(i, r)| (i, r.byte_range.clone(), r.path.clone()))
            .collect();

        // Then, scan control-flow rows and assign to containing functions
        for row in &table.rows {
            if !CONTROL_FLOW_KINDS.contains(&row.node_kind.as_str()) {
                continue;
            }
            for (func_idx, func_range, func_path) in &func_ranges {
                if row.path == *func_path
                    && row.byte_range.start >= func_range.start
                    && row.byte_range.end <= func_range.end
                {
                    let entry = func_metrics.entry(*func_idx).or_insert((0, 0, 0));
                    let tests: i64 = row
                        .fields
                        .get("condition_tests")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let depth: i64 = row
                        .fields
                        .get("paren_depth")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    entry.0 = entry.0.max(tests);
                    entry.1 = entry.1.max(depth);
                    entry.2 += 1;
                    break;
                }
            }
        }

        // Apply aggregated metrics to function rows
        for (func_idx, (max_tests, max_depth, branch_count)) in func_metrics {
            let row = &mut table.rows[func_idx];
            drop(
                row.fields
                    .insert("max_condition_tests".to_string(), max_tests.to_string()),
            );
            drop(
                row.fields
                    .insert("max_paren_depth".to_string(), max_depth.to_string()),
            );
            drop(
                row.fields
                    .insert("branch_count".to_string(), branch_count.to_string()),
            );
        }
    }
}

/// Count comparison and logical operators in a condition subtree.
fn count_condition_tests(node: tree_sitter::Node<'_>, source: &[u8]) -> usize {
    let mut count = 0;
    let mut cursor = node.walk();
    let mut visit = true;

    loop {
        if visit {
            let current = cursor.node();
            let kind = current.kind();
            if kind == "binary_expression" || kind == "logical_expression" {
                // Check the operator child
                if let Some(op_node) = current.child_by_field_name("operator") {
                    let op = node_text(source, op_node);
                    if matches!(
                        op.as_str(),
                        "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||"
                    ) {
                        count += 1;
                    }
                }
            }
        }

        // DFS traversal
        if visit && cursor.goto_first_child() {
            visit = true;
            continue;
        }
        if cursor.goto_next_sibling() {
            visit = true;
            continue;
        }
        // Backtrack
        loop {
            if !cursor.goto_parent() {
                return count;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Compute maximum parenthesis nesting depth in a string.
fn max_paren_depth(text: &str) -> usize {
    let mut depth: usize = 0;
    let mut max: usize = 0;
    for ch in text.chars() {
        match ch {
            '(' => {
                depth += 1;
                max = max.max(depth);
            }
            ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

/// Allocate the next label for a leaf term in a condition skeleton.
///
/// `a`–`z` (26 unique terms), then `A`–`Z` (52 total).  If all 52
/// slots are exhausted the label `$` is returned for every overflow term.
fn next_label(next_letter: &mut u8) -> String {
    if *next_letter <= b'z' {
        let l = *next_letter as char;
        *next_letter += 1;
        return l.to_string();
    }
    // Second tier: A-Z (next_letter tracks 123..148 → A..Z)
    let upper_base = b'z' + 1; // 123
    if *next_letter < upper_base + 26 {
        let l = (b'A' + (*next_letter - upper_base)) as char;
        *next_letter += 1;
        return l.to_string();
    }
    "$".to_string()
}

/// Maximum skeleton length (in chars).  Conditions longer than this are
/// truncated with a `…` suffix to keep output readable.
const MAX_SKELETON_LEN: usize = 120;

/// Build a structural skeleton of a condition subtree.
///
/// Replaces each leaf expression (identifier, member access, call, literal)
/// with a sequential letter (a, b, c, ..., z), keeping operators and parens.
/// Repeated sub-expressions get the same letter.  When more than 26 unique
/// terms exist, overflow terms are mapped to `$`.
fn skeleton_condition(node: tree_sitter::Node<'_>, source: &[u8]) -> String {
    let mut mapping: HashMap<String, String> = HashMap::new();
    let mut next_letter = b'a';
    let mut result = String::new();

    skeleton_walk(node, source, &mut mapping, &mut next_letter, &mut result);

    if result.len() > MAX_SKELETON_LEN {
        let truncated: String = result.chars().take(MAX_SKELETON_LEN).collect();
        format!("{truncated}…")
    } else {
        result
    }
}

#[allow(clippy::too_many_lines)]
fn skeleton_walk(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    mapping: &mut HashMap<String, String>,
    next_letter: &mut u8,
    result: &mut String,
) {
    let kind = node.kind();

    // Leaf-like nodes get mapped to letters
    if matches!(
        kind,
        "identifier"
            | "field_identifier"
            | "type_identifier"
            | "number_literal"
            | "string_literal"
            | "char_literal"
            | "true"
            | "false"
            | "null"
            | "nullptr"
    ) {
        let text = node_text(source, node);
        let label = mapping
            .entry(text)
            .or_insert_with(|| next_label(next_letter))
            .clone();
        result.push_str(&label);
        return;
    }

    // Call expressions: map the whole call as one letter
    if kind == "call_expression" {
        let text = node_text(source, node);
        let label = mapping
            .entry(text)
            .or_insert_with(|| next_label(next_letter))
            .clone();
        result.push_str(&label);
        return;
    }

    // Field expressions (member access): map as one unit
    if kind == "field_expression" {
        let text = node_text(source, node);
        let label = mapping
            .entry(text)
            .or_insert_with(|| next_label(next_letter))
            .clone();
        result.push_str(&label);
        return;
    }

    // Subscript expressions: map as one unit
    if kind == "subscript_expression" {
        let text = node_text(source, node);
        let label = mapping
            .entry(text)
            .or_insert_with(|| next_label(next_letter))
            .clone();
        result.push_str(&label);
        return;
    }

    // Operators and punctuation: keep as-is
    if !node.is_named() {
        let text = node_text(source, node);
        match text.as_str() {
            "(" | ")" | "!" | "&&" | "||" | "==" | "!=" | "<" | ">" | "<=" | ">=" | "&" | "|"
            | "^" | "~" => {
                result.push_str(&text);
            }
            _ => {}
        }
        return;
    }

    // Unary not: keep the ! and recurse
    if kind == "unary_expression" {
        // Recurse into children to preserve operator
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                skeleton_walk(child, source, mapping, next_letter, result);
            }
        }
        return;
    }

    // Binary/logical expressions: recurse into children
    if matches!(kind, "binary_expression" | "logical_expression") {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                skeleton_walk(child, source, mapping, next_letter, result);
            }
        }
        return;
    }

    // Parenthesized expression: recurse, keeping parens from unnamed children
    if kind == "parenthesized_expression" {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                skeleton_walk(child, source, mapping, next_letter, result);
            }
        }
        return;
    }

    // Catch-all: recurse into ALL children (named + unnamed) so that
    // operator tokens between operands are preserved.  The unnamed-node
    // handler above only emits known operators and silently drops the rest.
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            skeleton_walk(child, source, mapping, next_letter, result);
        }
    }
}

/// Check if an assignment operator (`=` but not `==`, `!=`, `<=`, `>=`)
/// appears inside a condition subtree.
fn has_assignment_in_condition(node: tree_sitter::Node<'_>, _source: &[u8]) -> bool {
    let mut cursor = node.walk();
    let mut visit = true;

    loop {
        if visit {
            let current = cursor.node();
            if current.kind() == "assignment_expression" {
                return true;
            }
        }

        if visit && cursor.goto_first_child() {
            visit = true;
            continue;
        }
        if cursor.goto_next_sibling() {
            visit = true;
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return false;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Check if a switch statement contains a `default` case.
fn has_default_case(switch_node: tree_sitter::Node<'_>) -> bool {
    let mut cursor = switch_node.walk();
    let mut visit = true;

    loop {
        if visit {
            let kind = cursor.node().kind();
            // tree-sitter-cpp 0.23: `default:` is a `case_statement` with no
            // `value` field.  Older grammars use a separate `default_statement`.
            if kind == "default_statement" {
                return true;
            }
            if kind == "case_statement" && cursor.node().child_by_field_name("value").is_none() {
                return true;
            }
        }

        if visit && cursor.goto_first_child() {
            visit = true;
            continue;
        }
        if cursor.goto_next_sibling() {
            visit = true;
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return false;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}
