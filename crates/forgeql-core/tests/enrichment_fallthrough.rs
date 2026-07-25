//! FallthroughEnricher integration tests (has_fallthrough, fallthrough_count).
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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

#[test]
fn fallthrough_annotated_zephyr_style() {
    // Bug 5: __fallthrough; must suppress has_fallthrough.
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'fallthroughAnnotated' WHERE fql_kind = 'function'",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "fallthroughAnnotated");
    assert!(
        m.fields.get("has_fallthrough").is_none(),
        "__fallthrough; annotation must suppress has_fallthrough, fields: {:?}",
        m.fields,
    );
}

#[test]
fn fallthrough_annotated_cpp17_attr() {
    // Bug 5: [[fallthrough]] must suppress has_fallthrough.
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'fallthroughAttr' WHERE fql_kind = 'function'",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "fallthroughAttr");
    assert!(
        m.fields.get("has_fallthrough").is_none(),
        "[[fallthrough]] annotation must suppress has_fallthrough, fields: {:?}",
        m.fields,
    );
}
