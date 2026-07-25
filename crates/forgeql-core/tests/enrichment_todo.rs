//! TodoEnricher integration tests (has_todo, todo_count, todo_tags).
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
// ── §19 — TodoEnricher ──────────────────────────────────────────────

#[test]
fn todo_single() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'todoSingle' WHERE fql_kind = 'function'",
    );
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
