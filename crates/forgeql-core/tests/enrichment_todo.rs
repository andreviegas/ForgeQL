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

// ── the leading position ────────────────────────────────────────────────────

/// Assert every function named in `present` carries exactly one TODO, and that
/// `absent` carries none at all.
///
/// One helper over four fixtures on purpose. The claim the fix makes is that
/// the leading position behaves like any other position in every grammar, so
/// the assertion has to be the same in every grammar too — a Python-only test
/// would have read as a fix for all four while proving one.
///
/// `todo_count` is pinned to exactly `1`, not merely to presence: the fix scans
/// the function node beside its body, and a grammar that put the leading
/// comment inside the body all along would double-count it here.
fn assert_one_marker_each(fixture: &str, present: &[&str], absent: &str) {
    let (mut e, sid, _d) = common::legacy_session(&[fixture]).into_parts();
    for name in present {
        let q = format!("FIND symbols WHERE name = '{name}' WHERE fql_kind = 'function' LIMIT 2");
        let r = exec(&mut e, &sid, &q);
        let qr = common::as_query(&r);
        let m = find_by_name(&qr.results, name);
        assert_eq!(field(m, "has_todo"), "true", "{fixture}: {name}");
        assert_eq!(
            field(m, "todo_count"),
            "1",
            "{fixture}: {name} counted its one marker twice, or not at all",
        );
        assert_eq!(field(m, "todo_tags"), "TODO", "{fixture}: {name}");
    }
    let q = format!("FIND symbols WHERE name = '{absent}' WHERE fql_kind = 'function' LIMIT 2");
    let r = exec(&mut e, &sid, &q);
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, absent);
    assert_eq!(
        field_opt(m, "has_todo"),
        None,
        "{fixture}: {absent} carries no marker and must answer none",
    );
}

/// Python is the grammar the defect was found in: the body block does not open
/// until the first statement, so a comment written as the first body line is a
/// sibling of the block and a body-only walk never reaches it.
/// `docstring_then_marker` is the neighbouring position that always worked —
/// one statement is enough to open the block — and holding both in one test is
/// what says the variable is position and not the file.
#[test]
fn a_marker_opening_a_python_body_is_found() {
    assert_one_marker_each(
        "todo_leading.py",
        &[
            "leading_marker",
            "docstring_then_marker",
            "marker_after_statement",
            "decorated_leading_marker",
            "trailing_marker",
        ],
        "no_marker",
    );
}

/// The third exclusion, and the one that is least visible from a result row: a
/// marker between a decorator and the definition it decorates is not scanned.
///
/// The wrapper is the parse node here, and the row is the definition inside it
/// (the row's rendered span folds back to the decorator, so this comment is
/// INSIDE the span an agent reads). The scan walks the definition's own
/// children, and this comment is a child of the wrapper — a sibling of the
/// definition, not a child of it. Reading it as part of the function would mean
/// deciding that a comment above `def` and below `@deco` belongs to the body
/// rather than to the decoration, which is a judgement, not a tree fact.
///
/// `trailing_marker` in the same fixture is the case this was mistaken for and
/// is NOT an exclusion: a comment on the last line of the body is inside the
/// block and was found before this change as well.
#[test]
fn a_marker_between_a_decorator_and_the_definition_is_not_scanned() {
    let (mut e, sid, _d) = common::legacy_session(&["todo_leading.py"]).into_parts();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'marker_between_decorator_and_def' \
         WHERE fql_kind = 'function' LIMIT 2",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "marker_between_decorator_and_def");
    assert_eq!(
        field_opt(m, "has_todo"),
        None,
        "a comment between a decorator and its definition is a child of the \
         wrapper, not of the function, so it is outside the scanned region. If \
         this now answers, the exclusion is stale everywhere it is written: grep \
         doc/ and crates/forgeql-core/src/ast/enrich/todo.rs for `decorator` and \
         move every site",
    );
}

/// C: a brace-delimited body already held its leading comment, so these pin
/// that the widened scan did not disturb it. `marker_before_the_body` is the
/// position the widening adds — a comment between the signature and the body
/// is inside the function and outside its body in every brace grammar.
#[test]
fn a_marker_opening_a_c_body_is_found() {
    assert_one_marker_each(
        "todo_leading.c",
        &[
            "leading_marker",
            "marker_after_statement",
            "marker_before_the_body",
        ],
        "no_marker",
    );
}

/// C++ is a separate plugin from C with a separate grammar, and the two have
/// already been shown to disagree about where a child sits (the zero-subscript
/// exemption fires in C and is dead in C++), so it is checked on its own.
#[test]
fn a_marker_opening_a_cpp_body_is_found() {
    assert_one_marker_each(
        "todo_leading.cpp",
        &[
            "leadingMarker",
            "markerAfterStatement",
            "markerBeforeTheBody",
        ],
        "noMarker",
    );
}

/// Rust, the fourth registered grammar.
#[test]
fn a_marker_opening_a_rust_body_is_found() {
    assert_one_marker_each(
        "todo_leading.rs",
        &[
            "leading_marker",
            "marker_after_statement",
            "marker_before_the_body",
        ],
        "no_marker",
    );
}

/// The boundary every doc site states in the same clause as the claim: a
/// marker in a Rust `/* */` is not found, in any position.
///
/// Rust is the one registered grammar that splits its comment styles into two
/// raw kinds (`line_comment`, `block_comment`) while the config declares one of
/// them, and `is_comment_kind` is an equality — so `block_comment` is a comment
/// ROW that no comment-scanning enricher visits. That is a gap this change did
/// not close, and a sentence repeated across the documentation is not something
/// a later change can be trusted to notice. When the gap closes, this test goes
/// red, and the assertion message says how to find every sentence that then has
/// to move with it.
#[test]
fn a_rust_block_comment_marker_is_not_scanned() {
    let (mut e, sid, _d) = common::legacy_session(&["todo_leading.rs"]).into_parts();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE name = 'block_comment_marker' WHERE fql_kind = 'function' LIMIT 2",
    );
    let qr = common::as_query(&r);
    let m = find_by_name(&qr.results, "block_comment_marker");
    assert_eq!(
        field_opt(m, "has_todo"),
        None,
        "a Rust block comment is outside the declared comment kind, so its marker \
         is not scanned. If this now answers, the exclusion is stale everywhere it \
         is written: grep doc/ and crates/forgeql-core/src/ast/enrich/todo.rs for \
         the phrase `/* */` and move every site, not only the ones you remember",
    );
}
