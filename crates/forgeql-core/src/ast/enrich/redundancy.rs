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
use crate::ast::lang;

/// Enricher that detects redundancy patterns (repeated calls, duplicate conditions).
pub struct RedundancyEnricher;

/// FQL kinds for control-flow rows (used in `post_pass`).
const CF_FQL_KINDS: &[&str] = &[
    lang::FQL_IF,
    lang::FQL_WHILE,
    lang::FQL_FOR,
    lang::FQL_SWITCH,
    lang::FQL_DO,
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
        let config = ctx.language_config;
        if !config.is_function_kind(ctx.node.kind()) {
            return;
        }

        // Collect call expressions inside conditions of control-flow nodes.
        let mut condition_calls: HashMap<String, usize> = HashMap::new();
        let mut null_checks: usize = 0;

        collect_condition_info(
            ctx.node,
            ctx.source,
            config,
            &mut condition_calls,
            &mut null_checks,
        );

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

    fn post_pass(
        &self,
        table: &mut SymbolTable,
        scope: Option<&std::collections::HashSet<std::path::PathBuf>>,
    ) {
        // Phase 1 (immutable): build file → sorted-functions lookup, map
        // each CF row to its containing function via binary search, then
        // detect duplicate skeletons within a function.  O(N log F).
        //
        // When `scope` is Some, only rows whose path is in the set are
        // considered — duplicate detection is intra-function so unchanged
        // files cannot affect the result.
        let in_scope = |row: &crate::ast::index::IndexRow| -> bool {
            scope
                .as_ref()
                .is_none_or(|s| s.contains(table.strings.paths.get(row.path_id)))
        };

        let duplicate_indices = {
            let mut funcs_by_file: HashMap<&std::path::Path, Vec<std::ops::Range<usize>>> =
                HashMap::new();
            for row in &table.rows {
                if table.strings.fql_kinds.get(row.fql_kind_id) == lang::FQL_FUNCTION
                    && in_scope(row)
                {
                    funcs_by_file
                        .entry(table.strings.paths.get(row.path_id))
                        .or_default()
                        .push(row.byte_range.clone());
                }
            }
            for funcs in funcs_by_file.values_mut() {
                funcs.sort_by_key(|range| range.start);
            }

            // Group CF rows by containing function index.
            // Key = (file ptr as usize, func_range_start) to identify a function.
            // Use String (not &str) for condition_text so the borrow on
            // table.strings ends within this block, before Phase 2 needs
            // &mut table.strings to intern the "duplicate_condition" key.
            let mut func_cf_rows: HashMap<(&std::path::Path, usize), Vec<(usize, String)>> =
                HashMap::new();

            for (i, row) in table.rows.iter().enumerate() {
                if !CF_FQL_KINDS.contains(&table.strings.fql_kinds.get(row.fql_kind_id)) {
                    continue;
                }
                if !in_scope(row) {
                    continue;
                }
                let ct = match table.strings.field_str(&row.fields, "condition_text") {
                    Some(t) if !t.is_empty() => t.to_owned(),
                    _ => continue,
                };
                let row_path = table.strings.paths.get(row.path_id);
                if let Some(funcs) = funcs_by_file.get(row_path) {
                    let pos = funcs.partition_point(|range| range.start <= row.byte_range.start);
                    if pos > 0 {
                        let func_range = &funcs[pos - 1];
                        if row.byte_range.end <= func_range.end {
                            func_cf_rows
                                .entry((row_path, func_range.start))
                                .or_default()
                                .push((i, ct));
                        }
                    }
                }
            }

            // Find duplicates within each function.
            // Skip trivial skeletons (≤ 4 chars after removing outer parens) —
            // simple guards like `(a)`, `(!a)`, `(a<b)`, `(a==b)` repeat
            // naturally and produce noise rather than actionable findings.
            let mut dups: Vec<usize> = Vec::new();
            for cf_rows in func_cf_rows.values() {
                let mut skeleton_counts: HashMap<&str, Vec<usize>> = HashMap::new();
                for (idx, text) in cf_rows {
                    let stripped = text
                        .strip_prefix('(')
                        .and_then(|s| s.strip_suffix(')'))
                        .unwrap_or(text.as_str());
                    if stripped.len() <= 4 {
                        continue;
                    }
                    skeleton_counts.entry(text.as_str()).or_default().push(*idx);
                }
                for indices in skeleton_counts.values() {
                    if indices.len() > 1 {
                        dups.extend(indices);
                    }
                }
            }
            dups
        };

        // Phase 2 (mutable): pre-intern the key+value, then apply to rows.
        // Two-phase pattern keeps borrow of table.strings separate from table.rows.
        let (k_dup, v_true) = table
            .strings
            .intern_field_entry("duplicate_condition", "true");
        for idx in duplicate_indices {
            let _ = table.rows[idx].fields.insert(k_dup, v_true);
        }
    }
}

/// Walk a function body and collect call expressions inside conditions,
/// Walk a function body and collect call expressions inside conditions,
/// plus count null-check patterns.
fn collect_condition_info(
    func_node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &crate::ast::lang::LanguageConfig,
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
            if config.is_control_flow_kind(kind)
                && let Some(cond) = node.child_by_field_name("condition")
            {
                collect_calls_in_subtree(
                    cond,
                    source,
                    condition_calls,
                    config.call_expression_kind(),
                    config.update_kinds(),
                );
                *null_checks += count_null_checks(
                    cond,
                    source,
                    config.null_literal_values(),
                    config.binary_expression_kind(),
                );
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
///
/// If a call expression's arguments contain a side-effectful `update_expression`
/// (e.g. `isdigit(*p++)`), the call reads a *different* value each time even
/// though the function name is the same.  We use a per-position unique key for
/// such calls so they are never counted as repeated-condition-calls.
///
/// For languages without `++`/`--` (Python, Rust, …) `update_raw_kinds` is
/// empty and the check is a no-op.
fn collect_calls_in_subtree(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    calls: &mut HashMap<String, usize>,
    call_expression_raw_kind: &str,
    update_raw_kinds: &[String],
) {
    let mut cursor = node.walk();
    let mut visit = true;

    loop {
        if visit
            && cursor.node().kind() == call_expression_raw_kind
            && let Some(func_node) = cursor.node().child_by_field_name("function")
        {
            let name = node_text(source, func_node);
            if !name.is_empty() {
                // If any argument to this call contains a ++/-- side-effect,
                // treat the call as unique at this position so it is never
                // matched against other occurrences of the same name.
                let key = if !update_raw_kinds.is_empty()
                    && has_update_descendant(cursor.node(), update_raw_kinds)
                {
                    format!("{name}@{}", cursor.node().start_byte())
                } else {
                    name
                };
                *calls.entry(key).or_insert(0) += 1;
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

/// Walk `node`'s subtree (incl. itself) and return `true` if any descendant
/// has a grammar kind listed in `update_raw_kinds` (i.e. `++` / `--`).
fn has_update_descendant(node: tree_sitter::Node<'_>, update_raw_kinds: &[String]) -> bool {
    let mut cursor = node.walk();
    let mut visit = true;
    loop {
        if visit && update_raw_kinds.iter().any(|k| k == cursor.node().kind()) {
            return true;
        }
        if visit && cursor.goto_first_child() {
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
            if cursor.node() == node {
                return false;
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
fn count_null_checks(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    null_literals: &[String],
    binary_expression_raw_kind: &str,
) -> usize {
    let mut count = 0;
    let mut cursor = node.walk();
    let mut visit = true;

    loop {
        if visit {
            let current = cursor.node();
            if current.kind() == binary_expression_raw_kind
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
                    if null_literals.iter().any(|s| s == left.as_str())
                        || null_literals.iter().any(|s| s == right.as_str())
                    {
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
