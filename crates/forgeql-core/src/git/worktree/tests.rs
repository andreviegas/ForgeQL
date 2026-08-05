//! Unit tests for the per-session worktree lifecycle: create, resume, remove
//! and teardown, plus the legacy-link compatibility shim.

use super::*;
use git2::{BranchType, Repository};
use tempfile::tempdir;

/// Initialise a normal repo, commit one file, bare-clone it, and return
/// the bare repo path so tests can add/remove worktrees.
fn make_bare_repo(dir: &Path) -> PathBuf {
    let src = dir.join("source");
    let repo = git2::Repository::init(&src).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    drop(cfg);

    std::fs::write(src.join("hello.cpp"), b"int main(){}\n").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("hello.cpp")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(0, 0)).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();

    let bare = dir.join("bare.git");
    git2::build::RepoBuilder::new()
        .bare(true)
        .clone(src.to_str().unwrap(), &bare)
        .unwrap();
    bare
}

/// Determine the default branch name in the repo at `path`
/// (either "main" or "master" depending on git config).
fn default_branch(path: &Path) -> String {
    let repo = Repository::open_bare(path).unwrap();

    repo.branches(Some(BranchType::Local))
        .unwrap()
        .find_map(|b| {
            let (br, _) = b.ok()?;
            br.name().ok()?.map(str::to_owned)
        })
        .expect("bare repo must have one branch")
}

#[test]
fn create_worktree_roundtrip() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);
    let wt_path = tmp.path().join("wt-session1");

    let info = create(&bare, "session1", &branch, &wt_path, None).unwrap();

    assert_eq!(info.name, "session1");
    assert_eq!(info.path, wt_path);
    assert_eq!(info.branch.as_deref(), Some(branch.as_str()));
    assert!(!info.is_locked);
    assert!(wt_path.exists());
}

#[test]
fn list_includes_created_worktree() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);
    let wt_path = tmp.path().join("wt-list");

    create(&bare, "listtest", &branch, &wt_path, None).unwrap();
    let worktrees = list(&bare).unwrap();
    assert!(
        worktrees.iter().any(|w| w.name == "listtest"),
        "newly created worktree must appear in list"
    );
}

#[test]
fn remove_worktree_cleans_up() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);
    let wt_path = tmp.path().join("wt-remove");

    create(&bare, "removetest", &branch, &wt_path, None).unwrap();
    assert!(wt_path.exists());

    remove(&bare, "removetest").unwrap();
    assert!(
        !wt_path.exists(),
        "worktree directory must be removed from disk"
    );
}

#[test]
fn teardown_worktree_removes_dir_and_registration() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);
    let wt_path = tmp.path().join("wt-teardown");

    create(&bare, "teardowntest", &branch, &wt_path, None).unwrap();
    assert!(wt_path.exists());

    crate::session::teardown_worktree(tmp.path(), &wt_path, "teardowntest");

    assert!(
        !wt_path.exists(),
        "teardown must remove the worktree directory from disk"
    );
    assert!(
        list(&bare)
            .unwrap()
            .iter()
            .all(|w| w.name != "teardowntest"),
        "teardown must remove the git worktree registration"
    );
}

/// Regression: explicit teardown must delete the real `fql/…` session
/// branch created by `USE … AS`, not just the worktree. Reconstructing the
/// name as `forgeql/<wt_name>` never matched the custom branch, so the
/// branch was orphaned in the bare repo (accumulating frozen/gt-* refs).
#[test]
fn teardown_deletes_custom_session_branch() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);
    // Mirror the engine: worktree NAME is the dotted `worktree_dir()`, while
    // the checked-out BRANCH is the slashed `git_branch()`.
    let wt_name = "forgeql-pub.frozen.gt-1-rw-0";
    let wt_path = tmp.path().join(wt_name);
    let session_branch = "fql/anonymous/forgeql-pub/frozen/gt-1-rw-0";

    create(&bare, wt_name, &branch, &wt_path, Some(session_branch)).unwrap();
    let repo = Repository::open_bare(&bare).unwrap();
    assert!(
        repo.find_branch(session_branch, BranchType::Local).is_ok(),
        "custom session branch must exist after create"
    );
    drop(repo);

    crate::session::teardown_worktree(tmp.path(), &wt_path, wt_name);

    assert!(!wt_path.exists(), "teardown must remove the worktree dir");
    let repo = Repository::open_bare(&bare).unwrap();
    assert!(
        repo.find_branch(session_branch, BranchType::Local).is_err(),
        "teardown must delete the custom session branch, not orphan it"
    );
}

/// `remove_with_branch` is the single teardown path: it must remove the
/// worktree AND delete the custom `fql/…` branch when the exact name is
/// passed, so no caller can leak a branch by forgetting the second step.
#[test]
fn remove_with_branch_removes_worktree_and_branch() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);
    let wt_name = "forgeql-pub.main.warm-x";
    let wt_path = tmp.path().join(wt_name);
    let session_branch = "fql/__warm__/main/deadbeef0000";

    create(&bare, wt_name, &branch, &wt_path, Some(session_branch)).unwrap();

    remove_with_branch(&bare, &wt_path, wt_name, Some(session_branch)).unwrap();

    assert!(!wt_path.exists(), "worktree dir must be removed");
    let repo = Repository::open_bare(&bare).unwrap();
    assert!(
        repo.find_branch(session_branch, BranchType::Local).is_err(),
        "remove_with_branch must delete the backing branch, not orphan it"
    );
}

#[test]
fn commit_based_session_creates_and_tears_down_cleanly() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);

    // Resolve the commit the branch points at, then base the session on that
    // hash directly (as `USE source.<commit-hash>` does) rather than on a name.
    let repo = Repository::open_bare(&bare).unwrap();
    let base_oid = repo
        .find_branch(&branch, BranchType::Local)
        .unwrap()
        .into_reference()
        .peel_to_commit()
        .unwrap()
        .id()
        .to_string();
    drop(repo);

    let wt_name = "bare.commit-x";
    let wt_path = tmp.path().join(wt_name);
    let session_branch = format!("fql/__commit__/{base_oid}/x");

    let info = create(&bare, wt_name, &base_oid, &wt_path, Some(&session_branch)).unwrap();
    assert_eq!(
        info.base_commit.as_deref(),
        Some(base_oid.as_str()),
        "a commit-based session must report the commit it resolved"
    );
    assert!(wt_path.exists(), "worktree dir must be created");

    remove_with_branch(&bare, &wt_path, wt_name, Some(&session_branch)).unwrap();
    assert!(!wt_path.exists(), "worktree dir must be removed");
    let repo = Repository::open_bare(&bare).unwrap();
    assert!(
        repo.find_branch(&session_branch, BranchType::Local)
            .is_err(),
        "teardown must delete the hex-based session branch, not orphan it"
    );
}

/// When no branch is known, `remove_with_branch` reads the live HEAD before
/// removal — so an auto `forgeql/<name>` session branch is still deleted.
#[test]
fn remove_with_branch_resolves_live_head_when_unknown() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);
    let wt_path = tmp.path().join("wt-auto");

    create(&bare, "autotest", &branch, &wt_path, None).unwrap();

    remove_with_branch(&bare, &wt_path, "autotest", None).unwrap();

    let repo = Repository::open_bare(&bare).unwrap();
    assert!(
        repo.find_branch("forgeql/autotest", BranchType::Local)
            .is_err(),
        "auto session branch must be deleted via live-HEAD resolution"
    );
}

#[test]
fn invalid_branch_create_fails() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let wt_path = tmp.path().join("wt-bad");

    let result = create(&bare, "bad", "nonexistent-branch", &wt_path, None);
    assert!(result.is_err());
}

/// Regression test for the "already checked out" bug found during pisco-ci
/// lab testing: two `USE pisco-code.main` calls must both succeed, each
/// getting its own isolated worktree.  The fix creates a per-session local
/// branch `forgeql/<session_id>` so the original branch (`main`) is never
/// exclusively checked out in any worktree.
#[test]
fn two_sessions_same_branch_succeed() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);
    let wt1 = tmp.path().join("wt-s1");
    let wt2 = tmp.path().join("wt-s2");

    create(&bare, "s1", &branch, &wt1, None).expect("first session must succeed");
    create(&bare, "s2", &branch, &wt2, None)
        .expect("second session on same branch must also succeed");

    // Both worktrees must exist and be independent directories.
    assert!(wt1.exists(), "first worktree directory must exist");
    assert!(wt2.exists(), "second worktree directory must exist");
    assert_ne!(wt1, wt2);

    // The per-session branches must exist in the bare repo.
    let repo = Repository::open_bare(&bare).unwrap();
    assert!(
        repo.find_branch("forgeql/s1", BranchType::Local).is_ok(),
        "forgeql/s1 branch must exist"
    );
    assert!(
        repo.find_branch("forgeql/s2", BranchType::Local).is_ok(),
        "forgeql/s2 branch must exist"
    );
}

/// `delete_session_branch` must remove the `forgeql/<id>` branch after
/// `remove()`.  Calling it when the branch is already gone must not error.
#[test]
fn delete_session_branch_cleans_up_and_is_idempotent() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);
    let wt = tmp.path().join("wt-del");

    create(&bare, "sdel", &branch, &wt, None).unwrap();
    remove(&bare, "sdel").unwrap();
    delete_session_branch(&bare, "sdel").expect("first delete must succeed");
    delete_session_branch(&bare, "sdel").expect("second delete (idempotent) must succeed");

    let repo = Repository::open_bare(&bare).unwrap();
    assert!(
        repo.find_branch("forgeql/sdel", BranchType::Local).is_err(),
        "branch must be gone after delete"
    );
}

/// `USE … AS 'custom/branch'` — the worktree must be created with the
/// exact branch name supplied, not the auto-generated `forgeql/<name>`.
#[test]
fn create_with_custom_branch_uses_supplied_name() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);
    let wt_path = tmp.path().join("wt-custom");

    create(
        &bare,
        "agent-refactor",
        &branch,
        &wt_path,
        Some("agent/refactor-signals"),
    )
    .unwrap();

    let repo = Repository::open_bare(&bare).unwrap();
    assert!(
        repo.find_branch("agent/refactor-signals", BranchType::Local)
            .is_ok(),
        "custom branch must exist in the bare repo"
    );
    assert!(
        repo.find_branch("forgeql/agent-refactor", BranchType::Local)
            .is_err(),
        "auto-generated forgeql/ branch must NOT be created when custom_branch is set"
    );
    assert!(wt_path.exists(), "worktree directory must exist");
}

/// Regression test: `USE pisco-code.main AS 'agent/task'` after a server
/// restart.  The branch and worktree directory already exist from the
/// previous session.  `create()` must succeed by reusing both.
#[test]
fn create_resumes_when_branch_and_worktree_exist() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);
    let wt_path = tmp.path().join("wt-resume");

    // First call — creates the branch and worktree normally.
    create(&bare, "wt-resume", &branch, &wt_path, Some("agent/my-task")).unwrap();
    assert!(wt_path.exists());

    let repo = Repository::open_bare(&bare).unwrap();
    assert!(repo.find_branch("agent/my-task", BranchType::Local).is_ok());

    // Second call — simulates server restart: same name, same branch, same path.
    // Must NOT fail with "branch already exists" or "path already in use".
    let info = create(&bare, "wt-resume", &branch, &wt_path, Some("agent/my-task"))
        .expect("second create (resume) must succeed");
    assert_eq!(info.path, wt_path);
    assert!(wt_path.exists());
}

/// Same resume scenario but with auto-generated `forgeql/<name>` branches.
#[test]
fn create_resumes_auto_branch_after_restart() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);
    let wt_path = tmp.path().join("wt-auto-resume");

    create(&bare, "s-auto", &branch, &wt_path, None).unwrap();
    assert!(wt_path.exists());

    // Second call with same parameters — must succeed.
    let info =
        create(&bare, "s-auto", &branch, &wt_path, None).expect("auto-branch resume must succeed");
    assert_eq!(info.path, wt_path);
    assert_eq!(info.path, wt_path);
}

/// Composite key test: same `as_branch` alias but different base branches
/// must produce independent worktrees and git branches with no collision.
/// This validates the engine's `branch.alias` / `fql/branch/alias` scheme.
///
/// The fql/ prefix is required because git loose refs store branch names as
/// paths under refs/heads/.  If a branch named `main` already exists at
/// refs/heads/main (a file), creating `main/fix-comments` would require
/// refs/heads/main to be a directory — which git rejects.  The fql/ namespace
/// sidesteps this entirely: refs/heads/fql/main/fix-comments is unambiguous.
#[test]
fn same_alias_different_base_branch_no_collision() {
    let tmp = tempdir().unwrap();
    let bare = make_bare_repo(tmp.path());
    let branch = default_branch(&bare);
    let wt_main = tmp.path().join("main.fix-comments");
    let wt_dev = tmp.path().join("dev.fix-comments");

    // Simulates: USE source.main AS 'fix-comments'
    create(
        &bare,
        "main.fix-comments",
        &branch,
        &wt_main,
        Some("fql/main/fix-comments"),
    )
    .expect("main-based worktree must succeed");

    // Simulates: USE source.dev AS 'fix-comments' — dev doesn't exist so we
    // reuse the same branch for this test, but wt_name and git branch differ.
    create(
        &bare,
        "dev.fix-comments",
        &branch,
        &wt_dev,
        Some("fql/dev/fix-comments"),
    )
    .expect("dev-based worktree must succeed");

    assert!(wt_main.exists(), "main worktree must exist");
    assert!(wt_dev.exists(), "dev worktree must exist");
    assert_ne!(wt_main, wt_dev, "worktree paths must differ");

    let repo = Repository::open_bare(&bare).unwrap();
    assert!(
        repo.find_branch("fql/main/fix-comments", BranchType::Local)
            .is_ok(),
        "fql/main/fix-comments branch must exist"
    );
    assert!(
        repo.find_branch("fql/dev/fix-comments", BranchType::Local)
            .is_ok(),
        "fql/dev/fix-comments branch must exist"
    );
}

/// Regression test for the cross-source corruption bug fixed in 0.38.2.
/// Pre-fix, `create()` resumed any pre-existing directory at
/// `worktree_path` without checking which bare repo it belonged to —
/// so two sources whose worktree paths collided on disk would silently
/// share a worktree.  The fix verifies the gitdir backlink and refuses
/// to reuse a worktree that points to a different bare repo.
#[test]
fn create_refuses_worktree_belonging_to_different_bare_repo() {
    let tmp = tempdir().unwrap();
    let bare_a = make_bare_repo(&tmp.path().join("a"));
    let bare_b = make_bare_repo(&tmp.path().join("b"));
    let branch_a = default_branch(&bare_a);
    let branch_b = default_branch(&bare_b);
    let shared_path = tmp.path().join("shared.wt");

    // First source legitimately creates the worktree.
    create(&bare_a, "shared.wt", &branch_a, &shared_path, None)
        .expect("first source must create worktree");
    assert!(shared_path.exists());

    // Second source tries to use the same worktree path — must fail loudly
    // rather than silently hand it the wrong source's worktree.
    let result = create(&bare_b, "shared.wt", &branch_b, &shared_path, None);
    assert!(
        result.is_err(),
        "create() must refuse a worktree that belongs to a different bare repo"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("does not belong to bare repo"),
        "error must mention cross-source corruption, got: {err_msg}"
    );
}

#[cfg(unix)]
#[test]
fn legacy_link_created_and_removed_with_worktree() {
    let tmp = tempdir().unwrap();
    let worktrees = tmp.path().join("worktrees");
    let wt_path = worktrees.join("anonymous").join("src.main.alias");
    std::fs::create_dir_all(&wt_path).unwrap();
    let legacy = worktrees.join("src.main.alias");

    ensure_legacy_link(&legacy, &wt_path);
    assert!(legacy.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(
        legacy.canonicalize().unwrap(),
        wt_path.canonicalize().unwrap()
    );

    // Idempotent: a second call leaves the existing link alone.
    ensure_legacy_link(&legacy, &wt_path);
    assert!(legacy.symlink_metadata().unwrap().file_type().is_symlink());

    remove_legacy_link(&wt_path);
    assert!(legacy.symlink_metadata().is_err());
}

#[cfg(unix)]
#[test]
fn legacy_link_never_clobbers_real_directory_or_foreign_link() {
    let tmp = tempdir().unwrap();
    let worktrees = tmp.path().join("worktrees");
    let wt_path = worktrees.join("anonymous").join("src.main.alias");
    std::fs::create_dir_all(&wt_path).unwrap();

    // A real old-layout directory at the legacy path is left untouched.
    let legacy = worktrees.join("src.main.alias");
    std::fs::create_dir_all(&legacy).unwrap();
    ensure_legacy_link(&legacy, &wt_path);
    assert!(legacy.symlink_metadata().unwrap().file_type().is_dir());
    remove_legacy_link(&wt_path);
    assert!(legacy.symlink_metadata().unwrap().file_type().is_dir());
    std::fs::remove_dir(&legacy).unwrap();

    // A link owned by another user's session is not removed.
    let other = worktrees.join("bob").join("src.main.alias");
    std::fs::create_dir_all(&other).unwrap();
    std::os::unix::fs::symlink(std::path::Path::new("bob").join("src.main.alias"), &legacy)
        .unwrap();
    remove_legacy_link(&wt_path);
    assert!(legacy.symlink_metadata().unwrap().file_type().is_symlink());
}
