#![allow(
    clippy::map_unwrap_or,
    clippy::single_char_pattern,
    clippy::unnecessary_get_then_check,
    clippy::uninlined_format_args
)]
//! Comprehensive integration tests for all enrichment fields.
//!
//! These tests exercise the full pipeline: **parser → IR → engine → result**
//! using the `enrichment_patterns.cpp` fixture plus the `motor_control`
//! fixtures in a temp workspace.
//!
//! Run with: `cargo test -p forgeql-core --test enrichment_integration`
//!
//! Organisation:
//!   §1  — NamingEnricher     (naming, name_length)
//!   §2  — CommentEnricher    (comment_style, has_doc)
//!   §3  — NumberEnricher     (num_format, has_separator, num_sign, num_value, num_suffix,
//!                              suffix_meaning, is_magic)
//!   §4  — ControlFlowEnricher (condition_tests, paren_depth, condition_text, has_catch_all,
//!                              catch_all_kind, for_style, has_assignment_in_condition,
//!                              mixed_logic, dup_logic, branch_count, max_condition_tests,
//!                              max_paren_depth)
//!   §5  — OperatorEnricher   (increment_style, increment_op, compound_op, operand,
//!                              shift_direction, shift_amount, shift_operand, operator_category)
//!   §6  — MetricsEnricher    (lines, param_count, return_count, goto_count, string_count,
//!                              throw_count, member_count, is_const, is_volatile, is_static,
//!                              is_inline, is_override, is_final, visibility)
//!   §7  — CastEnricher       (cast_style, cast_target_type, cast_safety)
//!   §8  — RedundancyEnricher (repeated_condition_calls, has_repeated_condition_calls,
//!                              null_check_count, duplicate_condition)
//!   §9  — ScopeEnricher      (scope, storage, binding_kind, is_exported)
//!   §9b — MemberEnricher     (body_symbol, member_kind, owner_kind)
//!   §10 — field_num() fallback (numeric comparison on dynamic fields)
//!   §15 — ShadowEnricher      (has_shadow, shadow_count, shadow_vars)
//!   §16 — UnusedParamEnricher  (has_unused_param, unused_param_count, unused_params)
//!   §17 — FallthroughEnricher  (has_fallthrough, fallthrough_count)
//!   §18 — RecursionEnricher    (is_recursive, recursion_count)
//!   §19 — TodoEnricher          (has_todo, todo_count, todo_tags)
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    unused_results
)]

use std::collections::HashSet;
mod common;

mod enrichment_harness;
use enrichment_harness::*;

// =======================================================================
// §1 — NamingEnricher
// =======================================================================

names_contains_case! {
    naming_camel_case:
        "FIND symbols WHERE naming = 'camelCase'" => ["camelCaseVar", "docLineTarget"];
    naming_pascal_case:
        "FIND symbols WHERE naming = 'PascalCase'" => ["PascalCaseVar", "SimpleStruct", "SimpleEnum", "SimpleClass"];
    naming_snake_case:
        "FIND symbols WHERE naming = 'snake_case' LIMIT 1000" => ["snake_case_var"];
    naming_upper_snake:
        "FIND symbols WHERE naming = 'UPPER_SNAKE'" => ["UPPER_SNAKE_VAR", "ENUM_A", "ENUM_B"];
    naming_flatcase:
        "FIND symbols WHERE naming = 'flatcase' LIMIT 1000" => ["flatcasevar"];
}

#[test]
fn naming_name_length() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // camelCaseVar has 12 chars
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'camelCaseVar'");
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty());
    let m = &qr.results[0];
    assert_eq!(field(m, "name_length"), "12");
    assert_eq!(field(m, "naming"), "camelCase");
}

#[test]
fn naming_name_length_numeric_comparison() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // Find symbols with name_length > 20 (long identifiers)
    let r = exec(&mut e, &sid, "FIND symbols WHERE name_length > 20");
    let qr = common::as_query(&r);
    // All returned symbols must have name_length > 20
    for m in &qr.results {
        let len: usize = field(m, "name_length").parse().unwrap();
        assert!(
            len > 20,
            "expected name_length > 20, got {len} for '{}'",
            m.name
        );
    }
}

// =======================================================================
// §2 — CommentEnricher
// =======================================================================

field_all_case! {
    comment_style_doc_line:
        "FIND symbols WHERE node_kind = 'comment' WHERE comment_style = 'doc_line'", field = "comment_style" => "doc_line";
    comment_style_doc_block:
        "FIND symbols WHERE node_kind = 'comment' WHERE comment_style = 'doc_block'", field = "comment_style" => "doc_block";
    comment_style_block:
        "FIND symbols WHERE node_kind = 'comment' WHERE comment_style = 'block'", field = "comment_style" => "block";
    comment_style_line:
        "FIND symbols WHERE node_kind = 'comment' WHERE comment_style = 'line'", field = "comment_style" => "line";
}

names_contains_case! {
    comment_has_doc_true:
        "FIND symbols WHERE node_kind = 'function_definition' WHERE has_doc = 'true'" => ["docBlockFunction"];
    comment_has_doc_false:
        "FIND symbols WHERE node_kind = 'function_definition' WHERE has_doc = 'false' LIMIT 1000" => ["noDocFunction", "anotherNoDocFunction"];
}

// =======================================================================
// §3 — NumberEnricher
// =======================================================================

names_contains_case! {
    number_format_dec:
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_format = 'dec' LIMIT 1000" => ["42"];
    number_format_hex:
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_format = 'hex'" => ["0xFF"];
    number_format_bin:
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_format = 'bin'" => ["0b1010"];
    number_format_oct:
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_format = 'oct'" => ["0777"];
    number_format_float:
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_format = 'float'" => ["3.14"];
    number_format_scientific:
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_format = 'scientific'" => ["1.5e-3"];
    number_suffix_u:
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_suffix = 'u'" => ["100u"];
    number_suffix_ul:
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_suffix = 'ul'" => ["200UL"];
    number_suffix_ll:
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_suffix = 'll'" => ["300LL"];
}

#[test]
fn number_is_magic_true() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE is_magic = 'true' LIMIT 100",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected magic numbers");

    // 42, 0xFF, 0b1010, etc. are all magic. `42` has to be asked for by name
    // rather than looked for in a page: a page is the k smallest rows under
    // the ordering the pipeline sorts by, not the first k the scan reached, so
    // whether a given value falls inside one is a fact about sort order.
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE is_magic = 'true' WHERE name = '42'",
    );
    let qr = common::as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(ns.contains(&"42"), "expected '42' as magic number: {ns:?}");
}

/// A page is a prefix of the answer, on this backend as on the other.
///
/// The in-memory backend used to build only the first `LIMIT` rows its scan
/// reached and then sort those, so a small `LIMIT` returned the k smallest of
/// a scan prefix rather than of the answer — the same defect the columnar
/// backend's segment fetch cap carried. Raising the limit therefore changed
/// which rows a smaller one had already shown, which is what this checks.
///
/// The prefix property holds up to rows the ordering does not separate: the
/// bounded partition is unstable, so where more rows than the page holds
/// compare equal on `name`, `line` and `path`, which of them survives is not
/// decided by the comparator. This fixture has no such tie across the
/// boundary — if one is ever introduced, the assertion below is what will say
/// so, and widening the ordering rather than loosening the assertion is the
/// fix.
#[test]
fn a_small_limit_returns_the_head_of_the_larger_page() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let wide = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE is_magic = 'true' LIMIT 100",
    );
    let wide = common::as_query(&wide);
    let wide_names: Vec<String> = names(&wide.results)
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert!(
        wide_names.len() > 3,
        "fixture must hold more magic numbers than the narrow page asks for"
    );

    let narrow = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE is_magic = 'true' LIMIT 3",
    );
    let narrow = common::as_query(&narrow);
    let narrow_names: Vec<&str> = names(&narrow.results);

    assert_eq!(narrow_names.len(), 3, "short page: {narrow_names:?}");
    assert_eq!(
        narrow_names,
        wide_names[..3]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        "LIMIT 3 must return the first three rows of the LIMIT 100 answer, \
         not three rows the scan happened to reach first"
    );
}

#[test]
fn number_is_magic_false() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE is_magic = 'false'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected non-magic numbers");
    // Values in named-constant contexts (init_declarator, enumerator, preproc_def)
    // are not magic regardless of their value.  zeroVal=0 and oneVal=1 (both in
    // init_declarator) must appear in the non-magic set.
    let values: HashSet<&str> = qr.results.iter().map(|m| field(m, "num_value")).collect();
    assert!(
        values.contains("0") || values.contains("1"),
        "expected 0 or 1 among non-magic values (from init_declarator context): {values:?}"
    );
}

// NOTE: #define values in tree-sitter-cpp are parsed as `preproc_arg` not
// `number_literal`, so BUG-06 #define suppression is verified at the config
// level only — no integration test needed.
#[test]
fn number_is_magic_false_enumerator() {
    // K_FLAG_A = 8 in enum NamedConstants — must NOT be magic (BUG-06)
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE name = '8' WHERE is_magic = 'false'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "8 in enum NamedConstants must NOT be magic (BUG-06)"
    );
}

#[test]
fn number_is_magic_false_const_var() {
    // kMaxRetries = 3 — file-scope const var initialiser must NOT be magic (BUG-06)
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE name = '3' WHERE is_magic = 'false'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "3 in 'const int kMaxRetries = 3' must NOT be magic (BUG-06)"
    );
}

#[test]
fn number_is_magic_true_bare_expr_regression() {
    // 42 in bare if-expression must STILL be magic after BUG-06 fix
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE name = '42' WHERE is_magic = 'true'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "42 in bare expression must still be magic after BUG-06 fix"
    );
}

// Bug 2a — 0/1 in semantic comparison contexts must be flagged is_magic='true'

#[test]
fn number_is_magic_true_one_in_comparison() {
    // `if (status == 1)` — the 1 carries semantic meaning (STATUS_OK) and must be magic.
    // Regression: previously blanket-excluded because value was in {0, 1, -1}.
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE name = '1' \
         WHERE is_magic = 'true' LIMIT 100",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "1 in `if (status == 1)` must be is_magic='true' (Bug 2a regression)"
    );
}

#[test]
fn number_is_magic_true_zero_in_comparison() {
    // `if (status == 0)` — the 0 carries semantic meaning (ERROR) and must be magic.
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE name = '0' \
         WHERE is_magic = 'true' LIMIT 100",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "0 in `if (status == 0)` must be is_magic='true' (Bug 2a regression)"
    );
}

#[test]
fn number_is_magic_false_subscript_zero() {
    // `buf[0]` — first-element subscript access is a structural idiom.
    // 0 as a subscript index must NOT be magic.
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE name = '0' \
         WHERE is_magic = 'false' LIMIT 100",
    );
    let qr = common::as_query(&r);
    // At least one '0' must be non-magic (the one in buf[0] subscript context,
    // plus any zeros in init_declarator contexts).
    assert!(
        !qr.results.is_empty(),
        "0 in array subscript `buf[0]` must be is_magic='false' (Bug 2a subscript exemption)"
    );
}

// Bug 2b — numbers inside string literals must NOT be indexed

#[test]
fn number_not_indexed_inside_string_literal() {
    // 9999 appears ONLY inside the string `"limit is 9999 per second"` inside
    // a function body.  string_content is always a leaf in tree-sitter-cpp so
    // no phantom node is emitted — this test guards against regressions where
    // the enricher would accidentally index string content.
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE name = '9999'",
    );
    let qr = common::as_query(&r);
    assert!(
        qr.results.is_empty(),
        "9999 inside a string literal must NOT be indexed as a number (Bug 2b-A regression)"
    );
}

#[test]
fn number_not_indexed_inside_error_recovery_node() {
    // 8881 and 8882 appear ONLY inside MODULE_PARM_DESC string arguments
    // that tree-sitter-cpp parses inside ERROR recovery subtrees (triggered by
    // using the `int` type keyword as a macro argument, which causes the parser
    // to misread the argument list as a parameter declaration).
    //
    // Without the inside_error guard in NumberEnricher, these phantom
    // number_literal nodes would be indexed as real magic numbers.
    let (mut e, sid, _d) = engine_bug2b_only();
    for sentinel in ["8881", "8882"] {
        let q =
            format!("FIND symbols WHERE node_kind = 'number_literal' WHERE name = '{sentinel}'");
        let r = exec(&mut e, &sid, &q);
        let qr = common::as_query(&r);
        assert!(
            qr.results.is_empty(),
            "{sentinel} inside a tree-sitter ERROR-recovery string must NOT be indexed (Bug 2b-B regression)"
        );
    }
}

#[test]
fn number_sign_zero() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_sign = 'zero'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected zero-valued numbers");
    for m in &qr.results {
        assert_eq!(
            field(m, "num_value"),
            "0",
            "expected num_value=0 for sign=zero"
        );
    }
}

#[test]
fn number_sign_positive() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_sign = 'positive'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected positive numbers");
    for m in &qr.results {
        let val: i64 = field(m, "num_value").parse().unwrap();
        assert!(
            val > 0,
            "expected positive num_value, got {val} for '{}'",
            m.name
        );
    }
}

#[test]
fn number_value_numeric_comparison() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // Find numbers with value > 200
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_value > 200",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected numbers with value > 200");
    for m in &qr.results {
        let val: i64 = field(m, "num_value").parse().unwrap();
        assert!(
            val > 200,
            "expected num_value > 200, got {val} for '{}'",
            m.name
        );
    }
}

// =======================================================================
// §4 — ControlFlowEnricher
// =======================================================================

#[test]
fn control_flow_if_statement_exists() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement'",
    );
    let qr = common::as_query(&r);
    assert!(qr.total > 0, "expected at least one if_statement");
}

#[test]
fn control_flow_condition_tests_simple() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // Simple if (a > 0) has 1 condition test
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' WHERE condition_tests = 1",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected if_statements with 1 condition test"
    );
}

#[test]
fn control_flow_condition_tests_complex() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // Complex condition: a > 0 && b < 10 || c == 5 → at least 3 tests
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' WHERE condition_tests > 2",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected if_statements with > 2 condition tests"
    );
}

#[test]
fn control_flow_paren_depth() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // The deeply nested condition: (((a > 0) && (b < 10)) || ((c == 5) && (d != 0)))
    // has paren_depth >= 3
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' WHERE paren_depth > 2",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected if_statements with paren_depth > 2"
    );
}

#[test]
fn control_flow_mixed_logic() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // "a > 0 && b < 10 || c == 5" mixes && and ||
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' WHERE mixed_logic = 'true'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected if_statements with mixed_logic=true"
    );
}

#[test]
fn control_flow_has_assignment_in_condition() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' WHERE has_assignment_in_condition = 'true'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one if_statement with assignment in condition"
    );
}

/// Regression: comparisons like `>=`, `<=`, `!=` must NOT trigger
/// `has_assignment_in_condition`. Only real `assignment_expression`
/// nodes should match.
#[test]
fn control_flow_no_false_positive_comparisons() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'noAssignCompare' WHERE fql_kind = 'function'",
    );
    let qr = common::as_query(&r);
    let _func = find_by_name(&qr.results, "noAssignCompare");

    // Now find all if_statements inside that function's file
    let r2 = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' WHERE has_assignment_in_condition = 'true'",
    );
    let qr2 = common::as_query(&r2);
    // None of the if-statements from noAssignCompare should be flagged
    for row in &qr2.results {
        // The condition skeletons from noAssignCompare are ((a)||((b-c)<d)) and (a&&(a))
        // They should NOT appear. Check by condition_text pattern.
        let cond = row
            .fields
            .get("condition_text")
            .map(String::as_str)
            .unwrap_or("");
        assert!(
            cond != "((a)||(a>=b))||((b-a)<c)" && cond != "(a<=b&&(a!=c))",
            "comparison-only condition should not be flagged as assignment: {cond}",
        );
    }
}

/// Regression: Zephyr-like ((offset < 0) || ((offset + len) > size)) must NOT
/// trigger has_assignment_in_condition.
#[test]
fn control_flow_no_false_positive_zephyr_like() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE fql_kind = 'if' WHERE has_assignment_in_condition = 'true'",
    );
    let qr = common::as_query(&r);
    let mut fps: Vec<String> = Vec::new();
    for row in &qr.results {
        let path = row
            .path
            .as_ref()
            .map(|p| p.to_string_lossy())
            .unwrap_or_default();
        if !path.contains("enrichment_patterns") {
            continue;
        }
        let cond = row
            .fields
            .get("condition_text")
            .map(String::as_str)
            .unwrap_or("");
        // The known true positive at line 76 has skeleton ((a=b)>c) from `(x = a + b) > 0`.
        // Skip it — it IS a real assignment.
        if cond == "((a=b)>c)" {
            continue;
        }
        fps.push(format!("line {:?}: '{cond}'", row.line));
    }
    assert!(
        fps.is_empty(),
        "false positives in enrichment_patterns.cpp: {:?}",
        fps,
    );
}

#[test]
fn control_flow_switch_has_catch_all() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'switch_statement' WHERE has_catch_all = 'true'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one switch with default"
    );
}

#[test]
fn control_flow_switch_no_default() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'switch_statement' WHERE has_catch_all = 'false'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one switch without default"
    );
}

#[test]
fn control_flow_while_statement() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'while_statement'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected while_statement");
    // while (a > 0 && b != 0) has 2 condition tests: one && joining two clauses
    let with_two = qr
        .results
        .iter()
        .any(|m| field(m, "condition_tests") == "2");
    assert!(with_two, "expected while_statement with condition_tests=2");
}

#[test]
fn control_flow_for_statement() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'for_statement'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected for_statement");
}

#[test]
fn control_flow_do_statement() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'do_statement'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected do_statement");
}

#[test]
fn control_flow_condition_text_has_skeleton() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' WHERE condition_tests > 1",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty());
    // Skeleton should use lowercase letters, not the original identifiers
    for m in &qr.results {
        let skeleton = field(m, "condition_text");
        assert!(
            !skeleton.is_empty(),
            "condition_text should not be empty for complex conditions"
        );
        // Skeleton should contain operator tokens
        let has_ops = skeleton.contains("&&")
            || skeleton.contains("||")
            || skeleton.contains("==")
            || skeleton.contains("!=")
            || skeleton.contains('>')
            || skeleton.contains('<');
        assert!(has_ops, "skeleton should contain operators: {skeleton}");
    }
}

#[test]
fn skeleton_keeps_assignment_operator() {
    // Regression: an assignment inside a condition (`=` where `==` was meant) is
    // the single most defect-shaped token a condition can hold, so it must
    // survive skeletonization — the skeleton has to agree with the
    // `has_assignment_in_condition` flag reported beside it. `if ((x = a + b) > 0)`
    // normalizes to `((a=b)>c)`: the `=` is kept, and the assigned value `a + b`
    // folds to a single operand (arithmetic only computes a value).
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE fql_kind = 'if' WHERE has_assignment_in_condition = 'true'",
    );
    let qr = common::as_query(&r);
    assert_eq!(
        qr.results.len(),
        1,
        "fixture has one assignment-in-condition"
    );
    let skeleton = field(&qr.results[0], "condition_text");
    assert_eq!(
        skeleton, "((a=b)>c)",
        "assignment `=` must survive in the skeleton"
    );
}

// -----------------------------------------------------------------------
// §4b — Skeleton regression tests (operator preservation, overflow, truncation)
// -----------------------------------------------------------------------

#[test]
fn skeleton_no_adjacent_letters() {
    // Regression: operators between leaf terms must never be dropped.
    // Condition `a > b && c < d || e != a` → skeleton with > && < || != operators.
    let (mut e, sid, _d) = engine_enrichment_only();

    // The skeleton for `a > b && c < d || e != a` contains all six operators.
    // Query for if_statements with mixed_logic that also contain !=.
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' \
         WHERE condition_text LIKE '%>%&&%<%||%!=%'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected skeleton with > && < || != operators"
    );
    let skeleton = field(&qr.results[0], "condition_text");
    // Must NOT contain two adjacent letter-like chars without operator between them
    let has_adjacent = skeleton
        .as_bytes()
        .windows(2)
        .any(|w| (w[0] as char).is_ascii_alphabetic() && (w[1] as char).is_ascii_alphabetic());
    assert!(
        !has_adjacent,
        "skeleton must not have adjacent letters without operator: {skeleton}"
    );
}

#[test]
fn skeleton_bitwise_operators_preserved() {
    // Regression: bitwise & and | must appear in skeleton.
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'switch_statement' \
         WHERE condition_text LIKE '%&%'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected switch skeleton with & operator"
    );
    let skeleton = field(&qr.results[0], "condition_text");
    assert!(
        skeleton.contains('&') && skeleton.contains('|'),
        "bitwise skeleton should have & and |: {skeleton}"
    );
}

#[test]
fn skeleton_overflow_uses_uppercase() {
    // With 28 unique terms, letters 27-28 must use uppercase A/B.
    let (mut e, sid, _d) = engine_enrichment_only();
    // The skeletonManyUniqueTerms function has an if_statement with 14 ==
    // comparisons chained by &&.  The skeleton has 28 unique leaf terms.
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' WHERE condition_tests > 13",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected if_statement with >13 condition tests (28-term condition)"
    );
    let skeleton = field(&qr.results[0], "condition_text");
    // Must contain uppercase letters (overflow beyond a-z)
    let has_upper = skeleton.chars().any(|c| c.is_ascii_uppercase());
    assert!(
        has_upper,
        "skeleton with 28 unique terms must use uppercase overflow labels: {skeleton}"
    );
    // Must NOT contain '$' (only 28 terms, not 53+)
    assert!(
        !skeleton.contains('$'),
        "28 terms should fit in a-z + A-B, no $ needed: {skeleton}"
    );
}

#[test]
fn skeleton_all_letters_have_operators() {
    // Global regression: for EVERY condition skeleton with >1 test,
    // there must be no adjacent leaf-letters without an operator.
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE condition_tests > 1");
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty());

    for m in &qr.results {
        let skeleton = field(m, "condition_text");
        // Check for two adjacent lowercase/uppercase letters
        let has_adjacent = skeleton.as_bytes().windows(2).any(|w| {
            let a = w[0] as char;
            let b = w[1] as char;
            a.is_ascii_alphabetic() && b.is_ascii_alphabetic() && a != '$' && b != '$'
        });
        assert!(
            !has_adjacent,
            "adjacent letters without operator in skeleton: {skeleton} (node_kind: {:?})",
            m.node_kind
        );
    }
}

#[test]
fn control_flow_branch_count_on_function() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'controlFlowPatterns'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty());
    let m = find_by_name(&qr.results, "controlFlowPatterns");
    // controlFlowPatterns has: 4 ifs + 2 switches + 1 while + 1 for + 1 do = 9 control-flow nodes
    let bc: usize = field(m, "branch_count").parse().unwrap();
    assert!(
        bc >= 9,
        "expected branch_count >= 9 for controlFlowPatterns, got {bc}"
    );
}

#[test]
fn control_flow_max_condition_tests_on_function() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'controlFlowPatterns'",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "controlFlowPatterns");
    // The most complex condition has 4+ tests
    let mct: usize = field(m, "max_condition_tests").parse().unwrap();
    assert!(
        mct >= 4,
        "expected max_condition_tests >= 4 for controlFlowPatterns, got {mct}"
    );
}

#[test]
fn control_flow_max_paren_depth_on_function() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'controlFlowPatterns'",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "controlFlowPatterns");
    let mpd: usize = field(m, "max_paren_depth").parse().unwrap();
    assert!(
        mpd >= 3,
        "expected max_paren_depth >= 3 for controlFlowPatterns, got {mpd}"
    );
}

// =======================================================================
// §5 — OperatorEnricher
// =======================================================================

#[test]
fn operator_prefix_increment() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'update_expression' WHERE increment_style = 'prefix' WHERE increment_op = '++'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected prefix ++ update_expression"
    );
    for m in &qr.results {
        assert_eq!(field(m, "increment_style"), "prefix");
        assert_eq!(field(m, "increment_op"), "++");
    }
}

#[test]
fn operator_prefix_decrement() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // --val is a prefix decrement
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'update_expression' WHERE increment_style = 'prefix'",
    );
    let qr = common::as_query(&r);
    // At least one should be a -- (--val)
    let has_dec = qr.results.iter().any(|m| field(m, "increment_op") == "--");
    assert!(has_dec, "expected prefix -- update_expression");
}

#[test]
fn operator_postfix_increment() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'update_expression' WHERE increment_style = 'postfix' WHERE increment_op = '++'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected postfix ++ update_expression"
    );
}

#[test]
fn operator_postfix_decrement() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // val-- is a postfix decrement
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'update_expression' WHERE increment_style = 'postfix'",
    );
    let qr = common::as_query(&r);
    // At least one should be a -- (val--)
    let has_dec = qr.results.iter().any(|m| field(m, "increment_op") == "--");
    assert!(has_dec, "expected postfix -- update_expression");
}

#[test]
fn operator_compound_add() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'compound_assignment' WHERE compound_op = '+='",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected += compound_assignment");
    for m in &qr.results {
        assert_eq!(field(m, "compound_op"), "+=");
    }
}

#[test]
fn operator_compound_sub() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'compound_assignment' WHERE compound_op = '-='",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected -= compound_assignment");
}

#[test]
fn operator_compound_mul_div_mod() {
    let (mut e, sid, _d) = engine_enrichment_only();
    for op in &["*=", "/=", "%="] {
        let r = exec(
            &mut e,
            &sid,
            &format!(
                "FIND symbols WHERE node_kind = 'compound_assignment' WHERE compound_op = '{op}'"
            ),
        );
        let qr = common::as_query(&r);
        assert!(!qr.results.is_empty(), "expected {op} compound_assignment");
    }
}

#[test]
fn operator_compound_bitwise() {
    let (mut e, sid, _d) = engine_enrichment_only();
    for op in &["&=", "|=", "^="] {
        let r = exec(
            &mut e,
            &sid,
            &format!(
                "FIND symbols WHERE node_kind = 'compound_assignment' WHERE compound_op = '{op}'"
            ),
        );
        let qr = common::as_query(&r);
        assert!(!qr.results.is_empty(), "expected {op} compound_assignment");
    }
}

#[test]
fn operator_compound_has_operand() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'compound_assignment'",
    );
    let qr = common::as_query(&r);
    // Every compound assignment should have an operand (right-hand side)
    for m in &qr.results {
        assert!(
            field_opt(m, "operand").is_some(),
            "compound_assignment '{}' should have operand field",
            m.name
        );
    }
}

#[test]
fn operator_shift_left() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'shift_expression' WHERE shift_direction = 'left'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected left shift_expression");
    for m in &qr.results {
        assert_eq!(field(m, "shift_direction"), "left");
        // shift_amount should be present
        assert!(
            field_opt(m, "shift_amount").is_some(),
            "expected shift_amount"
        );
    }
}

#[test]
fn operator_shift_right() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'shift_expression' WHERE shift_direction = 'right'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected right shift_expression");
    for m in &qr.results {
        assert_eq!(field(m, "shift_direction"), "right");
    }
}

#[test]
fn operator_shift_amount_value() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'shift_expression'",
    );
    let qr = common::as_query(&r);
    // val << 4 has shift_amount = "4", val >> 2 has shift_amount = "2"
    let amounts: Vec<&str> = qr
        .results
        .iter()
        .filter_map(|m| field_opt(m, "shift_amount"))
        .collect();
    assert!(
        amounts.contains(&"4"),
        "expected shift_amount '4' in {amounts:?}"
    );
    assert!(
        amounts.contains(&"2"),
        "expected shift_amount '2' in {amounts:?}"
    );
}

#[test]
fn operator_shift_operand_present() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'shift_expression'",
    );
    let qr = common::as_query(&r);
    for m in &qr.results {
        assert!(
            field_opt(m, "shift_operand").is_some(),
            "shift_expression '{}' should have shift_operand",
            m.name
        );
    }
}

// =======================================================================
// §6 — MetricsEnricher
// =======================================================================

#[test]
fn metrics_lines_on_function() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'controlFlowPatterns'",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "controlFlowPatterns");
    let lines: usize = field(m, "lines").parse().unwrap();
    assert!(
        lines > 10,
        "expected lines > 10 for controlFlowPatterns, got {lines}"
    );
}

#[test]
fn metrics_lines_numeric_comparison() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE lines > 10",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected functions with lines > 10");
    for m in &qr.results {
        let l: usize = field(m, "lines").parse().unwrap();
        assert!(l > 10, "expected lines > 10, got {l} for '{}'", m.name);
    }
}

#[test]
fn metrics_lines_order_by_desc() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' ORDER BY lines DESC LIMIT 5",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty());
    // Verify descending order
    let line_values: Vec<usize> = qr
        .results
        .iter()
        .map(|m| field(m, "lines").parse::<usize>().unwrap())
        .collect();
    for w in line_values.windows(2) {
        assert!(
            w[0] >= w[1],
            "lines should be in descending order: {line_values:?}"
        );
    }
}

#[test]
fn metrics_param_count() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'manyParams'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "manyParams");
    assert_eq!(field(m, "param_count"), "5", "manyParams has 5 parameters");
}

#[test]
fn metrics_param_count_comparison() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE param_count > 3",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected functions with > 3 params");
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"manyParams"),
        "manyParams should have > 3 params: {ns:?}"
    );
    assert!(
        ns.contains(&"controlFlowPatterns"),
        "controlFlowPatterns should have > 3 params: {ns:?}"
    );
}

#[test]
fn metrics_param_count_no_lambda_inflation() {
    // outerNoParams has 0 outer params; lambda inside has 2 — must report 0 (BUG-05/NEW-01)
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'outerNoParams'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "outerNoParams");
    let pc: usize = field(m, "param_count").parse().unwrap();
    assert_eq!(
        pc, 0,
        "outerNoParams has 0 params but lambda params inflated it to {pc}"
    );
}

#[test]
fn metrics_param_count_lambda_sibling() {
    // outerTwoParams has 2 outer params; lambda inside has 3 — must report 2 (BUG-05/NEW-01)
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'outerTwoParams'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "outerTwoParams");
    let pc: usize = field(m, "param_count").parse().unwrap();
    assert_eq!(
        pc, 2,
        "outerTwoParams has 2 outer params (lambda's 3 params must not be counted): got {pc}"
    );
}

#[test]
fn metrics_return_count_no_lambda_inflation() {
    // outerOneReturn has 1 outer return; lambda inside has another — must report 1 (BUG-05/NEW-01)
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'outerOneReturn'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "outerOneReturn");
    let rc: usize = field(m, "return_count").parse().unwrap();
    assert_eq!(
        rc, 1,
        "outerOneReturn has 1 outer return (lambda return must not inflate count): got {rc}"
    );
}
#[test]
fn metrics_return_count() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'multiReturn'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "multiReturn");
    let rc: usize = field(m, "return_count").parse().unwrap();
    assert_eq!(rc, 3, "multiReturn has 3 return statements");
}

#[test]
fn metrics_return_count_comparison() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE return_count > 1",
    );
    let qr = common::as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"multiReturn"),
        "multiReturn should have return_count > 1: {ns:?}"
    );
}

#[test]
fn metrics_string_count() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'withStrings'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "withStrings");
    let sc: usize = field(m, "string_count").parse().unwrap();
    assert_eq!(sc, 3, "withStrings has 3 string literals");
}

named_field_case! {
    metrics_member_count_struct:
        "FIND symbols WHERE name = 'SimpleStruct'", find = "SimpleStruct", field = "member_count" => "3";
    metrics_member_count_enum:
        "FIND symbols WHERE name = 'SimpleEnum'", find = "SimpleEnum", field = "member_count" => "4";
    metrics_is_inline:
        "FIND symbols WHERE name = 'inlineFunc'", find = "inlineFunc", field = "is_inline" => "true";
    metrics_is_const:
        "FIND symbols WHERE name = 'constVar'", find = "constVar", field = "is_const" => "true";
    metrics_is_volatile:
        "FIND symbols WHERE name = 'volatileVar'", find = "volatileVar", field = "is_volatile" => "true";
}

#[test]
fn metrics_member_count_class() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'SimpleClass'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "SimpleClass");
    let mc: usize = field(m, "member_count").parse().unwrap();
    // SimpleClass has: publicField, publicMethod, privateField, protectedField = 4 field_declarations
    assert!(mc >= 3, "SimpleClass should have >= 3 members, got {mc}");
}

// NOTE: field_declaration nodes are not indexed by extract_name() in
// tree-sitter-cpp 0.23, so individual class member fields (publicField,
// privateField, protectedField) don't produce rows.  The visibility
// enricher only works on node kinds that ARE indexed. We verify
// visibility on class_specifier member_count instead.

#[test]
fn metrics_class_has_member_count() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'SimpleClass'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "SimpleClass");
    let mc: usize = field(m, "member_count").parse().unwrap();
    assert!(
        mc >= 1,
        "SimpleClass should have member_count >= 1, got {mc}"
    );
}

#[test]
fn metrics_lines_on_struct() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'SimpleStruct'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "SimpleStruct");
    let lines: usize = field(m, "lines").parse().unwrap();
    assert!(
        lines >= 3,
        "SimpleStruct should span at least 3 lines, got {lines}"
    );
}

#[test]
fn metrics_lines_not_clipped_for_clean_function() {
    // Regression for the tree-sitter misparse lines-inflation fix: a correctly-parsed
    // function with no absorbed top-level declarations in any compound_statement
    // must retain its original line count (first_absorbed_toplevel_in_compound returns None).
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'multiReturn'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "multiReturn");
    let lines: usize = field(m, "lines").parse().unwrap();
    assert_eq!(
        lines, 5,
        "multiReturn spans exactly 5 lines and must not be clipped: got {lines}"
    );
}

#[test]
fn metrics_lines_not_clipped_for_c99_designator_array() {
    // Regression for false-positive absorbed-sibling detection: a function
    // containing a local static array with C99 subscript designators
    // ([N] = value) must NOT be mistaken for a misparsed function that
    // absorbed a file-scope global.  first_absorbed_toplevel_in_compound must
    // return None for this function.
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'withC99DesignatorArray'",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "withC99DesignatorArray");
    let lines: usize = field(m, "lines").parse().unwrap();
    assert!(
        lines >= 10,
        "withC99DesignatorArray contains a large C99 subscript-designator array \
         and must not be clipped; expected >= 10 lines, got {lines}"
    );
}

// =======================================================================
// §7 — CastEnricher
// =======================================================================

#[test]
fn cast_c_style() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'cast_expression' WHERE cast_style = 'c_style'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected at least one C-style cast");
    for m in &qr.results {
        assert_eq!(field(m, "cast_style"), "c_style");
    }
}

// NOTE: Named C++ casts (reinterpret_cast, const_cast, static_cast, dynamic_cast)
// are NOT indexed as separate node kinds in tree-sitter-cpp 0.23.
// cast_safety tests are therefore limited to c_style casts.

#[test]
fn cast_c_style_has_target_type() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'cast_expression'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty());
    // C-style casts should have cast_target_type
    for m in &qr.results {
        assert!(
            field_opt(m, "cast_target_type").is_some(),
            "C-style cast should have cast_target_type"
        );
    }
}

#[test]
fn cast_c_style_count() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'cast_expression' WHERE cast_style = 'c_style'",
    );
    let qr = common::as_query(&r);
    // enrichment_patterns.cpp has at least one C-style cast: (int)x
    assert!(
        qr.total >= 1,
        "expected at least 1 C-style cast, got {}",
        qr.total
    );
}

// =======================================================================
// §8 — RedundancyEnricher
// =======================================================================

#[test]
fn redundancy_has_repeated_condition_calls() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'redundancyPatterns'",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "redundancyPatterns");
    assert_eq!(
        field(m, "has_repeated_condition_calls"),
        "true",
        "redundancyPatterns should have repeated condition calls"
    );
}

#[test]
fn redundancy_repeated_condition_calls_contains_get_value() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'redundancyPatterns'",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "redundancyPatterns");
    let calls = field(m, "repeated_condition_calls");
    assert!(
        calls.contains("getValue"),
        "expected 'getValue' in repeated_condition_calls: '{calls}'"
    );
}

#[test]
fn redundancy_repeated_condition_calls_contains_is_ready() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'redundancyPatterns'",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "redundancyPatterns");
    let calls = field(m, "repeated_condition_calls");
    assert!(
        calls.contains("isReady"),
        "expected 'isReady' in repeated_condition_calls: '{calls}'"
    );
}

#[test]
fn redundancy_null_check_count() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'redundancyPatterns'",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "redundancyPatterns");
    let ncc: usize = field(m, "null_check_count").parse().unwrap();
    // ptr1 != nullptr, ptr2 != nullptr, ptr1 != nullptr, ptr2 == nullptr = 4 null checks
    assert!(
        ncc >= 4,
        "expected null_check_count >= 4 for redundancyPatterns, got {ncc}"
    );
}

#[test]
fn redundancy_no_repeated_calls_for_simple_function() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'controlFlowPatterns'",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "controlFlowPatterns");
    assert_eq!(
        field(m, "has_repeated_condition_calls"),
        "false",
        "controlFlowPatterns should NOT have repeated condition calls"
    );
}

#[test]
fn redundancy_null_check_count_zero_for_no_checks() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'operatorPatterns'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "operatorPatterns");
    assert_eq!(
        field(m, "null_check_count"),
        "0",
        "operatorPatterns should have 0 null checks"
    );
}

#[test]
fn redundancy_duplicate_condition_detected() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // duplicateConditions has two identical ifs: if (a > 0 && b < 10)
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' WHERE duplicate_condition = 'true'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one if_statement with duplicate_condition=true"
    );
    // Should have at least 2 (the pair of duplicates)
    assert!(
        qr.total >= 2,
        "expected at least 2 duplicate conditions, got {}",
        qr.total
    );
}

/// Simple guard conditions like `if (!ptr)` or `if (val < 0)` should not be
/// flagged even when repeated — their skeletons are too short to be useful.
#[test]
fn redundancy_duplicate_condition_skips_simple_guards() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE fql_kind = 'if' WHERE duplicate_condition = 'true'",
    );
    let qr = common::as_query(&r);
    for row in &qr.results {
        let path = row
            .path
            .as_ref()
            .map(|p| p.to_string_lossy())
            .unwrap_or_default();
        if !path.contains("enrichment_patterns") {
            continue;
        }
        let cond = row
            .fields
            .get("condition_text")
            .map(String::as_str)
            .unwrap_or("");
        // None of the simple guards from simpleGuards() should appear.
        assert!(
            cond != "(!a)" && cond != "(a<b)",
            "simple guard should not be flagged as duplicate: {cond}",
        );
    }
}

#[test]
fn redundancy_filter_repeated_calls_query() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE has_repeated_condition_calls = 'true'",
    );
    let qr = common::as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"redundancyPatterns"),
        "redundancyPatterns should be in results: {ns:?}"
    );
    // controlFlowPatterns should NOT be in results
    assert!(
        !ns.contains(&"controlFlowPatterns"),
        "controlFlowPatterns should NOT be in results: {ns:?}"
    );
}

#[test]
fn redundancy_null_check_count_on_motor_control() {
    let (mut e, sid, _d) = engine_with_session();
    // encenderMotor has: if (gCallbackEncendido != nullptr) → 1 null check
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'encenderMotor'");
    let qr = common::as_query(&r);
    // Find the function_definition (not declaration)
    let func = qr
        .results
        .iter()
        .find(|m| m.node_kind.as_deref() == Some("function_definition"));
    if let Some(m) = func {
        let ncc: usize = field(m, "null_check_count").parse().unwrap();
        assert!(
            ncc >= 1,
            "encenderMotor should have at least 1 null check, got {ncc}"
        );
    }
}

// =======================================================================
// §9 — ScopeEnricher
// =======================================================================

// ScopeEnricher sets scope/storage only on `declaration` nodes.
// function_definition nodes (like staticFunc) do NOT get scope.
// We test scope on a `static const` declaration instead.
#[test]
fn scope_file_for_static_declaration() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE storage = 'static' WHERE scope = 'file'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one file-scope static declaration"
    );
    // All results should indeed have file scope (verified by the WHERE)
    for m in &qr.results {
        assert_eq!(
            field(m, "scope"),
            "file",
            "static declaration '{}' should have file scope",
            m.name
        );
    }
}

#[test]
fn scope_local_for_regular_function() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'controlFlowPatterns'",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "controlFlowPatterns");
    // Non-static functions should not have scope=file
    let scope = field_opt(m, "scope").unwrap_or("global");
    assert_ne!(
        scope, "file",
        "non-static function should not have file scope"
    );
}

#[test]
fn scope_storage_static() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE storage = 'static'");
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one static-storage symbol"
    );
}

#[test]
fn scope_filter_file_scope() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE scope = 'file'");
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one file-scoped symbol"
    );
}

// =======================================================================
// §10 — field_num() fallback (numeric comparison on dynamic fields)
// =======================================================================

field_num_bound_case! {
    field_num_name_length_greater_than:
        "FIND symbols WHERE name_length > 15", field = "name_length", |v| v > 15, non_empty = true;
    field_num_name_length_less_than:
        "FIND symbols WHERE name_length < 3", field = "name_length", |v| v < 3, non_empty = false;
    field_num_condition_tests_gte:
        "FIND symbols WHERE condition_tests >= 3", field = "condition_tests", |v| v >= 3, non_empty = false;
    field_num_lines_lte:
        "FIND symbols WHERE node_kind = 'function_definition' WHERE lines <= 3", field = "lines", |v| v <= 3, non_empty = false;
}

names_contains_case! {
    field_num_return_count_eq:
        "FIND symbols WHERE node_kind = 'function_definition' WHERE return_count = 3" => ["multiReturn"];
    field_num_branch_count_comparison:
        "FIND symbols WHERE node_kind = 'function_definition' WHERE branch_count > 5" => ["controlFlowPatterns"];
    field_num_member_count_comparison:
        "FIND symbols WHERE member_count >= 3" => ["SimpleStruct"];
    field_num_null_check_count_comparison:
        "FIND symbols WHERE null_check_count > 3" => ["redundancyPatterns"];
}

// =======================================================================
// §11 — Cross-enricher queries (combining fields from multiple enrichers)
// =======================================================================

#[test]
fn cross_enricher_long_camel_case_functions() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE naming = 'camelCase' WHERE lines > 5",
    );
    let qr = common::as_query(&r);
    for m in &qr.results {
        assert_eq!(field(m, "naming"), "camelCase");
        let lines: usize = field(m, "lines").parse().unwrap();
        assert!(lines > 5);
    }
}

#[test]
fn cross_enricher_complex_conditions_in_long_functions() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE max_condition_tests > 2 WHERE lines > 10",
    );
    let qr = common::as_query(&r);
    for m in &qr.results {
        let mct: usize = field(m, "max_condition_tests").parse().unwrap();
        let lines: usize = field(m, "lines").parse().unwrap();
        assert!(mct > 2);
        assert!(lines > 10);
    }
}

#[test]
fn cross_enricher_magic_hex_numbers() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_format = 'hex' WHERE is_magic = 'true'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected magic hex numbers");
    for m in &qr.results {
        assert_eq!(field(m, "num_format"), "hex");
        assert_eq!(field(m, "is_magic"), "true");
    }
}

#[test]
fn cross_enricher_functions_with_many_params_and_returns() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE param_count > 2 WHERE return_count > 0",
    );
    let qr = common::as_query(&r);
    for m in &qr.results {
        let pc: usize = field(m, "param_count").parse().unwrap();
        let rc: usize = field(m, "return_count").parse().unwrap();
        assert!(pc > 2);
        assert!(rc > 0);
    }
}

// =======================================================================
// =======================================================================
// §12 — Enrichment on motor_control fixtures (cross-file validation)
// =======================================================================

#[test]
fn motor_control_functions_have_naming() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'encenderMotor'");
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty());
    for m in &qr.results {
        assert_eq!(field(m, "naming"), "camelCase");
        assert_eq!(field(m, "name_length"), "13");
    }
}

#[test]
fn motor_control_enum_naming() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'VELOCIDAD_MAX'");
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty());
    let m = &qr.results[0];
    assert_eq!(field(m, "naming"), "UPPER_SNAKE");
}

#[test]
fn motor_control_switch_in_leer_sensor() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'leerSensor'");
    let qr = common::as_query(&r);
    let func = qr
        .results
        .iter()
        .find(|m| m.node_kind.as_deref() == Some("function_definition"));
    if let Some(m) = func {
        // leerSensor contains a switch with default
        let bc = field_opt(m, "branch_count");
        assert!(bc.is_some(), "leerSensor should have branch_count");
    }
}

#[test]
fn motor_control_struct_member_count() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'struct_specifier'",
    );
    let qr = common::as_query(&r);
    // The typedef struct in motor_control.h should have member_count
    for m in &qr.results {
        assert!(
            field_opt(m, "member_count").is_some(),
            "struct '{}' should have member_count",
            m.name
        );
    }
}

#[test]
fn motor_control_has_doc_on_functions() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE has_doc = 'true'",
    );
    let qr = common::as_query(&r);
    // encenderSistema is preceded by a /** comment
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"encenderSistema"),
        "encenderSistema should have has_doc=true: {ns:?}"
    );
}

// =======================================================================
// §4b — ControlFlowEnricher: dup_logic
// =======================================================================

#[test]
fn dup_logic_detected_bitwise() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // a & FLAG1 || a & FLAG1 → dup_logic=true
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' WHERE dup_logic = 'true'",
    );
    let qr = common::as_query(&r);
    assert!(
        qr.total >= 4,
        "expected at least 4 if_statements with dup_logic=true, got {}",
        qr.total
    );
}

#[test]
fn dup_logic_false_for_non_duplicates() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // "a > 0 && b < 10" is NOT a dup, "a > 0 || b > 0" is NOT a dup,
    // "ptr != nullptr && *ptr != 0" is NOT a dup (pointer_expression leaf).
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' WHERE dup_logic = 'true'",
    );
    let qr = common::as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    // None of the dup_logic=true names should be for the "clean" conditions.
    // The clean conditions in dupLogicPatterns have skeletons:
    //   a>b&&c<d  /  a>b||c>b  /  a!=b&&c!=d
    // Make sure those specific skeletons are NOT in the results.
    for m in &qr.results {
        let ct = field(m, "condition_text");
        assert_ne!(
            ct, "a>b&&c<d",
            "'a>b&&c<d' should NOT have dup_logic=true: {ns:?}"
        );
        assert_ne!(
            ct, "a>b||c>b",
            "'a>b||c>b' should NOT have dup_logic=true: {ns:?}"
        );
        assert_ne!(
            ct, "a!=b&&c!=d",
            "'a!=b&&c!=d' should NOT have dup_logic=true: {ns:?}"
        );
    }
}

#[test]
fn dup_logic_pointer_expression_not_false_positive() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // The fixture has: if (ptr != nullptr && *ptr != 0)
    // With the pointer_expression fix, *ptr gets a different letter than ptr,
    // so this should NOT be flagged as dup_logic.
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' WHERE condition_text LIKE '%a!=b&&c!=d%'",
    );
    let qr = common::as_query(&r);
    for m in &qr.results {
        let dl = field(m, "dup_logic");
        assert_eq!(
            dl, "false",
            "ptr != nullptr && *ptr != 0 should have dup_logic=false, got {dl}"
        );
    }
}

// -----------------------------------------------------------------------
// §12b — dup_logic: pointer-increment false-positive regression tests
// -----------------------------------------------------------------------

#[test]
fn dup_logic_not_false_positive_pointer_increment() {
    // `!isdigit(*p++) || !isdigit(*p++) || ...` must NOT be flagged.
    // Each *p++ is side-effectful (advances p), so the operands are NOT
    // duplicates even though they are textually identical.
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE fql_kind = 'if' WHERE enclosing_fn = 'dupLogicNotFalsePositiveIncrement'",
    );
    let qr = common::as_query(&r);
    for m in &qr.results {
        let dl = field(m, "dup_logic");
        assert_eq!(
            dl, "false",
            "dupLogicNotFalsePositiveIncrement: *p++ conditions must not flag dup_logic, got {dl}"
        );
    }
}

#[test]
fn no_repeated_calls_with_side_effectful_args() {
    // `isdigit(*p++)` called multiple times in a condition must NOT be counted
    // as a repeated_condition_call — each call reads a different byte.
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'noRepeatedCallsWithSideEffects'",
    );
    let qr = common::as_query(&r);
    assert_eq!(
        qr.results.len(),
        1,
        "expected exactly one noRepeatedCallsWithSideEffects function"
    );
    let m = &qr.results[0];
    assert_eq!(
        field(m, "has_repeated_condition_calls"),
        "false",
        "isdigit(*p++) repeated calls must not be flagged as has_repeated_condition_calls"
    );
}
// =======================================================================
// §13 — Phase 8 new enrichment fields
// =======================================================================

// --- NumberEnricher: suffix_meaning ---

field_all_case! {
    number_suffix_meaning_unsigned:
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_suffix = 'u'",
        field = "suffix_meaning" => "unsigned";
    number_suffix_meaning_unsigned_long:
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_suffix = 'ul'",
        field = "suffix_meaning" => "unsigned_long";
    number_suffix_meaning_long_long:
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_suffix = 'll'",
        field = "suffix_meaning" => "long_long";
}

// --- ControlFlowEnricher: catch_all_kind ---

#[test]
fn control_flow_catch_all_kind_default() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'switch_statement' WHERE has_catch_all = 'true'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected switch with catch_all");
    for m in &qr.results {
        assert_eq!(
            field(m, "catch_all_kind"),
            "default",
            "switch with catch-all should have catch_all_kind='default' on '{}'",
            m.name,
        );
    }
}

#[test]
fn control_flow_catch_all_kind_absent_when_no_default() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'switch_statement' WHERE has_catch_all = 'false'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected switch without catch_all");
    for m in &qr.results {
        assert!(
            field_opt(m, "catch_all_kind").is_none(),
            "switch without catch-all should not have catch_all_kind on '{}'",
            m.name,
        );
    }
}

// --- ControlFlowEnricher: for_style ---

#[test]
fn control_flow_for_style_traditional() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'for_statement'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one for_statement"
    );
    for m in &qr.results {
        assert_eq!(
            field(m, "for_style"),
            "traditional",
            "for_statement should have for_style='traditional' on '{}'",
            m.name,
        );
    }
}

#[test]
fn control_flow_for_style_range() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'for_range_loop'",
    );
    let qr = common::as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one for_range_loop"
    );
    for m in &qr.results {
        assert_eq!(
            field(m, "for_style"),
            "range",
            "for_range_loop should have for_style='range' on '{}'",
            m.name,
        );
    }
}

// --- OperatorEnricher: operator_category ---

field_all_case! {
    operator_category_increment:
        "FIND symbols WHERE node_kind = 'update_expression'",
        field = "operator_category" => "increment";
    operator_category_compound_arithmetic:
        "FIND symbols WHERE node_kind = 'compound_assignment' WHERE compound_op = '+='",
        field = "operator_category" => "arithmetic";
    operator_category_compound_bitwise:
        "FIND symbols WHERE node_kind = 'compound_assignment' WHERE compound_op = '&='",
        field = "operator_category" => "bitwise";
    operator_category_shift:
        "FIND symbols WHERE node_kind = 'shift_expression'",
        field = "operator_category" => "bitwise";
}

// --- MetricsEnricher: throw_count ---

#[test]
fn metrics_throw_count() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'throwingFunction'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "throwingFunction");
    assert_eq!(
        field(m, "throw_count"),
        "2",
        "throwingFunction should have throw_count=2"
    );
}

// --- CastEnricher: cast_safety ---

field_all_case! {
    cast_safety_c_style_unsafe:
        "FIND symbols WHERE node_kind = 'cast_expression'",
        field = "cast_safety" => "unsafe";
    cast_safety_static_cast_safe:
        "FIND symbols WHERE cast_style = 'static_cast'",
        field = "cast_safety" => "safe";
    cast_safety_reinterpret_cast_unsafe:
        "FIND symbols WHERE cast_style = 'reinterpret_cast'",
        field = "cast_safety" => "unsafe";
    cast_safety_const_cast_moderate:
        "FIND symbols WHERE cast_style = 'const_cast'",
        field = "cast_safety" => "moderate";
}

// In tree-sitter-cpp 0.23, named C++ casts (static_cast, reinterpret_cast,
// etc.) are parsed as call_expression(template_function(identifier)) rather
// than as distinct node kinds.  CastEnricher detects them via
// LanguageConfig::named_cast_keywords.

#[test]
fn cast_safety_named_cast_has_target_type() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // All named casts should expose cast_target_type
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE fql_kind = 'cast' WHERE cast_style = 'static_cast'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected static_cast rows");
    for m in &qr.results {
        assert!(
            field_opt(m, "cast_target_type").is_some(),
            "static_cast should have cast_target_type on '{}'",
            m.name
        );
    }
}

#[test]
fn cast_count_includes_named_casts() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // castPatterns() contains: 2 c_style + 1 reinterpret_cast + 1 const_cast
    //                          + 1 static_cast + 1 dynamic_cast  = 6 casts
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'castPatterns'");
    let qr = common::as_query(&r);
    let func = qr
        .results
        .iter()
        .find(|m| m.node_kind.as_deref() == Some("function_definition"));
    if let Some(m) = func {
        let cc: usize = field(m, "cast_count").parse().unwrap_or(0);
        assert!(
            cc >= 5,
            "castPatterns should have cast_count >= 5 (c_style×2 + reinterpret + const + static + dynamic), got {cc}"
        );
    }
}

// --- ScopeEnricher: binding_kind, is_exported ---

#[test]
fn scope_binding_kind_variable() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // static const int decNum = 42 → declaration with scope=file, binding_kind=variable
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'decNum'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "decNum");
    assert_eq!(field(m, "binding_kind"), "variable");
}

#[test]
fn scope_is_exported_static_not_exported() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // static declarations should NOT be exported
    let r = exec(&mut e, &sid, "FIND symbols WHERE storage = 'static'");
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "expected static declarations");
    for m in &qr.results {
        assert!(
            field_opt(m, "is_exported").is_none(),
            "static declaration '{}' should not be is_exported",
            m.name,
        );
    }
}

// --- MemberEnricher: member_kind, owner_kind ---

named_field_case! {
    member_kind_method:
        "FIND symbols WHERE name = 'declaredMethod'", find = "declaredMethod", field = "member_kind" => "method";
    member_kind_field:
        "FIND symbols WHERE name = 'publicField'", find = "publicField", field = "member_kind" => "field";
    member_owner_kind_class:
        "FIND symbols WHERE name = 'publicField'", find = "publicField", field = "owner_kind" => "class_specifier";
}

#[test]
fn member_owner_kind_struct() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'fieldA'");
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "fieldA");
    assert_eq!(field(m, "member_kind"), "field");
    assert_eq!(field(m, "owner_kind"), "struct_specifier");
}

// =======================================================================
// §11 — parameter_declaration indexing and fql_kind
// =======================================================================

#[test]
fn parameter_declaration_has_fql_kind_variable() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // manyParams(int a, int b, int c, int d, int e) — 5 parameters
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'parameter_declaration' WHERE name = 'a'",
    );
    let qr = common::as_query(&r);
    assert!(!qr.results.is_empty(), "parameter 'a' should be indexed");
    let m = find_by_name(&qr.results, "a");
    assert_eq!(
        m.fql_kind.as_deref(),
        Some("variable"),
        "parameter_declaration should have fql_kind = 'variable', got {:?}",
        m.fql_kind,
    );
}

#[test]
fn parameter_fql_kind_variable_filter() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE fql_kind = 'variable' WHERE name = 'a'",
    );
    let qr = common::as_query(&r);
    // Parameter 'a' should appear when filtering by fql_kind = 'variable'
    let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"a"),
        "parameter 'a' should match fql_kind = 'variable', got: {names:?}",
    );
}
