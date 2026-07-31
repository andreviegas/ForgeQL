//! Gate tests for USE resume staleness — a re-`USE` of an existing alias must
//! never silently serve a base that `REFRESH SOURCE` has moved on.
//!
//! The incident shape these tests pin down: an agent opened a session while
//! the bare repo lagged its origin, the operator ran `REFRESH SOURCE`, the
//! agent re-issued the same `USE … AS '<alias>'` — and got the pre-REFRESH
//! snapshot back, with the response's `base_commit` claiming the new head.
//!
//! Run with: `cargo test -p forgeql-core --test session_resume_staleness`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    unused_results
)]

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use forgeql_core::auth::{AuthContext, auth};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::parser;
use forgeql_core::result::{ForgeQLResult, QueryResult, SourceOpResult};
use forgeql_core::session::SessionCoords;
use tempfile::tempdir;

mod common;

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn exec(engine: &mut ForgeQLEngine, session_id: Option<&str>, fql: &str) -> ForgeQLResult {
    let ops = parser::parse(fql).expect("parse");
    let op = ops.first().expect("op");
    let coords = session_id.map(|s| SessionCoords::from_session_id(s).expect("valid session_id"));
    engine
        .execute(auth(AuthContext::Tester), coords.as_ref(), op)
        .result
        .expect("execute")
}

fn exec_use(engine: &mut ForgeQLEngine, fql: &str) -> SourceOpResult {
    match exec(engine, None, fql) {
        ForgeQLResult::SourceOp(op) => op,
        other => panic!("expected SourceOp from USE, got {other:?}"),
    }
}

/// Create a non-bare git repo at `dir` with an initial commit containing the
/// `motor_control` fixtures.
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
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(0, 0)).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }
    repo
}

fn head_branch(repo: &git2::Repository) -> String {
    repo.head()
        .unwrap()
        .shorthand()
        .unwrap_or("master")
        .to_string()
}

/// Append a new function to `motor_control.cpp` in the origin repo and commit.
fn commit_new_function(src_dir: &Path, fn_name: &str) {
    let repo = git2::Repository::open(src_dir).unwrap();
    let file = src_dir.join("motor_control.cpp");
    let mut content = fs::read_to_string(&file).unwrap();
    writeln!(content, "\nvoid {fn_name}() {{}}").unwrap();
    fs::write(&file, content).unwrap();

    let mut index = repo.index().unwrap();
    index.add_path(Path::new("motor_control.cpp")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(1, 0)).unwrap();
    let parent = repo.head().unwrap().peel_to_commit().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "add fn", &tree, &[&parent])
        .unwrap();
}

/// Move the bare clone's `branch` to origin's current head — what
/// `REFRESH SOURCE` does, without going through the admin-gated statement.
fn sync_bare_from_origin(data_dir: &Path, branch: &str) -> String {
    let bare = git2::Repository::open_bare(data_dir.join("mysrc.git")).unwrap();
    let mut remote = bare.find_remote("origin").unwrap();
    remote.fetch(&[branch], None, None).unwrap();
    let fetched = bare
        .find_reference("FETCH_HEAD")
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    bare.reference(
        &format!("refs/heads/{branch}"),
        fetched,
        true,
        "test: sync from origin",
    )
    .unwrap();
    fetched.to_string()
}

fn find_symbol_rows(engine: &mut ForgeQLEngine, sess: &str, name: &str) -> usize {
    let fql = format!("FIND symbols WHERE name = '{name}'");
    match exec(engine, Some(sess), &fql) {
        ForgeQLResult::Query(QueryResult { results, .. }) => results.len(),
        other => panic!("unexpected result: {other:?}"),
    }
}

/// Boot an engine over a fresh clone and open the initial session.
fn setup() -> (
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

    let create_fql = format!("CREATE SOURCE 'mysrc' FROM '{}'", src_dir.path().display());
    exec(&mut engine, None, &create_fql);
    let use_fql = format!("USE mysrc.{branch} AS 'sess'");
    let first = exec_use(&mut engine, &use_fql);
    assert!(!first.resumed, "first USE must not be a resume");

    let wt_path = SessionCoords::user_worktrees_root(data_dir.path(), auth(AuthContext::Tester))
        .join(format!("mysrc.{branch}.sess"));
    let session_token =
        SessionCoords::new(auth(AuthContext::Tester), "mysrc", &branch, "sess").to_session_id();

    (engine, session_token, branch, wt_path, data_dir, src_dir)
}

// -----------------------------------------------------------------------
// Gate test 1 — the incident: re-USE after the base moved must NOT resume
// -----------------------------------------------------------------------

#[test]
fn re_use_after_base_move_serves_the_new_base() {
    let (mut engine, sess, branch, _wt, data_dir, src_dir) = setup();
    assert_eq!(find_symbol_rows(&mut engine, &sess, "refreshedFn"), 0);

    // Origin gains a commit; the bare syncs (= REFRESH SOURCE).
    commit_new_function(src_dir.path(), "refreshedFn");
    let new_head = sync_bare_from_origin(data_dir.path(), &branch);

    // Re-USE the SAME alias: the in-memory session observed the old head, so
    // it must be evicted and rebuilt at the new one — not resumed.
    let second = exec_use(&mut engine, &format!("USE mysrc.{branch} AS 'sess'"));
    // (`resumed` here means "the worktree was reused" — the discriminator for
    // the in-memory fast path is the message, and the truth the incident was
    // about is `base_commit` + queryable content.)
    assert!(
        second
            .message
            .as_deref()
            .is_none_or(|m| !m.contains("in-memory")),
        "a session whose base moved under REFRESH must not take the in-memory \
         resume path, got: {:?}",
        second.message
    );
    assert_eq!(
        second.base_commit.as_deref(),
        Some(new_head.as_str()),
        "the rebuilt session must be based on the refreshed head"
    );
    assert!(
        find_symbol_rows(&mut engine, &sess, "refreshedFn") > 0,
        "content from the refreshed base must be queryable after re-USE"
    );
}

// -----------------------------------------------------------------------
// Gate test 2 — unchanged base still resumes (no eviction churn)
// -----------------------------------------------------------------------

#[test]
fn re_use_with_unmoved_base_resumes() {
    let (mut engine, _sess, branch, _wt, _data_dir, _src_dir) = setup();

    let second = exec_use(&mut engine, &format!("USE mysrc.{branch} AS 'sess'"));
    assert!(
        second.resumed,
        "an unchanged base must resume the in-memory session"
    );
    assert!(
        second
            .message
            .as_deref()
            .is_some_and(|m| m.contains("in-memory")),
        "an unchanged base must take the in-memory fast path, got: {:?}",
        second.message
    );
    assert!(
        second.base_commit.is_some(),
        "a resumed USE must still report the session's actual base commit"
    );
}

// -----------------------------------------------------------------------
// Gate test 3 — local modifications survive the eviction rebuild, and the
// response reports the REAL base instead of pretending it moved
// -----------------------------------------------------------------------

#[test]
fn re_use_after_base_move_preserves_dirty_worktree_and_reports_truthfully() {
    let (mut engine, sess, branch, wt_path, data_dir, src_dir) = setup();

    // Uncommitted local edit in the session worktree.
    let edited = wt_path.join("motor_control.cpp");
    let mut content = fs::read_to_string(&edited).unwrap();
    content.push_str("\nvoid localDirtyFn() {}\n");
    fs::write(&edited, content).unwrap();

    // Base moves under the session.
    commit_new_function(src_dir.path(), "refreshedFn");
    let new_head = sync_bare_from_origin(data_dir.path(), &branch);

    let second = exec_use(&mut engine, &format!("USE mysrc.{branch} AS 'sess'"));
    // The dirty worktree cannot be fast-forwarded; the session keeps its real
    // checkout and must say so — reporting the refreshed head here is exactly
    // the lie that confused the incident's agent.
    assert_ne!(
        second.base_commit.as_deref(),
        Some(new_head.as_str()),
        "a preserved dirty worktree must not claim the refreshed head as base"
    );
    let edited_after = fs::read_to_string(&edited).unwrap();
    assert!(
        edited_after.contains("localDirtyFn"),
        "uncommitted local edits must survive the eviction rebuild"
    );
    assert!(
        find_symbol_rows(&mut engine, &sess, "localDirtyFn") > 0,
        "the rebuilt session must index the preserved local edit"
    );
}

// -----------------------------------------------------------------------
// Gate test 4 — a commit-hex base re-USE resumes (immutable base can't move)
// -----------------------------------------------------------------------

#[test]
fn re_use_of_commit_hex_base_resumes() {
    let (mut engine, _sess, branch, _wt, data_dir, _src_dir) = setup();

    let bare = git2::Repository::open_bare(data_dir.path().join("mysrc.git")).unwrap();
    let hex = bare
        .find_branch(&branch, git2::BranchType::Local)
        .unwrap()
        .into_reference()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();
    drop(bare);

    let first = exec_use(&mut engine, &format!("USE mysrc.{hex} AS 'hexsess'"));
    assert!(!first.resumed);
    assert_eq!(first.base_commit.as_deref(), Some(hex.as_str()));

    // Same hex, same alias: a hex is immutable, so this must resume — the old
    // `branch_head`-based check could never even evaluate a hex base.
    let second = exec_use(&mut engine, &format!("USE mysrc.{hex} AS 'hexsess'"));
    assert!(
        second
            .message
            .as_deref()
            .is_some_and(|m| m.contains("in-memory")),
        "same-hex re-USE must resume the in-memory session, got: {:?}",
        second.message
    );
    assert_eq!(
        second.base_commit.as_deref(),
        Some(hex.as_str()),
        "a resumed hex session must report the hex it is built on"
    );
}
