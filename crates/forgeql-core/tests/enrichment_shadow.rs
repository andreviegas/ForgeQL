//! ShadowEnricher integration tests (has_shadow, shadow_count, shadow_vars).
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
