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

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::{IndexRow, SymbolTable, node_text};
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

    fn extra_rows(&self, ctx: &EnrichContext<'_>) -> Vec<IndexRow> {
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
            fields,
        }]
    }

    fn post_pass(&self, table: &mut SymbolTable) {
        // Phase 1 (immutable): build file → sorted-functions lookup, then
        // scan CF rows and map each to its containing function via binary
        // search.  This is O(N log F) instead of the previous O(N × F).
        // Phase 1 (immutable): build file → sorted-functions lookup, then
        // scan CF rows and map each to its containing function via binary
        // search.  This is O(N log F) instead of the previous O(N × F).
        let (func_metrics, cf_encl) = {
            let mut funcs_by_file: HashMap<&std::path::Path, Vec<(usize, std::ops::Range<usize>)>> =
                HashMap::new();
            for (i, row) in table.rows.iter().enumerate() {
                if row.fql_kind == lang::FQL_FUNCTION {
                    funcs_by_file
                        .entry(row.path.as_path())
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
                if !CF_FQL_KINDS.contains(&row.fql_kind.as_str()) {
                    continue;
                }
                if let Some(funcs) = funcs_by_file.get(row.path.as_path()) {
                    // Binary search: find the last function whose start ≤ row start.
                    let pos =
                        funcs.partition_point(|(_, range)| range.start <= row.byte_range.start);
                    if pos > 0 {
                        let (func_idx, ref func_range) = funcs[pos - 1];
                        if row.byte_range.end <= func_range.end {
                            let entry = metrics.entry(func_idx).or_insert((0, 0, 0));
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
                            // Record enclosing function name for this CF row.
                            drop(cf_encl.insert(cf_idx, table.rows[func_idx].name.clone()));
                        }
                    }
                }
            }
            (metrics, cf_encl)
        };

        // Phase 2 (mutable): apply aggregated metrics to function rows.
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

        // Phase 3 (mutable): write enclosing_fn to CF rows.
        for (cf_idx, fn_name) in cf_encl {
            drop(
                table.rows[cf_idx]
                    .fields
                    .insert("enclosing_fn".to_string(), fn_name),
            );
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

    // Call expressions: map the whole call as one letter
    if config.is_call_expression_kind(kind) {
        let text = node_text(source, node);
        let label = mapping
            .entry(text)
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

    // Subscript expressions: map as one unit
    if config.is_subscript_expression_kind(kind) {
        let text = node_text(source, node);
        let label = mapping
            .entry(text)
            .or_insert_with(|| next_label(next_letter))
            .clone();
        result.push_str(&label);
        return;
    }

    // Pointer dereference / address-of: map as one unit so that
    // `*ptr` and `ptr` get distinct letters.
    if config.address_of_expression_kind().contains(kind) {
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
