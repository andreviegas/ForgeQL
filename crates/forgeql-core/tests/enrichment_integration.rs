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
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use forgeql_core::ast::lang::{CppLanguageInline, LanguageRegistry};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::parser;
use forgeql_core::result::{ForgeQLResult, SymbolMatch};
use tempfile::tempdir;

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/fixtures")
}

/// Create a temp workspace with ALL fixtures and boot the engine.
fn engine_with_session() -> (ForgeQLEngine, String, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let src = fixtures_dir();

    for file in &[
        "motor_control.h",
        "motor_control.cpp",
        "enrichment_patterns.cpp",
    ] {
        fs::copy(src.join(file), dir.path().join(file))
            .unwrap_or_else(|e| panic!("copy {file}: {e}"));
    }

    let data_dir = dir.path().join("data");
    let registry = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]));
    let mut engine = ForgeQLEngine::new(data_dir, registry).expect("engine");
    let session_id = engine
        .register_local_session(dir.path())
        .expect("register session");

    (engine, session_id, dir)
}

/// Create a temp workspace with ONLY enrichment_patterns.cpp.
fn engine_enrichment_only() -> (ForgeQLEngine, String, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let src = fixtures_dir();

    fs::copy(
        src.join("enrichment_patterns.cpp"),
        dir.path().join("enrichment_patterns.cpp"),
    )
    .expect("copy enrichment_patterns.cpp");

    let data_dir = dir.path().join("data");
    let registry = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]));
    let mut engine = ForgeQLEngine::new(data_dir, registry).expect("engine");
    let session_id = engine
        .register_local_session(dir.path())
        .expect("register session");

    (engine, session_id, dir)
}

fn exec(engine: &mut ForgeQLEngine, sid: &str, fql: &str) -> ForgeQLResult {
    let ops = parser::parse(fql).unwrap_or_else(|e| panic!("parse failed for: {fql}: {e}"));
    let op = ops.first().expect("at least one op");
    engine
        .execute(Some(sid), op)
        .unwrap_or_else(|e| panic!("execute failed for: {fql}: {e}"))
}

fn as_query(r: &ForgeQLResult) -> &forgeql_core::result::QueryResult {
    match r {
        ForgeQLResult::Query(qr) => qr,
        other => panic!("expected Query, got: {other:?}"),
    }
}

/// Find first result matching a given name.
fn find_by_name<'a>(results: &'a [SymbolMatch], name: &str) -> &'a SymbolMatch {
    results
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no result with name '{name}'"))
}

/// Collect all names from query results.
fn names(results: &[SymbolMatch]) -> Vec<&str> {
    results.iter().map(|r| r.name.as_str()).collect()
}

/// Get a field value from a SymbolMatch, panicking with a clear message if missing.
fn field<'a>(m: &'a SymbolMatch, key: &str) -> &'a str {
    m.fields
        .get(key)
        .unwrap_or_else(|| {
            panic!(
                "field '{key}' missing on '{}' (available: {:?})",
                m.name,
                m.fields.keys().collect::<Vec<_>>()
            )
        })
        .as_str()
}

/// Optionally get a field value (returns None if absent).
fn field_opt<'a>(m: &'a SymbolMatch, key: &str) -> Option<&'a str> {
    m.fields.get(key).map(String::as_str)
}

// =======================================================================
// §1 — NamingEnricher
// =======================================================================

#[test]
fn naming_camel_case() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE naming = 'camelCase'");
    let qr = as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"camelCaseVar"),
        "expected camelCaseVar in {ns:?}"
    );
    assert!(
        ns.contains(&"docLineTarget"),
        "expected docLineTarget in {ns:?}"
    );
}

#[test]
fn naming_pascal_case() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE naming = 'PascalCase'");
    let qr = as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"PascalCaseVar"),
        "expected PascalCaseVar in {ns:?}"
    );
    assert!(
        ns.contains(&"SimpleStruct"),
        "expected SimpleStruct in {ns:?}"
    );
    assert!(ns.contains(&"SimpleEnum"), "expected SimpleEnum in {ns:?}");
    assert!(
        ns.contains(&"SimpleClass"),
        "expected SimpleClass in {ns:?}"
    );
}

#[test]
fn naming_snake_case() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE naming = 'snake_case'");
    let qr = as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"snake_case_var"),
        "expected snake_case_var in {ns:?}"
    );
}

#[test]
fn naming_upper_snake() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE naming = 'UPPER_SNAKE'");
    let qr = as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"UPPER_SNAKE_VAR"),
        "expected UPPER_SNAKE_VAR in {ns:?}"
    );
    assert!(ns.contains(&"ENUM_A"), "expected ENUM_A in {ns:?}");
    assert!(ns.contains(&"ENUM_B"), "expected ENUM_B in {ns:?}");
}

#[test]
fn naming_flatcase() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE naming = 'flatcase'");
    let qr = as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"flatcasevar"),
        "expected flatcasevar in {ns:?}"
    );
}

#[test]
fn naming_name_length() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // camelCaseVar has 12 chars
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'camelCaseVar'");
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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

#[test]
fn comment_style_doc_line() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'comment' WHERE comment_style = 'doc_line'",
    );
    let qr = as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one doc_line comment"
    );
    for m in &qr.results {
        assert_eq!(field(m, "comment_style"), "doc_line");
    }
}

#[test]
fn comment_style_doc_block() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'comment' WHERE comment_style = 'doc_block'",
    );
    let qr = as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one doc_block comment"
    );
    for m in &qr.results {
        assert_eq!(field(m, "comment_style"), "doc_block");
    }
}

#[test]
fn comment_style_block() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'comment' WHERE comment_style = 'block'",
    );
    let qr = as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one block comment"
    );
    for m in &qr.results {
        assert_eq!(field(m, "comment_style"), "block");
    }
}

#[test]
fn comment_style_line() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'comment' WHERE comment_style = 'line'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected at least one line comment");
    for m in &qr.results {
        assert_eq!(field(m, "comment_style"), "line");
    }
}

#[test]
fn comment_has_doc_true() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE has_doc = 'true'",
    );
    let qr = as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    // docBlockFunction is preceded by a /** comment
    assert!(
        ns.contains(&"docBlockFunction"),
        "expected docBlockFunction in has_doc=true results: {ns:?}"
    );
}

#[test]
fn comment_has_doc_false() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE has_doc = 'false'",
    );
    let qr = as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    // noDocFunction is preceded by a /* comment (not doc)
    assert!(
        ns.contains(&"noDocFunction"),
        "expected noDocFunction in has_doc=false results: {ns:?}"
    );
    assert!(
        ns.contains(&"anotherNoDocFunction"),
        "expected anotherNoDocFunction in has_doc=false results: {ns:?}"
    );
}

// =======================================================================
// §3 — NumberEnricher
// =======================================================================

#[test]
fn number_format_dec() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_format = 'dec'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected decimal numbers");
    // 42 should be among them
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"42"),
        "expected '42' in decimal numbers: {ns:?}"
    );
}

#[test]
fn number_format_hex() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_format = 'hex'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected hex numbers");
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"0xFF"),
        "expected '0xFF' in hex numbers: {ns:?}"
    );
}

#[test]
fn number_format_bin() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_format = 'bin'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected binary numbers");
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"0b1010"),
        "expected '0b1010' in binary numbers: {ns:?}"
    );
}

#[test]
fn number_format_oct() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_format = 'oct'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected octal numbers");
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"0777"),
        "expected '0777' in octal numbers: {ns:?}"
    );
}

#[test]
fn number_format_float() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_format = 'float'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected float numbers");
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"3.14"),
        "expected '3.14' in float numbers: {ns:?}"
    );
}

#[test]
fn number_format_scientific() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_format = 'scientific'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected scientific numbers");
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"1.5e-3"),
        "expected '1.5e-3' in scientific numbers: {ns:?}"
    );
}

#[test]
fn number_suffix_u() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_suffix = 'u'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected unsigned suffix numbers");
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"100u"),
        "expected '100u' in u-suffix numbers: {ns:?}"
    );
}

#[test]
fn number_suffix_ul() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_suffix = 'ul'",
    );
    let qr = as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"200UL"),
        "expected '200UL' in ul-suffix numbers: {ns:?}"
    );
}

#[test]
fn number_suffix_ll() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_suffix = 'll'",
    );
    let qr = as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"300LL"),
        "expected '300LL' in ll-suffix numbers: {ns:?}"
    );
}

#[test]
fn number_is_magic_true() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE is_magic = 'true'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected magic numbers");
    // 42, 0xFF, 0b1010, etc. are all magic
    let ns: Vec<&str> = names(&qr.results);
    assert!(ns.contains(&"42"), "expected '42' as magic number: {ns:?}");
}

#[test]
fn number_is_magic_false() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE is_magic = 'false'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected non-magic numbers (0, 1)");
    // 0 and 1 are not magic
    let values: HashSet<&str> = qr.results.iter().map(|m| field(m, "num_value")).collect();
    assert!(
        values.contains("0") || values.contains("1"),
        "expected 0 or 1 among non-magic values: {values:?}"
    );
}

#[test]
fn number_sign_zero() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_sign = 'zero'",
    );
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
    let _func = find_by_name(&qr.results, "noAssignCompare");

    // Now find all if_statements inside that function's file
    let r2 = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'if_statement' WHERE has_assignment_in_condition = 'true'",
    );
    let qr2 = as_query(&r2);
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
    let qr = as_query(&r);
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
        // The known true positive at line 76 has skeleton ((a)>b) from `(x = a + b) > 0`.
        // Skip it — it IS a real assignment.
        if cond == "((a)>b)" {
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected while_statement");
    // while (a > 0 && b != 0) has 3 condition tests: >, !=, &&
    let with_three = qr
        .results
        .iter()
        .any(|m| field(m, "condition_tests") == "3");
    assert!(
        with_three,
        "expected while_statement with condition_tests=3"
    );
}

#[test]
fn control_flow_for_statement() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'for_statement'",
    );
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
        "FIND symbols WHERE node_kind = 'if_statement' WHERE condition_tests > 20",
    );
    let qr = as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected if_statement with >20 condition tests (28-term condition)"
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
        let qr = as_query(&r);
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
        let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
fn metrics_return_count() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'multiReturn'");
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "withStrings");
    let sc: usize = field(m, "string_count").parse().unwrap();
    assert_eq!(sc, 3, "withStrings has 3 string literals");
}

#[test]
fn metrics_member_count_struct() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'SimpleStruct'");
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "SimpleStruct");
    assert_eq!(field(m, "member_count"), "3", "SimpleStruct has 3 fields");
}

#[test]
fn metrics_member_count_enum() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'SimpleEnum'");
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "SimpleEnum");
    assert_eq!(
        field(m, "member_count"),
        "4",
        "SimpleEnum has 4 enumerators"
    );
}

#[test]
fn metrics_member_count_class() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'SimpleClass'");
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "SimpleClass");
    let mc: usize = field(m, "member_count").parse().unwrap();
    // SimpleClass has: publicField, publicMethod, privateField, protectedField = 4 field_declarations
    assert!(mc >= 3, "SimpleClass should have >= 3 members, got {mc}");
}

#[test]
fn metrics_is_inline() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'inlineFunc'");
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "inlineFunc");
    assert_eq!(field(m, "is_inline"), "true", "inlineFunc should be inline");
}

#[test]
fn metrics_is_const() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'constVar'");
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "constVar");
    assert_eq!(field(m, "is_const"), "true", "constVar should be const");
}

#[test]
fn metrics_is_volatile() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'volatileVar'");
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "volatileVar");
    assert_eq!(
        field(m, "is_volatile"),
        "true",
        "volatileVar should be volatile"
    );
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "SimpleStruct");
    let lines: usize = field(m, "lines").parse().unwrap();
    assert!(
        lines >= 3,
        "SimpleStruct should span at least 3 lines, got {lines}"
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one static-storage symbol"
    );
}

#[test]
fn scope_filter_file_scope() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE scope = 'file'");
    let qr = as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one file-scoped symbol"
    );
}

// =======================================================================
// §10 — field_num() fallback (numeric comparison on dynamic fields)
// =======================================================================

#[test]
fn field_num_name_length_greater_than() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name_length > 15");
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    for m in &qr.results {
        let len: usize = field(m, "name_length").parse().unwrap();
        assert!(
            len > 15,
            "name_length should be > 15, got {len} for '{}'",
            m.name
        );
    }
}

#[test]
fn field_num_name_length_less_than() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name_length < 3");
    let qr = as_query(&r);
    // All returned symbols must have name_length < 3
    for m in &qr.results {
        let len: usize = field(m, "name_length").parse().unwrap();
        assert!(
            len < 3,
            "name_length should be < 3, got {len} for '{}'",
            m.name
        );
    }
}

#[test]
fn field_num_condition_tests_gte() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE condition_tests >= 3");
    let qr = as_query(&r);
    for m in &qr.results {
        let ct: i64 = field(m, "condition_tests").parse().unwrap();
        assert!(
            ct >= 3,
            "condition_tests should be >= 3, got {ct} for '{}'",
            m.name
        );
    }
}

#[test]
fn field_num_lines_lte() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE lines <= 3",
    );
    let qr = as_query(&r);
    for m in &qr.results {
        let l: usize = field(m, "lines").parse().unwrap();
        assert!(l <= 3, "lines should be <= 3, got {l} for '{}'", m.name);
    }
}

#[test]
fn field_num_return_count_eq() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE return_count = 3",
    );
    let qr = as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"multiReturn"),
        "multiReturn should have return_count=3: {ns:?}"
    );
}

#[test]
fn field_num_branch_count_comparison() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE branch_count > 5",
    );
    let qr = as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"controlFlowPatterns"),
        "controlFlowPatterns should have branch_count > 5: {ns:?}"
    );
}

#[test]
fn field_num_member_count_comparison() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE member_count >= 3");
    let qr = as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"SimpleStruct"),
        "SimpleStruct should have member_count >= 3: {ns:?}"
    );
}

#[test]
fn field_num_null_check_count_comparison() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE null_check_count > 3");
    let qr = as_query(&r);
    let ns: Vec<&str> = names(&qr.results);
    assert!(
        ns.contains(&"redundancyPatterns"),
        "redundancyPatterns should have null_check_count > 3: {ns:?}"
    );
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    let m = &qr.results[0];
    assert_eq!(field(m, "naming"), "UPPER_SNAKE");
}

#[test]
fn motor_control_switch_in_leer_sensor() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'leerSensor'");
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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

#[test]
fn number_suffix_meaning_unsigned() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_suffix = 'u'",
    );
    let qr = as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one 'u' suffix literal"
    );
    for m in &qr.results {
        assert_eq!(
            field(m, "suffix_meaning"),
            "unsigned",
            "suffix 'u' should have suffix_meaning 'unsigned' on '{}'",
            m.name,
        );
    }
}

#[test]
fn number_suffix_meaning_unsigned_long() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_suffix = 'ul'",
    );
    let qr = as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one 'ul' suffix literal"
    );
    for m in &qr.results {
        assert_eq!(field(m, "suffix_meaning"), "unsigned_long");
    }
}

#[test]
fn number_suffix_meaning_long_long() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'number_literal' WHERE num_suffix = 'll'",
    );
    let qr = as_query(&r);
    assert!(
        !qr.results.is_empty(),
        "expected at least one 'll' suffix literal"
    );
    for m in &qr.results {
        assert_eq!(field(m, "suffix_meaning"), "long_long");
    }
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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

#[test]
fn operator_category_increment() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'update_expression'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected update expressions");
    for m in &qr.results {
        assert_eq!(field(m, "operator_category"), "increment");
    }
}

#[test]
fn operator_category_compound_arithmetic() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'compound_assignment' WHERE compound_op = '+='",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected += compound assignments");
    for m in &qr.results {
        assert_eq!(field(m, "operator_category"), "arithmetic");
    }
}

#[test]
fn operator_category_compound_bitwise() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'compound_assignment' WHERE compound_op = '&='",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected &= compound assignments");
    for m in &qr.results {
        assert_eq!(field(m, "operator_category"), "bitwise");
    }
}

#[test]
fn operator_category_shift() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'shift_expression'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected shift expressions");
    for m in &qr.results {
        assert_eq!(field(m, "operator_category"), "bitwise");
    }
}

// --- MetricsEnricher: throw_count ---

#[test]
fn metrics_throw_count() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'throwingFunction'");
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "throwingFunction");
    assert_eq!(
        field(m, "throw_count"),
        "2",
        "throwingFunction should have throw_count=2"
    );
}

// --- CastEnricher: cast_safety ---

#[test]
fn cast_safety_c_style_unsafe() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'cast_expression'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "expected c-style cast");
    for m in &qr.results {
        assert_eq!(field(m, "cast_safety"), "unsafe");
    }
}

// Named C++ casts (reinterpret_cast, const_cast, etc.) are NOT indexed
// as separate node kinds in tree-sitter-cpp 0.23, so cast_safety tests
// for those are omitted (see §7 note above).

// --- ScopeEnricher: binding_kind, is_exported ---

#[test]
fn scope_binding_kind_variable() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // static const int decNum = 42 → declaration with scope=file, binding_kind=variable
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'decNum'");
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "decNum");
    assert_eq!(field(m, "binding_kind"), "variable");
}

#[test]
fn scope_is_exported_static_not_exported() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // static declarations should NOT be exported
    let r = exec(&mut e, &sid, "FIND symbols WHERE storage = 'static'");
    let qr = as_query(&r);
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

#[test]
fn member_kind_method() {
    let (mut e, sid, _d) = engine_enrichment_only();
    // declaredMethod is a field_declaration with function_declarator (method prototype)
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'declaredMethod'");
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "declaredMethod");
    assert_eq!(field(m, "member_kind"), "method");
}

#[test]
fn member_kind_field() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'publicField'");
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "publicField");
    assert_eq!(field(m, "member_kind"), "field");
}

#[test]
fn member_owner_kind_class() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'publicField'");
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "publicField");
    assert_eq!(field(m, "owner_kind"), "class_specifier");
}

#[test]
fn member_owner_kind_struct() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'fieldA'");
    let qr = as_query(&r);
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
    let qr = as_query(&r);
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
    let qr = as_query(&r);
    // Parameter 'a' should appear when filtering by fql_kind = 'variable'
    let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"a"),
        "parameter 'a' should match fql_kind = 'variable', got: {names:?}",
    );
}

// =======================================================================
// §11 — DeclDistanceEnricher
// =======================================================================

#[test]
fn decl_distance_no_locals() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'noLocals' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "noLocals");
    assert_eq!(m.fields.get("decl_distance").map(String::as_str), Some("0"));
    assert_eq!(
        m.fields.get("decl_far_count").map(String::as_str),
        Some("0")
    );
    assert_eq!(
        m.fields.get("has_unused_reassign").map(String::as_str),
        Some("false"),
    );
}

#[test]
fn decl_distance_all_nearby() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'allNearby' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "allNearby");
    assert_eq!(m.fields.get("decl_distance").map(String::as_str), Some("0"));
    assert_eq!(
        m.fields.get("decl_far_count").map(String::as_str),
        Some("0")
    );
}

#[test]
fn decl_distance_one_far_decl() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'oneFarDecl' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "oneFarDecl");
    let dist: usize = m
        .fields
        .get("decl_distance")
        .expect("decl_distance field")
        .parse()
        .expect("numeric");
    assert!(
        dist >= 2,
        "oneFarDecl should have decl_distance >= 2, got {dist}",
    );
    let count: usize = m
        .fields
        .get("decl_far_count")
        .expect("decl_far_count field")
        .parse()
        .expect("numeric");
    assert_eq!(count, 1, "only one far local");
}

#[test]
fn decl_distance_two_far_decls() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'twoFarDecls' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "twoFarDecls");
    let dist: usize = m
        .fields
        .get("decl_distance")
        .expect("decl_distance field")
        .parse()
        .expect("numeric");
    assert!(
        dist >= 4,
        "twoFarDecls should have decl_distance >= 4 (two far locals), got {dist}",
    );
    let count: usize = m
        .fields
        .get("decl_far_count")
        .expect("decl_far_count field")
        .parse()
        .expect("numeric");
    assert_eq!(count, 2, "two far locals");
}

#[test]
fn decl_distance_dead_store() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'deadStorePattern' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "deadStorePattern");
    assert_eq!(
        m.fields.get("has_unused_reassign").map(String::as_str),
        Some("true"),
        "deadStorePattern should have has_unused_reassign=true",
    );
}

#[test]
fn decl_distance_compound_assign_not_dead_store() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'compoundAssignNotDeadStore' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "compoundAssignNotDeadStore");
    assert_eq!(
        m.fields.get("has_unused_reassign").map(String::as_str),
        Some("false"),
        "compound assign (+=) should not flag as dead store",
    );
}

#[test]
fn decl_distance_param_excluded() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'paramExcluded' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "paramExcluded");
    // Only 'loc' should be counted (1 far local), not 'param'
    let count: usize = m
        .fields
        .get("decl_far_count")
        .expect("decl_far_count field")
        .parse()
        .expect("numeric");
    assert!(
        count <= 1,
        "paramExcluded should count at most 1 far local (loc), got {count}",
    );
}

#[test]
fn decl_distance_where_filter() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' WHERE decl_distance > 0",
    );
    let qr = as_query(&r);
    let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
    // Functions with far declarations should appear
    assert!(
        names.contains(&"oneFarDecl") || names.contains(&"twoFarDecls"),
        "WHERE decl_distance > 0 should find far-decl functions, got: {names:?}",
    );
    // Functions with no far declarations should NOT appear
    assert!(
        !names.contains(&"noLocals"),
        "noLocals should not appear in decl_distance > 0 results",
    );
}

// -----------------------------------------------------------------------
// §14 — EscapeEnricher
// -----------------------------------------------------------------------

#[test]
fn escape_direct_addr() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'escapeDirectAddr' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "escapeDirectAddr");
    assert_eq!(m.fields.get("has_escape").map(String::as_str), Some("true"),);
    assert_eq!(
        m.fields.get("escape_tier").map(String::as_str),
        Some("1"),
        "direct &local should be tier 1",
    );
    let vars = m
        .fields
        .get("escape_vars")
        .map(String::as_str)
        .unwrap_or("");
    assert!(
        vars.contains("x"),
        "escape_vars should contain 'x', got: {vars}"
    );
    assert_eq!(
        m.fields.get("escape_count").map(String::as_str),
        Some("1"),
        "one escaping local → escape_count = 1",
    );
    let kinds = m
        .fields
        .get("escape_kinds")
        .map(String::as_str)
        .unwrap_or("");
    assert!(
        kinds.contains("address_of"),
        "direct &local → escape_kinds should contain 'address_of', got: {kinds}"
    );
}

#[test]
fn escape_array_decay() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'escapeArrayDecay' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "escapeArrayDecay");
    assert_eq!(m.fields.get("has_escape").map(String::as_str), Some("true"),);
    let tier: u8 = m
        .fields
        .get("escape_tier")
        .expect("escape_tier")
        .parse()
        .expect("numeric");
    assert!(tier <= 2, "array decay should be tier ≤ 2, got {tier}");
    let vars = m
        .fields
        .get("escape_vars")
        .map(String::as_str)
        .unwrap_or("");
    assert!(
        vars.contains("arr"),
        "escape_vars should contain 'arr', got: {vars}"
    );
    let kinds = m
        .fields
        .get("escape_kinds")
        .map(String::as_str)
        .unwrap_or("");
    assert!(
        kinds.contains("array_decay"),
        "array decay → escape_kinds should contain 'array_decay', got: {kinds}"
    );
}

#[test]
fn escape_indirect_alias() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'escapeIndirectAlias' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "escapeIndirectAlias");
    assert_eq!(m.fields.get("has_escape").map(String::as_str), Some("true"),);
    let tier: u8 = m
        .fields
        .get("escape_tier")
        .expect("escape_tier")
        .parse()
        .expect("numeric");
    assert_eq!(tier, 3, "indirect alias should be tier 3");
    let vars = m
        .fields
        .get("escape_vars")
        .map(String::as_str)
        .unwrap_or("");
    assert!(
        vars.contains("val"),
        "escape_vars should contain 'val', got: {vars}"
    );
    let kinds = m
        .fields
        .get("escape_kinds")
        .map(String::as_str)
        .unwrap_or("");
    assert!(
        kinds.contains("alias"),
        "indirect alias → escape_kinds should contain 'alias', got: {kinds}"
    );
}

#[test]
fn escape_static_safe() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'escapeStaticSafe' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "escapeStaticSafe");
    assert!(
        m.fields.get("has_escape").is_none(),
        "static local should not trigger has_escape, fields: {:?}",
        m.fields,
    );
}

#[test]
fn escape_no_escape_param() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'escapeNoEscapeParam' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "escapeNoEscapeParam");
    assert!(
        m.fields.get("has_escape").is_none(),
        "returning parameter should not trigger escape",
    );
}

#[test]
fn escape_no_locals() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'escapeNoLocals' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "escapeNoLocals");
    assert!(
        m.fields.get("has_escape").is_none(),
        "function with no locals should not trigger escape",
    );
}

#[test]
fn escape_ternary() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'escapeTernary' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "escapeTernary");
    assert_eq!(
        m.fields.get("has_escape").map(String::as_str),
        Some("true"),
        "ternary with &local should trigger escape",
    );
    assert_eq!(
        m.fields.get("escape_tier").map(String::as_str),
        Some("1"),
        "ternary &local is still tier 1",
    );
}

#[test]
fn escape_where_filter() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' WHERE has_escape = 'true'",
    );
    let qr = as_query(&r);
    let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
    // Should contain escaping functions
    assert!(
        names.contains(&"escapeDirectAddr"),
        "escapeDirectAddr should appear in has_escape=true results, got: {names:?}",
    );
    // Should NOT contain safe functions
    assert!(
        !names.contains(&"escapeStaticSafe"),
        "escapeStaticSafe should not appear in has_escape=true results",
    );
    assert!(
        !names.contains(&"escapeNoLocals"),
        "escapeNoLocals should not appear in has_escape=true results",
    );
}

// §15 — ShadowEnricher
// -----------------------------------------------------------------------

#[test]
fn shadow_basic() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'shadowBasic' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "shadowBasic");
    assert_eq!(
        m.fields.get("has_shadow").map(String::as_str),
        Some("true"),
        "shadowBasic should have has_shadow=true",
    );
    assert_eq!(
        m.fields.get("shadow_count").map(String::as_str),
        Some("1"),
        "shadowBasic should shadow 1 variable",
    );
    let vars = m
        .fields
        .get("shadow_vars")
        .map(String::as_str)
        .unwrap_or("");
    assert!(
        vars.contains("x"),
        "shadow_vars should contain 'x', got: {vars}"
    );
}

#[test]
fn shadow_for_loop() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'shadowForLoop' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "shadowForLoop");
    assert_eq!(
        m.fields.get("has_shadow").map(String::as_str),
        Some("true"),
        "shadowForLoop should have has_shadow=true",
    );
    let vars = m
        .fields
        .get("shadow_vars")
        .map(String::as_str)
        .unwrap_or("");
    assert!(
        vars.contains("i"),
        "shadow_vars should contain 'i', got: {vars}"
    );
}

#[test]
fn shadow_multiple() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'shadowMultiple' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "shadowMultiple");
    assert_eq!(m.fields.get("has_shadow").map(String::as_str), Some("true"),);
    let count: usize = m
        .fields
        .get("shadow_count")
        .expect("shadow_count")
        .parse()
        .expect("numeric");
    assert_eq!(count, 2, "shadowMultiple should shadow 2 variables (a, b)");
    let vars = m
        .fields
        .get("shadow_vars")
        .map(String::as_str)
        .unwrap_or("");
    assert!(
        vars.contains("a"),
        "shadow_vars should contain 'a', got: {vars}"
    );
    assert!(
        vars.contains("b"),
        "shadow_vars should contain 'b', got: {vars}"
    );
}

#[test]
fn shadow_none() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'shadowNone' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "shadowNone");
    assert!(
        m.fields.get("has_shadow").is_none(),
        "shadowNone should not have has_shadow, fields: {:?}",
        m.fields,
    );
}

#[test]
fn shadow_nested() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'shadowNested' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "shadowNested");
    assert_eq!(m.fields.get("has_shadow").map(String::as_str), Some("true"),);
    let vars = m
        .fields
        .get("shadow_vars")
        .map(String::as_str)
        .unwrap_or("");
    assert!(
        vars.contains("val"),
        "shadow_vars should contain 'val', got: {vars}"
    );
}

#[test]
fn shadow_where_filter() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' WHERE has_shadow = 'true'",
    );
    let qr = as_query(&r);
    let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"shadowBasic"),
        "shadowBasic should appear in has_shadow=true results, got: {names:?}",
    );
    assert!(
        names.contains(&"shadowForLoop"),
        "shadowForLoop should appear in has_shadow=true results, got: {names:?}",
    );
    assert!(
        !names.contains(&"shadowNone"),
        "shadowNone should not appear in has_shadow=true results",
    );
}

// §16 — UnusedParamEnricher
// -----------------------------------------------------------------------

#[test]
fn unused_param_one() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'unusedParamOne' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "unusedParamOne");
    assert_eq!(
        m.fields.get("has_unused_param").map(String::as_str),
        Some("true"),
    );
    assert_eq!(
        m.fields.get("unused_param_count").map(String::as_str),
        Some("1"),
    );
    let params = m
        .fields
        .get("unused_params")
        .map(String::as_str)
        .unwrap_or("");
    assert!(
        params.contains("unused_p"),
        "unused_params should contain 'unused_p', got: {params}"
    );
    // Exactly one unused param, so the whole field should be just "unused_p"
    assert_eq!(params, "unused_p", "only unused_p should be listed");
}

#[test]
fn unused_param_none() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'unusedParamNone' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "unusedParamNone");
    assert!(
        m.fields.get("has_unused_param").is_none(),
        "unusedParamNone should not have has_unused_param, fields: {:?}",
        m.fields,
    );
}

#[test]
fn unused_param_all() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'unusedParamAll' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "unusedParamAll");
    assert_eq!(
        m.fields.get("has_unused_param").map(String::as_str),
        Some("true"),
    );
    let count: usize = m
        .fields
        .get("unused_param_count")
        .expect("unused_param_count")
        .parse()
        .expect("numeric");
    assert_eq!(count, 3, "all 3 parameters should be unused");
    let params = m
        .fields
        .get("unused_params")
        .map(String::as_str)
        .unwrap_or("");
    assert!(params.contains("x"), "should contain 'x', got: {params}");
    assert!(params.contains("y"), "should contain 'y', got: {params}");
    assert!(params.contains("z"), "should contain 'z', got: {params}");
}

#[test]
fn unused_param_empty() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'unusedParamEmpty' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "unusedParamEmpty");
    assert!(
        m.fields.get("has_unused_param").is_none(),
        "function with no params should not have has_unused_param",
    );
}

#[test]
fn unused_param_where_filter() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' WHERE has_unused_param = 'true'",
    );
    let qr = as_query(&r);
    let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"unusedParamOne"),
        "unusedParamOne should appear, got: {names:?}",
    );
    assert!(
        names.contains(&"unusedParamAll"),
        "unusedParamAll should appear, got: {names:?}",
    );
    assert!(
        !names.contains(&"unusedParamNone"),
        "unusedParamNone should not appear",
    );
}

// §17 — FallthroughEnricher
// -----------------------------------------------------------------------

#[test]
fn fallthrough_one() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'fallthroughOne' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "fallthroughOne");
    assert_eq!(
        m.fields.get("has_fallthrough").map(String::as_str),
        Some("true"),
    );
    assert_eq!(
        m.fields.get("fallthrough_count").map(String::as_str),
        Some("1"),
        "only case 1 falls through",
    );
}

#[test]
fn fallthrough_none() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'fallthroughNone' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "fallthroughNone");
    assert!(
        m.fields.get("has_fallthrough").is_none(),
        "fallthroughNone should not have fallthrough, fields: {:?}",
        m.fields,
    );
}

#[test]
fn fallthrough_grouped() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'fallthroughGrouped' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "fallthroughGrouped");
    assert_eq!(
        m.fields.get("has_fallthrough").map(String::as_str),
        Some("true"),
    );
    // case 1 is empty (intentional), case 2 falls through to case 3
    assert_eq!(
        m.fields.get("fallthrough_count").map(String::as_str),
        Some("1"),
        "only case 2 should count as fallthrough (case 1 is empty grouping)",
    );
}

#[test]
fn fallthrough_no_switch() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'fallthroughNoSwitch' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "fallthroughNoSwitch");
    assert!(
        m.fields.get("has_fallthrough").is_none(),
        "function with no switch should not have fallthrough",
    );
}

#[test]
fn fallthrough_where_filter() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' WHERE has_fallthrough = 'true'",
    );
    let qr = as_query(&r);
    let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"fallthroughOne"),
        "fallthroughOne should appear, got: {names:?}",
    );
    assert!(
        !names.contains(&"fallthroughNone"),
        "fallthroughNone should not appear",
    );
}

// ── §18 — RecursionEnricher ──────────────────────────────────────────

#[test]
fn recursion_factorial() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'recursiveFactorial' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "recursiveFactorial");
    assert_eq!(
        m.fields.get("is_recursive").map(String::as_str),
        Some("true"),
    );
    assert_eq!(
        m.fields.get("recursion_count").map(String::as_str),
        Some("1"),
        "factorial has a single self-call site",
    );
}

#[test]
fn recursion_fib() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'recursiveFib' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "recursiveFib");
    assert_eq!(
        m.fields.get("is_recursive").map(String::as_str),
        Some("true"),
    );
    assert_eq!(
        m.fields.get("recursion_count").map(String::as_str),
        Some("2"),
        "fib has two self-call sites",
    );
}

#[test]
fn recursion_not_recursive() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'notRecursive' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "notRecursive");
    assert!(
        !m.fields.contains_key("is_recursive"),
        "non-recursive function should not have is_recursive field",
    );
}

#[test]
fn recursion_calls_other() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'callsOther' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "callsOther");
    assert!(
        !m.fields.contains_key("is_recursive"),
        "function that only calls others should not be recursive",
    );
}

#[test]
fn recursion_where_filter() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' WHERE is_recursive = 'true'",
    );
    let qr = as_query(&r);
    let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"recursiveFactorial"),
        "recursiveFactorial should appear, got: {names:?}",
    );
    assert!(
        names.contains(&"recursiveFib"),
        "recursiveFib should appear, got: {names:?}",
    );
    assert!(
        !names.contains(&"notRecursive"),
        "notRecursive should not appear",
    );
    assert!(
        !names.contains(&"callsOther"),
        "callsOther should not appear",
    );
}

// ── §19 — TodoEnricher ──────────────────────────────────────────────

#[test]
fn todo_single() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'todoSingle' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "todoSingle");
    assert_eq!(m.fields.get("has_todo").map(String::as_str), Some("true"),);
    assert_eq!(m.fields.get("todo_count").map(String::as_str), Some("1"),);
    assert_eq!(m.fields.get("todo_tags").map(String::as_str), Some("TODO"),);
}

#[test]
fn todo_multiple_markers() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'todoMultiple' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "todoMultiple");
    assert_eq!(m.fields.get("has_todo").map(String::as_str), Some("true"),);
    assert_eq!(
        m.fields.get("todo_count").map(String::as_str),
        Some("3"),
        "TODO + FIXME + HACK = 3 markers",
    );
    // BTreeSet → sorted: FIXME, HACK, TODO
    assert_eq!(
        m.fields.get("todo_tags").map(String::as_str),
        Some("FIXME,HACK,TODO"),
    );
}

#[test]
fn todo_none() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'todoNone' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "todoNone");
    assert!(
        !m.fields.contains_key("has_todo"),
        "function with no markers should not have has_todo field",
    );
}

#[test]
fn todo_repeated_same_marker() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'todoRepeated' WHERE fql_kind = 'function'",
    );
    let qr = as_query(&r);
    let m = find_by_name(&qr.results, "todoRepeated");
    assert_eq!(m.fields.get("has_todo").map(String::as_str), Some("true"),);
    assert_eq!(
        m.fields.get("todo_count").map(String::as_str),
        Some("3"),
        "2x TODO + 1x XXX = 3",
    );
    assert_eq!(
        m.fields.get("todo_tags").map(String::as_str),
        Some("TODO,XXX"),
    );
}

#[test]
fn todo_where_filter() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' WHERE has_todo = 'true'",
    );
    let qr = as_query(&r);
    let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"todoSingle"),
        "todoSingle should appear, got: {names:?}",
    );
    assert!(
        names.contains(&"todoMultiple"),
        "todoMultiple should appear, got: {names:?}",
    );
    assert!(!names.contains(&"todoNone"), "todoNone should not appear",);
}
