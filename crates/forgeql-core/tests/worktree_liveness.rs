//! The startup reclaim sweep must never delete a worktree another engine
//! process is creating or using.
//!
//! The incident these tests pin down: two engine processes were started on one
//! data dir, and the first statement of the second one failed with
//!
//! ```text
//! could not open '.../worktrees/anonymous/<wt>/doc/.../modem_pipes.svg'
//! for writing: No such file or directory
//! ```
//!
//! The sweeping process had deleted the checkout directory out from under a
//! `git worktree add` that was still filling it. It judged the directory
//! abandoned because the only evidence it consulted — its own in-memory
//! session maps — cannot contain another process's sessions.
//!
//! Run with: `cargo test -p forgeql-core --test worktree_liveness`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    unused_results
)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs};

use forgeql_core::auth::{AuthContext, auth};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::parser;
use forgeql_core::result::{ForgeQLResult, SourceOpResult};
use forgeql_core::session::SessionCoords;
use forgeql_core::session::liveness::{Protected, WorktreeClaim, claim_for_reclaim, claim_path};

mod common;

/// Data dir a re-executed child process should work in.
const DATA_DIR_ENV: &str = "FORGEQL_LIVENESS_DATA_DIR";
/// Branch a re-executed child process should `USE`.
const BRANCH_ENV: &str = "FORGEQL_LIVENESS_BRANCH";
/// Source registered by the parent before any child starts.
const SOURCE: &str = "mysrc";
/// Written by the session-creating child once its loop has completed. A child
/// whose test never ran exits 0 just the same, so the parent checks the marker
/// rather than the exit status alone.
const USE_MARKER: &str = "child-use-loop-finished";
/// Written by the sweeping child on its first sweep, so the parent can tell a
/// sweep that found nothing from a sweep that never happened.
const SWEEP_MARKER: &str = "child-sweep-loop-started";

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

/// Plant a worktree-shaped directory the restore sweep will consider: it has a
/// `.git` entry, a file to miss if it is deleted, and whatever sentinel the
/// caller wants it judged by.
fn plant_worktree(data_dir: &Path, wt_dir: &str, sentinel: Option<&str>) -> PathBuf {
    let wt = data_dir.join("worktrees/anonymous").join(wt_dir);
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join(".git"), "gitdir: /nonexistent").unwrap();
    fs::write(wt.join("keep.txt"), "content").unwrap();
    if let Some(body) = sentinel {
        fs::write(wt.join(".forgeql-session"), body).unwrap();
    }
    wt
}

/// A full-metadata sentinel whose last activity is far past every TTL, so the
/// restore sweep judges the session expired and reclaims it.
fn expired_sentinel(alias: &str) -> String {
    format!(
        "timestamp={}\nsource=src\nbranch=main\nalias={alias}\nuser=anonymous\n",
        now_secs().saturating_sub(200_000)
    )
}

fn sweep(data_dir: &Path) {
    let mut engine = ForgeQLEngine::new(data_dir.to_path_buf(), common::make_registry()).unwrap();
    engine.restore_sessions_from_disk();
}

// -----------------------------------------------------------------------
// The reclaim decision, at engine level
// -----------------------------------------------------------------------

/// The control: an expired session with no live owner is still reclaimed. A
/// gate that only ever answered "keep" would pass every other test here.
#[test]
fn an_expired_unclaimed_worktree_is_reclaimed() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let wt = plant_worktree(&data_dir, "src.main.gone", Some(&expired_sentinel("gone")));

    sweep(&data_dir);

    assert!(
        !wt.exists(),
        "an expired session nobody holds must still be reclaimed"
    );
}

/// The fix: the same expired worktree survives while a live process holds its
/// claim. The sweeping engine has never heard of this session — the claim is
/// the only evidence it has, and the only evidence it can have.
#[test]
fn an_expired_but_claimed_worktree_survives() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let wt = plant_worktree(&data_dir, "src.main.held", Some(&expired_sentinel("held")));

    let _claim = WorktreeClaim::acquire(&wt).expect("claim");
    sweep(&data_dir);

    assert!(
        wt.join("keep.txt").exists(),
        "a worktree a live process holds must survive a sweep that cannot see its session"
    );
}

/// A worktree still being checked out carries no sentinel yet, which used to
/// mean "orphan, prune unconditionally". It is now judged by its claim and its
/// age, and a fresh directory is left alone either way.
#[test]
fn a_newborn_sentinel_less_worktree_survives() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let wt = plant_worktree(&data_dir, "src.main.newborn", None);

    sweep(&data_dir);

    assert!(
        wt.join("keep.txt").exists(),
        "a directory a peer may still be checking out must survive the sweep"
    );
}

/// Reclaiming a worktree must not take the claim file with it: the file is the
/// lock two processes arbitrate on, and a lock destroyed by the operation it
/// guards guards nothing.
#[test]
fn reclaiming_a_worktree_leaves_its_claim_file_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let wt = plant_worktree(&data_dir, "src.main.gone", Some(&expired_sentinel("gone")));
    let claim_file = claim_path(&wt).expect("claim path");

    // Materialise the claim file, then let go so the sweep can reclaim.
    drop(WorktreeClaim::acquire(&wt).expect("claim"));
    sweep(&data_dir);

    assert!(!wt.exists(), "test setup: the worktree should be reclaimed");
    assert!(
        claim_file.exists(),
        "the claim file must outlive the worktree"
    );
}

// -----------------------------------------------------------------------
// Two real processes
// -----------------------------------------------------------------------

/// Create an origin repo with an initial commit and register it as a source in
/// `data_dir`. Returns the branch name.
fn make_source(src_dir: &Path, data_dir: &Path) -> String {
    let repo = git2::Repository::init(src_dir).expect("git init");
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    drop(cfg);

    let fixtures = common::fixtures_dir();
    fs::copy(
        fixtures.join("motor_control.h"),
        src_dir.join("motor_control.h"),
    )
    .expect("copy .h");

    let mut index = repo.index().unwrap();
    index.add_path(Path::new("motor_control.h")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    {
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(0, 0)).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }
    let branch = repo
        .head()
        .unwrap()
        .shorthand()
        .unwrap_or("master")
        .to_string();
    drop(repo);

    let mut engine = ForgeQLEngine::new(data_dir.to_path_buf(), common::make_registry()).unwrap();
    exec_source_op(
        &mut engine,
        &format!("CREATE SOURCE '{SOURCE}' FROM '{}'", src_dir.display()),
    );
    branch
}

fn exec_source_op(engine: &mut ForgeQLEngine, fql: &str) -> SourceOpResult {
    let ops = parser::parse(fql).expect("parse");
    let op = ops.first().expect("op");
    let result = engine
        .execute(auth(AuthContext::Tester), None, op)
        .result
        .unwrap_or_else(|e| panic!("{fql} failed: {e}"));
    match result {
        ForgeQLResult::SourceOp(op) => op,
        other => panic!("expected SourceOp, got {other:?}"),
    }
}

/// The wiring: a session that is still open holds its own worktree, so a sweep
/// in another process finds it claimed. Without this the claim machinery could
/// be entirely correct and simply never taken — `USE` is the only place that
/// takes one, and nothing else would notice if that call went missing.
#[test]
fn a_live_session_holds_its_worktree_and_lets_go_with_the_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let src_dir = tmp.path().join("origin");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&src_dir).unwrap();

    let branch = make_source(&src_dir, &data_dir);
    let mut engine = ForgeQLEngine::new(data_dir.clone(), common::make_registry()).unwrap();
    exec_source_op(&mut engine, &format!("USE {SOURCE}.{branch} AS 'held'"));

    let coords = SessionCoords::new(auth(AuthContext::Tester), SOURCE, &branch, "held");
    let wt = coords.worktree_path(&data_dir);
    assert!(
        wt.join("motor_control.h").exists(),
        "test setup: the checkout should be populated"
    );

    assert_eq!(
        claim_for_reclaim(&wt, 0).err(),
        Some(Protected::Claimed),
        "an open session must hold its worktree against a reclaim sweep"
    );

    // Dropping the engine drops the session, which is what releases the claim
    // — the same thing that happens when the process dies.
    drop(engine);
    assert!(
        claim_for_reclaim(&wt, 0).is_ok(),
        "the worktree must become reclaimable once no session holds it"
    );
}

/// `(data_dir, branch)` when this process is a child of the race test below,
/// `None` when the ignored test was invoked directly.
fn child_env() -> Option<(PathBuf, String)> {
    let data_dir = env::var(DATA_DIR_ENV).ok()?;
    let branch = env::var(BRANCH_ENV).ok()?;
    Some((PathBuf::from(data_dir), branch))
}

fn spawn_child(test_name: &str, data_dir: &Path, branch: &str) -> Child {
    Command::new(env::current_exe().expect("test binary path"))
        .args([test_name, "--exact", "--ignored", "--nocapture"])
        .env(DATA_DIR_ENV, data_dir)
        .env(BRANCH_ENV, branch)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn child")
}

/// Two engine processes on one data dir: one creates sessions in a loop while
/// the other sweeps for orphans in a loop. Before the liveness claim, the
/// sweep deleted a checkout mid-`git worktree add` and the `USE` failed.
///
/// A race test is evidence, never proof — it can pass on a build that
/// reintroduces the bug simply by never interleaving. The claims made here are
/// pinned deterministically by the engine-level tests above and by the decision
/// function's own tests in `session::liveness`; this one is what exercises the
/// actual two-process shape.
#[test]
fn concurrent_use_and_sweep_never_kills_a_live_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let src_dir = tmp.path().join("origin");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&src_dir).unwrap();

    let branch = make_source(&src_dir, &data_dir);

    let user = spawn_child("child_use_loop", &data_dir, &branch);
    let mut sweeper = spawn_child("child_sweep_loop", &data_dir, &branch);

    let user_out = user.wait_with_output().expect("child_use_loop");
    let swept = data_dir.join(SWEEP_MARKER).exists();
    sweeper.kill().ok();
    sweeper.wait().ok();

    assert!(
        user_out.status.success(),
        "a session-creating process lost its checkout to a concurrent sweep\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&user_out.stdout),
        String::from_utf8_lossy(&user_out.stderr),
    );
    // A child that never ran its body exits 0 too, so success alone proves
    // nothing. Both markers must be on disk or this test passed vacuously.
    assert!(
        data_dir.join(USE_MARKER).exists(),
        "the session-creating child never finished its loop\n\
         --- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&user_out.stdout),
        String::from_utf8_lossy(&user_out.stderr),
    );
    assert!(swept, "the sweeping child never ran a sweep");
}

/// Child role: open one fresh session after another, each one a brand-new
/// checkout passing through the window where it has no sentinel yet.
#[test]
#[ignore = "child process of concurrent_use_and_sweep_never_kills_a_live_checkout"]
fn child_use_loop() {
    let Some((data_dir, branch)) = child_env() else {
        return;
    };
    let mut engine = ForgeQLEngine::new(data_dir.clone(), common::make_registry()).unwrap();

    for i in 0..20 {
        let alias = format!("live{i}");
        exec_source_op(&mut engine, &format!("USE {SOURCE}.{branch} AS '{alias}'"));

        let coords = SessionCoords::new(auth(AuthContext::Tester), SOURCE, &branch, &alias);
        assert!(
            coords
                .worktree_path(&data_dir)
                .join("motor_control.h")
                .exists(),
            "checkout for '{alias}' is missing its content"
        );
    }
    fs::write(data_dir.join(USE_MARKER), "done").unwrap();
}

/// Child role: sweep for orphans as fast as the parent will let it, until the
/// parent kills it.
#[test]
#[ignore = "child process of concurrent_use_and_sweep_never_kills_a_live_checkout"]
fn child_sweep_loop() {
    let Some((data_dir, _branch)) = child_env() else {
        return;
    };
    // Bounded so a child orphaned by a dead parent cannot spin forever.
    for _ in 0..2_000 {
        sweep(&data_dir);
        fs::write(data_dir.join(SWEEP_MARKER), "started").unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
}
