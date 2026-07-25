//! What changed: change lists, dirty paths, and the diffs behind `SHOW DIFF`.
//!
//! Everything here reports on differences — between two branches, between
//! HEAD and the worktree, or for a single commit — and every path it returns
//! has already been filtered through the exclude policy in [`super::excludes`],
//! so `ForgeQL`'s own runtime files never appear in a diff.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use git2::{BranchType, Repository};

use super::excludes::{is_clean_commit_excluded, is_runtime_or_control};
use super::run_git;
use super::worktree;

/// Count of uncommitted worktree changes, ignoring `ForgeQL` runtime files.
///
/// `EXPORT PATCH` is commit-based; a non-zero count means edits exist that no
/// patch will carry, which the response surfaces as a hint.
///
/// # Errors
/// Returns an error when git cannot be spawned or `git status` fails.
pub fn uncommitted_source_changes(worktree: &Path) -> Result<usize> {
    let status = run_git(worktree, &["status", "--porcelain"])?;
    Ok(status
        .lines()
        .filter(|line| {
            // Porcelain v1: two status chars, a space, then the path
            // (possibly `old -> new` for renames — the new path decides).
            let path = line
                .get(3..)
                .unwrap_or("")
                .split(" -> ")
                .last()
                .unwrap_or("");
            let p = Path::new(path);
            !is_clean_commit_excluded(p)
                && !p.components().any(|c| {
                    matches!(c, std::path::Component::Normal(n)
                        if n.to_str().is_some_and(|s| s.starts_with(".forgeql-")))
                })
        })
        .count())
}

/// Files managed by `ForgeQL` itself that should not count as user source
/// changes when deciding whether to keep a session branch on disconnect.
/// Extend this list as new control files are introduced.
pub const FORGEQL_CONTROL_FILES: &[&str] = &[".forgeql-index", ".forgeql-session"];

/// Returns the list of changed source files between the session branch
/// and the base branch, ignoring files in [`FORGEQL_CONTROL_FILES`].
///
/// An empty list means no meaningful source changes exist.
///
/// Both `base_branch` and `session_branch` must be local branch names in
/// the bare repo at `repo_path`.
///
/// # Errors
///
/// Returns `Err` if the repository cannot be opened or either branch
/// cannot be resolved.
pub fn source_changes(
    repo_path: &Path,
    base_branch: &str,
    session_branch: &str,
) -> Result<Vec<String>> {
    let repo = Repository::open_bare(repo_path)?;

    // Resolve the base as a commit-ish (branch name or bare commit hash), so a
    // session based on `USE source.<commit-hash>` is comparable too.
    let base_tree = worktree::resolve_commit(&repo, base_branch)?.tree()?;
    let session_tree = repo
        .find_branch(session_branch, BranchType::Local)?
        .into_reference()
        .peel_to_tree()?;

    let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&session_tree), None)?;

    let mut changed = Vec::new();
    for delta in diff.deltas() {
        let path = delta
            .new_file()
            .path()
            .and_then(|p| p.to_str())
            .unwrap_or("")
            .to_string();
        if !FORGEQL_CONTROL_FILES.contains(&path.as_str()) {
            changed.push(path);
        }
    }
    Ok(changed)
}

/// Return the list of files that differ between two arbitrary commits in the
/// given repository, ignoring [`FORGEQL_CONTROL_FILES`].
///
/// Used by `ROLLBACK` to compute the minimal set of files that need to be
/// re-indexed after a `git reset --hard`, avoiding a full O(N) rebuild.
///
/// Returns an empty `Vec` when both OIDs point to identical trees (no source
/// changes between them — e.g. `BEGIN` with a clean tree → `ROLLBACK` with no
/// intervening edits, or a checkpoint commit that touches only control files).
///
/// # Errors
/// Returns `Err` if either OID cannot be resolved or peeled to a tree.
pub fn changed_files_between(
    repo: &Repository,
    from_oid: &str,
    to_oid: &str,
) -> Result<Vec<PathBuf>> {
    if from_oid == to_oid {
        return Ok(Vec::new());
    }
    let from = git2::Oid::from_str(from_oid)?;
    let to = git2::Oid::from_str(to_oid)?;
    let from_tree = repo.find_commit(from)?.tree()?;
    let to_tree = repo.find_commit(to)?.tree()?;

    let diff = repo.diff_tree_to_tree(Some(&from_tree), Some(&to_tree), None)?;

    let mut changed: Vec<PathBuf> = Vec::new();
    for delta in diff.deltas() {
        // Collect both the old and the new path so renames/deletions are
        // re-indexed correctly (the deleted side must be purged from the
        // in-memory index, the new side parsed fresh).
        if let Some(p) = delta.old_file().path()
            && !FORGEQL_CONTROL_FILES.contains(&p.to_string_lossy().as_ref())
        {
            changed.push(p.to_path_buf());
        }
        if let Some(p) = delta.new_file().path()
            && !FORGEQL_CONTROL_FILES.contains(&p.to_string_lossy().as_ref())
        {
            changed.push(p.to_path_buf());
        }
    }
    changed.sort();
    changed.dedup();
    Ok(changed)
}

/// Return the list of working-tree paths that differ from `HEAD`, ignoring
/// [`FORGEQL_CONTROL_FILES`].
///
/// Includes both staged and unstaged modifications, additions, deletions,
/// and renames. Used by `ROLLBACK` to identify files modified during a
/// transaction that need re-indexing after `git reset --hard` reverts them.
///
/// Returns an empty `Vec` when the worktree is clean.
///
/// # Errors
/// Returns `Err` if the status query fails.
pub fn dirty_paths(repo: &Repository) -> Result<Vec<PathBuf>> {
    let statuses = repo.statuses(None)?;
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in statuses.iter() {
        let Some(p) = entry.path() else { continue };
        if FORGEQL_CONTROL_FILES.contains(&p) {
            continue;
        }
        out.push(PathBuf::from(p));
        if let Some(diff) = entry.head_to_index()
            && let Some(old) = diff.old_file().path()
        {
            let s = old.to_string_lossy();
            if !FORGEQL_CONTROL_FILES.contains(&s.as_ref()) {
                out.push(old.to_path_buf());
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Returns the list of tracked files in the worktree that differ from HEAD,
/// as absolute paths under `worktree_path`.
///
/// This is the reconnect dirty-detection function (PhaseFT7): after
/// `resume_index` or `load_delta` restores the cached index, call this to
/// find files that were modified on disk but not captured in a checkpoint
/// commit.  Non-fatal caller pattern — errors should be logged and ignored.
///
/// Excludes ForgeQL-internal control files (same set as `CLEAN_COMMIT_EXCLUDED`).
/// Untracked files are out of scope and are NOT returned.
///
/// # Errors
///
/// Returns `Err` if the repository cannot be opened or the status query fails.
pub fn diff_head_to_worktree(worktree_path: &Path) -> Result<Vec<PathBuf>> {
    let repo = Repository::open(worktree_path)?;
    let mut opts = git2::StatusOptions::new();
    let _ = opts.include_untracked(false).include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts))?;
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in statuses.iter() {
        let Some(p) = entry.path() else { continue };
        if is_clean_commit_excluded(Path::new(p)) {
            continue;
        }
        out.push(worktree_path.join(p));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

// -----------------------------------------------------------------------
// SHOW DIFF — the uncommitted worktree diff, inline
// -----------------------------------------------------------------------

/// One file's worth of the uncommitted worktree diff.
#[derive(Debug, Clone)]
pub struct DiffFile {
    /// Path relative to the worktree root.
    pub path: PathBuf,
    /// Single-letter status: `A`dded (incl. untracked), `M`odified,
    /// `D`eleted, `R`enamed, `T`ypechange.
    pub status: char,
    /// Count of `+` lines in this file's hunks.
    pub added: usize,
    /// Count of `-` lines in this file's hunks.
    pub removed: usize,
    /// This file's unified-diff text (header + hunks).
    pub patch: String,
}

/// Map a libgit2 delta status to its one-letter git status code.
const fn delta_status_char(status: git2::Delta) -> char {
    match status {
        git2::Delta::Added | git2::Delta::Untracked => 'A',
        git2::Delta::Deleted => 'D',
        git2::Delta::Renamed => 'R',
        git2::Delta::Typechange => 'T',
        _ => 'M',
    }
}

/// The **uncommitted** diff of `worktree` against `HEAD`, one entry per file.
///
/// Includes staged *and* unstaged edits, and — critically — **untracked files**,
/// rendered as whole-file additions. A reviewer that could not see untracked
/// files would miss every newly added source file, which is exactly the kind of
/// silent omission a review must never suffer.
///
/// `ForgeQL` runtime files (`.forgeql-*` at any depth) and control files are
/// excluded, matching [`export_patches`].
///
/// # Errors
/// Returns an error when the worktree cannot be opened, `HEAD` cannot be peeled
/// to a tree, or the diff cannot be computed.
pub fn worktree_diff(worktree: &Path) -> Result<Vec<DiffFile>> {
    let repo = Repository::open(worktree)
        .with_context(|| format!("could not open worktree {}", worktree.display()))?;
    let head_tree = repo.head()?.peel_to_tree()?;

    let mut opts = git2::DiffOptions::new();
    let _ = opts
        .include_untracked(true)
        .show_untracked_content(true)
        .recurse_untracked_dirs(true)
        .include_typechange(true);

    let diff = repo.diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut opts))?;
    accumulate_diff_files(&diff)
}

/// Diff a commit against its first parent, in the same shape as `worktree_diff`.
///
/// Resolves `rev` (a commit hash or other commit-ish) against the repository at
/// `repo_at` and diffs the commit's tree against its first parent's, or the
/// empty tree for a root commit so an initial commit shows as all-added.
///
/// # Errors
/// Returns an error if the repo cannot be opened or `rev` is not a commit.
pub fn commit_diff(repo_at: &Path, rev: &str) -> Result<Vec<DiffFile>> {
    let repo = Repository::open(repo_at)
        .with_context(|| format!("could not open repo {}", repo_at.display()))?;
    let commit = repo
        .revparse_single(rev)
        .with_context(|| format!("SHOW DIFF OF '{rev}': not a resolvable commit"))?
        .peel_to_commit()
        .with_context(|| format!("SHOW DIFF OF '{rev}': object is not a commit"))?;
    let commit_tree = commit.tree()?;
    let parent_tree = match commit.parent(0) {
        Ok(parent) => Some(parent.tree()?),
        Err(_) => None,
    };

    let mut opts = git2::DiffOptions::new();
    let _ = opts.include_typechange(true);

    let diff = repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&commit_tree), Some(&mut opts))?;
    accumulate_diff_files(&diff)
}

/// List the commits a session branch carries over its base, newest first.
///
/// Walks `base..head`, where `head` is the worktree's current HEAD and `base` is
/// `base_ref` resolved to a commit. The fork point is an ancestor of `base` and
/// so is hidden, leaving only the session's own commits. Each entry is the
/// abbreviated hash and the commit subject line.
///
/// # Errors
/// Returns an error if the worktree repo cannot be opened, HEAD cannot be
/// resolved, or `base_ref` does not resolve to a commit.
pub fn commits_since(worktree: &Path, base_ref: &str) -> Result<Vec<(String, String)>> {
    let repo = Repository::open(worktree)
        .with_context(|| format!("could not open worktree {}", worktree.display()))?;
    let head = repo.head()?.peel_to_commit()?.id();
    let base = worktree::resolve_commit(&repo, base_ref)?.id();

    let mut walk = repo.revwalk()?;
    walk.set_sorting(git2::Sort::TIME)?;
    walk.push(head)?;
    // Hiding `base` and its ancestors leaves only commits unique to `head`.
    // When `base == head` the session has made no commits and the walk is empty.
    let _ = walk.hide(base);

    let mut out = Vec::new();
    for oid in walk {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        let short: String = oid.to_string().chars().take(12).collect();
        let subject = commit.summary().unwrap_or_default().to_string();
        out.push((short, subject));
    }
    Ok(out)
}

/// Accumulate a libgit2 patch-format diff into per-file [`DiffFile`] entries.
///
/// Shared by `worktree_diff` and `commit_diff`: libgit2 streams lines in delta
/// order, so a plain ordered Vec keyed by the delta's path is enough. Runtime
/// and control files are skipped.
fn accumulate_diff_files(diff: &git2::Diff<'_>) -> Result<Vec<DiffFile>> {
    let mut files: Vec<DiffFile> = Vec::new();

    diff.print(git2::DiffFormat::Patch, |delta, _hunk, line| {
        let path = delta
            .new_file()
            .path()
            .or_else(|| delta.old_file().path())
            .map(Path::to_path_buf)
            .unwrap_or_default();

        if is_runtime_or_control(&path) {
            return true;
        }

        if files.last().map(|f| &f.path) != Some(&path) {
            files.push(DiffFile {
                path,
                status: delta_status_char(delta.status()),
                added: 0,
                removed: 0,
                patch: String::new(),
            });
        }

        let Some(entry) = files.last_mut() else {
            return true;
        };

        match line.origin() {
            '+' => entry.added += 1,
            '-' => entry.removed += 1,
            _ => {}
        }
        // `+`/`-`/` ` carry the origin char separately from the content; file
        // and hunk headers already embed their own prefix.
        if matches!(line.origin(), '+' | '-' | ' ') {
            entry.patch.push(line.origin());
        }
        entry
            .patch
            .push_str(&String::from_utf8_lossy(line.content()));
        true
    })?;

    Ok(files)
}

#[cfg(test)]
mod tests {
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
            let sig =
                git2::Signature::new("test", "test@test.com", &git2::Time::new(1, 0)).unwrap();
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
}
