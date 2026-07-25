/// Git integration — bare-repo management, worktree lifecycle, and
/// low-level branch/commit helpers.
///
/// Submodules:
/// - `source`  — `Source` + `SourceRegistry` (Phase B)
/// - `worktree` — per-session worktree lifecycle (Phase B)
///
/// Low-level branch/commit helpers are in this module (Phase 3 stub).
mod diff;
mod excludes;
pub mod source;
pub mod worktree;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use git2::{BranchType, Repository};
use tracing::debug;

pub use diff::{
    DiffFile, FORGEQL_CONTROL_FILES, changed_files_between, commit_diff, commits_since,
    diff_head_to_worktree, dirty_paths, source_changes, uncommitted_source_changes, worktree_diff,
};
pub use excludes::{PATCHES_DIR_NAME, ensure_runtime_excludes};

use excludes::{
    CHECKPOINT_EXCLUDED, is_checkpoint_excluded, is_clean_commit_excluded,
    purge_excluded_index_entries,
};

/// Return the current HEAD commit hash of a local branch in a bare repo.
///
/// Returns `None` if the repo cannot be opened or the branch does not exist.
#[must_use]
pub fn branch_head(repo_path: &Path, branch: &str) -> Option<String> {
    let repo = git2::Repository::open_bare(repo_path).ok()?;
    let commit = repo
        .find_branch(branch, BranchType::Local)
        .ok()?
        .into_reference()
        .peel_to_commit()
        .ok()?;
    Some(commit.id().to_string())
}

/// Resolve a USE base token to its full commit hash.
///
/// Accepts a branch name or commit-ish, resolving it against the bare repo and
/// returning `None` if it does not resolve. Lets a resumed session report the
/// same base a fresh session gets from `worktree::create`.
#[must_use]
pub fn resolve_base_commit(repo_path: &Path, rev: &str) -> Option<String> {
    let repo = git2::Repository::open_bare(repo_path).ok()?;
    worktree::resolve_commit(&repo, rev)
        .ok()
        .map(|commit| commit.id().to_string())
}

/// Open the git repository containing `workspace_root`.
///
/// # Errors
/// Returns `Err` if no git repository is found at or above `workspace_root`.
pub fn open(workspace_root: &Path) -> Result<Repository> {
    let repo = Repository::discover(workspace_root)?;
    debug!(path = %repo.path().display(), "git repository opened");
    Ok(repo)
}

/// Create a new branch from HEAD and check it out.
///
/// # Errors
/// Returns `Err` if HEAD cannot be resolved or the branch already exists.
pub fn create_branch(repo: &Repository, name: &str) -> Result<()> {
    let head = repo.head()?.peel_to_commit()?;
    let _branch = repo.branch(name, &head, false)?;
    debug!(branch = name, "created branch");
    Ok(())
}

/// Return the current HEAD commit OID as a hex string.
///
/// # Errors
/// Returns `Err` if HEAD cannot be resolved.
pub fn head_oid(repo: &Repository) -> Result<String> {
    let oid = repo.head()?.peel_to_commit()?.id();
    Ok(oid.to_string())
}

/// Hard-reset the repository to the commit identified by `oid_hex`.
///
/// This is equivalent to `git reset --hard <oid>`.  It moves HEAD, updates
/// the index, and checks out the tree — any uncommitted changes are lost.
///
/// # Errors
/// Returns `Err` if the OID cannot be resolved or the reset fails.
pub fn reset_hard(repo: &Repository, oid_hex: &str) -> Result<()> {
    let oid = git2::Oid::from_str(oid_hex)?;
    let commit = repo.find_commit(oid)?;
    let obj = commit.into_object();
    repo.reset(&obj, git2::ResetType::Hard, None)?;
    debug!(oid = oid_hex, "git reset --hard");
    Ok(())
}

/// Soft-reset the repository to the commit identified by `oid_hex`.
///
/// This is equivalent to `git reset --soft <oid>`.  It moves HEAD to the
/// target commit but leaves the index and working tree unchanged.  Used by
/// `COMMIT` to squash checkpoint commits into a single clean commit.
///
/// # Errors
/// Returns `Err` if the OID cannot be resolved or the reset fails.
pub fn soft_reset(repo: &Repository, oid_hex: &str) -> Result<()> {
    let oid = git2::Oid::from_str(oid_hex)?;
    let commit = repo.find_commit(oid)?;
    let obj = commit.into_object();
    repo.reset(&obj, git2::ResetType::Soft, None)?;
    debug!(oid = oid_hex, "git reset --soft");
    Ok(())
}

/// Stage all modified files and commit as an internal checkpoint.
///
/// The `.forgeql-index` cache is **included** so that `git reset --hard`
/// restores it, enabling instant rollback without re-indexing.
/// Only `.forgeql-session` is excluded.
///
/// # Errors
/// Returns `Err` if staging, tree writing, or the commit itself fails.
pub fn stage_and_commit(repo: &Repository, message: &str) -> Result<()> {
    let mut index = repo.index()?;
    index.add_all(
        std::iter::once("*"),
        git2::IndexAddOption::DEFAULT,
        Some(&mut |path: &std::path::Path, _: &[u8]| i32::from(is_checkpoint_excluded(path))),
    )?;
    for f in CHECKPOINT_EXCLUDED {
        let _ = index.remove_path(std::path::Path::new(f));
    }
    index.write()?;

    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    let sig = repo
        .signature()
        .or_else(|_| git2::Signature::now("ForgeQL", "forgeql@localhost"))?;
    let parent = repo.head()?.peel_to_commit()?;

    let _oid = repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])?;
    debug!(message, "committed (checkpoint)");
    Ok(())
}

/// Stage all modified files (excluding runtime and index files) and commit.
///
/// Produces a clean user-facing commit that never contains `.forgeql-index`
/// or `.forgeql-session`. Any previously tracked copies are also removed.
///
/// # Errors
/// Returns `Err` if staging, tree writing, or the commit itself fails.
pub fn stage_and_commit_clean(repo: &Repository, message: &str) -> Result<()> {
    let mut index = repo.index()?;
    index.add_all(
        std::iter::once("*"),
        git2::IndexAddOption::DEFAULT,
        Some(&mut |path: &std::path::Path, _: &[u8]| i32::from(is_clean_commit_excluded(path))),
    )?;
    purge_excluded_index_entries(&mut index);
    index.write()?;

    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    let sig = repo
        .signature()
        .or_else(|_| git2::Signature::now("ForgeQL", "forgeql@localhost"))?;
    let parent = repo.head()?.peel_to_commit()?;

    let _oid = repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])?;
    debug!(message, "committed (clean, no runtime files)");
    Ok(())
}

/// Stage working-tree changes and create a squashed commit whose parent is
/// `parent_oid`, updating the branch that HEAD points to **by name**.
///
/// Unlike [`stage_and_commit_clean`], this function never calls
/// `git reset --soft` and never relies on HEAD-chasing through `Some("HEAD")`.
/// Instead it:
///
/// 1. Resolves HEAD → branch ref name (e.g. `refs/heads/forgeql/s123`)
///    *before* any destructive operation.
/// 2. Stages all working-tree changes (excluding runtime files).
/// 3. Creates the commit with an explicit parent OID.
/// 4. Updates the branch ref **directly by name**.
///
/// This is safe in linked worktrees where `git reset --soft` can detach
/// HEAD and leave the branch ref stale.
///
/// Returns the hex SHA of the new commit.
///
/// # Errors
/// Returns `Err` if HEAD is detached, staging fails, or the commit fails.
pub fn squash_commit_on_branch(
    repo: &Repository,
    parent_oid: &str,
    message: &str,
) -> Result<String> {
    // 1. Capture the branch ref name HEAD points to.
    let head_ref = repo.find_reference("HEAD")?;
    let branch_ref_name = head_ref
        .symbolic_target()
        .ok_or_else(|| anyhow::anyhow!("HEAD is detached — cannot determine target branch"))?
        .to_string();

    // 2. Stage working-tree changes (excluding runtime + index files).
    let mut index = repo.index()?;
    index.add_all(
        std::iter::once("*"),
        git2::IndexAddOption::DEFAULT,
        Some(&mut |path: &std::path::Path, _: &[u8]| i32::from(is_clean_commit_excluded(path))),
    )?;
    purge_excluded_index_entries(&mut index);
    index.write()?;

    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;

    let sig = repo
        .signature()
        .or_else(|_| git2::Signature::now("ForgeQL", "forgeql@localhost"))?;

    // 3. Explicit parent — not derived from HEAD.
    let parent = repo.find_commit(git2::Oid::from_str(parent_oid)?)?;

    // 4. Create the commit *without* updating any ref — this avoids
    //    libgit2's compare-and-swap check which would fail because the
    //    branch tip (a checkpoint commit) differs from `parent_oid`
    //    (the pre-transaction base).
    let oid = repo.commit(None, &sig, &sig, message, &tree, &[&parent])?;

    // 5. Force-update the branch ref to point to the new squash commit.
    let _ref = repo.reference(
        &branch_ref_name,
        oid,
        true, // force
        &format!("ForgeQL squash: {message}"),
    )?;

    debug!(%message, oid = %oid, branch = %branch_ref_name, "squash-committed on branch");
    Ok(oid.to_string())
}

/// Stage only `touched` files and commit with `message` on the current HEAD branch.
///
/// `worktree_root` is the working directory of the git checkout.  All paths in
/// `touched` must be absolute children of `worktree_root`.
///
/// Returns the SHA-1 hex string of the new commit.
///
/// # Errors
/// Returns `Err` if any path cannot be relativised, staging fails, or the
/// commit itself fails.
pub fn stage_paths_and_commit(
    repo: &Repository,
    worktree_root: &Path,
    touched: &[PathBuf],
    message: &str,
) -> Result<String> {
    let mut index = repo.index()?;
    for abs in touched {
        let rel = abs.strip_prefix(worktree_root).map_err(|_| {
            anyhow::anyhow!(
                "path {} is outside worktree {}",
                abs.display(),
                worktree_root.display()
            )
        })?;
        // A path that no longer exists on disk was deleted by the mutation
        // (`CHANGE FILE … WITH NOTHING`) — stage the removal instead.
        if abs.exists() {
            index.add_path(rel)?;
        } else {
            index.remove_path(rel)?;
        }
    }
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    let sig = repo
        .signature()
        .or_else(|_| git2::Signature::now("ForgeQL", "forgeql@localhost"))?;
    let parent = repo.head()?.peel_to_commit()?;
    let oid = repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])?;
    debug!(%message, oid = %oid, "committed");
    Ok(oid.to_string())
}

// -----------------------------------------------------------------------
// EXPORT PATCH — format-patch export of session commits
// -----------------------------------------------------------------------

/// One mbox patch file produced by [`export_patches`].
#[derive(Debug, Clone)]
pub struct ExportedPatch {
    /// Absolute path of the patch file inside the worktree.
    pub path: std::path::PathBuf,
    /// File size in bytes.
    pub bytes: u64,
    /// SHA-256 of the file contents (hex) — verify after transfer with
    /// `sha256sum` before `git am`.
    pub sha256: String,
}

/// Run one git subcommand in `worktree` and return its stdout as a string.
///
/// Arguments are passed as separate argv entries (no shell), so nothing the
/// engine splices can be interpreted as shell syntax.
fn run_git(worktree: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn git {}", args.join(" ")))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The commit the session branch grew from: `merge-base(<base_ref>, HEAD)`.
///
/// # Errors
/// Returns an error when git cannot be spawned or the merge-base fails
/// (e.g. `base_ref` does not name a commit).
pub fn merge_base_with(worktree: &Path, base_ref: &str) -> Result<String> {
    Ok(run_git(worktree, &["merge-base", base_ref, "HEAD"])?
        .trim()
        .to_owned())
}

/// The worktree's current HEAD commit id.
///
/// # Errors
/// Returns an error when git cannot be spawned or HEAD cannot be resolved.
pub fn head_oid_of(worktree: &Path) -> Result<String> {
    Ok(run_git(worktree, &["rev-parse", "HEAD"])?.trim().to_owned())
}

/// Write `git am`-ready mbox files for `range_args` into
/// `.forgeql-patches/` in `worktree` (the directory is cleared first).
///
/// Every patch is generated with an exclude pathspec for `.forgeql-*` paths
/// at any depth, so commits touching only `ForgeQL` runtime files — such as
/// transaction checkpoints — produce no patch at all, and commits mixing
/// source and runtime files export only their source part. `--binary`
/// includes base85 literal data so binary files survive `git am`.
///
/// `range_args` is either `["<oid>..HEAD"]` or `["-<n>", "HEAD"]`, always
/// engine-computed — never user text.
///
/// # Errors
/// Returns an error when the output directory cannot be cleared, git cannot
/// be spawned, `format-patch` fails, or a produced file cannot be read back.
pub fn export_patches(worktree: &Path, range_args: &[String]) -> Result<Vec<ExportedPatch>> {
    use sha2::{Digest, Sha256};

    let out_dir = worktree.join(PATCHES_DIR_NAME);
    if out_dir.exists() {
        std::fs::remove_dir_all(&out_dir)
            .with_context(|| format!("could not clear {}", out_dir.display()))?;
    }

    let mut args: Vec<&str> = vec!["format-patch", "--binary", "-o", PATCHES_DIR_NAME];
    args.extend(range_args.iter().map(String::as_str));
    args.extend(["--", ":(exclude,glob)**/.forgeql-*"]);
    let stdout = run_git(worktree, &args)?;

    // format-patch prints one created file per line, in series order.
    let mut files = Vec::new();
    for line in stdout.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let abs = worktree.join(line);
        let data =
            std::fs::read(&abs).with_context(|| format!("could not read {}", abs.display()))?;
        let bytes = u64::try_from(data.len()).unwrap_or(u64::MAX);
        let sha256 = format!("{:x}", Sha256::digest(&data));
        files.push(ExportedPatch {
            path: abs,
            bytes,
            sha256,
        });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::tempdir;

    use super::excludes::RUNTIME_EXCLUDE_MARKER;
    use super::*;

    fn make_normal_repo(dir: &Path) -> git2::Repository {
        let repo = git2::Repository::init(dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        drop(cfg);

        // Initial commit — scope tree to drop its borrow before returning repo.
        std::fs::write(dir.join("file.cpp"), b"int main(){}\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("file.cpp")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        {
            let tree = repo.find_tree(tree_id).unwrap();
            let sig =
                git2::Signature::new("test", "test@test.com", &git2::Time::new(0, 0)).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();
        } // tree dropped here — borrow on repo released
        repo
    }

    #[test]
    fn stage_paths_and_commit_creates_commit_with_message() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        let repo = make_normal_repo(dir);

        // Modify the file after the initial commit.
        std::fs::write(dir.join("file.cpp"), b"int main() { return 0; }\n").unwrap();
        let touched = vec![dir.join("file.cpp")];

        let oid_str =
            stage_paths_and_commit(&repo, dir, &touched, "refactor: update main").unwrap();

        // The newly created commit must be HEAD.
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head_commit.id().to_string(), oid_str);
        assert_eq!(
            head_commit.message().unwrap().trim(),
            "refactor: update main"
        );
        // The parent of HEAD is the initial commit.
        assert_eq!(head_commit.parent_count(), 1);
    }

    #[test]
    fn stage_paths_and_commit_errors_on_path_outside_worktree() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        let repo = make_normal_repo(dir);

        let outside = std::path::PathBuf::from("/tmp/not-in-worktree.cpp");
        let result = stage_paths_and_commit(&repo, dir, &[outside], "oops");
        assert!(result.is_err(), "must fail when path is outside worktree");
    }

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

    #[test]
    fn clean_commit_excludes_staging_contents_and_runtime_files() {
        use std::path::Path;
        // Entries inside the staging dir have ordinary leaf names — the
        // component-wise check must still exclude them.
        assert!(is_clean_commit_excluded(Path::new(
            ".forgeql-staging/ab/cdef0123.fqsf"
        )));
        assert!(is_clean_commit_excluded(Path::new(".forgeql-showmore-3")));
        assert!(is_clean_commit_excluded(Path::new(".forgeql-index")));
        assert!(is_clean_commit_excluded(Path::new(
            crate::session::found_set::FILE_NAME
        )));
        assert!(!is_clean_commit_excluded(Path::new("src/main.rs")));
    }

    #[test]
    fn checkpoint_excludes_showmore_but_keeps_index() {
        use std::path::Path;
        assert!(is_checkpoint_excluded(Path::new(".forgeql-showmore-0")));
        assert!(is_checkpoint_excluded(Path::new(
            ".forgeql-staging/ab/cdef0123.fqsf"
        )));
        // The index cache is intentionally checkpoint-committed so
        // `git reset --hard` restores it without a re-index.
        assert!(!is_checkpoint_excluded(Path::new(".forgeql-index")));
        assert!(!is_checkpoint_excluded(Path::new("src/main.rs")));
    }

    #[test]
    fn runtime_excludes_written_once_and_scoped() {
        let dir = tempfile::tempdir().unwrap();
        ensure_runtime_excludes(dir.path());
        ensure_runtime_excludes(dir.path());
        let content = std::fs::read_to_string(dir.path().join("info/exclude")).unwrap();
        assert_eq!(content.matches(RUNTIME_EXCLUDE_MARKER).count(), 1);
        assert!(content.contains(".forgeql-session"));
        assert!(content.contains(".forgeql-staging/"));
        assert!(content.contains(".forgeql-showmore*"));
        assert!(content.contains(".forgeql-patches/"));
        // Checkpoint-committed files must never be git-ignored.
        assert!(!content.contains(".forgeql-index"));
        assert!(!content.contains(".forgeql-undo"));
    }

    /// A block written by an earlier version (no patches entry) gains the
    /// missing line on the next call — exactly once.
    #[test]
    fn runtime_excludes_upgrades_existing_block() {
        let dir = tempfile::tempdir().unwrap();
        let info = dir.path().join("info");
        std::fs::create_dir_all(&info).unwrap();
        let old_block = format!("{RUNTIME_EXCLUDE_MARKER}\n.forgeql-session\n");
        std::fs::write(info.join("exclude"), &old_block).unwrap();

        ensure_runtime_excludes(dir.path());
        ensure_runtime_excludes(dir.path());
        let content = std::fs::read_to_string(info.join("exclude")).unwrap();
        assert_eq!(content.matches(RUNTIME_EXCLUDE_MARKER).count(), 1);
        assert_eq!(content.matches(".forgeql-patches/").count(), 1);
        assert!(
            content.starts_with(&old_block),
            "existing entries preserved"
        );
    }

    /// Stage everything (runtime files included) and commit — a raw commit
    /// like the ones transaction checkpoints or older releases produced.
    fn raw_commit_all(worktree: &Path, msg: &str) {
        run_git(worktree, &["add", "-A"]).unwrap();
        run_git(worktree, &["commit", "-m", msg]).unwrap();
    }

    /// The transaction-safety contract of EXPORT PATCH: commits touching only
    /// `ForgeQL` runtime files (transaction checkpoints) produce no patch, and
    /// commits mixing source with runtime files export only the source part.
    #[test]
    fn export_patches_excludes_runtime_files_and_checkpoint_commits() {
        let dir = tempfile::tempdir().unwrap();
        let _repo = make_normal_repo(dir.path());

        std::fs::write(dir.path().join("file.cpp"), b"int main(){return 1;}\n").unwrap();
        raw_commit_all(dir.path(), "user change 1");

        // Checkpoint-style commit: runtime files only (top-level and nested).
        std::fs::write(dir.path().join(".forgeql-index"), b"idx").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/.forgeql-columnar-delta"), b"d").unwrap();
        raw_commit_all(dir.path(), "forgeql: checkpoint 'txn'");

        // Mixed commit: source + runtime file in one commit.
        std::fs::write(dir.path().join("file.cpp"), b"int main(){return 2;}\n").unwrap();
        std::fs::write(dir.path().join(".forgeql-index"), b"idx2").unwrap();
        raw_commit_all(dir.path(), "user change 2");

        // Explicit range (the session merge-base..HEAD form): 3 commits in
        // range, the checkpoint-only one drops out of the series entirely.
        let files = export_patches(dir.path(), &["HEAD~3..HEAD".to_string()]).unwrap();
        assert_eq!(
            files.len(),
            2,
            "checkpoint-only commit must produce no patch"
        );
        for f in &files {
            assert!(f.bytes > 0);
            assert_eq!(f.sha256.len(), 64, "sha256 hex digest");
            let text = std::fs::read_to_string(&f.path).unwrap();
            assert!(
                !text.contains(".forgeql-"),
                "runtime files leaked into {}",
                f.path.display()
            );
        }

        // `-<n>` counts pathspec-matching commits, so LAST n means the last
        // n commits that touched source — checkpoints never consume the
        // count. -1 therefore yields the mixed commit's source part.
        let last = export_patches(dir.path(), &["-1".to_string(), "HEAD".to_string()]).unwrap();
        assert_eq!(last.len(), 1);
        let text = std::fs::read_to_string(&last[0].path).unwrap();
        assert!(text.contains("user change 2"));
        assert!(!text.contains(".forgeql-"));

        // Re-running cleared the directory instead of accumulating series.
        let on_disk = std::fs::read_dir(dir.path().join(PATCHES_DIR_NAME))
            .unwrap()
            .count();
        assert_eq!(on_disk, 1, "stale patches from earlier exports removed");
    }

    #[test]
    fn squash_commit_drops_checkpoint_inherited_undo_slots() {
        let dir = tempfile::tempdir().unwrap();
        let repo = make_normal_repo(dir.path());

        std::fs::write(dir.path().join("file.cpp"), b"int main(){return 1;}\n").unwrap();
        raw_commit_all(dir.path(), "base");
        let base = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();

        // Checkpoint commits keep undo slots on purpose (restart durability),
        // so by the final squash the slot file is already tracked at the
        // branch tip — `add_all`'s callback never sees it.
        std::fs::write(dir.path().join(".forgeql-undo-0"), b"FQLUNDO\tv1\n").unwrap();
        std::fs::write(dir.path().join("file.cpp"), b"int main(){return 2;}\n").unwrap();
        raw_commit_all(dir.path(), "forgeql: checkpoint 'txn'");

        let sha = squash_commit_on_branch(&repo, &base, "user commit").unwrap();
        let commit = repo
            .find_commit(git2::Oid::from_str(&sha).unwrap())
            .unwrap();
        let tree = commit.tree().unwrap();
        assert!(tree.get_name("file.cpp").is_some(), "source change kept");
        assert!(
            tree.get_name(".forgeql-undo-0").is_none(),
            "undo slot inherited from a checkpoint must not reach the user-facing commit"
        );
    }

    /// Range helpers and the uncommitted-changes probe used by EXPORT PATCH.
    #[test]
    fn export_patch_range_helpers_and_dirty_probe() {
        let dir = tempfile::tempdir().unwrap();
        let _repo = make_normal_repo(dir.path());
        run_git(dir.path(), &["branch", "base"]).unwrap();

        assert_eq!(uncommitted_source_changes(dir.path()).unwrap(), 0);

        // Runtime-only dirt is invisible to the probe; source dirt counts.
        std::fs::write(dir.path().join(".forgeql-session"), b"s").unwrap();
        assert_eq!(uncommitted_source_changes(dir.path()).unwrap(), 0);
        std::fs::write(dir.path().join("new.cpp"), b"int x;\n").unwrap();
        assert_eq!(uncommitted_source_changes(dir.path()).unwrap(), 1);

        // No commits over the base branch: merge-base == HEAD.
        let mb = merge_base_with(dir.path(), "base").unwrap();
        let head = head_oid_of(dir.path()).unwrap();
        assert_eq!(mb, head);

        // One commit later the range opens up.
        raw_commit_all(dir.path(), "work");
        let mb2 = merge_base_with(dir.path(), "base").unwrap();
        assert_eq!(mb2, mb, "merge-base stays at the fork point");
        assert_ne!(head_oid_of(dir.path()).unwrap(), mb2);
    }
}
