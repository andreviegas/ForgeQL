/// Redundancy detection enrichment — finds repeated patterns within functions.
///
/// `enrich_row()` adds to `function_definition` rows:
/// - `repeated_condition_calls`: comma-separated function names called more than
///   once inside condition expressions (candidates for caching)
/// - `has_repeated_condition_calls`: `"true"` / `"false"`
/// - `null_check_count`: total number of null comparisons (`== nullptr`, `!= nullptr`,
///   `== NULL`, `!= NULL`, `== 0` in pointer context)
///
/// `post_pass()` adds to control-flow rows:
/// - `duplicate_condition`: `"true"` if the same `condition_text` skeleton appears
///   in another control-flow row within the same function
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::{SymbolTable, node_text};

/// Enricher that detects redundancy patterns (repeated calls, duplicate conditions).
pub struct RedundancyEnricher;

const CONTROL_FLOW_KINDS: &[&str] = &[
    "if_statement",
    "while_statement",
    "for_statement",
    "switch_statement",
    "do_statement",
];

impl NodeEnricher for RedundancyEnricher {
    fn name(&self) -> &'static str {
        "redundancy"
    }

    fn enrich_row(
        &self,
        ctx: &EnrichContext<'_>,
        _name: &str,
        fields: &mut HashMap<String, String>,
    ) {
        if ctx.node.kind() != "function_definition" {
            return;
        }

        // Collect call expressions inside conditions of control-flow nodes.
        let mut condition_calls: HashMap<String, usize> = HashMap::new();
        let mut null_checks: usize = 0;

        collect_condition_info(ctx.node, ctx.source, &mut condition_calls, &mut null_checks);

        // Repeated condition calls: functions called more than once across conditions.
        let mut repeated: Vec<&str> = condition_calls
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(name, _)| name.as_str())
            .collect();
        repeated.sort_unstable();

        let has_repeated = !repeated.is_empty();
        drop(fields.insert(
            "has_repeated_condition_calls".to_string(),
            has_repeated.to_string(),
        ));
        if has_repeated {
            drop(fields.insert("repeated_condition_calls".to_string(), repeated.join(",")));
        }

        drop(fields.insert("null_check_count".to_string(), null_checks.to_string()));
    }

    fn post_pass(&self, table: &mut SymbolTable) {
        // Phase 1 (immutable): build file → sorted-functions lookup, map
        // each CF row to its containing function via binary search, then
        // detect duplicate skeletons within a function.  O(N log F).
        let duplicate_indices = {
            let mut funcs_by_file: HashMap<&std::path::Path, Vec<std::ops::Range<usize>>> =
                HashMap::new();
            for row in &table.rows {
                if row.node_kind == "function_definition" {
                    funcs_by_file
                        .entry(row.path.as_path())
                        .or_default()
                        .push(row.byte_range.clone());
                }
            }
            for funcs in funcs_by_file.values_mut() {
                funcs.sort_by_key(|range| range.start);
            }

            // Group CF rows by containing function index.
            // Key = (file ptr as usize, func_range_start) to identify a function.
            let mut func_cf_rows: HashMap<(&std::path::Path, usize), Vec<(usize, &str)>> =
                HashMap::new();

            for (i, row) in table.rows.iter().enumerate() {
                if !CONTROL_FLOW_KINDS.contains(&row.node_kind.as_str()) {
                    continue;
                }
                let ct = match row.fields.get("condition_text") {
                    Some(t) if !t.is_empty() => t.as_str(),
                    _ => continue,
                };
                if let Some(funcs) = funcs_by_file.get(row.path.as_path()) {
                    let pos = funcs.partition_point(|range| range.start <= row.byte_range.start);
                    if pos > 0 {
                        let func_range = &funcs[pos - 1];
                        if row.byte_range.end <= func_range.end {
                            func_cf_rows
                                .entry((row.path.as_path(), func_range.start))
                                .or_default()
                                .push((i, ct));
                        }
                    }
                }
            }

            // Find duplicates within each function.
            let mut dups: Vec<usize> = Vec::new();
            for cf_rows in func_cf_rows.values() {
                let mut skeleton_counts: HashMap<&str, Vec<usize>> = HashMap::new();
                for &(idx, text) in cf_rows {
                    skeleton_counts.entry(text).or_default().push(idx);
                }
                for indices in skeleton_counts.values() {
                    if indices.len() > 1 {
                        dups.extend(indices);
                    }
                }
            }
            dups
        };

        // Phase 2 (mutable): apply the duplicate_condition flag.
        for idx in duplicate_indices {
            drop(
                table.rows[idx]
                    .fields
                    .insert("duplicate_condition".to_string(), "true".to_string()),
            );
        }
    }
}

/// Walk a function body and collect call expressions inside conditions,
/// plus count null-check patterns.
fn collect_condition_info(
    func_node: tree_sitter::Node<'_>,
    source: &[u8],
    condition_calls: &mut HashMap<String, usize>,
    null_checks: &mut usize,
) {
    let mut cursor = func_node.walk();
    let mut visit = true;

    loop {
        if visit {
            let node = cursor.node();
            let kind = node.kind();

            // When we hit a control-flow node, inspect its condition subtree.
            if CONTROL_FLOW_KINDS.contains(&kind)
                && let Some(cond) = node.child_by_field_name("condition")
            {
                collect_calls_in_subtree(cond, source, condition_calls);
                *null_checks += count_null_checks(cond, source);
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
        loop {
            if !cursor.goto_parent() {
                return;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Collect all `call_expression` function names inside a condition subtree.
fn collect_calls_in_subtree(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    calls: &mut HashMap<String, usize>,
) {
    let mut cursor = node.walk();
    let mut visit = true;

    loop {
        if visit
            && cursor.node().kind() == "call_expression"
            && let Some(func_node) = cursor.node().child_by_field_name("function")
        {
            let name = node_text(source, func_node);
            if !name.is_empty() {
                *calls.entry(name).or_insert(0) += 1;
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
                return;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Count null-check comparisons in a condition subtree.
///
/// Matches: `== nullptr`, `!= nullptr`, `== NULL`, `!= NULL`, `== 0` when the
/// other operand is a pointer (heuristic: identifier or `field_expression`).
fn count_null_checks(node: tree_sitter::Node<'_>, source: &[u8]) -> usize {
    let mut count = 0;
    let mut cursor = node.walk();
    let mut visit = true;

    loop {
        if visit {
            let current = cursor.node();
            if current.kind() == "binary_expression"
                && let Some(op) = current.child_by_field_name("operator")
            {
                let op_text = node_text(source, op);
                if op_text == "==" || op_text == "!=" {
                    let left = current
                        .child_by_field_name("left")
                        .map(|n| node_text(source, n))
                        .unwrap_or_default();
                    let right = current
                        .child_by_field_name("right")
                        .map(|n| node_text(source, n))
                        .unwrap_or_default();
                    if is_null_value(&left) || is_null_value(&right) {
                        count += 1;
                    }
                }
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
                return count;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Check if a value text represents a null pointer constant.
fn is_null_value(text: &str) -> bool {
    matches!(text, "nullptr" | "NULL" | "0")
}
