//! Unit tests for `ForgeQLEngine` and its helper / converter functions.

use super::*;
use crate::ast::lang::CppLanguageInline;
use crate::ir::{Backend, Clauses};
use crate::session::SessionCoords;
use crate::transforms::TransformPlan;

fn make_registry() -> Arc<LanguageRegistry> {
    Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]))
}

#[cfg(feature = "test-helpers")]
#[test]
fn generate_session_id_starts_with_s() {
    let id = generate_session_id();
    assert!(
        id.starts_with('s'),
        "test helper session ID must start with 's': {id}"
    );
}

#[cfg(feature = "test-helpers")]
#[test]
fn generate_session_id_unique() {
    let id1 = generate_session_id();
    // Wait 1 ms to ensure different timestamp.
    std::thread::sleep(std::time::Duration::from_millis(1));
    let id2 = generate_session_id();
    assert_ne!(
        id1, id2,
        "consecutive test-helper session IDs should differ"
    );
}

#[test]
fn require_session_id_empty_fails() {
    let result = require_session_id(None);
    assert!(result.is_err());
    let result = require_session_id(Some(""));
    assert!(result.is_err());
}

#[test]
fn require_session_id_valid() {
    let result = require_session_id(Some("s12345"));
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "s12345");
}

#[test]
fn mutation_op_name_mapping() {
    let change = ForgeQLIR::ChangeContent {
        files: vec!["f.cpp".into()],
        target: crate::ir::ChangeTarget::Delete,
        clauses: Clauses::default(),
    };
    assert_eq!(mutation_op_name(&change), "change_content");

    let unknown = ForgeQLIR::ShowSources;
    assert_eq!(mutation_op_name(&unknown), "unknown_mutation");
}

#[test]
fn engine_new_creates_worktrees_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_path_buf();
    let engine = ForgeQLEngine::new(data_dir.clone(), make_registry()).unwrap();
    assert!(SessionCoords::worktrees_root(&data_dir).exists());
    assert_eq!(engine.session_count(), 0);
    assert_eq!(engine.source_count(), 0);
    assert_eq!(engine.commands_served(), 0);
}

#[test]
fn engine_show_sources_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
    let result = engine.execute(None, &ForgeQLIR::ShowSources).unwrap();
    match result {
        ForgeQLResult::Query(qr) => {
            assert_eq!(qr.op, "show_sources");
            assert!(qr.results.is_empty());
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

#[test]
fn engine_show_branches_requires_session() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
    let result = engine.execute(None, &ForgeQLIR::ShowBranches);
    assert!(result.is_err());
}

// (engine_disconnect_without_session_fails test removed — DISCONNECT eliminated)

// (engine_disconnect_unknown_session_fails removed — DISCONNECT command eliminated)

#[test]
fn engine_find_symbols_without_session_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
    let op = ForgeQLIR::FindSymbols {
        backend: Backend::default(),
        clauses: Clauses::default(),
    };
    let result = engine.execute(None, &op);
    assert!(result.is_err());
}

#[test]
fn convert_suggestions_from_empty_plan() {
    let plan = TransformPlan::default();
    let suggestions = convert_suggestions(&plan);
    assert!(suggestions.is_empty());
}

/// `FIND globals` now maps to `FIND symbols WHERE node_kind = 'declaration'`
/// and correctly returns variable declarations from the index.
///
/// `motor_control.cpp` declares several `static` variables at file scope
/// (`motorPrincipal`, `motorSecundario`, `gCallbackEncendido`, `kMotorLabel`).
/// These are `declaration` nodes in the tree-sitter AST and must now
/// appear in results.
#[cfg(feature = "test-helpers")]
#[test]
fn find_globals_returns_declaration_nodes() {
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    let fixtures = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/fixtures");
    fs::copy(
        fixtures.join("motor_control.h"),
        tmp.path().join("motor_control.h"),
    )
    .unwrap();
    fs::copy(
        fixtures.join("motor_control.cpp"),
        tmp.path().join("motor_control.cpp"),
    )
    .unwrap();

    let data_dir = tmp.path().join("data");
    let mut engine = ForgeQLEngine::new(data_dir, make_registry()).unwrap();
    let session_id = engine.register_local_session(tmp.path()).unwrap();

    // FIND globals → FIND symbols WHERE fql_kind = 'variable' WHERE scope = 'file'
    let op = crate::parser::parse("FIND globals LIMIT 200").unwrap();
    let result = engine.execute(Some(&session_id), &op[0]).unwrap();
    let results = match result {
        ForgeQLResult::Query(qr) => qr.results,
        other => panic!("expected Query, got: {other:?}"),
    };

    // All returned rows must be file-scope variable nodes.
    for r in &results {
        assert_eq!(
            r.fql_kind.as_deref(),
            Some("variable"),
            "FIND globals must only return variable nodes, got {:?} for '{}'",
            r.fql_kind,
            r.name,
        );
        assert_eq!(
            r.fields.get("scope").map(String::as_str),
            Some("file"),
            "FIND globals must only return file-scope declarations, got scope={:?} for '{}'",
            r.fields.get("scope"),
            r.name,
        );
    }

    // The known file-scope static variables should appear.
    let names: Vec<&str> = results.iter().map(|r| r.name.as_str()).collect();
    for expected in [
        "motorPrincipal",
        "motorSecundario",
        "gCallbackEncendido",
        "kMotorLabel",
    ] {
        assert!(
            names.contains(&expected),
            "declaration '{expected}' must appear in FIND globals; got: {names:?}"
        );
    }

    // All file-scope declarations in the fixture are static.
    for r in &results {
        assert_eq!(
            r.fields.get("storage").map(String::as_str),
            Some("static"),
            "expected storage='static' for '{}'; got {:?}",
            r.name,
            r.fields.get("storage"),
        );
    }

    // Local variables must NOT appear.
    for local in ["vel", "velocidad"] {
        assert!(
            !names.contains(&local),
            "local variable '{local}' must NOT appear in FIND globals; got: {names:?}"
        );
    }
}

/// `FIND globals WHERE node_kind = 'enum_specifier'` must return zero results,
/// not silently drop the `node_kind` predicate and return all variables.
///
/// Regression: the `non_usages_preds` filter incorrectly stripped the
/// `node_kind` predicate when `fql_kind_exact` was also present, because
/// the `kind_exact.is_some()` guard didn't account for the index-selection
/// priority (`fql_kind` wins over `node_kind`).
#[cfg(feature = "test-helpers")]
#[test]
fn find_globals_with_conflicting_node_kind_returns_empty() {
    use std::fs;

    let tmp = tempfile::tempdir().unwrap();
    let fixtures = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/fixtures");
    fs::copy(
        fixtures.join("motor_control.h"),
        tmp.path().join("motor_control.h"),
    )
    .unwrap();
    fs::copy(
        fixtures.join("motor_control.cpp"),
        tmp.path().join("motor_control.cpp"),
    )
    .unwrap();

    let data_dir = tmp.path().join("data");
    let mut engine = ForgeQLEngine::new(data_dir, make_registry()).unwrap();
    let session_id = engine.register_local_session(tmp.path()).unwrap();

    // FIND globals adds fql_kind='variable' + scope='file' implicitly.
    // Adding WHERE node_kind = 'enum_specifier' must further filter,
    // not be silently dropped.
    let op =
        crate::parser::parse("FIND globals WHERE node_kind = 'enum_specifier' LIMIT 200").unwrap();
    let result = engine.execute(Some(&session_id), &op[0]).unwrap();
    let results = match result {
        ForgeQLResult::Query(qr) => qr.results,
        other => panic!("expected Query, got: {other:?}"),
    };

    assert!(
        results.is_empty(),
        "FIND globals WHERE node_kind = 'enum_specifier' should return 0 results \
         (no variable has node_kind='enum_specifier'), got {} results: {:?}",
        results.len(),
        results.iter().map(|r| &r.name).collect::<Vec<_>>(),
    );
}
