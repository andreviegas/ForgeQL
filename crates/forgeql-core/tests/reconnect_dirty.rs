//! Gate tests for `PhaseFT7` — git-diff reindex on reconnect.
//!
//! Verifies that after modifying files without `BEGIN TRANSACTION`, a server
//! restart followed by `USE source.branch AS 'alias'` (which internally calls
//! `use_source`) reindexes the dirty files so subsequent queries return
//! post-change symbols.
//!
//! Run with: `cargo test -p forgeql-core --test reconnect_dirty`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    unused_results
)]

use std::fs;
use std::path::{Path, PathBuf};

use forgeql_core::auth::{AuthContext, auth};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::parser;
use forgeql_core::result::{ForgeQLResult, QueryResult};
use forgeql_core::session::SessionCoords;
use tempfile::tempdir;

mod common;

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// Execute a FQL statement, return the result.
fn exec(engine: &mut ForgeQLEngine, session_id: Option<&str>, fql: &str) -> ForgeQLResult {
    let ops = parser::parse(fql).expect("parse");
    let op = ops.first().expect("op");
    let coords = session_id.map(|s| SessionCoords::from_session_id(s).expect("valid session_id"));
    engine
        .execute(auth(AuthContext::Tester), coords.as_ref(), op)
        .result
        .expect("execute")
}

/// Create a non-bare git repo at `dir` with an initial commit containing the
/// `motor_control` fixtures.  Returns the `git2::Repository`.
fn make_source_repo(dir: &Path) -> git2::Repository {
    let repo = git2::Repository::init(dir).expect("git init");
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    drop(cfg);

    let src = common::fixtures_dir();
    fs::copy(src.join("motor_control.h"), dir.join("motor_control.h")).expect("copy .h");
    fs::copy(src.join("motor_control.cpp"), dir.join("motor_control.cpp")).expect("copy .cpp");

    let mut index = repo.index().unwrap();
    index.add_path(Path::new("motor_control.h")).unwrap();
    index.add_path(Path::new("motor_control.cpp")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    {
        // Scope `tree` so the borrow on `repo` is released before we return it.
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(0, 0)).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }

    repo
}

/// Returns the short branch name that HEAD currently points to.
fn head_branch(repo: &git2::Repository) -> String {
    repo.head()
        .unwrap()
        .shorthand()
        .unwrap_or("master")
        .to_string()
}

/// Boot an engine, clone the source repo, open a worktree session, and return:
/// `(engine, session_token, branch_name, worktree_path, data_dir, source_dir)`
///
/// The last two `TempDir` values must remain alive for the duration of the test.
fn engine_with_source_session() -> (
    ForgeQLEngine,
    String,
    String,
    PathBuf,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let src_dir = tempdir().expect("src tempdir");
    let repo = make_source_repo(src_dir.path());
    let branch = head_branch(&repo);
    drop(repo);

    let data_dir = tempdir().expect("data tempdir");
    let mut engine =
        ForgeQLEngine::new(data_dir.path().to_path_buf(), common::make_registry()).expect("engine");

    // CREATE SOURCE — clones the non-bare source repo into data_dir/mysrc.git.
    let create_fql = format!("CREATE SOURCE 'mysrc' FROM '{}'", src_dir.path().display());
    exec(&mut engine, None, &create_fql);

    // USE — creates a worktree + indexes.  No existing session context — pass None.
    let use_fql = format!("USE mysrc.{branch} AS 'sess'");
    exec(&mut engine, None, &use_fql);

    // Worktree path now lives in the per-user subdir:
    // data_dir/worktrees/{user}/{source}.{branch}.{alias}
    let wt_path = SessionCoords::user_worktrees_root(data_dir.path(), auth(AuthContext::Tester))
        .join(format!("mysrc.{branch}.sess"));

    assert!(
        wt_path.is_dir(),
        "worktree must exist after USE: {}",
        wt_path.display()
    );

    // Build the full session token used as the map key.
    let session_token =
        SessionCoords::new(auth(AuthContext::Tester), "mysrc", &branch, "sess").to_session_id();

    (engine, session_token, branch, wt_path, data_dir, src_dir)
}

/// Assert that a FIND symbols query returns at least one hit.
fn assert_symbol_found(engine: &mut ForgeQLEngine, sess: &str, name: &str) {
    let fql = format!("FIND symbols WHERE name = '{name}'");
    let result = exec(engine, Some(sess), &fql);
    match result {
        ForgeQLResult::Query(QueryResult { results, .. }) => {
            assert!(
                !results.is_empty(),
                "expected symbol '{name}' in index but found none"
            );
        }
        other => panic!("unexpected result: {other:?}"),
    }
}

/// Assert that a FIND symbols query returns zero hits.
fn assert_symbol_not_found(engine: &mut ForgeQLEngine, sess: &str, name: &str) {
    let fql = format!("FIND symbols WHERE name = '{name}'");
    let result = exec(engine, Some(sess), &fql);
    match result {
        ForgeQLResult::Query(QueryResult { results, .. }) => {
            assert!(
                results.is_empty(),
                "expected symbol '{name}' to be absent but found {} row(s)",
                results.len()
            );
        }
        other => panic!("unexpected result: {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Gate test 1 — dirty files are reindexed on reconnect
// -----------------------------------------------------------------------

/// After `CHANGE FILE` (no `BEGIN TRANSACTION`), crash, reconnect:
/// `FIND symbols` must return symbols from the post-change content.
#[test]
fn reconnect_reindexes_dirty_files() {
    let (mut engine, sess, branch, wt_path, data_dir, _src_dir) = engine_with_source_session();

    // Verify the initial state — encenderMotor is in the index.
    assert_symbol_found(&mut engine, &sess, "encenderMotor");

    // Overwrite motor_control.cpp via the engine's CHANGE command (no BEGIN).
    // This updates both disk and the in-memory index.
    exec(
        &mut engine,
        Some(&sess),
        "CHANGE FILE 'motor_control.cpp' WITH 'void reconnectTestFn(){}'",
    );

    // Confirm the in-memory index reflects the change.
    assert_symbol_found(&mut engine, &sess, "reconnectTestFn");
    assert_symbol_not_found(&mut engine, &sess, "encenderMotor");

    // --- Simulate crash: drop the engine (all in-memory sessions gone). ---
    drop(engine);

    // New engine with same data_dir — auto-discovers the bare repo.
    let mut new_engine = ForgeQLEngine::new(data_dir.path().to_path_buf(), common::make_registry())
        .expect("new engine");

    // Reconnect: USE calls use_source → resume_index → FT7 dirty reindex.
    let use_fql = format!("USE mysrc.{branch} AS 'sess'");
    exec(&mut new_engine, None, &use_fql);
    let new_sess =
        SessionCoords::new(auth(AuthContext::Tester), "mysrc", &branch, "sess").to_session_id();

    // FT7 must reindex the dirty file so the post-change symbols are present.
    assert_symbol_found(&mut new_engine, &new_sess, "reconnectTestFn");
    assert_symbol_not_found(&mut new_engine, &new_sess, "encenderMotor");

    drop((wt_path, data_dir));
}

// -----------------------------------------------------------------------
// Gate test 2 — clean repo does not trigger reindex
// -----------------------------------------------------------------------

/// If no files were modified between the initial USE and the reconnect,
/// `git diff HEAD` reports nothing and the cached index is used as-is.
#[test]
fn reconnect_does_not_reindex_clean_files() {
    let (engine, _sess, branch, wt_path, data_dir, _src_dir) = engine_with_source_session();

    // No changes — drop engine to simulate a clean restart.
    drop(engine);

    let mut new_engine = ForgeQLEngine::new(data_dir.path().to_path_buf(), common::make_registry())
        .expect("new engine");

    // Reconnect — git diff HEAD must be empty; index is restored from cache.
    let use_fql = format!("USE mysrc.{branch} AS 'sess'");
    exec(&mut new_engine, None, &use_fql);
    let new_sess =
        SessionCoords::new(auth(AuthContext::Tester), "mysrc", &branch, "sess").to_session_id();

    // Original symbols must still be present (index was valid, no reindex needed).
    assert_symbol_found(&mut new_engine, &new_sess, "encenderMotor");

    drop((wt_path, data_dir));
}

// -----------------------------------------------------------------------
// Gate test 3 — BEGIN before crash → no dirty files, no double-index
// -----------------------------------------------------------------------

/// After `BEGIN TRANSACTION` the changed files are committed into the
/// checkpoint commit.  On reconnect, `git diff HEAD` reports no diff for
/// those files, so FT7 must NOT reindex them.
/// The delta file restored from the checkpoint commit already has the updated
/// index — the session must be correct without any extra reindex.
#[test]
fn reconnect_after_begin_does_not_double_index() {
    let (mut engine, sess, branch, wt_path, data_dir, _src_dir) = engine_with_source_session();

    // Checkpoint with BEGIN before modifying (BEGIN commits first).
    exec(&mut engine, Some(&sess), "BEGIN TRANSACTION 'txn-save'");

    // Now change a file (the change is on top of the checkpoint commit).
    exec(
        &mut engine,
        Some(&sess),
        "CHANGE FILE 'motor_control.cpp' WITH 'void afterBeginFn(){}'",
    );

    // The in-memory index must reflect the new function.
    assert_symbol_found(&mut engine, &sess, "afterBeginFn");

    // --- Simulate crash. ---
    drop(engine);

    let mut new_engine = ForgeQLEngine::new(data_dir.path().to_path_buf(), common::make_registry())
        .expect("new engine");

    // Reconnect: the file was modified AFTER the checkpoint commit, so it IS
    // dirty relative to HEAD.  FT7 will reindex it.
    // The important assertion is that the symbol is correct, not double-indexed.
    let use_fql = format!("USE mysrc.{branch} AS 'sess'");
    exec(&mut new_engine, None, &use_fql);
    let new_sess =
        SessionCoords::new(auth(AuthContext::Tester), "mysrc", &branch, "sess").to_session_id();

    assert_symbol_found(&mut new_engine, &new_sess, "afterBeginFn");

    drop((wt_path, data_dir));
}

/// A staged edit whose staging segment is gone by reconnect is queued for
/// re-index by the delta loader — as a workspace-relative path. The reconnect
/// must anchor that path to the worktree before re-indexing: read as given it
/// would be looked for against the process's working directory, found absent,
/// and treated as a deletion, hiding the file's rows instead of restoring them.
#[test]
fn a_queued_reindex_path_is_anchored_to_the_worktree_on_reconnect() {
    let (mut engine, sess, branch, wt_path, data_dir, _src_dir) = engine_with_source_session();

    exec(
        &mut engine,
        Some(&sess),
        "CHANGE FILE 'motor_control.cpp' WITH 'void queuedFn(){}'",
    );
    assert_symbol_found(&mut engine, &sess, "queuedFn");

    // --- Simulate a crash that also loses the staging directory: the delta
    // survives and names the edit, but its segment is gone. ---
    drop(engine);
    let staging = wt_path.join(".forgeql-staging");
    assert!(staging.is_dir(), "the edit staged no segment — vacuous");
    std::fs::remove_dir_all(&staging).expect("remove staging");

    let mut new_engine = ForgeQLEngine::new(data_dir.path().to_path_buf(), common::make_registry())
        .expect("new engine");
    let use_fql = format!("USE mysrc.{branch} AS 'sess'");
    exec(&mut new_engine, None, &use_fql);
    let new_sess =
        SessionCoords::new(auth(AuthContext::Tester), "mysrc", &branch, "sess").to_session_id();

    // The file is dirty against HEAD and queued by the loader; both routes
    // must lead to a re-index from disk, never to a deletion.
    assert_symbol_found(&mut new_engine, &new_sess, "queuedFn");

    drop((wt_path, data_dir));
}

/// The same queue is drained by ROLLBACK: the checkpoint's delta names the
/// edit made before BEGIN, its staging is gone by the time the rollback
/// reloads it, and the file must be re-indexed from the restored worktree —
/// found, not taken for a deletion because its queued path was relative.
#[test]
fn a_queued_reindex_path_is_anchored_to_the_worktree_on_rollback() {
    let (mut engine, sess, _branch, wt_path, data_dir, _src_dir) = engine_with_source_session();

    // An edit before BEGIN, so the checkpoint's delta names its segment.
    exec(
        &mut engine,
        Some(&sess),
        "CHANGE FILE 'motor_control.cpp' WITH 'void beforeBeginFn(){}'",
    );
    exec(&mut engine, Some(&sess), "BEGIN TRANSACTION 'txn-anchor'");
    exec(
        &mut engine,
        Some(&sess),
        "CHANGE FILE 'motor_control.cpp' WITH 'void afterBeginFn(){}'",
    );
    assert_symbol_found(&mut engine, &sess, "afterBeginFn");

    // Lose the staging directory, then roll back: the reload finds the
    // checkpoint's delta with every segment missing and queues the file.
    let staging = wt_path.join(".forgeql-staging");
    assert!(staging.is_dir(), "the edits staged no segment — vacuous");
    std::fs::remove_dir_all(&staging).expect("remove staging");
    exec(&mut engine, Some(&sess), "ROLLBACK");

    assert_symbol_found(&mut engine, &sess, "beforeBeginFn");
    assert_symbol_not_found(&mut engine, &sess, "afterBeginFn");

    drop((wt_path, data_dir));
}

/// A columnar open that refuses must say so in the USE response, not only in
/// the server log. The session still answers — the in-memory index is the
/// complete fallback — but the refusal names its own repair, and an agent that
/// never sees it cannot act on it. Delete the committed overlay and make its
/// directory unwritable so the rebuild cannot persist one: the columnar open
/// fails while the segment store — and with it the in-memory build — stays
/// intact. The reconnect must succeed, answer a real query, and carry the
/// refusal in its message; a USE that resumes the same in-memory session must
/// carry it too; and once the store is writable again, the next fresh attach
/// heals and the note is gone.
///
/// The sabotage deliberately avoids the segment store: `build_index` emits
/// inline segments there, so a read-only segment store empties the in-memory
/// fallback itself — a coupling this test must stay clear of to measure the
/// note alone.
#[test]
#[cfg(unix)]
fn a_failed_columnar_open_reports_its_fallback_in_the_use_response() {
    use std::os::unix::fs::PermissionsExt;

    let (engine, sess, branch, _wt_path, data_dir, _src_dir) = engine_with_source_session();

    // The committed overlay store for this source.
    let overlays_root = data_dir
        .path()
        .join("mysrc.git")
        .join("forgeql")
        .join("overlays");

    // Delete the overlay and lock every directory that could hold a rebuilt
    // one — the open must then refuse rather than heal.
    let mut overlay_dirs: Vec<PathBuf> = vec![overlays_root.clone()];
    let mut victim: Option<PathBuf> = None;
    let mut dirs = vec![overlays_root];
    while let Some(d) = dirs.pop() {
        for entry in fs::read_dir(&d).expect("read overlays dir") {
            let p = entry.expect("dir entry").path();
            if p.is_dir() {
                overlay_dirs.push(p.clone());
                dirs.push(p);
            } else if victim.is_none() {
                victim = Some(p);
            }
        }
    }
    let victim = victim.expect("a committed overlay file to delete");
    fs::remove_file(&victim).expect("delete the overlay");

    let chmod = |p: &Path, mode: u32| {
        let mut perm = fs::metadata(p).expect("stat").permissions();
        perm.set_mode(mode);
        fs::set_permissions(p, perm).expect("chmod");
    };
    for d in &overlay_dirs {
        chmod(d, 0o555);
    }

    // Restart and reconnect.
    drop(engine);
    let mut engine =
        ForgeQLEngine::new(data_dir.path().to_path_buf(), common::make_registry()).expect("engine");
    let use_fql = format!("USE mysrc.{branch} AS 'sess'");
    let result = exec(&mut engine, None, &use_fql);
    let ForgeQLResult::SourceOp(op) = &result else {
        panic!("expected SourceOp, got {result:?}");
    };
    let message = op.message.as_deref().unwrap_or_default();
    assert!(
        message.contains("columnar index unavailable"),
        "the USE response must carry the fallback note: {message}"
    );
    // The fallback must actually answer — the note promises a complete
    // in-memory index behind it. That was not always so: with columnar
    // configured, `build_index` returns an empty in-memory table by design
    // (segments are emitted inline instead), so a session on this path once
    // served zero rows while USE reported success. The fallback rebuild in
    // `load_session_index` is what this assertion pins.
    assert_symbol_found(&mut engine, &sess, "encenderMotor");

    // Resuming the same in-memory session must carry the note too — the
    // degradation lasts as long as the session serves from the fallback.
    let resumed = exec(&mut engine, None, &use_fql);
    let ForgeQLResult::SourceOp(op) = &resumed else {
        panic!("expected SourceOp, got {resumed:?}");
    };
    let message = op.message.as_deref().unwrap_or_default();
    assert!(
        message.contains("columnar index unavailable"),
        "a resumed USE must carry the fallback note: {message}"
    );

    // Writable again: the next fresh attach rebuilds the overlay and the note
    // must be gone — a healed session must not keep warning.
    for d in &overlay_dirs {
        chmod(d, 0o755);
    }
    drop(engine);
    let mut engine =
        ForgeQLEngine::new(data_dir.path().to_path_buf(), common::make_registry()).expect("engine");
    let healed = exec(&mut engine, None, &use_fql);
    let ForgeQLResult::SourceOp(op) = &healed else {
        panic!("expected SourceOp, got {healed:?}");
    };
    let message = op.message.as_deref().unwrap_or_default();
    assert!(
        !message.contains("columnar index unavailable"),
        "a healed attach must not keep the stale note: {message}"
    );
}
