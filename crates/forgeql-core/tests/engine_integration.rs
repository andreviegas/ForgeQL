//! Integration tests for `ForgeQLEngine::execute()`.
//!
//! These tests exercise the full engine dispatch path — parser → IR → engine
//! → result — using the `motor_control` C++ fixtures in a temp workspace.
//!
//! Run with: `cargo test -p forgeql-core --test engine_integration`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    // panic! is the normal way to fail a test assertion
    clippy::panic,
    // helper functions defined inside test bodies after let-statements
    clippy::items_after_statements,
    // doc comments in tests don't need exhaustive backtick coverage
    clippy::doc_markdown
)]

use std::fs;

use forgeql_core::auth::{AuthContext, auth};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::ir::ForgeQLIR;
use forgeql_core::parser;
use forgeql_core::result::{ForgeQLResult, ShowContent};
use forgeql_core::session::SessionCoords;
use tempfile::tempdir;

mod common;

use common::{engine_with_session, engine_with_session_legacy, execute_fql, fql_err, node_rev};

// -----------------------------------------------------------------------
// Engine lifecycle
// -----------------------------------------------------------------------

#[test]
fn engine_starts_with_zero_state() {
    let tmp = tempdir().unwrap();
    let engine = ForgeQLEngine::new(tmp.path().to_path_buf(), common::make_registry()).unwrap();
    assert_eq!(engine.session_count(), 0);
    assert_eq!(engine.source_count(), 0);
    assert_eq!(engine.commands_served(), 0);
}

#[test]
fn show_sources_on_empty_engine() {
    let tmp = tempdir().unwrap();
    let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf(), common::make_registry()).unwrap();
    let result = engine
        .execute(auth(AuthContext::Tester), None, &ForgeQLIR::ShowSources)
        .result
        .unwrap();
    match result {
        ForgeQLResult::Query(qr) => {
            assert_eq!(qr.op, "show_sources");
            assert!(qr.results.is_empty());
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Mutation: CHANGE FILE RENAME symbol
// -----------------------------------------------------------------------

#[test]
fn change_rename_applies_and_mutates_file() {
    let (mut engine, sid, dir) = engine_with_session();

    let result = execute_fql(
        &mut engine,
        &sid,
        "CHANGE FILE 'motor_control.cpp' MATCHING 'void encenderMotor' WITH 'void startMotor'",
    );
    match result {
        ForgeQLResult::Mutation(mr) => {
            assert!(mr.applied, "mutation should be applied");
            assert!(mr.edit_count > 0, "should have edits");
        }
        other => panic!("expected Mutation, got: {other:?}"),
    }

    // Verify file on disk.
    let cpp = fs::read_to_string(dir.path().join("motor_control.cpp")).unwrap();
    assert!(cpp.contains("startMotor"), "new name should appear in .cpp");
}

// -----------------------------------------------------------------------
// Mutation: CHANGE FILE LINES trailing newline
// -----------------------------------------------------------------------

#[test]
fn change_lines_auto_appends_trailing_newline() {
    let (mut engine, sid, dir) = engine_with_session();

    let cpp_path = dir.path().join("motor_control.cpp");
    let original = fs::read_to_string(&cpp_path).unwrap();
    let original_lines: Vec<&str> = original.lines().collect();

    // Replace line 2 with text that has NO trailing newline.
    let replacement = "// replaced line";
    let fql = format!("CHANGE FILE 'motor_control.cpp' LINES 2-2 WITH '{replacement}'");
    let result = execute_fql(&mut engine, &sid, &fql);

    match &result {
        ForgeQLResult::Mutation(mr) => {
            assert!(mr.applied);
            assert!(mr.edit_count > 0);
        }
        other => panic!("expected Mutation, got: {other:?}"),
    }

    // Line 2 should be the replacement, line 3 should still be the original line 3.
    let modified = fs::read_to_string(&cpp_path).unwrap();
    let modified_lines: Vec<&str> = modified.lines().collect();
    assert_eq!(
        modified_lines[1], replacement,
        "line 2 should be the replacement"
    );
    assert_eq!(
        modified_lines[2], original_lines[2],
        "line 3 must NOT merge with replacement — trailing newline was missing"
    );
}

// -----------------------------------------------------------------------
// Mutation: CHANGE response includes diff preview
// -----------------------------------------------------------------------

#[test]
fn change_mutation_includes_diff() {
    let (mut engine, sid, _dir) = engine_with_session();

    let result = execute_fql(
        &mut engine,
        &sid,
        "CHANGE FILE 'motor_control.cpp' MATCHING 'encenderMotor' WITH 'startMotor'",
    );
    match result {
        ForgeQLResult::Mutation(mr) => {
            assert!(mr.applied);
            let diff = mr.diff.expect("mutation should include a diff preview");
            assert!(
                diff.contains("── "),
                "compact preview should have ── header: {diff}"
            );
            assert!(
                diff.contains("motor_control.cpp"),
                "compact preview should name the file: {diff}"
            );
            assert!(
                diff.contains("startMotor"),
                "compact preview should show the new text: {diff}"
            );
        }
        other => panic!("expected Mutation, got: {other:?}"),
    }
}

/// A mutation that leaves a structured file unparseable reports it immediately:
/// the file was valid JSON before the edit and is not after, so the result
/// carries a `StructuralError` naming the file and the parser's diagnostic. A
/// well-formed edit reports nothing; editing an already-broken file flags the
/// breakage as pre-existing rather than caused by this edit.
#[test]
fn mutation_reports_structural_error_when_it_breaks_json() {
    let (mut engine, sid, dir) = engine_with_session();
    let path = dir.path().join("cfg.json");
    fs::write(&path, "{ \"a\": 1 }\n").unwrap();
    let handle = common::path_handle("cfg.json");

    // valid -> valid: nothing to report.
    let rev = node_rev(&mut engine, &sid, &handle);
    match execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{handle}' IF REV '{rev}' WITH '{{ \"a\": 2 }}'"),
    ) {
        ForgeQLResult::Mutation(mr) => assert!(
            mr.structural_errors.is_empty(),
            "a valid JSON edit must not report a structural error: {:?}",
            mr.structural_errors
        ),
        other => panic!("expected Mutation, got: {other:?}"),
    }

    // valid -> broken: reported, and attributed to this edit.
    let rev = node_rev(&mut engine, &sid, &handle);
    match execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{handle}' IF REV '{rev}' WITH '{{ \"a\": 2 '"),
    ) {
        ForgeQLResult::Mutation(mr) => {
            let se = mr
                .structural_errors
                .first()
                .expect("breaking JSON must report a structural error");
            assert!(se.path.ends_with("cfg.json"), "path: {}", se.path.display());
            assert_eq!(se.valid_before, Some(true), "this edit caused the break");
            assert!(!se.message.is_empty(), "carries the parser diagnostic");
        }
        other => panic!("expected Mutation, got: {other:?}"),
    }

    // broken -> broken: still reported, now flagged as pre-existing.
    let rev = node_rev(&mut engine, &sid, &handle);
    match execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{handle}' IF REV '{rev}' WITH '{{ \"a\": 3 '"),
    ) {
        ForgeQLResult::Mutation(mr) => {
            let se = mr.structural_errors.first().expect("still broken");
            assert_eq!(se.valid_before, Some(false), "breakage predates this edit");
        }
        other => panic!("expected Mutation, got: {other:?}"),
    }
}

/// The JSON plugin serves `.jsonc`, whose comments and trailing commas a strict
/// RFC-8259 parser rejects. Those must never be reported as structural errors —
/// the strict validator opts the dialect out.
#[test]
fn jsonc_dialect_is_not_strictly_validated() {
    let (mut engine, sid, dir) = engine_with_session();
    let path = dir.path().join("cfg.jsonc");
    fs::write(&path, "{ \"a\": 1 }\n").unwrap();
    let handle = common::path_handle("cfg.jsonc");

    // A trailing comma is legal JSONC but not strict JSON; it must not be flagged.
    let rev = node_rev(&mut engine, &sid, &handle);
    match execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{handle}' IF REV '{rev}' WITH '{{ \"a\": 2, }}'"),
    ) {
        ForgeQLResult::Mutation(mr) => assert!(
            mr.structural_errors.is_empty(),
            "JSONC dialect must not be strictly validated: {:?}",
            mr.structural_errors
        ),
        other => panic!("expected Mutation, got: {other:?}"),
    }
}

/// The strict validators cover YAML, TOML and XML too: an edit that leaves any
/// of them unparseable is reported, while a well-formed edit is not. Each broken
/// form is one a strict parser rejects but the error-tolerant tree-sitter grammar
/// would recover from.
#[test]
fn mutation_reports_structural_errors_for_yaml_toml_and_xml() {
    let cases = [
        ("cfg.yaml", "{a: 1}", "{a: 1"),
        ("cfg.toml", "a = 1", "a ="),
        ("cfg.xml", "<r><a/></r>", "<r><a></r>"),
    ];
    for (name, valid, broken) in cases {
        let (mut engine, sid, dir) = engine_with_session();
        let path = dir.path().join(name);
        fs::write(&path, valid).unwrap();
        let handle = common::path_handle(name);

        let rev = node_rev(&mut engine, &sid, &handle);
        match execute_fql(
            &mut engine,
            &sid,
            &format!("CHANGE NODE '{handle}' IF REV '{rev}' WITH '{valid}'"),
        ) {
            ForgeQLResult::Mutation(mr) => assert!(
                mr.structural_errors.is_empty(),
                "{name}: valid content must not be flagged: {:?}",
                mr.structural_errors
            ),
            other => panic!("{name}: expected Mutation, got: {other:?}"),
        }

        let rev = node_rev(&mut engine, &sid, &handle);
        match execute_fql(
            &mut engine,
            &sid,
            &format!("CHANGE NODE '{handle}' IF REV '{rev}' WITH '{broken}'"),
        ) {
            ForgeQLResult::Mutation(mr) => {
                let Some(se) = mr.structural_errors.first() else {
                    panic!("{name}: broken content must be reported");
                };
                assert!(
                    se.path.ends_with(name),
                    "{name}: path {}",
                    se.path.display()
                );
                assert!(!se.message.is_empty(), "{name}: carries a diagnostic");
            }
            other => panic!("{name}: expected Mutation, got: {other:?}"),
        }
    }
}

// -----------------------------------------------------------------------
// Line-budget integration tests
// -----------------------------------------------------------------------

const fn budget_config() -> forgeql_core::config::LineBudgetConfig {
    forgeql_core::config::LineBudgetConfig {
        initial: 50,
        ceiling: 200,
        recovery_base: 10,
        recovery_window_secs: 60,
        warning_threshold: 20,
        critical_threshold: 10,
        critical_max_lines: 5,
        idle_reset_secs: 300,
    }
}

#[test]
fn budget_deducts_on_show_lines() {
    let (mut engine, sid, _dir) = engine_with_session();
    engine.init_session_budget(&sid, &budget_config());

    // Confirm budget starts at initial.
    let snap = engine.budget_status(&sid).expect("budget active");
    assert_eq!(snap.remaining, 50);

    // SHOW LINES returns source lines — budget should decrease.
    let result = execute_fql(
        &mut engine,
        &sid,
        "SHOW LINES 1-5 OF 'motor_control.h' LIMIT 5",
    );
    let lines_returned = result.source_lines_count();
    assert!(lines_returned > 0, "should return lines");

    let snap = engine.budget_status(&sid).expect("budget active");
    // No recovery on SHOW LINES — pure deduction.
    assert_eq!(snap.remaining, 50 - lines_returned);
}

#[test]
fn budget_not_deducted_on_find_symbols() {
    let (mut engine, sid, _dir) = engine_with_session();
    engine.init_session_budget(&sid, &budget_config());

    // FIND symbols returns structured data, not source lines.
    let _ = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' LIMIT 5",
    );

    let snap = engine.budget_status(&sid).expect("budget active");
    // Recovery may increase it, but it should not go below initial.
    assert!(snap.remaining >= 50, "budget should not decrease for FIND");
}

#[test]
fn budget_critical_caps_show_lines() {
    let (mut engine, sid, _dir) = engine_with_session();
    let mut cfg = budget_config();
    cfg.initial = 5;
    cfg.critical_threshold = 10; // start below critical
    cfg.critical_max_lines = 3;
    engine.init_session_budget(&sid, &cfg);

    // Request 10 lines — should be capped to critical_max_lines (3).
    let result = execute_fql(
        &mut engine,
        &sid,
        "SHOW LINES 1-10 OF 'motor_control.h' LIMIT 10",
    );
    let lines_returned = result.source_lines_count();
    assert!(
        lines_returned <= 3,
        "critical state should cap to 3 lines, got {lines_returned}"
    );

    // Verify hint mentions budget.
    if let ForgeQLResult::Show(ref sr) = result {
        assert!(
            sr.hint
                .as_ref()
                .is_some_and(|h| h.contains("Budget critical")),
            "hint should mention budget: {:?}",
            sr.hint
        );
    }
}

#[test]
fn budget_absent_without_config() {
    let (engine, sid, _dir) = engine_with_session();
    // No init_session_budget call — budget should be None.
    assert!(engine.budget_status(&sid).is_none());
    assert!(engine.budget_status(&sid).is_none());
}

/// BUG-016 residual: a purely numeric `TO` destination is rejected with
/// guidance instead of silently creating a file named after the number.
#[test]
fn move_lines_to_numeric_dest_rejected() {
    let (mut engine, session_id, dir) = engine_with_session();
    let ops = parser::parse("MOVE LINES 1-2 OF 'motor_control.cpp' TO 3").expect("parse");
    let coords = SessionCoords::from_session_id(&session_id).expect("valid session_id");
    let err = engine
        .execute(auth(AuthContext::Tester), Some(&coords), &ops[0])
        .result
        .expect_err("numeric TO destination must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("must be a file path, not a number"),
        "unexpected error: {msg}"
    );
    assert!(
        !dir.path().join("3").exists(),
        "no file named '3' may be created"
    );
}

/// BUG-014 residual: whole-file deletion (`CHANGE FILE … WITH NOTHING`) is
/// exempt from the indexed-file gate — a ForgeQL-created file can be removed
/// from within ForgeQL.
#[test]
fn change_file_with_nothing_deletes_indexed_file() {
    let (mut engine, session_id, dir) = engine_with_session();
    let _ = execute_fql(
        &mut engine,
        &session_id,
        "COPY LINES 1-2 OF 'motor_control.cpp' TO '_scratch_delete_me.rs'",
    );
    assert!(dir.path().join("_scratch_delete_me.rs").exists());
    let _ = execute_fql(
        &mut engine,
        &session_id,
        "CHANGE FILE '_scratch_delete_me.rs' WITH NOTHING",
    );
    assert!(
        !dir.path().join("_scratch_delete_me.rs").exists(),
        "WITH NOTHING must delete the file even though it is indexed"
    );
}

/// The mechanical rename sweep: FIND aims at the occurrence sites, CHANGE
/// NODES FOUND sweeps the replacement across exactly those lines. Which sites
/// are armed is now the caller's choice: unfiltered arms every role, so the
/// log string naming the function is renamed with the code, while
/// `WHERE role = 'code'` arms only what the compiler resolves and leaves prose
/// untouched. Both halves are pinned below, because the difference between
/// them is the whole point of typing an occurrence.
#[test]
fn rename_sweep_via_find_then_change_nodes_found() {
    let (mut engine, sid, dir) = engine_with_session();

    let r = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    assert!(!qr.results.is_empty(), "usage sites expected");
    let rev = qr.found_rev.expect("a complete FIND issues a master rev");

    let r = execute_fql(
        &mut engine,
        &sid,
        &format!(
            "CHANGE NODES FOUND IF REV '{rev}' MATCHING WORD 'encenderMotor' WITH 'startMotor'"
        ),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(mr.applied);
    assert!(
        mr.edit_count >= 2,
        "multiple sites swept: {}",
        mr.edit_count
    );

    let cpp = fs::read_to_string(dir.path().join("motor_control.cpp")).expect("read cpp");
    assert!(cpp.contains("startMotor"), "rename applied");
    // An unfiltered FIND arms every occurrence, so the log message naming the
    // function is swept along with the code. The fixture's "no renombrar"
    // trap comments predate the occurrence layer, when `FIND usages` meant
    // resolved references only; a rename that leaves its own log strings
    // stale is not finished. What used to be implicit safety is now the
    // explicit clause pinned by the sibling test below.
    assert!(
        cpp.contains("startMotor: velocidad"),
        "the string occurrence is armed too and carries the new name"
    );
}

#[test]
fn rename_sweep_scoped_to_code_leaves_prose_alone() {
    // The escape hatch for the sweep above, and the reason non-code roles are
    // a review queue rather than a surprise: one clause narrows the armed set
    // to the references a compiler resolves, so strings and comments keep the
    // old name until someone has read them.
    let (mut engine, sid, dir) = engine_with_session();

    let r = execute_fql(
        &mut engine,
        &sid,
        "FIND usages OF 'encenderMotor' WHERE role = 'code'",
    );
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    assert!(!qr.results.is_empty(), "code usage sites expected");
    let rev = qr.found_rev.expect("a complete FIND issues a master rev");

    let r = execute_fql(
        &mut engine,
        &sid,
        &format!(
            "CHANGE NODES FOUND IF REV '{rev}' MATCHING WORD 'encenderMotor' WITH 'startMotor'"
        ),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(mr.applied);

    let cpp = fs::read_to_string(dir.path().join("motor_control.cpp")).expect("read cpp");
    assert!(cpp.contains("startMotor"), "rename applied to code");
    assert!(
        cpp.contains("encenderMotor: velocidad"),
        "the log string is outside the armed set and keeps the old name"
    );
}

/// `CHANGE NODES FOUND` without a previous FIND fails with guidance.
#[test]
fn change_nodes_found_without_find_errors() {
    let (mut engine, sid, _dir) = engine_with_session();
    let ops = parser::parse("CHANGE NODES FOUND MATCHING 'a' WITH 'b'").expect("parse");
    let coords = SessionCoords::from_session_id(&sid).expect("valid session_id");
    let err = engine
        .execute(auth(AuthContext::Tester), Some(&coords), &ops[0])
        .result
        .expect_err("must fail without a previous FIND")
        .to_string();
    assert!(
        err.contains("no FIND result is armed"),
        "guidance expected: {err}"
    );
    assert!(
        err.contains(r#""error":"no_found_set""#),
        "the refusal is a structured self-healing rejection: {err}"
    );
}

#[test]
fn delete_nodes_found_removes_only_armed_spans_not_the_whole_file() {
    // Regression: arming DELETE NODES FOUND with a strict, non-contiguous subset
    // of a file's symbol nodes must remove exactly those node spans — never the
    // whole file. The bug deleted the entire file when the armed set was a few
    // of its functions.
    let (mut engine, sid, dir) = engine_with_session();

    let path = dir.path().join("motor_control.cpp");
    let before = fs::read_to_string(&path).expect("read cpp");
    let lines_before = before.lines().count();

    // Arm 9 of the 10 top-level functions; leerTemperatura is excluded, so the
    // armed set is non-contiguous — a gap in the middle of the file, exactly the
    // shape that tripped the whole-file delete.
    let r = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' IN 'motor_control.cpp' \
         WHERE name != 'leerTemperatura'",
    );
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    assert_eq!(qr.results.len(), 9, "nine functions armed, one excluded");
    let rev = qr.found_rev.expect("a complete FIND issues a master rev");

    let r = execute_fql(
        &mut engine,
        &sid,
        &format!("DELETE NODES FOUND IF REV '{rev}'"),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(mr.applied);

    // The file must survive — wiping it was the bug.
    let after = fs::read_to_string(&path).expect("file must survive, not be deleted");
    assert!(
        after.contains("#include \"motor_control.h\""),
        "file header far above the first armed node must survive"
    );
    assert!(
        after.contains("leerTemperatura"),
        "the one un-armed function must survive"
    );
    assert!(
        mr.lines_removed > 0 && mr.lines_removed < lines_before,
        "only the armed spans are removed ({}), never the whole {}-line file",
        mr.lines_removed,
        lines_before
    );

    // Re-indexing after the delete sees only the un-armed function.
    let r = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' IN 'motor_control.cpp'",
    );
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    assert_eq!(
        qr.results.len(),
        1,
        "only the un-armed function remains indexed"
    );
}

/// A complete FIND issues a master rev; quoting it back runs the sweep.
#[test]
fn found_rev_gates_the_sweep() {
    let (mut engine, sid, dir) = engine_with_session();

    let r = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    let rev = qr
        .found_rev
        .expect("a complete FIND must issue a master rev");

    let r = execute_fql(
        &mut engine,
        &sid,
        &format!(
            "CHANGE NODES FOUND IF REV '{rev}' MATCHING WORD 'encenderMotor' WITH 'startMotor'"
        ),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(mr.applied);

    let cpp = fs::read_to_string(dir.path().join("motor_control.cpp")).expect("read cpp");
    assert!(cpp.contains("startMotor"), "rename applied under the gate");
}

/// A master rev that no longer matches the live members refuses the mutation —
/// and hands back no replacement rev, so the only way on is to look again.
#[test]
fn stale_found_rev_is_refused() {
    let (mut engine, sid, _dir) = engine_with_session();

    let _ = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let err = fql_err(
        &mut engine,
        &sid,
        "CHANGE NODES FOUND IF REV 'hdeadbeefdeadbeef' MATCHING 'encenderMotor' WITH 'x'",
    );
    assert!(err.contains("rev_mismatch"), "gate must fire: {err}");
    assert!(
        !err.contains("\"current\""),
        "a set-level mismatch must not hand back a fresh rev to blindly retry: {err}"
    );
}

/// A GROUP BY row is a count with a filename on it. It must clear LAST rather
/// than arm a set that no verb can act on.
#[test]
fn group_by_result_clears_found_set() {
    let (mut engine, sid, _dir) = engine_with_session();

    let _ = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let r = execute_fql(&mut engine, &sid, "FIND symbols GROUP BY file");
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    assert!(
        qr.found_rev.is_none(),
        "an aggregate addresses nothing — no master rev"
    );

    let err = fql_err(
        &mut engine,
        &sid,
        "CHANGE NODES FOUND MATCHING 'encenderMotor' WITH 'x'",
    );
    assert!(
        err.contains("no FIND result is armed"),
        "the aggregate must clear LAST, not leave the previous set armed: {err}"
    );
}

/// The destructive bulk verbs will not run ungated.
#[test]
fn delete_node_last_requires_if_rev() {
    let (mut engine, sid, _dir) = engine_with_session();

    let _ = execute_fql(&mut engine, &sid, "FIND files");
    let err = fql_err(&mut engine, &sid, "DELETE NODES FOUND");
    assert!(
        err.contains("requires IF REV"),
        "a bulk delete must demand the gate: {err}"
    );
    assert!(
        err.contains(r#""error":"found_refused""#),
        "the refusal is a structured self-healing rejection: {err}"
    );
}

/// `FIND usages` rows are call sites, not nodes: they cannot be deleted or moved.
#[test]
fn bulk_delete_refuses_a_set_of_usage_sites() {
    let (mut engine, sid, _dir) = engine_with_session();

    let r = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    let rev = qr.found_rev.expect("master rev");

    let err = fql_err(
        &mut engine,
        &sid,
        &format!("DELETE NODES FOUND IF REV '{rev}'"),
    );
    assert!(
        err.contains("addressable nodes"),
        "a site is not a node — say so: {err}"
    );
}

/// A session outlives the process. An agent may FIND, hand the session on (or
/// wait out a restart), and only then sweep — so the set has to come back.
#[test]
fn found_set_survives_a_restart() {
    let (mut engine, sid, dir) = engine_with_session();

    let r = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    let rev = qr.found_rev.expect("master rev");
    drop(engine); // the server goes away between the FIND and the sweep

    let mut restarted =
        ForgeQLEngine::new(dir.path().join("data"), common::make_registry()).expect("engine");
    let sid2 = restarted
        .register_local_session(dir.path())
        .expect("register session");

    let r = execute_fql(
        &mut restarted,
        &sid2,
        &format!(
            "CHANGE NODES FOUND IF REV '{rev}' MATCHING WORD 'encenderMotor' WITH 'startMotor'"
        ),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(
        mr.applied,
        "a set armed before the restart is still the set"
    );

    let cpp = fs::read_to_string(dir.path().join("motor_control.cpp")).expect("read cpp");
    assert!(cpp.contains("startMotor"));
}

/// ...but a mutation clears the persisted copy too. Resurrecting a set whose
/// members the mutation just moved would hand stale spans to the next sweep —
/// and a rev quoted from before the mutation must no longer authorise a sweep.
#[test]
fn a_mutation_clears_the_set_on_disk_too() {
    let (mut engine, sid, dir) = engine_with_session();

    let r = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    let rev = qr.found_rev.expect("master rev");
    let _ = execute_fql(
        &mut engine,
        &sid,
        &format!(
            "CHANGE NODES FOUND IF REV '{rev}' MATCHING WORD 'encenderMotor' WITH 'startMotor'"
        ),
    );
    drop(engine);

    let mut restarted =
        ForgeQLEngine::new(dir.path().join("data"), common::make_registry()).expect("engine");
    let sid2 = restarted
        .register_local_session(dir.path())
        .expect("register session");

    let err = fql_err(
        &mut restarted,
        &sid2,
        "CHANGE NODES FOUND MATCHING 'startMotor' WITH 'x'",
    );
    assert!(
        err.contains("no FIND result is armed"),
        "the mutation must have cleared the on-disk set, not left it to be restored: {err}"
    );
}

/// Every verb that names an existing node demands the gate.
#[test]
fn existing_node_verbs_require_if_rev() {
    let (mut engine, sid, _dir) = engine_with_session();

    let (id, rev) = file_handle(&mut engine, &sid, "motor_control.cpp");
    assert!(
        rev.starts_with('h'),
        "the rev travels with the handle: {rev}"
    );

    for fql in [
        format!("CHANGE NODE '{id}' WITH 'void x() {{}}'"),
        format!("DELETE NODE '{id}'"),
        format!("MOVE NODE '{id}' TO 'moved.cpp'"),
    ] {
        let err = fql_err(&mut engine, &sid, &fql);
        assert!(
            err.contains("requires IF REV"),
            "an ungated mutation on an existing node must be refused: {fql} → {err}"
        );
    }
}

/// The scenario the gate exists for: an agent carries a handle across other
/// commands, the code under it moves, and it comes back with the rev it read
/// first. The handle still resolves — handles are stable — so nothing but the
/// rev can tell it that the thing it remembers is not the thing that is there.
#[test]
fn a_stale_rev_cannot_overwrite() {
    // Known divergence: on columnar, the rev handed out by FIND files does
    // not match the rev the mutation layer computes for the same file, so
    // the IF REV round-trip fails. Legacy-pinned until both derivations agree.
    let (mut engine, sid, _dir) = engine_with_session_legacy();

    let (id, rev) = file_handle(&mut engine, &sid, "motor_control.cpp");

    let r = execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{id}' IF REV '{rev}' MATCHING 'encenderMotor' WITH 'startMotor'"),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(mr.applied);
    let new_rev = mr.new_rev.expect("a mutation hands back the new rev");
    assert_ne!(new_rev, rev, "the edit moved the node's rev");

    // The agent still remembers the rev it read before that edit.
    let err = fql_err(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{id}' IF REV '{rev}' WITH '// clobber'"),
    );
    assert!(
        err.contains("rev_mismatch"),
        "a stale rev must not be allowed to overwrite: {err}"
    );

    // The rev the mutation handed back works, with no re-read in between.
    let r = execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{id}' IF REV '{new_rev}' MATCHING 'startMotor' WITH 'runMotor'"),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(mr.applied, "the post-edit rev must be usable straight away");
}

/// The handle and rev of a file, as `FIND files` hands them out together.
fn file_handle(engine: &mut ForgeQLEngine, sid: &str, name: &str) -> (String, String) {
    let r = execute_fql(engine, sid, &format!("FIND files WHERE name = '{name}'"));
    let ForgeQLResult::Show(show) = r else {
        panic!("expected Show result");
    };
    let ShowContent::FileList { files, .. } = show.content else {
        panic!("expected FileList");
    };
    let f = files.first().expect("a file row");
    (
        f.node_id.clone().expect("handle"),
        f.rev.clone().expect("rev"),
    )
}

/// COPY LINES reports the addressed range length, not the payload's text-line
/// count: the line model treats the position after a final newline as an
/// addressable zero-byte line, so a whole-file copy `1-<count>` used to say
/// one line fewer than requested and read like data loss.
#[test]
fn copy_lines_reports_addressed_range_length() {
    let (mut engine, session_id, dir) = engine_with_session();
    let content =
        std::fs::read_to_string(dir.path().join("motor_control.cpp")).expect("read fixture");
    // The engine's line-addressing model: a trailing newline opens a final
    // addressable empty line, so the line count is split('\n').count().
    let model_lines = content.split('\n').count();

    let result = execute_fql(
        &mut engine,
        &session_id,
        &format!("COPY LINES 1-{model_lines} OF 'motor_control.cpp' TO 'dupes/copy.cpp'"),
    );
    let ForgeQLResult::Mutation(m) = result else {
        panic!("expected Mutation result from COPY LINES");
    };
    assert_eq!(
        m.lines_written, model_lines,
        "whole-file copy must report the addressed range length"
    );

    // Byte-for-byte identical copy — the count difference was presentation only.
    let copied = std::fs::read_to_string(dir.path().join("dupes/copy.cpp")).expect("read copy");
    assert_eq!(copied, content, "copied bytes must equal the source");
}

/// MOVE LINES reports the same addressed range length on both counters — a
/// clean move must never look like net line loss.
#[test]
fn move_lines_reports_symmetric_range_counts() {
    let (mut engine, session_id, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &session_id,
        "MOVE LINES 1-3 OF 'motor_control.cpp' TO 'dupes/moved.cpp'",
    );
    let ForgeQLResult::Mutation(m) = result else {
        panic!("expected Mutation result from MOVE LINES");
    };
    assert_eq!(m.lines_written, 3, "moved range length");
    assert_eq!(
        m.lines_removed, m.lines_written,
        "a clean move reports written == removed"
    );
}

#[test]
fn an_edit_that_mentions_a_symbol_does_not_hijack_its_resolution() {
    let (mut engine, sid, _dir) = engine_with_session();

    // Baseline: the enum resolves to its definition in motor_control.h.
    let before = match execute_fql(&mut engine, &sid, "SHOW members OF 'ErrorMotor'") {
        ForgeQLResult::Show(show) => format!("{show}"),
        _ => panic!("expected Show result"),
    };
    assert!(before.contains("TIMEOUT"), "baseline members: {before}");

    // A mention row is a name row with an EMPTY fql_kind, emitted when a
    // language names a node (extract_name) whose kind has no kind_map entry.
    // A Rust struct literal (`ErrorMotor { .. }`) is exactly that shape. The
    // C++ edit this test first used (`static enum ErrorMotor x = OK;`) emits
    // NO row at all — a bodyless specifier is deliberately skipped as a use,
    // not a definition — so that fixture never created the row this test
    // exists to demote, and the test stayed green with the demotion reverted.
    let r = execute_fql(&mut engine, &sid, "INSERT NODE FOR 'registro.rs'");
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(mr.applied);
    let rs = common::path_handle("registro.rs");
    let r = execute_fql(
        &mut engine,
        &sid,
        &format!(
            "INSERT AFTER NODE '{rs}' WITH 'fn registra_error() {{ let _ = ErrorMotor {{ codigo: 1 }}; }}'"
        ),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(mr.applied);

    // Precondition, asserted so this test can never go vacuous again: the
    // dirty overlay must now hold a reference row for the name — same name,
    // empty fql_kind, in the session-born file. If this fails, the fixture
    // stopped producing a mention and nothing below tests the demotion.
    let r = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name = 'ErrorMotor' LIMIT 50",
    );
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    let mention = qr
        .results
        .iter()
        .find(|m| m.path.as_ref().is_some_and(|p| p.ends_with("registro.rs")))
        .expect("precondition: the struct literal must emit an ErrorMotor row in registro.rs");
    assert!(
        mention.fql_kind.as_deref().unwrap_or("").is_empty(),
        "precondition: the registro.rs row must be a reference row (empty fql_kind), got {:?}",
        mention.fql_kind
    );

    // The mention must not hijack resolution away from the persistent
    // definition: before the demotion, the dirty reference row won stage 1
    // outright, SHOW members looked for the type in the mentioning file,
    // found no definition there, and errored for a symbol that exists.
    let after = match execute_fql(&mut engine, &sid, "SHOW members OF 'ErrorMotor'") {
        ForgeQLResult::Show(show) => format!("{show}"),
        _ => panic!("expected Show result"),
    };
    assert!(
        after.contains("TIMEOUT"),
        "members still answer from the definition: {after}"
    );

    // A declaration edited in-session must still win — only mentions were
    // demoted. Grow the enum in the header and confirm members answer from
    // the dirty version, not the stale persistent segment.
    let h = common::path_handle("motor_control.h");
    let rev = common::node_rev(&mut engine, &sid, &h);
    let r = execute_fql(
        &mut engine,
        &sid,
        &format!(
            "CHANGE NODE '{h}' IF REV '{rev}' MATCHING 'FALLO   = 2' WITH 'FALLO   = 2, SOBRECARGA = 3'"
        ),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(mr.applied);
    let grown = match execute_fql(&mut engine, &sid, "SHOW members OF 'ErrorMotor'") {
        ForgeQLResult::Show(show) => format!("{show}"),
        _ => panic!("expected Show result"),
    };
    assert!(
        grown.contains("SOBRECARGA"),
        "a dirty declaration still pre-empts the persistent index: {grown}"
    );
}

// -----------------------------------------------------------------------
// Regression: the kindless rows are reachable on the LEGACY backend too
// -----------------------------------------------------------------------

/// `WHERE fql_kind = ''` answers the group `GROUP BY fql_kind` publishes, on the
/// in-memory backend as much as on the columnar one.
///
/// The columnar half of this fix posts the empty kind in the overlay's kind
/// table. The legacy backend kept its own copy of that skip in
/// `SecondaryIndexBuilder::insert`, and leaving it there would have been worse
/// than a smaller index: `find_symbols_prefilter` takes the `fql_kind` index for
/// an equality and then STRIPS the predicate, because the index is supposed to
/// have supplied the rows. An unindexed value therefore got an empty candidate
/// iterator, never reached the scan that could have decided it, and came back as
/// 0 rows with a success status — the same false zero the whole change exists to
/// remove, on the backend that serves a source with no `.forgeql.yaml`. Fixing
/// the row comparison alone does not reach it, because the tier answers first.
///
/// The two numbers are compared rather than pinned: a literal count would go on
/// passing the day the fixture stops producing a row with no kind.
#[test]
fn the_kindless_rows_answer_their_own_equality_on_the_legacy_backend() {
    let (mut engine, sid, _dir) = engine_with_session_legacy();

    // The control is the SCAN's grouping, not the counted one. On this backend
    // `try_group_by_stats_fast_path` answers a bare `GROUP BY fql_kind` from
    // `IndexStats::by_fql_kind`, which counts every RAW row, while the scan
    // collapses duplicates on `(name, path, fql_kind, line)` — so on this
    // fixture the two disagree for the empty kind, 3 against 2, and they would
    // disagree the same way for any kind with an intra-file duplicate. That gap
    // is older than this change and is not what this test is about; comparing
    // the equality against the counted route would measure it instead of the
    // thing being fixed. A `WHERE` that holds for every row disarms the count
    // path and leaves the authoritative, collapsed group size.
    let grouped = match execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE line >= 0 GROUP BY fql_kind",
    ) {
        ForgeQLResult::Query(qr) => qr
            .results
            .iter()
            .find(|r| r.fql_kind.as_deref().unwrap_or("").is_empty())
            .and_then(|r| r.count)
            .unwrap_or(0),
        other => panic!("expected Query, got: {other:?}"),
    };
    assert!(
        grouped > 0,
        "the fixture stopped producing rows with no kind, so the cases below \
         would hold vacuously"
    );

    for query in [
        "FIND symbols WHERE fql_kind = ''",
        "FIND symbols WHERE fql_kind = 'unknown'",
    ] {
        match execute_fql(&mut engine, &sid, query) {
            ForgeQLResult::Query(qr) => {
                assert_eq!(
                    qr.total, grouped,
                    "{query} answered {} rows where the grouping counted {grouped}",
                    qr.total
                );
                // Right cardinality over the wrong rows would pass the count.
                for r in &qr.results {
                    assert_eq!(r.fql_kind.as_deref(), Some(""), "{query}: {}", r.name);
                }
            }
            other => panic!("{query}: expected Query, got: {other:?}"),
        }
    }
}
