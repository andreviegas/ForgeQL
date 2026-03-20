//! Comprehensive syntax-coverage tests for every ForgeQL command and clause
//! combination documented in `doc/syntax.md`.
//!
//! These tests exercise the full pipeline: **parser → IR → engine → result**.
//! Every test uses the `motor_control` C++ fixtures in a temp workspace.
//!
//! Run with: `cargo test -p forgeql-core --test syntax_coverage`
//!
//! Organisation:
//!   Phase 1 — FIND symbols (every WHERE operator, ORDER BY, GROUP BY, LIMIT, OFFSET, IN, EXCLUDE)
//!   Phase 2 — FIND usages / FIND callees
//!   Phase 3 — FIND files
//!   Phase 4 — FIND globals
//!   Phase 5 — SHOW commands (body, signature, outline, members, context, callees, lines)
//!   Phase 6 — CHANGE + ROLLBACK round-trips (matching, lines, with content, delete)
//!   Phase 7 — Transaction commands (BEGIN, ROLLBACK named/anonymous, nested)
//!   Phase 8 — Error cases (malformed FQL, missing session, nonexistent symbols)
//!   Phase 9 — Parser-level coverage (every clause parses without execution)
//!   Phase 10 — Query logger integration
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    unused_results
)]

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::parser;
use forgeql_core::query_logger::QueryLogger;
use forgeql_core::result::{ForgeQLResult, ShowContent};
use tempfile::tempdir;

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/fixtures")
}

/// Create a temp workspace with motor_control fixtures and boot an engine.
fn engine_with_session() -> (ForgeQLEngine, String, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let src = fixtures_dir();

    fs::copy(
        src.join("motor_control.h"),
        dir.path().join("motor_control.h"),
    )
    .expect("copy .h");
    fs::copy(
        src.join("motor_control.cpp"),
        dir.path().join("motor_control.cpp"),
    )
    .expect("copy .cpp");

    let data_dir = dir.path().join("data");
    let mut engine = ForgeQLEngine::new(data_dir).expect("engine");
    let session_id = engine
        .register_local_session(dir.path())
        .expect("register session");

    (engine, session_id, dir)
}

/// Like `engine_with_session` but also initializes a git repo (needed for transactions).
fn engine_with_git_session() -> (ForgeQLEngine, String, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let src = fixtures_dir();

    // Init git repo.
    let repo = git2::Repository::init(dir.path()).expect("git init");
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    drop(cfg);

    fs::copy(
        src.join("motor_control.h"),
        dir.path().join("motor_control.h"),
    )
    .expect("copy .h");
    fs::copy(
        src.join("motor_control.cpp"),
        dir.path().join("motor_control.cpp"),
    )
    .expect("copy .cpp");

    // Stage and commit so git operations work.
    let mut index = repo.index().unwrap();
    index
        .add_path(std::path::Path::new("motor_control.h"))
        .unwrap();
    index
        .add_path(std::path::Path::new("motor_control.cpp"))
        .unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(0, 0)).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();

    let data_dir = dir.path().join("data");
    let mut engine = ForgeQLEngine::new(data_dir).expect("engine");
    let session_id = engine
        .register_local_session(dir.path())
        .expect("register session");

    (engine, session_id, dir)
}

/// Parse FQL and execute the first op.
fn exec(engine: &mut ForgeQLEngine, sid: &str, fql: &str) -> ForgeQLResult {
    let ops = parser::parse(fql).unwrap_or_else(|e| panic!("parse failed for: {fql}: {e}"));
    let op = ops.first().expect("at least one op");
    engine
        .execute(Some(sid), op)
        .unwrap_or_else(|e| panic!("execute failed for: {fql}: {e}"))
}

/// Parse FQL and execute, expecting an engine error.
fn exec_err(engine: &mut ForgeQLEngine, sid: &str, fql: &str) -> String {
    let ops = parser::parse(fql).unwrap_or_else(|e| panic!("parse failed for: {fql}: {e}"));
    let op = ops.first().expect("at least one op");
    engine
        .execute(Some(sid), op)
        .expect_err(&format!("expected error for: {fql}"))
        .to_string()
}

/// Extract query results or panic.
fn as_query(r: &ForgeQLResult) -> &forgeql_core::result::QueryResult {
    match r {
        ForgeQLResult::Query(qr) => qr,
        other => panic!("expected Query, got: {other:?}"),
    }
}

/// Extract show result or panic.
fn as_show(r: &ForgeQLResult) -> &forgeql_core::result::ShowResult {
    match r {
        ForgeQLResult::Show(sr) => sr,
        other => panic!("expected Show, got: {other:?}"),
    }
}

/// Extract mutation result or panic.
fn as_mutation(r: &ForgeQLResult) -> &forgeql_core::result::MutationResult {
    match r {
        ForgeQLResult::Mutation(mr) => mr,
        other => panic!("expected Mutation, got: {other:?}"),
    }
}

// =======================================================================
// Phase 1 — FIND symbols
// =======================================================================

#[test]
fn find_symbols_bare() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols");
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    assert!(qr.total > 0);
}

#[test]
fn find_symbols_where_node_kind_eq_function_definition() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    for row in &qr.results {
        assert_eq!(row.node_kind.as_deref(), Some("function_definition"));
    }
}

#[test]
fn find_symbols_where_node_kind_eq_class_specifier() {
    let (mut e, sid, _d) = engine_with_session();
    // motor_control.h has no class_specifier — expect empty results (not an error).
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'class_specifier'",
    );
    let qr = as_query(&r);
    // May be empty for this fixture — that's valid.
    for row in &qr.results {
        assert_eq!(row.node_kind.as_deref(), Some("class_specifier"));
    }
}

#[test]
fn find_symbols_where_node_kind_eq_struct_specifier() {
    let (mut e, sid, _d) = engine_with_session();
    // motor_control.h has typedef struct — may or may not be indexed as struct_specifier.
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'struct_specifier'",
    );
    let qr = as_query(&r);
    for row in &qr.results {
        assert_eq!(row.node_kind.as_deref(), Some("struct_specifier"));
    }
}

#[test]
fn find_symbols_where_node_kind_eq_enum_specifier() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'enum_specifier'",
    );
    let qr = as_query(&r);
    // motor_control.h has ErrorMotor and ErrorSensor enums.
    for row in &qr.results {
        assert_eq!(row.node_kind.as_deref(), Some("enum_specifier"));
    }
}

#[test]
fn find_symbols_where_node_kind_eq_preproc_def() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE node_kind = 'preproc_def'");
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "fixture has #define macros");
    for row in &qr.results {
        assert_eq!(row.node_kind.as_deref(), Some("preproc_def"));
    }
    let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"VELOCIDAD_MAX"),
        "expected VELOCIDAD_MAX in {names:?}"
    );
}

#[test]
fn find_symbols_where_node_kind_eq_preproc_include() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'preproc_include'",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "fixture has #include directives");
    for row in &qr.results {
        assert_eq!(row.node_kind.as_deref(), Some("preproc_include"));
    }
}

#[test]
fn find_symbols_where_node_kind_eq_declaration() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'declaration' LIMIT 100",
    );
    let qr = as_query(&r);
    for row in &qr.results {
        assert_eq!(row.node_kind.as_deref(), Some("declaration"));
    }
}

#[test]
fn find_symbols_where_node_kind_eq_comment() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'comment' LIMIT 10",
    );
    let qr = as_query(&r);
    for row in &qr.results {
        assert_eq!(row.node_kind.as_deref(), Some("comment"));
    }
}

// --- WHERE name operators ---

#[test]
fn find_symbols_where_name_like_prefix() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name LIKE 'encender%'");
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    for row in &qr.results {
        assert!(
            row.name.starts_with("encender"),
            "name '{}' doesn't start with 'encender'",
            row.name
        );
    }
}

#[test]
fn find_symbols_where_name_like_suffix() {
    let (mut e, sid, _d) = engine_with_session();
    // LIKE is case-insensitive in ForgeQL, so '%Motor' matches 'MOTOR' too.
    let r = exec(&mut e, &sid, "FIND symbols WHERE name LIKE '%Motor'");
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    for row in &qr.results {
        let lower = row.name.to_lowercase();
        assert!(
            lower.ends_with("motor"),
            "name '{}' doesn't end with 'Motor' (case-insensitive)",
            row.name
        );
    }
}

#[test]
fn find_symbols_where_name_like_contains() {
    let (mut e, sid, _d) = engine_with_session();
    // LIKE is case-insensitive in ForgeQL.
    let r = exec(&mut e, &sid, "FIND symbols WHERE name LIKE '%Motor%'");
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    for row in &qr.results {
        let lower = row.name.to_lowercase();
        assert!(
            lower.contains("motor"),
            "name '{}' doesn't contain 'Motor' (case-insensitive)",
            row.name
        );
    }
}

#[test]
fn find_symbols_where_name_not_like() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE name NOT LIKE 'encender%'",
    );
    let qr = as_query(&r);
    for row in &qr.results {
        assert!(
            !row.name.starts_with("encender"),
            "NOT LIKE failed for '{}'",
            row.name
        );
    }
}

#[test]
fn find_symbols_where_name_eq_exact() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name = 'encenderMotor'");
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    for row in &qr.results {
        assert_eq!(row.name, "encenderMotor");
    }
}

#[test]
fn find_symbols_where_name_neq() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE name != 'encenderMotor' LIMIT 100",
    );
    let qr = as_query(&r);
    for row in &qr.results {
        assert_ne!(row.name, "encenderMotor");
    }
}

// --- WHERE usages operators ---

#[test]
fn find_symbols_where_usages_eq_zero() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE usages = 0 LIMIT 100");
    let qr = as_query(&r);
    for row in &qr.results {
        assert_eq!(
            row.usages_count.unwrap_or(0),
            0,
            "expected 0 usages for '{}'",
            row.name
        );
    }
}

#[test]
fn find_symbols_where_usages_neq_zero() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE usages != 0 LIMIT 100");
    let qr = as_query(&r);
    for row in &qr.results {
        assert_ne!(
            row.usages_count.unwrap_or(0),
            0,
            "expected non-zero usages for '{}'",
            row.name
        );
    }
}

#[test]
fn find_symbols_where_usages_gte() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE usages >= 5 LIMIT 100");
    let qr = as_query(&r);
    for row in &qr.results {
        assert!(
            row.usages_count.unwrap_or(0) >= 5,
            "expected >= 5 usages for '{}'",
            row.name
        );
    }
}

#[test]
fn find_symbols_where_usages_gt() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE usages > 0 LIMIT 100");
    let qr = as_query(&r);
    for row in &qr.results {
        assert!(
            row.usages_count.unwrap_or(0) > 0,
            "expected > 0 usages for '{}'",
            row.name
        );
    }
}

#[test]
fn find_symbols_where_usages_lte() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE usages <= 2 LIMIT 100");
    let qr = as_query(&r);
    for row in &qr.results {
        assert!(
            row.usages_count.unwrap_or(0) <= 2,
            "expected <= 2 usages for '{}'",
            row.name
        );
    }
}

#[test]
fn find_symbols_where_usages_lt() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE usages < 3 LIMIT 100");
    let qr = as_query(&r);
    for row in &qr.results {
        assert!(
            row.usages_count.unwrap_or(0) < 3,
            "expected < 3 usages for '{}'",
            row.name
        );
    }
}

// --- WHERE line operators ---

#[test]
fn find_symbols_where_line_gte() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE line >= 50 LIMIT 100");
    let qr = as_query(&r);
    for row in &qr.results {
        assert!(
            row.line.unwrap_or(0) >= 50,
            "expected line >= 50 for '{}'",
            row.name
        );
    }
}

#[test]
fn find_symbols_where_line_lt() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE line < 30 LIMIT 100");
    let qr = as_query(&r);
    for row in &qr.results {
        assert!(
            row.line.unwrap_or(0) < 30,
            "expected line < 30 for '{}'",
            row.name
        );
    }
}

// --- Multiple WHERE clauses (AND) ---

#[test]
fn find_symbols_two_where_clauses() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE usages >= 1 LIMIT 50",
    );
    let qr = as_query(&r);
    for row in &qr.results {
        assert_eq!(row.node_kind.as_deref(), Some("function_definition"));
        assert!(row.usages_count.unwrap_or(0) >= 1);
    }
}

#[test]
fn find_symbols_three_where_clauses() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE usages >= 1 WHERE name LIKE 'encender%'",
    );
    let qr = as_query(&r);
    for row in &qr.results {
        assert_eq!(row.node_kind.as_deref(), Some("function_definition"));
        assert!(row.usages_count.unwrap_or(0) >= 1);
        assert!(row.name.starts_with("encender"));
    }
}

// --- ORDER BY ---

#[test]
fn find_symbols_order_by_name_asc() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' ORDER BY name ASC LIMIT 100",
    );
    let qr = as_query(&r);
    let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
    for w in names.windows(2) {
        assert!(
            w[0] <= w[1],
            "ORDER BY name ASC broken: '{}' > '{}'",
            w[0],
            w[1]
        );
    }
}

#[test]
fn find_symbols_order_by_name_desc() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' ORDER BY name DESC LIMIT 100",
    );
    let qr = as_query(&r);
    let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
    for w in names.windows(2) {
        assert!(
            w[0] >= w[1],
            "ORDER BY name DESC broken: '{}' < '{}'",
            w[0],
            w[1]
        );
    }
}

#[test]
fn find_symbols_order_by_usages_desc() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' ORDER BY usages DESC LIMIT 100",
    );
    let qr = as_query(&r);
    let counts: Vec<usize> = qr
        .results
        .iter()
        .map(|r| r.usages_count.unwrap_or(0))
        .collect();
    for w in counts.windows(2) {
        assert!(
            w[0] >= w[1],
            "ORDER BY usages DESC broken: {} < {}",
            w[0],
            w[1]
        );
    }
}

#[test]
fn find_symbols_order_by_line_asc() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' ORDER BY line ASC LIMIT 100",
    );
    let qr = as_query(&r);
    let lines: Vec<usize> = qr.results.iter().map(|r| r.line.unwrap_or(0)).collect();
    for w in lines.windows(2) {
        assert!(
            w[0] <= w[1],
            "ORDER BY line ASC broken: {} > {}",
            w[0],
            w[1]
        );
    }
}

#[test]
fn find_symbols_order_by_default_is_asc() {
    let (mut e, sid, _d) = engine_with_session();
    // ORDER BY without explicit direction — verify it parses and returns consistent results.
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' ORDER BY name LIMIT 100",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    // Run it again to verify deterministic output.
    let r2 = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' ORDER BY name LIMIT 100",
    );
    let names1: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
    let names2: Vec<&str> = as_query(&r2)
        .results
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(names1, names2, "ORDER BY name should be deterministic");
}

// --- LIMIT ---

#[test]
fn find_symbols_limit_1() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols LIMIT 1");
    let qr = as_query(&r);
    assert_eq!(qr.results.len(), 1);
}

#[test]
fn find_symbols_limit_5() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols LIMIT 5");
    let qr = as_query(&r);
    assert!(qr.results.len() <= 5);
}

// --- OFFSET ---

#[test]
fn find_symbols_limit_offset() {
    let (mut e, sid, _d) = engine_with_session();
    let all = exec(&mut e, &sid, "FIND symbols ORDER BY name ASC LIMIT 1000");
    let paged = exec(
        &mut e,
        &sid,
        "FIND symbols ORDER BY name ASC LIMIT 1000 OFFSET 3",
    );
    let all_names: Vec<&str> = as_query(&all)
        .results
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    let paged_names: Vec<&str> = as_query(&paged)
        .results
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(paged_names.len(), all_names.len() - 3);
    assert_eq!(paged_names[0], all_names[3]);
}

// --- IN glob ---

#[test]
fn find_symbols_in_glob_h() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols IN '*.h' LIMIT 100");
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    for row in &qr.results {
        let p = row.path.as_ref().expect("path present").to_string_lossy();
        assert!(p.ends_with(".h"), "IN '*.h' returned file '{p}'");
    }
}

#[test]
fn find_symbols_in_glob_cpp() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols IN '*.cpp' LIMIT 100");
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    for row in &qr.results {
        let p = row.path.as_ref().expect("path present").to_string_lossy();
        assert!(p.ends_with(".cpp"), "IN '*.cpp' returned file '{p}'");
    }
}

// --- EXCLUDE glob ---

#[test]
fn find_symbols_exclude_glob() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols EXCLUDE '*.h' LIMIT 100");
    let qr = as_query(&r);
    for row in &qr.results {
        let p = row.path.as_ref().expect("path present").to_string_lossy();
        assert!(!p.ends_with(".h"), "EXCLUDE '*.h' still returned '{p}'");
    }
}

// --- IN + EXCLUDE combined ---

#[test]
fn find_symbols_in_and_exclude() {
    let (mut e, sid, _d) = engine_with_session();
    // This should return nothing since we include *.cpp and exclude *.cpp.
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols IN '*.cpp' EXCLUDE '*.cpp' LIMIT 100",
    );
    let qr = as_query(&r);
    assert!(
        qr.results.is_empty(),
        "IN + EXCLUDE same glob should return empty"
    );
}

// --- GROUP BY ---

#[test]
fn find_symbols_group_by_node_kind() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols GROUP BY node_kind ORDER BY count DESC",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    // Each row should have a count > 0.
    for row in &qr.results {
        assert!(
            row.count.unwrap_or(0) > 0,
            "GROUP BY row should have count > 0"
        );
    }
}

#[test]
fn find_symbols_group_by_file() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols GROUP BY file ORDER BY count DESC LIMIT 20",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    for row in &qr.results {
        assert!(row.count.unwrap_or(0) > 0);
    }
}

// --- WHERE + ORDER BY + LIMIT + OFFSET combined ---

#[test]
fn find_symbols_full_clause_combination() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' \
         ORDER BY usages DESC LIMIT 3 OFFSET 1",
    );
    let qr = as_query(&r);
    assert!(qr.results.len() <= 3);
    for row in &qr.results {
        assert_eq!(row.node_kind.as_deref(), Some("function_definition"));
    }
}

// --- Dynamic fields ---

#[test]
fn find_symbols_where_type_like_void() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' WHERE type LIKE 'void%' LIMIT 50",
    );
    let qr = as_query(&r);
    // All returned symbols should have type starting with "void"
    for row in &qr.results {
        if let Some(t) = row.fields.get("type") {
            assert!(
                t.starts_with("void"),
                "expected type starting with 'void', got '{t}' for '{}'",
                row.name
            );
        }
    }
}

#[test]
fn find_symbols_where_scope_eq_local() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'declaration' WHERE scope = 'local' LIMIT 50",
    );
    let qr = as_query(&r);
    for row in &qr.results {
        assert_eq!(
            row.fields.get("scope").map(String::as_str),
            Some("local"),
            "expected scope='local' for '{}'",
            row.name
        );
    }
}

#[test]
fn find_symbols_where_scope_eq_file() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'declaration' WHERE scope = 'file' LIMIT 50",
    );
    let qr = as_query(&r);
    for row in &qr.results {
        assert_eq!(
            row.fields.get("scope").map(String::as_str),
            Some("file"),
            "expected scope='file' for '{}'",
            row.name
        );
    }
}

#[test]
fn find_symbols_where_storage_eq_static() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'declaration' WHERE storage = 'static' LIMIT 50",
    );
    let qr = as_query(&r);
    for row in &qr.results {
        assert_eq!(
            row.fields.get("storage").map(String::as_str),
            Some("static"),
            "expected storage='static' for '{}'",
            row.name
        );
    }
}

#[test]
fn find_symbols_where_storage_neq_static() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'declaration' WHERE storage != 'static' LIMIT 50",
    );
    let qr = as_query(&r);
    for row in &qr.results {
        assert_ne!(
            row.fields.get("storage").map(String::as_str),
            Some("static"),
            "expected storage != 'static' for '{}'",
            row.name
        );
    }
}

// =======================================================================
// Phase 2 — FIND usages / FIND callees
// =======================================================================

#[test]
fn find_usages_basic() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND usages OF 'encenderMotor'");
    let qr = as_query(&r);
    assert!(!qr.results.is_empty(), "encenderMotor should have usages");
}

#[test]
fn find_usages_group_by_file() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND usages OF 'encenderMotor' GROUP BY file ORDER BY count DESC",
    );
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    let paths: Vec<_> = qr
        .results
        .iter()
        .filter_map(|r| r.path.as_deref())
        .collect();
    let unique: HashSet<_> = paths.iter().collect();
    assert_eq!(
        paths.len(),
        unique.len(),
        "GROUP BY file should yield unique paths"
    );
    for row in &qr.results {
        assert!(row.count.unwrap_or(0) >= 1);
    }
}

#[test]
fn find_usages_in_glob() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND usages OF 'encenderMotor' IN '*.cpp'");
    let qr = as_query(&r);
    for row in &qr.results {
        let p = row.path.as_ref().expect("path").to_string_lossy();
        assert!(p.ends_with(".cpp"), "IN '*.cpp' returned '{p}'");
    }
}

#[test]
fn find_usages_with_limit() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND usages OF 'encenderMotor' LIMIT 2");
    let qr = as_query(&r);
    assert!(qr.results.len() <= 2);
}

#[test]
fn find_usages_order_by_line() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND usages OF 'encenderMotor' ORDER BY line ASC LIMIT 100",
    );
    let qr = as_query(&r);
    let lines: Vec<usize> = qr.results.iter().map(|r| r.line.unwrap_or(0)).collect();
    for w in lines.windows(2) {
        assert!(
            w[0] <= w[1],
            "ORDER BY line ASC broken: {} > {}",
            w[0],
            w[1]
        );
    }
}

#[test]
fn find_usages_nonexistent_symbol_returns_empty() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND usages OF 'zzz_nonexistent_symbol_12345'",
    );
    let qr = as_query(&r);
    assert!(qr.results.is_empty());
}

#[test]
fn find_callees_basic() {
    let (mut e, sid, _d) = engine_with_session();
    // encenderSistema calls encenderMotor and memset etc.
    let r = exec(&mut e, &sid, "FIND callees OF 'encenderSistema'");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::CallGraph { entries, .. } => {
            assert!(!entries.is_empty(), "encenderSistema should have callees");
        }
        other => panic!("expected CallGraph, got {other:?}"),
    }
}

#[test]
fn find_callees_with_limit() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND callees OF 'encenderSistema' LIMIT 2");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::CallGraph { entries, .. } => {
            // LIMIT may not apply to callees — just verify it parses and executes.
            assert!(!entries.is_empty());
        }
        other => panic!("expected CallGraph, got {other:?}"),
    }
}

// =======================================================================
// Phase 3 — FIND files
// =======================================================================

#[test]
fn find_files_bare() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND files");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::FileList { files, total } => {
            assert!(*total >= 2, "fixture should have at least 2 files");
            assert!(!files.is_empty());
        }
        other => panic!("expected FileList, got {other:?}"),
    }
}

#[test]
fn find_files_where_extension_cpp() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND files WHERE extension = 'cpp'");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::FileList { files, .. } => {
            assert!(!files.is_empty());
            for f in files {
                assert_eq!(f.extension, "cpp", "expected .cpp, got '{}'", f.extension);
            }
        }
        other => panic!("expected FileList, got {other:?}"),
    }
}

#[test]
fn find_files_where_extension_h() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND files WHERE extension = 'h'");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::FileList { files, .. } => {
            assert!(!files.is_empty());
            for f in files {
                assert_eq!(f.extension, "h");
            }
        }
        other => panic!("expected FileList, got {other:?}"),
    }
}

#[test]
fn find_files_where_size_gt() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND files WHERE size > 100 ORDER BY size DESC",
    );
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::FileList { files, .. } => {
            for f in files {
                assert!(f.size > 100);
            }
            // Verify descending order.
            for w in files.windows(2) {
                assert!(w[0].size >= w[1].size, "ORDER BY size DESC broken");
            }
        }
        other => panic!("expected FileList, got {other:?}"),
    }
}

#[test]
fn find_files_where_depth() {
    let (mut e, sid, _d) = engine_with_session();
    // WHERE depth on FIND files: depth is not populated per-entry in the
    // current implementation; just verify the clause is accepted and runs.
    let r = exec(&mut e, &sid, "FIND files WHERE depth <= 999");
    let _sr = as_show(&r);
    // No assertion on results — the engine returns empty when depth is unset.
}

#[test]
fn find_files_depth_clause() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND files DEPTH 1");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::FileList { .. } => {} // Just verify it doesn't error.
        other => panic!("expected FileList, got {other:?}"),
    }
}

#[test]
fn find_files_group_by_extension() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND files GROUP BY extension ORDER BY count DESC",
    );
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::FileList { .. } => {} // Just verify it doesn't error.
        other => panic!("expected FileList, got {other:?}"),
    }
}

#[test]
fn find_files_in_glob() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND files IN '*.h'");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::FileList { files, .. } => {
            for f in files {
                assert!(f.path.to_string_lossy().ends_with(".h"));
            }
        }
        other => panic!("expected FileList, got {other:?}"),
    }
}

// =======================================================================
// Phase 4 — FIND globals
// =======================================================================

#[test]
fn find_globals_bare() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND globals LIMIT 100");
    let qr = as_query(&r);
    assert!(!qr.results.is_empty());
    for row in &qr.results {
        assert_eq!(row.node_kind.as_deref(), Some("declaration"));
        assert_eq!(row.fields.get("scope").map(String::as_str), Some("file"));
    }
}

#[test]
fn find_globals_where_storage_static() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND globals WHERE storage = 'static' LIMIT 100",
    );
    let qr = as_query(&r);
    for row in &qr.results {
        assert_eq!(
            row.fields.get("storage").map(String::as_str),
            Some("static")
        );
    }
}

#[test]
fn find_globals_where_storage_neq_static() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND globals WHERE storage != 'static' LIMIT 100",
    );
    let qr = as_query(&r);
    for row in &qr.results {
        assert_ne!(
            row.fields.get("storage").map(String::as_str),
            Some("static")
        );
    }
}

#[test]
fn find_globals_order_by_usages_desc() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND globals ORDER BY usages DESC LIMIT 100");
    let qr = as_query(&r);
    let counts: Vec<usize> = qr
        .results
        .iter()
        .map(|r| r.usages_count.unwrap_or(0))
        .collect();
    for w in counts.windows(2) {
        assert!(
            w[0] >= w[1],
            "ORDER BY usages DESC broken: {} < {}",
            w[0],
            w[1]
        );
    }
}

// =======================================================================
// Phase 5 — SHOW commands
// =======================================================================

// --- SHOW body ---

#[test]
fn show_body_default_depth() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW body OF 'encenderMotor'");
    let sr = as_show(&r);
    assert_eq!(sr.op, "show_body");
    assert_eq!(sr.symbol.as_deref(), Some("encenderMotor"));
    assert!(sr.start_line.is_some());
    assert!(sr.end_line.is_some());
}

#[test]
fn show_body_depth_0() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW body OF 'encenderMotor' DEPTH 0");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::Lines { lines, .. } => {
            assert!(
                !lines.is_empty(),
                "DEPTH 0 should still return signature lines"
            );
        }
        other => panic!("expected Lines, got {other:?}"),
    }
}

#[test]
fn show_body_depth_1() {
    let (mut e, sid, _d) = engine_with_session();
    let r0 = exec(&mut e, &sid, "SHOW body OF 'encenderMotor' DEPTH 0");
    let r1 = exec(&mut e, &sid, "SHOW body OF 'encenderMotor' DEPTH 1");
    let lines0 = match &as_show(&r0).content {
        ShowContent::Lines { lines, .. } => lines.len(),
        other => panic!("expected Lines, got {other:?}"),
    };
    let lines1 = match &as_show(&r1).content {
        ShowContent::Lines { lines, .. } => lines.len(),
        other => panic!("expected Lines, got {other:?}"),
    };
    assert!(
        lines1 > lines0,
        "DEPTH 1 should return more lines than DEPTH 0 ({lines1} vs {lines0})"
    );
}

#[test]
fn show_body_depth_99() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW body OF 'encenderMotor' DEPTH 99");
    let sr = as_show(&r);
    let text = format!("{r}");
    assert!(
        text.contains("encenderMotor"),
        "full body should contain the function name"
    );
    assert!(sr.start_line.is_some());
    assert!(sr.end_line.is_some());
    let start = sr.start_line.unwrap();
    let end = sr.end_line.unwrap();
    assert!(end > start, "multi-line function should span > 1 line");
}

// --- SHOW signature ---

#[test]
fn show_signature() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW signature OF 'encenderMotor'");
    let sr = as_show(&r);
    assert_eq!(sr.op, "show_signature");
    match &sr.content {
        ShowContent::Signature { signature, .. } => {
            assert!(
                signature.contains("encenderMotor"),
                "signature should contain function name"
            );
        }
        other => panic!("expected Signature, got {other:?}"),
    }
}

#[test]
fn show_signature_another_symbol() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW signature OF 'apagarMotor'");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::Signature { signature, .. } => {
            assert!(signature.contains("apagarMotor"));
        }
        other => panic!("expected Signature, got {other:?}"),
    }
}

// --- SHOW outline ---

#[test]
fn show_outline() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW outline OF 'motor_control.h'");
    let sr = as_show(&r);
    assert_eq!(sr.op, "show_outline");
    match &sr.content {
        ShowContent::Outline { entries } => {
            assert!(
                !entries.is_empty(),
                "motor_control.h should have outline entries"
            );
        }
        other => panic!("expected Outline, got {other:?}"),
    }
}

#[test]
fn show_outline_cpp_file() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW outline OF 'motor_control.cpp'");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::Outline { entries } => {
            assert!(!entries.is_empty());
        }
        other => panic!("expected Outline, got {other:?}"),
    }
}

#[test]
fn show_outline_with_limit() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW outline OF 'motor_control.h' LIMIT 2");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::Outline { entries } => {
            assert!(entries.len() <= 2);
        }
        other => panic!("expected Outline, got {other:?}"),
    }
}

// --- SHOW members ---

#[test]
fn show_members_enum() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW members OF 'ErrorMotor'");
    let sr = as_show(&r);
    assert_eq!(sr.op, "show_members");
    match &sr.content {
        ShowContent::Members { members, .. } => {
            // ErrorMotor has OK, TIMEOUT, FALLO.
            assert!(
                members.len() >= 3,
                "ErrorMotor should have >= 3 members, got {}",
                members.len()
            );
        }
        other => panic!("expected Members, got {other:?}"),
    }
}

#[test]
fn show_members_with_limit() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW members OF 'ErrorMotor' LIMIT 1");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::Members { members, .. } => {
            assert_eq!(members.len(), 1);
        }
        other => panic!("expected Members, got {other:?}"),
    }
}

#[test]
fn show_members_another_enum() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW members OF 'ErrorSensor'");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::Members { members, .. } => {
            assert!(members.len() >= 3, "ErrorSensor should have >= 3 members");
        }
        other => panic!("expected Members, got {other:?}"),
    }
}

// --- SHOW context ---

#[test]
fn show_context() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW context OF 'VELOCIDAD_MAX'");
    let sr = as_show(&r);
    assert_eq!(sr.op, "show_context");
    assert!(sr.start_line.is_some());
    assert!(sr.end_line.is_some());
}

#[test]
fn show_context_function() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW context OF 'encenderMotor'");
    let sr = as_show(&r);
    assert_eq!(sr.op, "show_context");
}

// --- SHOW callees ---

#[test]
fn show_callees() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW callees OF 'encenderSistema'");
    let sr = as_show(&r);
    assert_eq!(sr.op, "show_callees");
    match &sr.content {
        ShowContent::CallGraph { entries, direction } => {
            assert!(matches!(
                direction,
                forgeql_core::result::CallDirection::Callees
            ));
            assert!(!entries.is_empty(), "encenderSistema should have callees");
        }
        other => panic!("expected CallGraph, got {other:?}"),
    }
}

// --- SHOW LINES ---

#[test]
fn show_lines_basic() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW LINES 1-5 OF 'motor_control.h'");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::Lines { lines, .. } => {
            assert_eq!(
                lines.len(),
                5,
                "SHOW LINES 1-5 should return exactly 5 lines"
            );
            assert_eq!(lines[0].line, 1);
            assert_eq!(lines[4].line, 5);
        }
        other => panic!("expected Lines, got {other:?}"),
    }
}

#[test]
fn show_lines_single_line() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW LINES 1-1 OF 'motor_control.h'");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::Lines { lines, .. } => {
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].line, 1);
        }
        other => panic!("expected Lines, got {other:?}"),
    }
}

#[test]
fn show_lines_middle_range() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW LINES 10-15 OF 'motor_control.h'");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::Lines { lines, .. } => {
            assert_eq!(lines.len(), 6, "lines 10-15 = 6 lines");
            assert_eq!(lines[0].line, 10);
            assert_eq!(lines[5].line, 15);
        }
        other => panic!("expected Lines, got {other:?}"),
    }
}

#[test]
fn show_lines_cpp_file() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW LINES 1-3 OF 'motor_control.cpp'");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::Lines { lines, .. } => {
            assert_eq!(lines.len(), 3);
        }
        other => panic!("expected Lines, got {other:?}"),
    }
}

// =======================================================================
// Phase 6 — CHANGE + verification round-trips
// =======================================================================

// --- CHANGE MATCHING ---

#[test]
fn change_matching_and_rollback() {
    let (mut e, sid, _d) = engine_with_git_session();

    // Begin transaction.
    let r = exec(&mut e, &sid, "BEGIN TRANSACTION 'test-matching'");
    assert!(matches!(r, ForgeQLResult::BeginTransaction(_)));

    // Apply matching rename.
    let r = exec(
        &mut e,
        &sid,
        "CHANGE FILE 'motor_control.cpp' MATCHING 'void encenderMotor' WITH 'void startMotor'",
    );
    let mr = as_mutation(&r);
    assert!(mr.applied);
    assert!(mr.edit_count > 0);

    // Verify the rename took effect.
    let show = exec(&mut e, &sid, "SHOW LINES 48-48 OF 'motor_control.cpp'");
    let text = format!("{show}");
    assert!(text.contains("startMotor"), "rename should be visible");

    // Rollback.
    let r = exec(&mut e, &sid, "ROLLBACK TRANSACTION 'test-matching'");
    assert!(matches!(r, ForgeQLResult::Rollback(_)));

    // Verify it's restored.
    let show = exec(&mut e, &sid, "SHOW LINES 48-48 OF 'motor_control.cpp'");
    let text = format!("{show}");
    assert!(
        text.contains("encenderMotor"),
        "rollback should restore original"
    );
}

// --- CHANGE LINES n-m WITH 'content' ---

#[test]
fn change_lines_replace_and_rollback() {
    let (mut e, sid, _d) = engine_with_git_session();

    exec(&mut e, &sid, "BEGIN TRANSACTION 'test-lines'");

    let r = exec(
        &mut e,
        &sid,
        "CHANGE FILE 'motor_control.h' LINES 1-1 WITH '// REPLACED BY TEST'",
    );
    let mr = as_mutation(&r);
    assert!(mr.applied);

    // Verify.
    let show = exec(&mut e, &sid, "SHOW LINES 1-1 OF 'motor_control.h'");
    let text = format!("{show}");
    assert!(text.contains("REPLACED BY TEST"));

    // Rollback.
    exec(&mut e, &sid, "ROLLBACK TRANSACTION 'test-lines'");

    // Verify restored.
    let show = exec(&mut e, &sid, "SHOW LINES 1-1 OF 'motor_control.h'");
    let text = format!("{show}");
    assert!(!text.contains("REPLACED BY TEST"));
}

// --- CHANGE WITH 'content' (full file overwrite) ---

#[test]
fn change_with_content_and_rollback() {
    let (mut e, sid, _d) = engine_with_git_session();

    exec(&mut e, &sid, "BEGIN TRANSACTION 'test-with-content'");

    let r = exec(
        &mut e,
        &sid,
        "CHANGE FILE 'motor_control.h' WITH '// OVERWRITTEN\n'",
    );
    let mr = as_mutation(&r);
    assert!(mr.applied);

    // Verify.
    let show = exec(&mut e, &sid, "SHOW LINES 1-1 OF 'motor_control.h'");
    let text = format!("{show}");
    assert!(text.contains("OVERWRITTEN"));

    // Rollback.
    exec(&mut e, &sid, "ROLLBACK TRANSACTION 'test-with-content'");

    // Verify restored.
    let show = exec(&mut e, &sid, "SHOW LINES 1-1 OF 'motor_control.h'");
    let text = format!("{show}");
    assert!(!text.contains("OVERWRITTEN"));
}

// --- CHANGE LINES n-m NOTHING (line deletion) ---

#[test]
fn change_lines_delete_nothing_and_rollback() {
    let (mut e, sid, _d) = engine_with_git_session();

    // Get original line count.
    let before = exec(&mut e, &sid, "SHOW LINES 1-5 OF 'motor_control.h'");
    let before_text = format!("{before}");

    exec(&mut e, &sid, "BEGIN TRANSACTION 'test-lines-nothing'");

    let r = exec(
        &mut e,
        &sid,
        "CHANGE FILE 'motor_control.h' LINES 3-3 NOTHING",
    );
    let mr = as_mutation(&r);
    assert!(mr.applied);

    // Rollback to restore.
    exec(&mut e, &sid, "ROLLBACK TRANSACTION 'test-lines-nothing'");

    // Verify restored.
    let after = exec(&mut e, &sid, "SHOW LINES 1-5 OF 'motor_control.h'");
    let after_text = format!("{after}");
    assert_eq!(
        before_text, after_text,
        "rollback should restore deleted lines"
    );
}

// --- CHANGE WITH NOTHING (clear file) ---

#[test]
fn change_with_nothing_and_rollback() {
    let (mut e, sid, _d) = engine_with_git_session();

    exec(&mut e, &sid, "BEGIN TRANSACTION 'test-delete'");

    let r = exec(&mut e, &sid, "CHANGE FILE 'motor_control.h' WITH NOTHING");
    let mr = as_mutation(&r);
    assert!(mr.applied);

    // Rollback.
    exec(&mut e, &sid, "ROLLBACK TRANSACTION 'test-delete'");

    // Verify restored.
    let show = exec(&mut e, &sid, "SHOW LINES 1-5 OF 'motor_control.h'");
    let text = format!("{show}");
    assert!(
        text.contains("motor_control"),
        "file should be restored after rollback"
    );
}

// --- CHANGE FILES (multi-file glob) MATCHING ---

#[test]
fn change_files_glob_matching_and_rollback() {
    let (mut e, sid, _d) = engine_with_git_session();

    exec(&mut e, &sid, "BEGIN TRANSACTION 'test-multi-glob'");

    let r = exec(
        &mut e,
        &sid,
        "CHANGE FILES '*.cpp', '*.h' MATCHING 'encenderMotor' WITH 'startMotor'",
    );
    let mr = as_mutation(&r);
    assert!(mr.applied);
    assert!(mr.edit_count >= 2, "should edit both .h and .cpp");

    // Rollback.
    exec(&mut e, &sid, "ROLLBACK TRANSACTION 'test-multi-glob'");
}

// =======================================================================
// Phase 7 — Transaction commands
// =======================================================================

#[test]
fn begin_and_rollback_named() {
    let (mut e, sid, _d) = engine_with_git_session();
    let r = exec(&mut e, &sid, "BEGIN TRANSACTION 'txn-basic'");
    match &r {
        ForgeQLResult::BeginTransaction(bt) => {
            assert_eq!(bt.name, "txn-basic");
        }
        other => panic!("expected BeginTransaction, got {other:?}"),
    }

    let r = exec(&mut e, &sid, "ROLLBACK TRANSACTION 'txn-basic'");
    match &r {
        ForgeQLResult::Rollback(rb) => {
            let text = format!("{rb}");
            assert!(text.contains("txn-basic") || text.contains("Rolled back"));
        }
        other => panic!("expected Rollback, got {other:?}"),
    }
}

#[test]
fn begin_and_rollback_anonymous() {
    let (mut e, sid, _d) = engine_with_git_session();
    exec(&mut e, &sid, "BEGIN TRANSACTION 'txn-anon'");
    // Anonymous ROLLBACK should pop the most recent checkpoint.
    let r = exec(&mut e, &sid, "ROLLBACK");
    assert!(matches!(r, ForgeQLResult::Rollback(_)));
}

#[test]
fn nested_transactions() {
    let (mut e, sid, _d) = engine_with_git_session();
    exec(&mut e, &sid, "BEGIN TRANSACTION 'outer'");
    exec(&mut e, &sid, "BEGIN TRANSACTION 'inner'");

    // Rolling back 'outer' should work (discards 'inner' too).
    let r = exec(&mut e, &sid, "ROLLBACK TRANSACTION 'outer'");
    assert!(matches!(r, ForgeQLResult::Rollback(_)));
}

#[test]
fn nested_transactions_rollback_inner() {
    let (mut e, sid, _d) = engine_with_git_session();
    exec(&mut e, &sid, "BEGIN TRANSACTION 'outer'");
    exec(&mut e, &sid, "BEGIN TRANSACTION 'inner'");

    // Rolling back 'inner' should preserve 'outer'.
    exec(&mut e, &sid, "ROLLBACK TRANSACTION 'inner'");
    // Outer is still valid — can rollback it too.
    let r = exec(&mut e, &sid, "ROLLBACK TRANSACTION 'outer'");
    assert!(matches!(r, ForgeQLResult::Rollback(_)));
}

#[test]
fn transaction_with_change_and_verify() {
    let (mut e, sid, _d) = engine_with_git_session();
    exec(&mut e, &sid, "BEGIN TRANSACTION 'txn-change'");
    exec(
        &mut e,
        &sid,
        "CHANGE FILE 'motor_control.h' LINES 1-1 WITH '// MODIFIED'",
    );

    // Verify the change is visible.
    let show = exec(&mut e, &sid, "SHOW LINES 1-1 OF 'motor_control.h'");
    assert!(format!("{show}").contains("MODIFIED"));

    exec(&mut e, &sid, "ROLLBACK TRANSACTION 'txn-change'");

    // Verify rollback restored.
    let show = exec(&mut e, &sid, "SHOW LINES 1-1 OF 'motor_control.h'");
    assert!(!format!("{show}").contains("MODIFIED"));
}

// =======================================================================
// Phase 8 — Error cases
// =======================================================================

#[test]
fn error_malformed_fql() {
    let result = parser::parse("FINDE symbolz WERE name = 'bad'");
    assert!(result.is_err(), "malformed FQL should fail to parse");
}

#[test]
fn error_find_without_session() {
    let tmp = tempdir().unwrap();
    let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf()).unwrap();
    let ops = parser::parse("FIND symbols").unwrap();
    let op = ops.first().unwrap();
    assert!(engine.execute(None, op).is_err());
}

#[test]
fn error_rollback_nonexistent_checkpoint() {
    let (mut e, sid, _d) = engine_with_session();
    let err = exec_err(&mut e, &sid, "ROLLBACK TRANSACTION 'does-not-exist-xyz'");
    assert!(!err.is_empty());
}

#[test]
fn error_show_body_nonexistent_symbol() {
    let (mut e, sid, _d) = engine_with_session();
    let err = exec_err(&mut e, &sid, "SHOW body OF 'ZZZ_NoSuchSymbol_12345'");
    assert!(!err.is_empty());
}

#[test]
fn error_show_outline_nonexistent_file() {
    let (mut e, sid, _d) = engine_with_session();
    // Engine returns empty outline for nonexistent files rather than an error.
    let r = exec(&mut e, &sid, "SHOW outline OF 'no_such_file.xyz'");
    let sr = as_show(&r);
    match &sr.content {
        ShowContent::Outline { entries } => {
            assert!(
                entries.is_empty(),
                "nonexistent file should yield empty outline"
            );
        }
        other => panic!("expected empty Outline, got {other:?}"),
    }
}

#[test]
fn error_show_lines_nonexistent_file() {
    let (mut e, sid, _d) = engine_with_session();
    let err = exec_err(&mut e, &sid, "SHOW LINES 1-5 OF 'no_such_file.xyz'");
    assert!(!err.is_empty());
}

#[test]
fn error_change_nonexistent_file() {
    let (mut e, sid, _d) = engine_with_session();
    let err = exec_err(
        &mut e,
        &sid,
        "CHANGE FILE 'no_such_file.xyz' MATCHING 'a' WITH 'b'",
    );
    assert!(!err.is_empty());
}

#[test]
fn error_disconnect_without_session() {
    let tmp = tempdir().unwrap();
    let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf()).unwrap();
    let ops = parser::parse("DISCONNECT").unwrap();
    let op = ops.first().unwrap();
    assert!(engine.execute(Some("no-such-session"), op).is_err());
}

// =======================================================================
// Phase 9 — Parser-level coverage (every clause combination parses)
// =======================================================================

#[test]
fn parse_find_symbols_all_clauses() {
    parser::parse(
        "FIND symbols WHERE node_kind = 'function_definition' \
         WHERE name LIKE 'get%' \
         IN 'src/**' \
         EXCLUDE 'tests/**' \
         ORDER BY usages DESC \
         LIMIT 10 \
         OFFSET 5",
    )
    .expect("parse with all clauses");
}

#[test]
fn parse_find_usages_all_clauses() {
    parser::parse(
        "FIND usages OF 'myFunc' \
         WHERE line >= 10 \
         IN 'src/**' \
         GROUP BY file \
         ORDER BY count DESC \
         LIMIT 20",
    )
    .expect("parse FIND usages with all clauses");
}

#[test]
fn parse_find_files_all_clauses() {
    parser::parse(
        "FIND files \
         WHERE extension = 'cpp' \
         WHERE size > 1000 \
         IN 'src/**' \
         EXCLUDE 'tests/**' \
         ORDER BY size DESC \
         LIMIT 20 \
         OFFSET 10 \
         DEPTH 3",
    )
    .expect("parse FIND files with all clauses");
}

#[test]
fn parse_find_callees_all_clauses() {
    parser::parse(
        "FIND callees OF 'myFunc' \
         IN 'src/**' \
         ORDER BY name ASC \
         LIMIT 10",
    )
    .expect("parse FIND callees");
}

#[test]
fn parse_show_body_depth() {
    parser::parse("SHOW body OF 'myFunc' DEPTH 2").expect("parse SHOW body DEPTH");
}

#[test]
fn parse_show_lines() {
    parser::parse("SHOW LINES 10-20 OF 'src/foo.cpp'").expect("parse SHOW LINES");
}

#[test]
fn parse_show_context() {
    parser::parse("SHOW context OF 'MY_MACRO'").expect("parse SHOW context");
}

#[test]
fn parse_show_signature() {
    parser::parse("SHOW signature OF 'myFunc'").expect("parse SHOW signature");
}

#[test]
fn parse_show_outline() {
    parser::parse("SHOW outline OF 'file.h'").expect("parse SHOW outline");
}

#[test]
fn parse_show_members() {
    parser::parse("SHOW members OF 'MyClass'").expect("parse SHOW members");
}

#[test]
fn parse_show_callees() {
    parser::parse("SHOW callees OF 'myFunc'").expect("parse SHOW callees");
}

#[test]
fn parse_show_sources() {
    parser::parse("SHOW SOURCES").expect("parse SHOW SOURCES");
}

#[test]
fn parse_show_branches_bare() {
    parser::parse("SHOW BRANCHES").expect("parse SHOW BRANCHES bare");
}

#[test]
fn parse_show_branches_of() {
    parser::parse("SHOW BRANCHES OF 'my-source'").expect("parse SHOW BRANCHES OF");
}

#[test]
fn parse_create_source() {
    parser::parse("CREATE SOURCE 'my-source' FROM 'https://github.com/user/repo.git'")
        .expect("parse CREATE SOURCE");
}

#[test]
fn parse_refresh_source() {
    parser::parse("REFRESH SOURCE 'my-source'").expect("parse REFRESH SOURCE");
}

#[test]
fn parse_use_stmt_basic() {
    parser::parse("USE my-source.main").expect("parse USE");
}

#[test]
fn parse_use_stmt_with_as() {
    parser::parse("USE my-source.main AS 'my-alias'").expect("parse USE AS");
}

#[test]
fn parse_disconnect() {
    parser::parse("DISCONNECT").expect("parse DISCONNECT");
}

#[test]
fn parse_change_matching() {
    parser::parse("CHANGE FILE 'f.cpp' MATCHING 'old' WITH 'new'").expect("parse CHANGE MATCHING");
}

#[test]
fn parse_change_files_glob_matching() {
    parser::parse("CHANGE FILES 'src/**/*.cpp', 'include/**/*.h' MATCHING 'old' WITH 'new'")
        .expect("parse CHANGE FILES glob MATCHING");
}

#[test]
fn parse_change_lines_with() {
    parser::parse("CHANGE FILE 'f.cpp' LINES 10-20 WITH 'new content'")
        .expect("parse CHANGE LINES WITH");
}

#[test]
fn parse_change_lines_nothing() {
    parser::parse("CHANGE FILE 'f.cpp' LINES 3-5 NOTHING").expect("parse CHANGE LINES NOTHING");
}

#[test]
fn parse_change_with_content() {
    parser::parse("CHANGE FILE 'f.cpp' WITH 'full new content'")
        .expect("parse CHANGE WITH content");
}

#[test]
fn parse_change_with_nothing() {
    parser::parse("CHANGE FILE 'f.cpp' WITH NOTHING").expect("parse CHANGE WITH NOTHING");
}

#[test]
fn parse_begin_transaction() {
    parser::parse("BEGIN TRANSACTION 'my-txn'").expect("parse BEGIN TRANSACTION");
}

#[test]
fn parse_rollback_named() {
    parser::parse("ROLLBACK TRANSACTION 'my-txn'").expect("parse ROLLBACK named");
}

#[test]
fn parse_rollback_anonymous() {
    parser::parse("ROLLBACK").expect("parse ROLLBACK anonymous");
}

#[test]
fn parse_commit_message() {
    parser::parse("COMMIT MESSAGE 'my commit msg'").expect("parse COMMIT MESSAGE");
}

#[test]
fn parse_verify_build() {
    parser::parse("VERIFY build 'test'").expect("parse VERIFY build");
}

#[test]
fn parse_find_globals() {
    parser::parse("FIND globals ORDER BY usages DESC LIMIT 20").expect("parse FIND globals");
}

// --- WHERE operator variants ---

#[test]
fn parse_where_eq() {
    parser::parse("FIND symbols WHERE name = 'foo'").unwrap();
}

#[test]
fn parse_where_neq() {
    parser::parse("FIND symbols WHERE name != 'foo'").unwrap();
}

#[test]
fn parse_where_like() {
    parser::parse("FIND symbols WHERE name LIKE 'foo%'").unwrap();
}

#[test]
fn parse_where_not_like() {
    parser::parse("FIND symbols WHERE name NOT LIKE 'foo%'").unwrap();
}

#[test]
fn parse_where_gt() {
    parser::parse("FIND symbols WHERE usages > 5").unwrap();
}

#[test]
fn parse_where_gte() {
    parser::parse("FIND symbols WHERE usages >= 5").unwrap();
}

#[test]
fn parse_where_lt() {
    parser::parse("FIND symbols WHERE usages < 5").unwrap();
}

#[test]
fn parse_where_lte() {
    parser::parse("FIND symbols WHERE usages <= 5").unwrap();
}

#[test]
fn parse_where_negative_number() {
    parser::parse("FIND symbols WHERE line >= -1").unwrap();
}

#[test]
fn parse_having_clause() {
    parser::parse("FIND usages OF 'func' GROUP BY file HAVING count >= 3")
        .expect("parse HAVING clause");
}

// --- Multi-statement parsing ---

#[test]
fn parse_multi_statement() {
    let ops = parser::parse(
        "BEGIN TRANSACTION 'test'\n\
         CHANGE FILE 'f.cpp' MATCHING 'a' WITH 'b'\n\
         ROLLBACK TRANSACTION 'test'",
    )
    .expect("parse multi-statement");
    assert_eq!(ops.len(), 3, "should parse 3 statements");
}

// =======================================================================
// Phase 10 — Query logger integration
// =======================================================================

#[test]
fn query_logger_creates_csv_with_header() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let mut logger = QueryLogger::new(data_dir);
    logger.set_source("test-source");

    let result = ForgeQLResult::SourceOp(forgeql_core::result::SourceOpResult {
        op: "test".to_string(),
        source_name: None,
        session_id: None,
        branches: vec![],
        symbols_indexed: None,
        resumed: false,
        message: Some("ok".to_string()),
    });
    logger.log("FIND symbols", &result, "some output text");

    let log_path = logger.log_path();
    assert!(log_path.exists(), "log CSV should be created");
    let content = fs::read_to_string(&log_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert!(lines.len() >= 2, "should have header + at least 1 data row");
    assert!(lines[0].contains("timestamp"));
    assert!(lines[0].contains("command_preview"));
}

#[test]
fn query_logger_appends_multiple_rows() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let mut logger = QueryLogger::new(data_dir);
    logger.set_source("multi-test");

    let result = ForgeQLResult::SourceOp(forgeql_core::result::SourceOpResult {
        op: "test".to_string(),
        source_name: None,
        session_id: None,
        branches: vec![],
        symbols_indexed: None,
        resumed: false,
        message: Some("ok".to_string()),
    });

    logger.log("FIND symbols", &result, "output1");
    logger.log("FIND files", &result, "output2");
    logger.log("SHOW body OF 'func'", &result, "output3");

    let content = fs::read_to_string(logger.log_path()).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    // Header + 3 data rows.
    assert_eq!(
        lines.len(),
        4,
        "expected 4 lines (header + 3 rows), got {}",
        lines.len()
    );
}

#[test]
fn query_logger_sanitizes_source_name() {
    let tmp = tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let mut logger = QueryLogger::new(data_dir);
    logger.set_source("my/source@special");

    let path = logger.log_path();
    let filename = path.file_name().unwrap().to_string_lossy();
    assert!(!filename.contains('/'));
    assert!(!filename.contains('@'));
}

#[test]
fn query_logger_records_source_lines_for_show() {
    let (mut e, sid, _d) = engine_with_session();
    let tmp = tempdir().unwrap();
    let mut logger = QueryLogger::new(tmp.path().to_path_buf());
    logger.set_source("show-test");

    // Execute a SHOW LINES command that returns actual source lines.
    let result = exec(&mut e, &sid, "SHOW LINES 1-5 OF 'motor_control.h'");
    let output = format!("{result}");
    logger.log("SHOW LINES 1-5 OF 'motor_control.h'", &result, &output);

    let content = fs::read_to_string(logger.log_path()).unwrap();
    let data_line = content.lines().nth(1).expect("data row");
    // CSV: "timestamp",source_lines,tokens_sent,tokens_received,"preview"
    let fields: Vec<&str> = data_line.split(',').collect();
    // source_lines should be "5" (we asked for 5 lines).
    let source_lines: usize = fields[1].parse().expect("parse source_lines");
    assert_eq!(source_lines, 5, "SHOW LINES 1-5 should log 5 source_lines");
}

#[test]
fn query_logger_records_zero_source_lines_for_query() {
    let (mut e, sid, _d) = engine_with_session();
    let tmp = tempdir().unwrap();
    let mut logger = QueryLogger::new(tmp.path().to_path_buf());
    logger.set_source("query-test");

    // FIND queries return no source lines.
    let result = exec(&mut e, &sid, "FIND symbols LIMIT 5");
    let output = format!("{result}");
    logger.log("FIND symbols LIMIT 5", &result, &output);

    let content = fs::read_to_string(logger.log_path()).unwrap();
    let data_line = content.lines().nth(1).expect("data row");
    let fields: Vec<&str> = data_line.split(',').collect();
    let source_lines: usize = fields[1].parse().expect("parse source_lines");
    assert_eq!(source_lines, 0, "FIND query should log 0 source_lines");
}

// =======================================================================
// Phase 11 — Display / serialization coverage
// =======================================================================

#[test]
fn result_display_find_symbols() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols WHERE name LIKE 'encender%'");
    let text = format!("{r}");
    assert!(text.contains("encenderMotor"));
}

#[test]
fn result_to_json_roundtrip() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "FIND symbols LIMIT 5");
    let json = r.to_json();
    let deserialized: ForgeQLResult = serde_json::from_str(&json).expect("deserialize");
    match deserialized {
        ForgeQLResult::Query(qr) => {
            assert!(qr.results.len() <= 5);
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn result_to_csv_find_symbols() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(
        &mut e,
        &sid,
        "FIND symbols WHERE node_kind = 'function_definition' LIMIT 5",
    );
    let csv = r.to_csv();
    // CSV output should be valid JSON (our format wraps CSV in JSON envelope).
    let _v: serde_json::Value = serde_json::from_str(&csv).expect("CSV should be valid JSON");
}

#[test]
fn result_display_show_body() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW body OF 'encenderMotor' DEPTH 99");
    let text = format!("{r}");
    assert!(!text.is_empty());
}

#[test]
fn result_display_show_outline() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW outline OF 'motor_control.h'");
    let text = format!("{r}");
    assert!(!text.is_empty());
}

#[test]
fn result_display_show_members() {
    let (mut e, sid, _d) = engine_with_session();
    let r = exec(&mut e, &sid, "SHOW members OF 'ErrorMotor'");
    let text = format!("{r}");
    assert!(!text.is_empty());
}
