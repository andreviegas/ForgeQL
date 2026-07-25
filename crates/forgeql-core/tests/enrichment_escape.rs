//! EscapeEnricher integration tests (has_escape, escape_count, escape_vars,
//! escape_tier, escape_kinds).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::map_unwrap_or,
    clippy::single_char_pattern,
    clippy::unnecessary_get_then_check,
    clippy::uninlined_format_args,
    unused_results
)]

mod common;
mod enrichment_harness;
use enrichment_harness::*;
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
