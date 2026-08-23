//! Gate tests for `PhaseFT6` — checkpoint stack persistence.
//!
//! Verifies that `.forgeql-checkpoints` is written on BEGIN, survives a
//! simulated restart, and is correctly cleared on COMMIT / updated on ROLLBACK.
//!
//! Run with: `cargo test -p forgeql-core --test checkpoint_persist`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    unused_results
)]

use std::fs;

use forgeql_core::result::ForgeQLResult;
use forgeql_core::session::Session;
use tempfile::tempdir;

mod common;

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// Create a temp dir with a git repo + initial commit + `motor_control` fixtures.
/// Returns a `TestSession` whose `Drop` frees the workspace.
fn engine_with_git_session() -> common::TestSession {
    let dir = tempdir().expect("tempdir");
    let src = common::fixtures_dir();

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

    common::legacy_session_in(dir)
}

// -----------------------------------------------------------------------
// Unit-level tests for checkpoint_file module
// -----------------------------------------------------------------------

/// `save` writes the stack; `try_restore` reads it back into a fresh session.
/// This is the core "survives restart" property — verified directly against
/// the module without going through the engine.
#[test]
fn checkpoint_survives_restart() {
    let dir = tempdir().expect("tempdir");
    let reg = common::make_registry();

    // Build a session with two checkpoints in memory.
    let mut session_a = Session::new(
        "sid",
        "user",
        dir.path().to_path_buf(),
        "src",
        "branch",
        &reg,
    );
    session_a.last_clean_oid = Some("aaaa".to_string());
    session_a.checkpoints = vec![
        forgeql_core::session::Checkpoint {
            name: "txn-a".to_string(),
            oid: "bbbb".to_string(),
            pre_txn_oid: "aaaa".to_string(),
            created: Vec::new(),
        },
        forgeql_core::session::Checkpoint {
            name: "txn-b".to_string(),
            oid: "cccc".to_string(),
            pre_txn_oid: "bbbb".to_string(),
            created: Vec::new(),
        },
    ];

    // Save to disk.
    forgeql_core::session::checkpoint_file::save(&session_a, dir.path()).expect("save");

    let checkpoint_file = dir.path().join(".forgeql-checkpoints");
    assert!(
        checkpoint_file.exists(),
        ".forgeql-checkpoints must exist after save"
    );

    // Restore into a fresh session — the HEAD check uses the top checkpoint oid.
    let mut session_b = Session::new(
        "sid2",
        "user",
        dir.path().to_path_buf(),
        "src",
        "branch",
        &reg,
    );
    forgeql_core::session::checkpoint_file::try_restore(&mut session_b, dir.path(), "cccc");

    assert_eq!(
        session_b.checkpoints.len(),
        2,
        "both checkpoints must be restored"
    );
    assert_eq!(session_b.checkpoints[0].name, "txn-a");
    assert_eq!(session_b.checkpoints[1].name, "txn-b");
    assert_eq!(session_b.last_clean_oid.as_deref(), Some("aaaa"));
}

/// When the stored HEAD does not match the current HEAD, `try_restore` must
/// leave the session's checkpoint stack empty (stale file → discard).
#[test]
fn stale_checkpoint_file_is_discarded() {
    let dir = tempdir().expect("tempdir");
    let reg = common::make_registry();

    let mut session_a = Session::new(
        "sid",
        "user",
        dir.path().to_path_buf(),
        "src",
        "branch",
        &reg,
    );
    session_a.checkpoints = vec![forgeql_core::session::Checkpoint {
        name: "txn-a".to_string(),
        oid: "old-oid".to_string(),
        pre_txn_oid: "base-oid".to_string(),
        created: Vec::new(),
    }];
    forgeql_core::session::checkpoint_file::save(&session_a, dir.path()).expect("save");

    // Restore with a DIFFERENT current_head — file must be discarded.
    let mut session_b = Session::new(
        "sid2",
        "user",
        dir.path().to_path_buf(),
        "src",
        "branch",
        &reg,
    );
    forgeql_core::session::checkpoint_file::try_restore(
        &mut session_b,
        dir.path(),
        "new-oid-head-moved",
    );

    assert!(
        session_b.checkpoints.is_empty(),
        "stale checkpoint file must be discarded; checkpoints should be empty"
    );
}

// -----------------------------------------------------------------------
// Engine-level integration tests
// -----------------------------------------------------------------------

/// `BEGIN TRANSACTION` writes `.forgeql-checkpoints`.
/// `COMMIT MESSAGE` removes it.
#[test]
fn commit_clears_checkpoint_file() {
    let mut t = engine_with_git_session();

    let checkpoint_file = t.workspace().join(".forgeql-checkpoints");
    assert!(
        !checkpoint_file.exists(),
        "file must not exist before BEGIN"
    );

    let r = t.exec("BEGIN TRANSACTION 'txn-a'");
    assert!(matches!(r, ForgeQLResult::BeginTransaction(_)));
    assert!(checkpoint_file.exists(), "file must exist after BEGIN");

    t.exec("COMMIT MESSAGE 'clean commit'");
    assert!(
        !checkpoint_file.exists(),
        ".forgeql-checkpoints must be removed after COMMIT"
    );
}

/// Nested checkpoints: BEGIN A → BEGIN B → ROLLBACK B → ROLLBACK A.
/// The file must be updated after each ROLLBACK, and removed when the
/// stack reaches zero (though currently ROLLBACK does not delete the file
/// when the stack is empty — that is acceptable and tested here).
#[test]
fn nested_checkpoints_rollback() {
    let mut t = engine_with_git_session();
    let checkpoint_file = t.workspace().join(".forgeql-checkpoints");

    t.exec("BEGIN TRANSACTION 'txn-a'");
    assert!(
        checkpoint_file.exists(),
        "file must exist after first BEGIN"
    );

    t.exec("BEGIN TRANSACTION 'txn-b'");
    assert!(
        checkpoint_file.exists(),
        "file must still exist after second BEGIN"
    );

    let r = t.exec("ROLLBACK TRANSACTION 'txn-b'");
    assert!(
        matches!(r, ForgeQLResult::Rollback(_)),
        "ROLLBACK txn-b failed"
    );
    // File still present — txn-a is still in the stack.
    assert!(
        checkpoint_file.exists(),
        "file must persist after partial ROLLBACK"
    );

    let r = t.exec("ROLLBACK TRANSACTION 'txn-a'");
    assert!(
        matches!(r, ForgeQLResult::Rollback(_)),
        "ROLLBACK txn-a failed"
    );
}

/// `SHOW COMMITS` reports every commit the session made, not the size of the
/// page a `LIMIT` or an `OFFSET` left it holding.
///
/// The count is taken before the clauses cut the page. Taking it after is the
/// "an early exit that assumes LIMIT bounds the answer" shape the
/// `total_counts_the_answer` golden suite exists to kill, and this is the one
/// verb that suite cannot reach: `SHOW COMMITS` counts commits the session
/// itself made, a golden session makes none, and one that committed would meet
/// the commit gate. So the pin lives here, where a session can commit.
#[test]
fn show_commits_reports_every_commit_not_the_page_size() {
    let mut t = engine_with_git_session();

    // `register_local_session` names the session branch `test-branch`, which the
    // fixture's repo does not hold — and `SHOW COMMITS` counts commits since
    // exactly that ref. Point it at the initial commit so the ones below are
    // what the verb has to count.
    {
        let repo = git2::Repository::open(t.workspace()).expect("open fixture repo");
        let head = repo.head().expect("HEAD").peel_to_commit().expect("commit");
        repo.branch("test-branch", &head, true).expect("branch");
    }

    // One past the default page, so the truncate that page comes from is
    // reached at all. A handful of commits never crosses it, and a case that
    // stays inside it stays green with the default paging deleted outright.
    const MADE: usize = 21;
    const DEFAULT_PAGE: usize = 20;
    const SKIP: usize = 19;

    for i in 1..=MADE {
        fs::write(
            t.workspace().join(format!("note_{i}.txt")),
            format!("{i}\n"),
        )
        .expect("write a file to commit");
        t.exec(&format!("COMMIT MESSAGE 'commit number {i}'"));
    }

    let ForgeQLResult::Query(all) = t.exec("SHOW COMMITS") else {
        panic!("SHOW COMMITS did not answer with a query result");
    };
    assert_eq!(
        all.results.len(),
        DEFAULT_PAGE,
        "with no LIMIT written the page is the session's find_limit"
    );
    assert_eq!(
        all.total, MADE,
        "and the default page does not clip the count"
    );

    let ForgeQLResult::Query(page) = t.exec("SHOW COMMITS LIMIT 5") else {
        panic!("SHOW COMMITS LIMIT 5 did not answer with a query result");
    };
    assert_eq!(
        page.results.len(),
        5,
        "the page holds what the LIMIT asked for"
    );
    assert_eq!(
        page.total, MADE,
        "and the total is still every commit — a total equal to the page size \
         tells an agent it has seen everything when it has seen five of many"
    );

    // An OFFSET cuts the same answer from the other end, and the count is taken
    // before it as well: rows a skip removed are still part of the answer.
    let ForgeQLResult::Query(skipped) = t.exec(&format!("SHOW COMMITS OFFSET {SKIP}")) else {
        panic!("SHOW COMMITS OFFSET did not answer with a query result");
    };
    assert_eq!(
        skipped.results.len(),
        MADE - SKIP,
        "the skip leaves the tail of the answer"
    );
    assert_eq!(
        skipped.total, MADE,
        "and an OFFSET is counted before the cut as much as a LIMIT is"
    );
}
