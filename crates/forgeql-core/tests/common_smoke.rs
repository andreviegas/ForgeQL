//! Smoke tests for the shared `common` harness itself.
//!
//! These exercise the harness surfaces that no migrated suite drives yet —
//! `columnar_session` (the production read path) and `file_handle` (a file's
//! `(node_id, rev)` handle) — so those helpers can never rot into dead code
//! behind a green gate.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forgeql_core::result::ForgeQLResult;

mod common;

/// `columnar_session` indexes fixtures and serves a query off the columnar path.
#[test]
fn columnar_session_indexes_and_queries() {
    let mut t = common::columnar_session(&["motor_control.cpp"]);
    match t.exec("FIND symbols WHERE fql_kind = 'function'") {
        ForgeQLResult::Query(qr) => {
            assert!(!qr.results.is_empty(), "expected at least one function");
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

/// `file_handle` returns a usable `(handle, rev)` for a workspace file.
#[test]
fn file_handle_resolves_on_columnar() {
    let mut t = common::columnar_session(&["motor_control.cpp"]);
    let (handle, rev) = t.file_handle("motor_control.cpp");
    assert!(handle.starts_with('n'), "unexpected handle: {handle}");
    assert!(!rev.is_empty(), "rev must not be empty");
}

/// `err` returns the refusal message for a statement the engine rejects.
#[test]
fn err_reports_a_refusal_message() {
    let mut t = common::columnar_session(&["motor_control.cpp"]);
    let msg = t.err("SHOW body OF 'this_symbol_does_not_exist_xyz'");
    assert!(!msg.is_empty(), "expected a non-empty refusal message");
}

/// `legacy_session` builds on the in-memory backend; `exec` and the `Ok` arm of
/// `try_fql` both round-trip a query.
#[test]
fn legacy_session_exec_and_try_fql_ok() {
    let mut t = common::legacy_session(&["motor_control.cpp"]);
    match t.exec("FIND symbols WHERE fql_kind = 'function'") {
        ForgeQLResult::Query(qr) => assert!(!qr.results.is_empty()),
        other => panic!("expected Query, got {other:?}"),
    }
    let ok = t.try_fql("FIND files");
    assert!(ok.is_ok(), "FIND files must succeed");
}

/// `columnar_session_in` registers a session over a workspace the test populated
/// itself (rather than from named fixtures).
#[test]
fn columnar_session_in_over_a_bespoke_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("solo.cpp"),
        "int add(int a, int b) { return a + b; }\n",
    )
    .expect("write cpp");

    let mut t = common::columnar_session_in(dir);
    // The workspace accessor points at the populated root.
    assert!(t.workspace().join("solo.cpp").exists());
    match t.exec("FIND symbols WHERE name = 'add'") {
        ForgeQLResult::Query(qr) => assert!(!qr.results.is_empty(), "expected 'add'"),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// `exec_blocking` waits out any spawned job; `try_fql_blocking` surfaces the
/// error instead of panicking. A plain query exercises both cleanly.
#[test]
fn blocking_variants_round_trip() {
    let mut t = common::columnar_session(&["motor_control.cpp"]);
    assert!(matches!(
        t.exec_blocking("FIND symbols"),
        ForgeQLResult::Query(_)
    ));
    let bad = t.try_fql_blocking("SHOW body OF 'this_symbol_does_not_exist_xyz'");
    assert!(bad.is_err(), "a nonexistent symbol must be refused");
}

/// `path_handle` is the deterministic `n<12 hex>` handle for a workspace path.
#[test]
fn path_handle_is_the_stable_file_handle() {
    let h = common::path_handle("motor_control.cpp");
    assert!(h.starts_with('n'), "handle: {h}");
    assert_eq!(h.len(), 13, "n + 12 hex chars: {h}");
}
