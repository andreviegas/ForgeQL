//! RecursionEnricher integration tests (is_recursive, recursion_count).
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
// ── §18 — RecursionEnricher ──────────────────────────────────────────

#[test]
fn recursion_factorial() {
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'recursiveFactorial' WHERE fql_kind = 'function'",
    );
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "notRecursive");
    assert!(
        !m.fields.contains_key("is_recursive"),
        "non-recursive function should not have is_recursive field",
    );
}

#[test]
fn recursion_called_by_many() {
    // Bug 4 regression: a function that is called by several other functions
    // in the same file must NOT be flagged as is_recursive.
    // The name `calledByMany` appears 4 times in the fixture file (3 callers
    // + the definition), mirroring the real-world spi_max32_transceive case.
    let (mut e, sid, _d) = engine_enrichment_only();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'calledByMany' WHERE fql_kind = 'function'",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "calledByMany");
    assert!(
        !m.fields.contains_key("is_recursive"),
        "function called by others should not have is_recursive; fields: {:?}",
        m.fields,
    );
    assert!(
        !m.fields.contains_key("recursion_count"),
        "function called by others should not have recursion_count; fields: {:?}",
        m.fields,
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
