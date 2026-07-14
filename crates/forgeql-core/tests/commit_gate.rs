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
    engine.execute_blocking(auth(AuthContext::Tester), Some(&coords), op)
}

fn exec(engine: &mut ForgeQLEngine, sid: &str, fql: &str) -> ForgeQLResult {
    try_exec(engine, sid, fql).expect("execute")
}

#[test]
fn rollback_removes_files_the_transaction_created() {
    let (mut engine, sid, dir) = gated_session();

    let _ = exec(&mut engine, &sid, "BEGIN TRANSACTION 'txn'");
    let _ = exec(&mut engine, &sid, "INSERT NODE FOR 'scratch/new.txt'");
    assert!(dir.path().join("scratch/new.txt").exists());

    let _ = exec(&mut engine, &sid, "ROLLBACK");

    // ROLLBACK is `git reset --hard`, which restores **tracked** paths. Staging
    // is deferred to COMMIT, so a file created inside the transaction is still
    // untracked and the reset walked straight past it — it survived, on disk and
    // in the index (BUG-025). The created paths are now removed explicitly.
    assert!(
        !dir.path().join("scratch/new.txt").exists(),
        "ROLLBACK must remove what the transaction created"
    );
    assert!(
        !dir.path().join("scratch").exists(),
        "and the directory it created for it"
    );
    // The pre-existing file is untouched — this is not a `git clean`.
    assert_eq!(
        fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
        "initial\n"
    );
}

#[test]
fn rollback_leaves_alone_a_directory_it_did_not_create() {
    let (mut engine, sid, dir) = gated_session();
    // An empty directory that was already there. Git does not track empty
    // directories, so `reset --hard` will not restore it — if ROLLBACK deletes
    // it, it is gone for good. Only what the transaction created may be removed.
    fs::create_dir_all(dir.path().join("existing")).unwrap();

    let _ = exec(&mut engine, &sid, "BEGIN TRANSACTION 'txn'");
    let _ = exec(&mut engine, &sid, "INSERT NODE FOR 'existing/new.txt'");
    let _ = exec(&mut engine, &sid, "INSERT NODE FOR 'made/up/deep.txt'");
    let _ = exec(&mut engine, &sid, "ROLLBACK");

    assert!(
        !dir.path().join("existing/new.txt").exists(),
        "the created file goes"
    );
    assert!(
        dir.path().join("existing").is_dir(),
        "the directory it was put in was not created by the transaction — it stays"
    );
    // Directories the transaction did create go, all the way up.
    assert!(
        !dir.path().join("made").exists(),
        "created directories go too"
    );
}

#[test]
fn rollback_removes_a_directory_created_by_insert_node_for() {
    let (mut engine, sid, dir) = gated_session();

    let _ = exec(&mut engine, &sid, "BEGIN TRANSACTION 'txn'");
    let _ = exec(&mut engine, &sid, "INSERT NODE FOR 'docs/'");
    assert!(dir.path().join("docs").is_dir());

    let _ = exec(&mut engine, &sid, "ROLLBACK");
    assert!(
        !dir.path().join("docs").exists(),
        "a directory the transaction created is removed as a directory, not attempted as a file"
    );
}

#[test]
fn rollback_after_a_server_restart_still_removes_created_files() {
    let (mut engine, sid, dir) = gated_session();

    let _ = exec(&mut engine, &sid, "BEGIN TRANSACTION 'txn'");
    let _ = exec(&mut engine, &sid, "INSERT NODE FOR 'scratch/new.txt'");
    let _ = exec(&mut engine, &sid, "INSERT NODE FOR 'docs/'");

    // The server goes away mid-transaction — a session outlives the process, and
    // an agent can reconnect hours later to finish. Everything ROLLBACK needs
    // has to be on disk, not in this engine's RAM: drop it and rebuild.
    drop(engine);
    let mut engine = ForgeQLEngine::new(dir.path().join("data"), make_registry()).expect("engine");
    let sid = engine
        .register_local_session(dir.path())
        .expect("register session");

    let _ = exec(&mut engine, &sid, "ROLLBACK");

    assert!(
        !dir.path().join("scratch/new.txt").exists(),
        "a created file must not survive a rollback just because the server restarted"
    );
    assert!(
        !dir.path().join("scratch").exists(),
        "nor the directory made for it"
    );
    assert!(!dir.path().join("docs").exists(), "nor a created directory");
    assert_eq!(
        fs::read_to_string(dir.path().join("notes.txt")).unwrap(),
        "initial\n",
        "and nothing else is touched"
    );
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
fn a_gated_job_satisfies_commit_after_completion() {
    let (mut engine, sid, _dir) = gated_session();

    // Start the gated step as a background job and wait for it to finish.
    let started = exec(&mut engine, &sid, "JOB START 'gate'");
    let ForgeQLResult::JobStarted(job) = started else {
        panic!("expected JobStarted, got {started:?}");
    };
    let snap = engine
        .jobs_handle()
        .wait(&job.id, std::time::Duration::from_secs(30))
        .expect("job id must be known");
    assert!(
        matches!(snap.state, forgeql_core::jobs::JobState::Succeeded),
        "gate job must succeed: {snap:?}"
    );

    // COMMIT reconciles the finished gate job and proceeds.
    assert!(matches!(
        exec(&mut engine, &sid, "COMMIT MESSAGE 'gated via job'"),
        ForgeQLResult::Commit(_)
    ));
}

#[test]
fn an_edit_before_the_gate_job_reconciles_keeps_commit_blocked() {
    let (mut engine, sid, _dir) = gated_session();

    // Start the gated step as a background job and let it finish.
    let started = exec(&mut engine, &sid, "JOB START 'gate'");
    let ForgeQLResult::JobStarted(job) = started else {
        panic!("expected JobStarted, got {started:?}");
    };
    let _ = engine
        .jobs_handle()
        .wait(&job.id, std::time::Duration::from_secs(30))
        .expect("job id must be known");

    // Mutate before the job's result is reconciled: the run no longer
    // describes the worktree, so it must not satisfy the gate.
    exec(
        &mut engine,
        &sid,
        "CHANGE FILE 'notes.txt' WITH <<TXT\nedited\nTXT",
    );
    try_exec(&mut engine, &sid, "COMMIT MESSAGE 'stale gate'")
        .expect_err("a stale gate job must not unblock COMMIT");
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
