//! DeclDistanceEnricher integration tests (decl_distance, decl_far_count,
//! has_unused_reassign).
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
    let qr = common::as_query(&r);
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
