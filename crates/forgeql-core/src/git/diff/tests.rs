//! Unit tests for the git diff readouts: the HEAD-to-worktree diff and what it
//! leaves out, the worktree diff that does include untracked files, the runtime
//! and control files both exclude, and the source-changes classification that
//! tells research apart from work.

use std::path::Path;

use tempfile::tempdir;

use super::*;
use crate::git::testutil::make_normal_repo;
#[test]
fn diff_head_to_worktree_empty_for_clean_repo() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let _repo = make_normal_repo(dir);

    let paths = diff_head_to_worktree(dir).unwrap();
    assert!(paths.is_empty(), "clean repo must report no dirty files");
}

#[test]
fn diff_head_to_worktree_detects_modified_tracked_file() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let _repo = make_normal_repo(dir);

    // Modify the tracked file without staging or committing.
    std::fs::write(dir.join("file.cpp"), b"int main() { return 42; }\n").unwrap();

    let paths = diff_head_to_worktree(dir).unwrap();
    assert!(
        paths.contains(&dir.join("file.cpp")),
        "modified tracked file must appear in the dirty list"
    );
}

#[test]
fn diff_head_to_worktree_excludes_untracked_files() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let _repo = make_normal_repo(dir);

    // A brand-new file that has never been committed.
    std::fs::write(dir.join("new_file.cpp"), b"// untracked\n").unwrap();

    let paths = diff_head_to_worktree(dir).unwrap();
    assert!(
        !paths.contains(&dir.join("new_file.cpp")),
        "untracked file must not appear in the dirty list"
    );
}

#[test]
fn worktree_diff_includes_untracked_files() {
    // The whole point of SHOW DIFF: a reviewer must see NEW files. `git
    // diff HEAD` alone omits them (see the test above — that behaviour is
    // correct for dirty-detection but fatal for review), so worktree_diff
    // opts into untracked content explicitly.
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let _repo = make_normal_repo(dir);

    std::fs::write(dir.join("brand_new.rs"), b"fn added() {}\n").unwrap();

    let files = worktree_diff(dir).unwrap();
    let entry = files
        .iter()
        .find(|f| f.path == Path::new("brand_new.rs"))
        .expect("untracked file must appear in the diff");

    assert_eq!(entry.status, 'A', "an untracked file is an addition");
    assert_eq!(entry.added, 1, "its single line counts as added");
    assert_eq!(entry.removed, 0);
    assert!(
        entry.patch.contains("fn added() {}"),
        "the new file's content must be present, not just its name: {}",
        entry.patch
    );
}

#[test]
fn worktree_diff_reports_modified_tracked_file() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let _repo = make_normal_repo(dir);

    std::fs::write(dir.join("file.cpp"), b"int main() { return 42; }\n").unwrap();

    let files = worktree_diff(dir).unwrap();
    let entry = files
        .iter()
        .find(|f| f.path == Path::new("file.cpp"))
        .expect("modified tracked file must appear in the diff");
    assert_eq!(entry.status, 'M');
    assert!(entry.added >= 1 && entry.removed >= 1);
}

#[test]
fn worktree_diff_excludes_runtime_files() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let _repo = make_normal_repo(dir);

    std::fs::create_dir_all(dir.join(".forgeql-patches")).unwrap();
    std::fs::write(dir.join(".forgeql-patches/0001.patch"), b"noise\n").unwrap();
    std::fs::write(dir.join(".forgeql-showmore"), b"noise\n").unwrap();

    let files = worktree_diff(dir).unwrap();
    assert!(
        files
            .iter()
            .all(|f| !f.path.to_string_lossy().contains(".forgeql-")),
        "ForgeQL runtime files must never leak into a diff: {files:?}"
    );
}

#[test]
fn worktree_diff_is_empty_for_a_clean_worktree() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let _repo = make_normal_repo(dir);

    assert!(worktree_diff(dir).unwrap().is_empty());
}

#[test]
fn diff_head_to_worktree_excludes_control_files() {
    let tmp = tempdir().unwrap();
    let dir = tmp.path();
    let repo = make_normal_repo(dir);

    // Commit a ForgeQL control file so it is tracked.
    let ctrl = dir.join(".forgeql-checkpoints");
    std::fs::write(&ctrl, b"{}").unwrap();
    let mut index = repo.index().unwrap();
    index
        .add_path(std::path::Path::new(".forgeql-checkpoints"))
        .unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    {
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(1, 0)).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "add ctrl", &tree, &[&parent])
            .unwrap();
    }
    // Modify the control file in the worktree.
    std::fs::write(&ctrl, b"{updated}").unwrap();

    let paths = diff_head_to_worktree(dir).unwrap();
    assert!(
        !paths.contains(&ctrl),
        "ForgeQL control file must be excluded from the dirty list"
    );
}

/// `source_changes` drives the TTL "keep work, GC research" decision: a
/// session branch identical to its base (a research session that committed
/// nothing) reports empty, a branch with a real source commit reports the
/// changed file, and a branch that only touches control files is treated as
/// having no reviewable work.
#[test]
fn source_changes_distinguishes_research_from_work() {
    let tmp = tempdir().unwrap();
    let bare = tmp.path().join("repo.git");
    let repo = git2::Repository::init_bare(&bare).unwrap();
    let sig = git2::Signature::new("t", "t@t.com", &git2::Time::new(0, 0)).unwrap();

    // Base commit on `main` with one source file.
    let base_blob = repo.blob(b"int main(){}\n").unwrap();
    let base_oid = {
        let mut tb = repo.treebuilder(None).unwrap();
        tb.insert("a.cpp", base_blob, 0o100_644).unwrap();
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        repo.commit(Some("refs/heads/main"), &sig, &sig, "base", &tree, &[])
            .unwrap()
    };
    let base_commit = repo.find_commit(base_oid).unwrap();

    // Research branch: same commit as main → no changes.
    repo.branch("research", &base_commit, false).unwrap();
    assert!(
        source_changes(&bare, "main", "research")
            .unwrap()
            .is_empty(),
        "a research branch with no commits must report no changes"
    );

    // Work branch: a new commit that edits the source file.
    let work_oid = {
        let mut tb = repo.treebuilder(None).unwrap();
        let blob = repo.blob(b"int main(){return 1;}\n").unwrap();
        tb.insert("a.cpp", blob, 0o100_644).unwrap();
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        repo.commit(None, &sig, &sig, "work", &tree, &[&base_commit])
            .unwrap()
    };
    repo.branch("work", &repo.find_commit(work_oid).unwrap(), false)
        .unwrap();
    assert_eq!(
        source_changes(&bare, "main", "work").unwrap(),
        vec!["a.cpp".to_string()],
        "a branch with a real source commit must report the changed file"
    );

    // Control-file-only branch: identical source, extra `.forgeql-index` →
    // treated as no reviewable work.
    let ctrl_oid = {
        let mut tb = repo.treebuilder(None).unwrap();
        tb.insert("a.cpp", base_blob, 0o100_644).unwrap();
        let ctrl = repo.blob(b"index-data").unwrap();
        tb.insert(".forgeql-index", ctrl, 0o100_644).unwrap();
        let tree = repo.find_tree(tb.write().unwrap()).unwrap();
        repo.commit(None, &sig, &sig, "ctrl", &tree, &[&base_commit])
            .unwrap()
    };
    repo.branch("ctrl", &repo.find_commit(ctrl_oid).unwrap(), false)
        .unwrap();
    assert!(
        source_changes(&bare, "main", "ctrl").unwrap().is_empty(),
        "a branch touching only control files must report no reviewable work"
    );

    // The base may be given as a bare commit hash, not just a branch name.
    let base_hex = base_oid.to_string();
    assert_eq!(
        source_changes(&bare, &base_hex, "work").unwrap(),
        vec!["a.cpp".to_string()],
        "a commit-hash base must resolve and report the changed file"
    );
    assert!(
        source_changes(&bare, &base_hex, "research")
            .unwrap()
            .is_empty(),
        "a commit-hash base with no descendant changes must report none"
    );
}
