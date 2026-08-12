//! Regression test for the enrichment fetch-cap early-exit bug.
//!
//! The cap this is named after no longer exists — a `LIMIT` now bounds only
//! what is delivered, and the running top-K trim bounds the working set — so
//! the case can no longer fail the way it once did. It is kept because what it
//! asserts is the invariant, not the mechanism: a `LIMIT 1` whose only
//! matching row lives in the last segment read must still return that row.
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

use std::fs;

use forgeql_core::engine::ForgeQLEngine;
use tempfile::tempdir;

mod common;
mod enrichment_harness;
use enrichment_harness::*;
// ── Regression: fetch-cap early-exit bug ─────────────────────────────────────
//
// Before the fix, `FIND … WHERE <enrichment-only> LIMIT N` returned 0 results
// when the alphabetically-first segment had no posting blob for the queried
// field.  All rows from that segment filled the fetch cap; `apply_clauses`
// then filtered them all away.
//
// The regression test places:
//   • "a_noop.cpp"  — alphabetically FIRST, many non-recursive functions
//                     → no posting blob for `is_recursive`
//   • "z_recursive.cpp" — alphabetically LAST, one recursive function
//                         → has posting blob for `is_recursive = 'true'`
//
// `FIND symbols WHERE is_recursive = 'true' LIMIT 1` must still return 1
// result despite the earlier segment filling a naïve fetch cap.

fn engine_fetchcap_regression() -> (ForgeQLEngine, String, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");

    // Alphabetically first: many non-recursive functions, no is_recursive posting.
    fs::write(
        dir.path().join("a_noop.cpp"),
        r"
// Many non-recursive functions — fills the raw fetch-cap if not pre-filtered.
int noop1(int x) { return x + 1; }
int noop2(int x) { return x + 2; }
int noop3(int x) { return x + 3; }
int noop4(int x) { return x + 4; }
int noop5(int x) { return x + 5; }
int noop6(int x) { return x + 6; }
int noop7(int x) { return x + 7; }
int noop8(int x) { return x + 8; }
int noop9(int x) { return x + 9; }
int noop10(int x) { return x + 10; }
",
    )
    .expect("write a_noop.cpp");

    // Alphabetically last: one self-recursive function.
    fs::write(
        dir.path().join("z_recursive.cpp"),
        r"
int fact(int n) {
    if (n <= 1) return 1;
    return n * fact(n - 1);
}
",
    )
    .expect("write z_recursive.cpp");

    let data_dir = dir.path().join("data");
    let registry = common::make_registry();
    let mut engine = ForgeQLEngine::new(data_dir, registry).expect("engine");
    let session_id = engine
        .register_local_session(dir.path())
        .expect("register session");

    (engine, session_id, dir)
}

#[test]
fn fetch_cap_limit_enrichment_only_no_false_zero() {
    // Verify that LIMIT N does not return 0 when matching rows exist in a
    // segment that is alphabetically later than a segment without a posting
    // blob for the queried enrichment field.
    let (mut e, sid, _d) = engine_fetchcap_regression();

    // No LIMIT — must find `fact` as recursive.
    let r_all = exec(&mut e, &sid, "FIND symbols WHERE is_recursive = 'true'");
    let qr_all = common::as_query(&r_all);
    let names_all: Vec<&str> = qr_all.results.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names_all.contains(&"fact"),
        "expected 'fact' in unrestricted results, got: {names_all:?}",
    );

    // LIMIT 1 — must still return 1 result, not 0.
    let r_lim = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE is_recursive = 'true' LIMIT 1",
    );
    let qr_lim = common::as_query(&r_lim);
    assert_eq!(
        qr_lim.results.len(),
        1,
        "LIMIT 1 with enrichment-only predicate must return 1 result; \
         got 0 before the fetch-cap per-segment pre-filter fix",
    );
    assert_eq!(
        qr_lim.results[0].name, "fact",
        "the returned result must be 'fact'",
    );
}

/// The same query with `HAVING` in place of `WHERE` — the one keyword that moves
/// the predicate to after the page has been cut.
///
/// A backend that stops reading once it holds `LIMIT` rows answers such a query
/// with the rows it fetched minus those failing the predicate: nothing at all
/// here, since the alphabetically first function is not the recursive one. This
/// exercises that end to end through the engine rather than against a storage
/// struct directly.
///
/// It does **not** cover the in-memory backend's own gate, however much its
/// shape suggests otherwise. Removing the `HAVING` condition from
/// `find_symbols_prefilter::can_trim` alone leaves this test green, so whatever
/// it reaches, it is not that line. No golden case can reach that backend
/// either, because every golden corpus carries a `.forgeql.yaml` and is served
/// by the columnar backend, whose install drops the in-memory table. What does
/// reach it is `a_small_limit_returns_the_head_of_the_larger_page` in
/// `tests/enrichment_integration.rs`, which runs over a source with no
/// `.forgeql.yaml` and asserts the page property directly.
#[test]
fn having_after_a_limit_still_selects_the_rows_that_qualify() {
    let (mut e, sid, _d) = engine_fetchcap_regression();

    // Control: the identical predicate as WHERE, which is filtered while rows
    // are scanned rather than after the page is chosen.
    let r_where = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE is_recursive = 'true' LIMIT 1",
    );
    let qr_where = common::as_query(&r_where);
    assert_eq!(
        qr_where.results.len(),
        1,
        "control: as WHERE, a page of one holds the qualifying row"
    );

    let r_having = exec(
        &mut e,
        &sid,
        "FIND symbols HAVING is_recursive = 'true' LIMIT 1",
    );
    let qr_having = common::as_query(&r_having);
    let names: Vec<&str> = qr_having.results.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        qr_having.results.len(),
        1,
        "a page of one must hold a row that satisfies HAVING, got {names:?}"
    );
    assert_eq!(names, vec!["fact"], "and it must be the recursive one");
}
