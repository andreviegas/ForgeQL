//! UnusedParamEnricher integration tests (has_unused_param, unused_param_count,
//! unused_params).
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
