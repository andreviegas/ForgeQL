/// Git integration — bare-repo management, worktree lifecycle, and
/// low-level branch/commit helpers.
///
/// Submodules:
/// - `source`  — `Source` + `SourceRegistry` (Phase B)
/// - `worktree` — per-session worktree lifecycle (Phase B)
///
/// Low-level branch/commit helpers are in this module (Phase 3 stub).
pub mod source;
pub mod worktree;

use std::path::{Path, PathBuf};

use anyhow::Result;
use git2::{BranchType, Repository};
use tracing::debug;

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

/// Files that `ForgeQL` generates at runtime and must never appear in any commit
/// — neither user-visible commits nor internal checkpoint commits.
const RUNTIME_FILES: &[&str] = &[".forgeql-index", ".forgeql-session"];

fn is_runtime_file(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| RUNTIME_FILES.contains(&name))
}

/// Stage all modified files (excluding `ForgeQL` runtime files) and commit.
///
/// Used for internal checkpoint commits inside a transaction.
///
/// # Errors
/// Returns `Err` if staging, tree writing, or the commit itself fails.
pub fn stage_and_commit(repo: &Repository, message: &str) -> Result<()> {
    let mut index = repo.index()?;
    index.add_all(
        std::iter::once("*"),
        git2::IndexAddOption::DEFAULT,
        Some(&mut |path: &std::path::Path, _: &[u8]| i32::from(is_runtime_file(path))),
    )?;
    for f in RUNTIME_FILES {
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
    debug!(message, "committed");
    Ok(())
}

/// Stage all modified files (excluding `ForgeQL` runtime files) and commit.
///
/// Produces a clean user-facing commit that never contains runtime-generated
/// files. Any previously tracked runtime files are also removed from the index.
///
/// # Errors
/// Returns `Err` if staging, tree writing, or the commit itself fails.
pub fn stage_and_commit_clean(repo: &Repository, message: &str) -> Result<()> {
    let mut index = repo.index()?;
    index.add_all(
        std::iter::once("*"),
        git2::IndexAddOption::DEFAULT,
        Some(&mut |path: &std::path::Path, _: &[u8]| i32::from(is_runtime_file(path))),
    )?;
    for f in RUNTIME_FILES {
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

    // 2. Stage working-tree changes (excluding runtime files).
    let mut index = repo.index()?;
    index.add_all(
        std::iter::once("*"),
        git2::IndexAddOption::DEFAULT,
        Some(&mut |path: &std::path::Path, _: &[u8]| i32::from(is_runtime_file(path))),
    )?;
    for f in RUNTIME_FILES {
        let _ = index.remove_path(std::path::Path::new(f));
    }
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
        index.add_path(rel)?;
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

    let base_tree = repo
        .find_branch(base_branch, BranchType::Local)?
        .into_reference()
        .peel_to_tree()?;
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::tempdir;

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
}
