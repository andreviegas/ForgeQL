//! Commit-gate tests — `COMMIT` is refused until every `commit_gate: true`
//! verify step has passed since the most recent mutation.
//!
//! Run with: `cargo test -p forgeql-core --test commit_gate`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    unused_results
)]

use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use forgeql_core::ast::lang::{CppLanguageInline, LanguageRegistry};
use forgeql_core::auth::{AuthContext, auth};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::parser;
use forgeql_core::result::ForgeQLResult;
use forgeql_core::session::SessionCoords;
use tempfile::tempdir;

const FIXTURE_CPP: &str =
    "int calcularPotencia(int velocidad, int carga)\n{\n    return velocidad * carga;\n}\n";

const FIXTURE_YAML: &str = "verify_steps:\n  - name: gate\n    command: \"true\"\n    commit_gate: true\n  - name: nogate\n    command: \"true\"\n  - name: echo-target\n    command: \"printf %s $target\"\n    params:\n      - { name: target, type: ident }\n  - name: env-probe\n    command: \"printf '%s|%s' $FORGEQL_SOURCE $FORGEQL_BUILD_DIR\"\nrun_steps:\n  - name: echo-it\n    command: \"printf '%s' $msg\"\n    params:\n      - { name: msg, type: ident }\n  - name: cat-stdin\n    command: \"cat\"\n    params:\n      - { name: text, type: string }\n";

fn make_registry() -> Arc<LanguageRegistry> {
    Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]))
}

/// Temp git repo with one indexed C++ file, a non-indexed `notes.txt`, and a
/// `.forgeql.yaml` whose `gate` step is commit-gated.  Returns
/// `(engine, session_id, TempDir)`; the `TempDir` must stay alive.
fn gated_session() -> (ForgeQLEngine, String, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let repo = git2::Repository::init(dir.path()).expect("git init");
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    drop(cfg);

    fs::write(dir.path().join("power.cpp"), FIXTURE_CPP).expect("write cpp");
    fs::write(dir.path().join("notes.txt"), "initial\n").expect("write notes");
    fs::write(dir.path().join(".forgeql.yaml"), FIXTURE_YAML).expect("write yaml");

    let mut index = repo.index().unwrap();
    index.add_path(Path::new("power.cpp")).unwrap();
    index.add_path(Path::new("notes.txt")).unwrap();
    index.add_path(Path::new(".forgeql.yaml")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(0, 0)).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();

    let data_dir = dir.path().join("data");
    let mut engine = ForgeQLEngine::new(data_dir, make_registry()).expect("engine");
    let session_id = engine
        .register_local_session(dir.path())
        .expect("register session");
    (engine, session_id, dir)
}

fn try_exec(engine: &mut ForgeQLEngine, sid: &str, fql: &str) -> Result<ForgeQLResult> {
    let ops = parser::parse(fql).expect("parse");
    let op = ops.first().expect("op");
    let coords = SessionCoords::from_session_id(sid).expect("valid sid");
    engine.execute(auth(AuthContext::Tester), Some(&coords), op)
}

fn exec(engine: &mut ForgeQLEngine, sid: &str, fql: &str) -> ForgeQLResult {
    try_exec(engine, sid, fql).expect("execute")
}

#[test]
fn commit_is_gated_until_the_gated_step_passes() {
    let (mut engine, sid, _dir) = gated_session();

    // Pristine session: the gate has never run, so COMMIT is refused.
    let blocked = try_exec(&mut engine, &sid, "COMMIT MESSAGE 'first'");
    let err = blocked.expect_err("COMMIT must be blocked before the gate runs");
    assert!(
        err.to_string().contains("gate"),
        "error should name the stale gate: {err}"
    );

    // A non-gated step must NOT satisfy the gate.
    exec(&mut engine, &sid, "VERIFY build 'nogate'");
    try_exec(&mut engine, &sid, "COMMIT MESSAGE 'second'")
        .expect_err("a non-gated step must not satisfy the commit gate");

    // Running the gated step satisfies it -> COMMIT succeeds.
    exec(&mut engine, &sid, "VERIFY build 'gate'");
    assert!(matches!(
        exec(&mut engine, &sid, "COMMIT MESSAGE 'gated commit'"),
        ForgeQLResult::Commit(_)
    ));
}

#[test]
fn an_edit_after_the_gate_re_blocks_commit() {
    let (mut engine, sid, _dir) = gated_session();

    // Satisfy the gate.
    exec(&mut engine, &sid, "VERIFY build 'gate'");

    // A mutation (on a non-indexed file) invalidates every commit gate.
    exec(
        &mut engine,
        &sid,
        "CHANGE FILE 'notes.txt' WITH <<TXT\nedited\nTXT",
    );

    // COMMIT is refused again until the gate is re-run.
    try_exec(&mut engine, &sid, "COMMIT MESSAGE 'after edit'")
        .expect_err("an edit after the gate must re-block COMMIT");

    // Re-running the gate clears the block.
    exec(&mut engine, &sid, "VERIFY build 'gate'");
    assert!(matches!(
        exec(&mut engine, &sid, "COMMIT MESSAGE 'final'"),
        ForgeQLResult::Commit(_)
    ));
}

#[test]
fn verify_step_substitutes_typed_params() {
    let (mut engine, sid, _dir) = gated_session();

    // A valid ident arg is substituted into the command template.
    match exec(
        &mut engine,
        &sid,
        "VERIFY build 'echo-target' 'multistring-pxrox'",
    ) {
        ForgeQLResult::VerifyBuild(r) => {
            assert!(r.success, "step should run: {}", r.output);
            assert!(
                r.output.contains("multistring-pxrox"),
                "output should echo the substituted target: {}",
                r.output
            );
        }
        other => panic!("expected VerifyBuild, got {other:?}"),
    }
}

#[test]
fn verify_step_rejects_bad_arity_and_injection() {
    let (mut engine, sid, _dir) = gated_session();

    // Wrong argument count.
    try_exec(&mut engine, &sid, "VERIFY build 'echo-target'")
        .expect_err("missing required argument must be rejected");

    // Non-ident argument (shell metacharacters) is refused before running.
    try_exec(
        &mut engine,
        &sid,
        "VERIFY build 'echo-target' 'foo; rm -rf /'",
    )
    .expect_err("a non-ident argument must be rejected");

    // A parameterless step still works with zero args (back-compat).
    assert!(matches!(
        exec(&mut engine, &sid, "VERIFY build 'nogate'"),
        ForgeQLResult::VerifyBuild(_)
    ));
}

#[test]
fn verify_step_sees_forgeql_env_vars() {
    let (mut engine, sid, _dir) = gated_session();

    // run_standalone injects per-session FORGEQL_* vars into the command env.
    match exec(&mut engine, &sid, "VERIFY build 'env-probe'") {
        ForgeQLResult::VerifyBuild(r) => {
            assert!(r.success, "env-probe should run: {}", r.output);
            // FORGEQL_SOURCE is the synthetic local source name.
            assert!(
                r.output.contains("local"),
                "output should contain FORGEQL_SOURCE: {}",
                r.output
            );
            // FORGEQL_BUILD_DIR is the per-worktree target dir.
            assert!(
                r.output.contains("target"),
                "output should contain FORGEQL_BUILD_DIR: {}",
                r.output
            );
        }
        other => panic!("expected VerifyBuild, got {other:?}"),
    }
}

#[test]
fn run_step_substitutes_ident_arg() {
    let (mut engine, sid, _dir) = gated_session();
    let result = exec(&mut engine, &sid, "RUN 'echo-it' 'hello'");
    let ForgeQLResult::Run(r) = result else {
        panic!("expected a Run result");
    };
    assert!(r.success, "RUN should succeed: {}", r.output);
    assert!(r.output.contains("hello"), "output: {}", r.output);
}

#[test]
fn run_step_binds_string_arg_to_stdin() {
    let (mut engine, sid, _dir) = gated_session();
    // `cat` echoes its stdin; the string arg is piped, never shell-interpolated.
    let result = exec(&mut engine, &sid, "RUN 'cat-stdin' 'piped via stdin'");
    let ForgeQLResult::Run(r) = result else {
        panic!("expected a Run result");
    };
    assert!(r.success, "RUN should succeed: {}", r.output);
    assert!(
        r.output.contains("piped via stdin"),
        "stdin not echoed; output: {}",
        r.output
    );
}

#[test]
fn run_step_unknown_name_is_rejected() {
    let (mut engine, sid, _dir) = gated_session();
    try_exec(&mut engine, &sid, "RUN 'nonexistent'").expect_err("an unknown RUN step must error");
}

#[test]
fn run_step_rejects_ident_injection() {
    let (mut engine, sid, _dir) = gated_session();
    try_exec(&mut engine, &sid, "RUN 'echo-it' 'foo; rm -rf /'")
        .expect_err("an ident arg with shell metacharacters must be rejected");
}
