//! Git integration — bare-repo management, worktree lifecycle, and the
//! plumbing behind commits, diffs and patch export.
//!
//! Submodules:
//! - `commit`   — the staging-and-commit shapes (checkpoint, clean, squash)
//! - `diff`     — change lists, dirty paths, and the `SHOW DIFF` surface
//! - `excludes` — which files never reach a commit, and the runtime-exclude block
//! - `patch`    — `git am`-ready patch export
//! - `source`   — `Source` + `SourceRegistry`
//! - `worktree` — per-session worktree lifecycle
//!
//! What stays here is the repository basics every submodule builds on: opening
//! a repo, resolving and creating branches, resetting, and running a single git
//! subcommand.
mod commit;
mod diff;
mod excludes;
mod patch;
pub mod source;
pub mod worktree;

use std::path::Path;

use anyhow::{Context, Result};
use git2::{BranchType, Repository};
use tracing::debug;

pub use commit::{
    squash_commit_on_branch, stage_and_commit, stage_and_commit_clean, stage_paths_and_commit,
};
pub use diff::{
    DiffFile, FORGEQL_CONTROL_FILES, changed_files_between, commit_diff, commits_since,
    diff_head_to_worktree, dirty_paths, source_changes, uncommitted_source_changes, worktree_diff,
};
pub use excludes::{PATCHES_DIR_NAME, ensure_runtime_excludes};
pub use patch::{ExportedPatch, export_patches};

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

#[cfg(test)]
mod testutil {
    use std::path::Path;

    use super::*;

    pub(super) fn make_normal_repo(dir: &Path) -> git2::Repository {
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

    /// Stage everything (runtime files included) and commit — a raw commit
    /// like the ones transaction checkpoints or older releases produced.
    pub(super) fn raw_commit_all(worktree: &Path, msg: &str) {
        run_git(worktree, &["add", "-A"]).unwrap();
        run_git(worktree, &["commit", "-m", msg]).unwrap();
    }
}
