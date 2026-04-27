/// Control flow enrichment — indexes condition complexity for if/while/for/switch/do.
///
/// Creates a new [`IndexRow`] for each control-flow node with fields:
/// - `condition_tests`: count of comparison/logical operators in condition
/// - `paren_depth`: maximum nesting depth of parentheses
/// - `condition_text`: structural skeleton with sequential letter substitution
/// - `has_catch_all`: (switch only) `"true"` if a default/catch-all case exists
/// - `catch_all_kind`: (switch only, when `has_catch_all` is true) e.g. `"default"`
/// - `for_style`: (for loops only) `"traditional"` or `"range"`
/// - `has_assignment_in_condition`: `"true"` if `=` (not `==`) in condition
/// - `mixed_logic`: `"true"` if both `&&` and `||` appear
/// - `dup_logic`: `"true"` if duplicate sub-expression in `&&`/`||` chain
///
/// Post-pass aggregates on `function_definition` rows:
/// - `max_condition_tests`, `max_paren_depth`, `branch_count`
use std::collections::HashMap;

use super::{EnrichContext, ExtraRow, NodeEnricher};
use crate::ast::index::{SymbolTable, node_text};
use crate::ast::lang;

/// Enricher for control-flow statement complexity analysis.
pub struct ControlFlowEnricher;

/// FQL kinds that represent control-flow statements.
const CF_FQL_KINDS: &[&str] = &[
    lang::FQL_IF,
    lang::FQL_WHILE,
    lang::FQL_FOR,
    lang::FQL_SWITCH,
    lang::FQL_DO,
];

impl NodeEnricher for ControlFlowEnricher {
    fn name(&self) -> &'static str {
        "control_flow"
    }

    fn extra_rows(&self, ctx: &EnrichContext<'_>) -> Vec<ExtraRow> {
        let kind = ctx.node.kind();
        let config = ctx.language_config;
        if !config.is_control_flow_kind(kind) {
            return vec![];
        }

        let mut fields = HashMap::new();

        // Extract the condition subtree
        let condition_node = ctx.node.child_by_field_name("condition");
        let condition_text_raw = condition_node
            .map(|n| node_text(ctx.source, n))
            .unwrap_or_default();

        if let Some(cond) = condition_node {
            let tests = count_condition_tests(cond, ctx.source, config);
            drop(fields.insert("condition_tests".to_string(), tests.to_string()));

            let depth = max_paren_depth(&condition_text_raw);
            drop(fields.insert("paren_depth".to_string(), depth.to_string()));

            let skeleton = skeleton_condition(cond, ctx.source, config);
            drop(fields.insert("condition_text".to_string(), skeleton.clone()));

            let has_assign = has_assignment_in_condition(cond, ctx.source, config);
            drop(fields.insert(
                "has_assignment_in_condition".to_string(),
                has_assign.to_string(),
            ));

            let mixed = skeleton.contains("&&") && skeleton.contains("||");
            drop(fields.insert("mixed_logic".to_string(), mixed.to_string()));

            let dup = detect_dup_logic(&skeleton);
            drop(fields.insert("dup_logic".to_string(), dup.to_string()));
        }

        // Switch: check for default case
        if config.is_switch_kind(kind) {
            let has_catch_all = has_default_case(ctx.node);
            drop(fields.insert("has_catch_all".to_string(), has_catch_all.to_string()));
            if has_catch_all {
                drop(fields.insert("catch_all_kind".to_string(), "default".to_string()));
            }
        }

        // For loops: detect style (traditional vs range-based)
        if let Some(style_name) = config.for_style(kind) {
            drop(fields.insert("for_style".to_string(), style_name.to_string()));
        }

        // Name = the skeleton (or raw condition text if no condition)
        let name = fields
            .get("condition_text")
            .cloned()
            .unwrap_or_else(|| condition_text_raw.clone());

        vec![ExtraRow {
            name,
            node_kind: kind.to_string(),
            fql_kind: ctx
                .language_support
                .map_kind(kind)
                .unwrap_or("")
                .to_string(),
            byte_range: ctx.node.byte_range(),
            line: ctx.node.start_position().row + 1,
            fields,
            path_override: None,
        }]
    }

    fn post_pass(
        &self,
        table: &mut SymbolTable,
        scope: Option<&std::collections::HashSet<std::path::PathBuf>>,
    ) {
        // Phase 1 (immutable): build file → sorted-functions lookup, then
        // scan CF rows and map each to its containing function via binary
        // search.  This is O(N log F) instead of the previous O(N × F).
        //
        // When `scope` is Some, we only iterate rows whose path is in the
        // set — this is what makes incremental re-indexing O(P) instead of
        // O(N) on large workspaces.

        let (func_metrics, cf_encl) = {
            let strings = &table.strings;
            let in_scope = |row: &crate::ast::index::IndexRow| -> bool {
                scope
                    .as_ref()
                    .is_none_or(|s| s.contains(strings.paths.get(row.path_id)))
            };

            let mut funcs_by_file: HashMap<&std::path::Path, Vec<(usize, std::ops::Range<usize>)>> =
                HashMap::new();
            for (i, row) in table.rows.iter().enumerate() {
                if strings.fql_kinds.get(row.fql_kind_id) == lang::FQL_FUNCTION && in_scope(row) {
                    funcs_by_file
                        .entry(strings.paths.get(row.path_id))
                        .or_default()
                        .push((i, row.byte_range.clone()));
                }
            }
            for funcs in funcs_by_file.values_mut() {
                funcs.sort_by_key(|(_, range)| range.start);
            }

            let mut metrics: HashMap<usize, (i64, i64, i64)> = HashMap::new();
            // Maps CF row index → enclosing function name.
            let mut cf_encl: HashMap<usize, String> = HashMap::new();
            for (cf_idx, row) in table.rows.iter().enumerate() {
                let row_fql_kind = strings.fql_kinds.get(row.fql_kind_id);
                if !CF_FQL_KINDS.contains(&row_fql_kind) {
                    continue;
                }
                if !in_scope(row) {
                    continue;
                }
                let row_path = strings.paths.get(row.path_id);
                if let Some(funcs) = funcs_by_file.get(row_path) {
                    // Binary search: find the last function whose start ≤ row start.
                    let pos =
                        funcs.partition_point(|(_, range)| range.start <= row.byte_range.start);
                    if pos > 0 {
                        let (func_idx, ref func_range) = funcs[pos - 1];
                        if row.byte_range.end <= func_range.end {
                            let entry = metrics.entry(func_idx).or_insert((0, 0, 0));
                            let tests: i64 = strings
                                .field_str(&row.fields, "condition_tests")
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);
                            let depth: i64 = strings
                                .field_str(&row.fields, "paren_depth")
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);
                            entry.0 = entry.0.max(tests);
                            entry.1 = entry.1.max(depth);
                            entry.2 += 1;
                            // Record enclosing function name for this CF row.
                            let fn_name =
                                strings.names.get(table.rows[func_idx].name_id).to_owned();
                            drop(cf_encl.insert(cf_idx, fn_name));
                        }
                    }
                }
            }
            (metrics, cf_encl)
        };

        // Phase 2 (mutable): pre-intern field keys and values, then apply.
        // Two-phase pattern: intern first (borrows strings), apply second (borrows rows).
        let phase2_entries: Vec<(usize, u32, u32, u32, u32, u32, u32)> = func_metrics
            .into_iter()
            .map(|(func_idx, (max_tests, max_depth, branch_count))| {
                let (k_tests, v_tests) = table
                    .strings
                    .intern_field_entry("max_condition_tests", &max_tests.to_string());
                let (k_depth, v_depth) = table
                    .strings
                    .intern_field_entry("max_paren_depth", &max_depth.to_string());
                let (k_branch, v_branch) = table
                    .strings
                    .intern_field_entry("branch_count", &branch_count.to_string());
                (
                    func_idx, k_tests, v_tests, k_depth, v_depth, k_branch, v_branch,
                )
            })
            .collect();
        for (func_idx, k_tests, v_tests, k_depth, v_depth, k_branch, v_branch) in phase2_entries {
            let row = &mut table.rows[func_idx];
            let _ = row.fields.insert(k_tests, v_tests);
            let _ = row.fields.insert(k_depth, v_depth);
            let _ = row.fields.insert(k_branch, v_branch);
        }

        // Phase 3 (mutable): pre-intern enclosing_fn key, then write fn name values.
        let k_encl = table.strings.field_keys.intern("enclosing_fn");
        let phase3_entries: Vec<(usize, u32)> = cf_encl
            .into_iter()
            .map(|(cf_idx, fn_name)| {
                let v = table.strings.field_values.intern(fn_name.as_str());
                (cf_idx, v)
            })
            .collect();
        for (cf_idx, v_encl) in phase3_entries {
            let _ = table.rows[cf_idx].fields.insert(k_encl, v_encl);
        }
    }
}

/// Count comparison and logical operators in a condition subtree.
fn count_condition_tests(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &crate::ast::lang::LanguageConfig,
) -> usize {
    let mut count = 0;
    let mut cursor = node.walk();
    let mut visit = true;

    loop {
        if visit {
            let current = cursor.node();
            let kind = current.kind();
            if config.is_binary_expression_kind(kind) || config.is_logical_expression_kind(kind) {
                // Most grammars use "operator" (singular); tree-sitter-python's
                // comparison_operator uses "operators" (plural).  Try both.
                let op_node = current
                    .child_by_field_name("operator")
                    .or_else(|| current.child_by_field_name("operators"));
                if let Some(op_node) = op_node {
                    let op = node_text(source, op_node);
                    if matches!(
                        op.as_str(),
                        "==" | "!=" | "<" | ">" | "<=" | ">=" | "&&" | "||" | "and" | "or" // Python and other word-operator languages
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

/// Return `true` when any node in `node`'s subtree has a kind that the
/// language config classifies as an update/increment expression (i.e. `++`/`--`
/// or the equivalent in other languages).
///
/// This is used to give side-effectful expressions a position-unique skeleton
/// label: `*p++` at byte 100 and `*p++` at byte 120 are structurally
/// identical but semantically distinct (they read *different* bytes), so
/// treating them as the same leaf would produce spurious `dup_logic` hits.
///
/// For languages that have no increment operators (Python, Rust, Swift …)
/// `config.update_kinds()` is empty, so this function always returns `false`
/// and has no runtime cost.
fn subtree_has_update(
    node: tree_sitter::Node<'_>,
    config: &crate::ast::lang::LanguageConfig,
) -> bool {
    let update_kinds = config.update_kinds();
    if update_kinds.is_empty() {
        return false;
    }
    let mut cursor = node.walk();
    let mut visit = true;
    loop {
        if visit && update_kinds.iter().any(|k| k == cursor.node().kind()) {
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
                // Back at the root of the subtree we were asked to search.
                return false;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}
/// Build a structural skeleton of a condition subtree.
///
/// Replaces each leaf expression (identifier, member access, call, literal)
/// with a sequential letter (a, b, c, ..., z), keeping operators and parens.
/// Repeated sub-expressions get the same letter.  When more than 26 unique
/// terms exist, overflow terms are mapped to `$`.
fn skeleton_condition(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &crate::ast::lang::LanguageConfig,
) -> String {
    let mut mapping: HashMap<String, String> = HashMap::new();
    let mut next_letter = b'a';
    let mut result = String::new();

    skeleton_walk(
        node,
        source,
        &mut mapping,
        &mut next_letter,
        &mut result,
        config,
    );

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
    config: &crate::ast::lang::LanguageConfig,
) {
    let kind = node.kind();

    // Leaf-like nodes get mapped to letters: identifiers, number/string/char
    // literals, boolean literals, null literals.
    if config.is_usage_node_kind(kind)
        || config.is_number_literal_kind(kind)
        || config.is_string_literal_kind(kind)
        || config.is_char_literal_kind(kind)
        || config.is_boolean_literal(kind)
        || config.is_null_literal(kind)
    {
        let text = node_text(source, node);
        let label = mapping
            .entry(text)
            .or_insert_with(|| next_label(next_letter))
            .clone();
        result.push_str(&label);
        return;
    }

    // Call expressions: map the whole call as one letter.
    // If any argument contains a side-effectful ++/-- operation, the call
    // produces a different result at each position (e.g. `isdigit(*p++)`)
    // — treat every such occurrence as unique to avoid false dup_logic hits.
    if config.is_call_expression_kind(kind) {
        let text = node_text(source, node);
        let key = if subtree_has_update(node, config) {
            format!("{text}@{}", node.start_byte())
        } else {
            text
        };
        let label = mapping
            .entry(key)
            .or_insert_with(|| next_label(next_letter))
            .clone();
        result.push_str(&label);
        return;
    }

    // Field expressions (member access): map as one unit
    if config.is_field_expression_kind(kind) {
        let text = node_text(source, node);
        let label = mapping
            .entry(text)
            .or_insert_with(|| next_label(next_letter))
            .clone();
        result.push_str(&label);
        return;
    }

    // Subscript expressions: map as one unit.
    // Position-unique key if the index contains a side-effectful ++/--.
    if config.is_subscript_expression_kind(kind) {
        let text = node_text(source, node);
        let key = if subtree_has_update(node, config) {
            format!("{text}@{}", node.start_byte())
        } else {
            text
        };
        let label = mapping
            .entry(key)
            .or_insert_with(|| next_label(next_letter))
            .clone();
        result.push_str(&label);
        return;
    }

    // Pointer dereference / address-of: map as one unit so that
    // `*ptr` and `ptr` get distinct letters.
    // Position-unique key when the expression contains ++/--, e.g. `*p++`.
    if config.address_of_expression_kind().contains(kind) {
        let text = node_text(source, node);
        let key = if subtree_has_update(node, config) {
            format!("{text}@{}", node.start_byte())
        } else {
            text
        };
        let label = mapping
            .entry(key)
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
            | "^" | "~" | "+" | "-" | "*" | "/" | "%" | "<<" | ">>"
            // Word operators (Python, SQL, Swift, etc.)
            | "and" | "or" | "not" => {
                result.push_str(&text);
            }
            _ => {}
        }
        return;
    }

    // Unary not: keep the ! and recurse
    if config.is_unary_expression_kind(kind) {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                skeleton_walk(child, source, mapping, next_letter, result, config);
            }
        }
        return;
    }

    // Binary/logical expressions: recurse into children
    if config.is_binary_expression_kind(kind) || config.is_logical_expression_kind(kind) {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                skeleton_walk(child, source, mapping, next_letter, result, config);
            }
        }
        return;
    }

    // Parenthesized expression: recurse, keeping parens from unnamed children
    if config.is_parenthesized_expression_kind(kind) {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                skeleton_walk(child, source, mapping, next_letter, result, config);
            }
        }
        return;
    }

    // Wrapper / transparent nodes that should be recursed through
    if config.is_condition_clause_kind(kind)
        || config.cast_info(kind).is_some()
        || config.is_comma_expression_kind(kind)
    {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                skeleton_walk(child, source, mapping, next_letter, result, config);
            }
        }
        return;
    }

    // Catch-all for named nodes: map the whole text as one opaque leaf.
    // This prevents unknown expression types (e.g. C++ `operator` keyword
    // in member access) from being recursed into and losing structure.
    if node.is_named() {
        let text = node_text(source, node);
        let label = mapping
            .entry(text)
            .or_insert_with(|| next_label(next_letter))
            .clone();
        result.push_str(&label);
    }

    // Unnamed nodes not caught by the operator handler above: drop silently.
}

// -----------------------------------------------------------------------
// Duplicate logic detection
// -----------------------------------------------------------------------

/// Detect duplicate sub-expressions in `&&` / `||` chains within a skeleton.
///
/// Splits the skeleton on top-level `&&` or `||` operators (respecting
/// parentheses) and returns `true` if any two operands are identical.
/// Handles nested chains by recursing into parenthesised sub-expressions.
fn detect_dup_logic(skeleton: &str) -> bool {
    // Strip outermost parens that wrap the entire skeleton (common for
    // condition nodes: the tree-sitter condition is always parenthesised).
    let s = strip_outer_parens(skeleton);
    if s.is_empty() {
        return false;
    }

    // Try splitting on `||` first (lower precedence), then `&&`.
    for op in &["||", "&&"] {
        let parts = split_top_level(s, op);
        if parts.len() >= 2 {
            // Check for exact duplicate operands.
            let mut seen = std::collections::HashSet::new();
            for part in &parts {
                let trimmed = strip_outer_parens(part);
                if !seen.insert(trimmed) {
                    return true;
                }
            }
            // Recurse into each operand for nested chains.
            for part in &parts {
                if detect_dup_logic(part) {
                    return true;
                }
            }
            return false;
        }
    }
    false
}

/// Strip matching outermost parentheses from a skeleton fragment.
fn strip_outer_parens(s: &str) -> &str {
    let s = s.trim();
    if !s.starts_with('(') || !s.ends_with(')') {
        return s;
    }
    // Verify the opening `(` matches the closing `)` (not two separate groups).
    let inner = &s[1..s.len() - 1];
    let mut depth = 0i32;
    for ch in inner.chars() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return s; // parens don't match → don't strip
                }
            }
            _ => {}
        }
    }
    if depth == 0 { inner } else { s }
}

/// Split a skeleton string on a top-level operator (`&&` or `||`),
/// respecting parenthesis nesting.
fn split_top_level<'a>(s: &'a str, op: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    let bytes = s.as_bytes();
    let op_bytes = op.as_bytes();
    let op_len = op_bytes.len();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && i + op_len <= bytes.len() && &bytes[i..i + op_len] == op_bytes => {
                parts.push(&s[start..i]);
                start = i + op_len;
                i = start;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&s[start..]);
    // Only return splits if we actually found the operator.
    if parts.len() == 1 { vec![] } else { parts }
}

/// Check if an assignment operator (`=` but not `==`, `!=`, `<=`, `>=`)
/// appears inside a condition subtree.
///
/// **Caveat:** tree-sitter-cpp sometimes mis-parses `>=` inside complex
/// conditions as a template closing `>` followed by assignment `=`, producing
/// a spurious `assignment_expression` whose left-hand side contains a
/// `template_function` / `template_argument_list`.  We skip those to avoid
/// false positives.
fn has_assignment_in_condition(
    node: tree_sitter::Node<'_>,
    _source: &[u8],
    config: &crate::ast::lang::LanguageConfig,
) -> bool {
    let mut cursor = node.walk();
    let mut visit = true;

    loop {
        if visit {
            let current = cursor.node();
            if config.is_assignment_kind(current.kind())
                && !contains_template_misparse(current, config)
            {
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

/// Check if `node` contains a `template_function`, `template_type`, or
/// `template_argument_list` as a descendant — a sign that tree-sitter-cpp
/// mis-parsed `>=` as template-close `>` followed by assignment `=`.
fn contains_template_misparse(
    node: tree_sitter::Node<'_>,
    config: &crate::ast::lang::LanguageConfig,
) -> bool {
    let mut cursor = node.walk();
    let mut visit = true;
    loop {
        if visit && config.is_template_misparse_kind(cursor.node().kind()) {
            return true;
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
            if cursor.node().id() == node.id() {
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

#[cfg(test)]
mod tests {
    use super::*;

    // -- max_paren_depth -------------------------------------------------

    #[test]
    fn max_paren_depth_no_parens() {
        assert_eq!(max_paren_depth("a && b"), 0);
    }

    #[test]
    fn max_paren_depth_one_level() {
        assert_eq!(max_paren_depth("(a && b)"), 1);
    }

    #[test]
    fn max_paren_depth_two_levels() {
        assert_eq!(max_paren_depth("((a))"), 2);
    }

    #[test]
    fn max_paren_depth_two_separate_groups_max_one() {
        assert_eq!(max_paren_depth("(a) && (b)"), 1);
    }

    #[test]
    fn max_paren_depth_empty_string() {
        assert_eq!(max_paren_depth(""), 0);
    }

    // -- strip_outer_parens ----------------------------------------------

    #[test]
    fn strip_outer_parens_matched_pair() {
        assert_eq!(strip_outer_parens("(a && b)"), "a && b");
    }

    #[test]
    fn strip_outer_parens_no_parens_unchanged() {
        assert_eq!(strip_outer_parens("a && b"), "a && b");
    }

    #[test]
    fn strip_outer_parens_two_separate_groups_unchanged() {
        // "(a) && (b)" — outer parens don't match each other.
        assert_eq!(strip_outer_parens("(a) && (b)"), "(a) && (b)");
    }

    #[test]
    fn strip_outer_parens_empty_string() {
        assert_eq!(strip_outer_parens(""), "");
    }

    #[test]
    fn strip_outer_parens_with_whitespace() {
        assert_eq!(strip_outer_parens("  (x)  "), "x");
    }

    #[test]
    fn strip_outer_parens_nested_strips_one_level() {
        assert_eq!(strip_outer_parens("((x))"), "(x)");
    }

    // -- split_top_level -------------------------------------------------

    #[test]
    fn split_top_level_two_parts_and() {
        let parts = split_top_level("a && b", "&&");
        assert_eq!(parts, vec!["a ", " b"]);
    }

    #[test]
    fn split_top_level_three_parts_or() {
        let parts = split_top_level("a || b || c", "||");
        assert_eq!(parts, vec!["a ", " b ", " c"]);
    }

    #[test]
    fn split_top_level_respects_nesting() {
        // "(a && b) && c" — the inner && is inside parens, only top-level && splits.
        let parts = split_top_level("(a && b) && c", "&&");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].trim(), "c");
    }

    #[test]
    fn split_top_level_no_operator_returns_empty() {
        let parts = split_top_level("a && b", "||");
        assert!(parts.is_empty());
    }

    #[test]
    fn split_top_level_empty_string_no_split() {
        let parts = split_top_level("", "&&");
        // Empty string: no operator found → empty vec.
        assert!(parts.is_empty());
    }

    // -- detect_dup_logic ------------------------------------------------

    #[test]
    fn detect_dup_logic_no_dup() {
        assert!(!detect_dup_logic("a && b"));
    }

    #[test]
    fn detect_dup_logic_exact_dup_and() {
        assert!(detect_dup_logic("a && a"));
    }

    #[test]
    fn detect_dup_logic_exact_dup_or() {
        assert!(detect_dup_logic("x || x"));
    }

    #[test]
    fn detect_dup_logic_three_different_no_dup() {
        assert!(!detect_dup_logic("a && b && c"));
    }

    #[test]
    fn detect_dup_logic_three_with_dup() {
        assert!(detect_dup_logic("a && b && a"));
    }

    #[test]
    fn detect_dup_logic_wrapped_in_outer_parens() {
        assert!(detect_dup_logic("(a && a)"));
    }

    #[test]
    fn detect_dup_logic_empty_string() {
        assert!(!detect_dup_logic(""));
    }
}
