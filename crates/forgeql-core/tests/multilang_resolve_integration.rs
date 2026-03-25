//! Multi-language symbol resolution tests.
//!
//! Verifies that SHOW commands correctly detect cross-language ambiguity
//! and allow disambiguation via `WHERE language = '...'` or `IN '*.ext'`.
//!
//! Run with: `cargo test -p forgeql-core --test multilang_resolve_integration`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements
)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use forgeql_core::ast::lang::{CppLanguageInline, LanguageRegistry, RustLanguageInline};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::parser;
use forgeql_core::result::ForgeQLResult;
use tempfile::tempdir;

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/fixtures/canonical")
}

fn make_registry() -> Arc<LanguageRegistry> {
    Arc::new(LanguageRegistry::new(vec![
        Arc::new(CppLanguageInline),
        Arc::new(RustLanguageInline),
    ]))
}

/// Create a temp workspace with both canonical.cpp and canonical.rs,
/// boot an engine with a session pointing at that workspace.
fn multilang_engine() -> (ForgeQLEngine, String, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let src = fixtures_dir();

    let _ =
        fs::copy(src.join("canonical.cpp"), dir.path().join("canonical.cpp")).expect("copy .cpp");
    let _ = fs::copy(src.join("canonical.rs"), dir.path().join("canonical.rs")).expect("copy .rs");

    let data_dir = dir.path().join("data");
    let mut engine = ForgeQLEngine::new(data_dir, make_registry()).expect("engine");
    let session_id = engine
        .register_local_session(dir.path())
        .expect("register session");

    (engine, session_id, dir)
}

/// Parse and execute FQL, return the result.
fn exec(engine: &mut ForgeQLEngine, session: &str, fql: &str) -> ForgeQLResult {
    let ops = parser::parse(fql).expect("parse");
    let op = ops.first().expect("at least one op");
    engine.execute(Some(session), op).expect("execute")
}

/// Parse and execute FQL, expecting an error. Returns the error message.
fn exec_err(engine: &mut ForgeQLEngine, session: &str, fql: &str) -> String {
    let ops = parser::parse(fql).expect("parse");
    let op = ops.first().expect("at least one op");
    engine
        .execute(Some(session), op)
        .expect_err("expected error")
        .to_string()
}

/// Assert the result is a successful SHOW with the expected op name.
fn assert_show_ok(result: &ForgeQLResult, expected_op: &str) {
    match result {
        ForgeQLResult::Show(sr) => {
            assert_eq!(sr.op, expected_op, "unexpected op");
        }
        other => panic!("expected ForgeQLResult::Show, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Ambiguity detection
// -----------------------------------------------------------------------

#[test]
fn show_body_ambiguous_symbol_returns_error() {
    let (mut engine, sid, _dir) = multilang_engine();
    // 'foo' exists in both canonical.cpp and canonical.rs
    let err = exec_err(&mut engine, &sid, "SHOW body OF 'foo'");
    assert!(
        err.contains("multiple languages"),
        "expected ambiguity error, got: {err}"
    );
    assert!(err.contains("cpp"), "error should mention cpp: {err}");
    assert!(err.contains("rust"), "error should mention rust: {err}");
}

#[test]
fn show_signature_ambiguous_symbol_returns_error() {
    let (mut engine, sid, _dir) = multilang_engine();
    let err = exec_err(&mut engine, &sid, "SHOW signature OF 'bar'");
    assert!(
        err.contains("multiple languages"),
        "expected ambiguity error, got: {err}"
    );
}

#[test]
fn show_context_ambiguous_symbol_returns_error() {
    let (mut engine, sid, _dir) = multilang_engine();
    let err = exec_err(&mut engine, &sid, "SHOW context OF 'foo'");
    assert!(
        err.contains("multiple languages"),
        "expected ambiguity error, got: {err}"
    );
}

#[test]
fn show_members_ambiguous_symbol_returns_error() {
    let (mut engine, sid, _dir) = multilang_engine();
    // 'Motor' struct exists in both languages
    let err = exec_err(&mut engine, &sid, "SHOW members OF 'Motor'");
    assert!(
        err.contains("multiple languages"),
        "expected ambiguity error, got: {err}"
    );
}

// -----------------------------------------------------------------------
// Disambiguation via WHERE language
// -----------------------------------------------------------------------

#[test]
fn show_body_disambiguate_with_where_language() {
    let (mut engine, sid, _dir) = multilang_engine();
    let result = exec(
        &mut engine,
        &sid,
        "SHOW body OF 'foo' WHERE language = 'cpp'",
    );
    assert_show_ok(&result, "show_body");
}

#[test]
fn show_body_disambiguate_with_where_language_rust() {
    let (mut engine, sid, _dir) = multilang_engine();
    let result = exec(
        &mut engine,
        &sid,
        "SHOW body OF 'foo' WHERE language = 'rust'",
    );
    assert_show_ok(&result, "show_body");
}

// -----------------------------------------------------------------------
// Disambiguation via IN glob
// -----------------------------------------------------------------------

#[test]
fn show_signature_disambiguate_with_in_glob() {
    let (mut engine, sid, _dir) = multilang_engine();
    let result = exec(&mut engine, &sid, "SHOW signature OF 'bar' IN '*.rs'");
    assert_show_ok(&result, "show_signature");
}

#[test]
fn show_context_disambiguate_with_in_glob_cpp() {
    let (mut engine, sid, _dir) = multilang_engine();
    let result = exec(&mut engine, &sid, "SHOW context OF 'foo' IN '*.cpp'");
    assert_show_ok(&result, "show_context");
}

// -----------------------------------------------------------------------
// Single-language workspace — no ambiguity
// -----------------------------------------------------------------------

#[test]
fn show_body_single_language_no_ambiguity() {
    // Create a workspace with only C++ — no disambiguation needed.
    let dir = tempdir().expect("tempdir");
    let src = fixtures_dir();
    let _ =
        fs::copy(src.join("canonical.cpp"), dir.path().join("canonical.cpp")).expect("copy .cpp");

    let data_dir = dir.path().join("data");
    let mut engine = ForgeQLEngine::new(data_dir, make_registry()).expect("engine");
    let sid = engine
        .register_local_session(dir.path())
        .expect("register session");

    let result = exec(&mut engine, &sid, "SHOW body OF 'foo'");
    assert_show_ok(&result, "show_body");
}
