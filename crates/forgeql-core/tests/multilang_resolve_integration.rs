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

use forgeql_core::result::ForgeQLResult;
use tempfile::tempdir;

mod common;

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// The `canonical.cpp` + `canonical.rs` fixtures — both define `foo`, `bar`, and
/// `Motor`, the cross-language ambiguity every test below turns on.
const CANONICAL: &[&str] = &["canonical/canonical.cpp", "canonical/canonical.rs"];

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
    let mut t = common::legacy_session(CANONICAL);
    // 'foo' exists in both canonical.cpp and canonical.rs
    let err = t.err("SHOW body OF 'foo'");
    assert!(
        err.contains("multiple languages"),
        "expected ambiguity error, got: {err}"
    );
    assert!(err.contains("cpp"), "error should mention cpp: {err}");
    assert!(err.contains("rust"), "error should mention rust: {err}");
}

#[test]
fn show_signature_ambiguous_symbol_returns_error() {
    let mut t = common::legacy_session(CANONICAL);
    let err = t.err("SHOW signature OF 'bar'");
    assert!(
        err.contains("multiple languages"),
        "expected ambiguity error, got: {err}"
    );
}

#[test]
fn show_context_ambiguous_symbol_returns_error() {
    let mut t = common::legacy_session(CANONICAL);
    let err = t.err("SHOW context OF 'foo'");
    assert!(
        err.contains("multiple languages"),
        "expected ambiguity error, got: {err}"
    );
}

#[test]
fn show_members_ambiguous_symbol_returns_error() {
    let mut t = common::legacy_session(CANONICAL);
    // 'Motor' struct exists in both languages
    let err = t.err("SHOW members OF 'Motor'");
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
    let mut t = common::legacy_session(CANONICAL);
    let result = t.exec("SHOW body OF 'foo' WHERE language = 'cpp'");
    assert_show_ok(&result, "show_body");
}

#[test]
fn show_body_disambiguate_with_where_language_rust() {
    let mut t = common::legacy_session(CANONICAL);
    let result = t.exec("SHOW body OF 'foo' WHERE language = 'rust'");
    assert_show_ok(&result, "show_body");
}

// -----------------------------------------------------------------------
// Disambiguation via IN glob
// -----------------------------------------------------------------------

#[test]
fn show_signature_disambiguate_with_in_glob() {
    let mut t = common::legacy_session(CANONICAL);
    let result = t.exec("SHOW signature OF 'bar' IN '*.rs'");
    assert_show_ok(&result, "show_signature");
}

#[test]
fn show_context_disambiguate_with_in_glob_cpp() {
    let mut t = common::legacy_session(CANONICAL);
    let result = t.exec("SHOW context OF 'foo' IN '*.cpp'");
    assert_show_ok(&result, "show_context");
}

#[test]
fn show_body_single_language_no_ambiguity() {
    // Only C++ present — no disambiguation needed.
    let mut t = common::legacy_session(&["canonical/canonical.cpp"]);
    let result = t.exec("SHOW body OF 'foo'");
    assert_show_ok(&result, "show_body");
}

// -----------------------------------------------------------------------
// Attribute-folded span resolution
// -----------------------------------------------------------------------

/// Regression: `SHOW body OF 'fn'` must resolve a function whose indexed span
/// was folded back over a leading attribute (`#[...]`). Before the four-strategy
/// `find_function_node_for_symbol`, the resolver searched at the folded
/// (attribute) start byte and returned "function definition not found in AST"
/// for every attributed function (`#[test]`, `#[inline]`, `#[must_use]`, ...).
#[test]
fn show_body_resolves_attributed_function() {
    let dir = tempdir().expect("tempdir");
    fs::write(
        dir.path().join("attr.rs"),
        "#[inline]\nfn decorated(x: i32) -> i32 {\n    x + 1\n}\n",
    )
    .expect("write rs");

    let mut t = common::legacy_session_in(dir);
    let result = t.exec("SHOW body OF 'decorated'");
    match &result {
        ForgeQLResult::Show(sr) => match &sr.content {
            forgeql_core::result::ShowContent::Lines { lines, .. } => {
                let text: String = lines
                    .iter()
                    .map(|l| l.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    text.contains("fn decorated"),
                    "attributed function body must resolve, got: {text}"
                );
            }
            other => panic!("expected Lines content, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    }
}
