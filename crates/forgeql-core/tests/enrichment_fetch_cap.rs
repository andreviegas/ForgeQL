//! Regression test for the enrichment fetch-cap early-exit bug.
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
