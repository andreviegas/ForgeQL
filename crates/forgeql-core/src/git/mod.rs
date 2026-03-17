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
use git2::Repository;
use tracing::debug;

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

/// Stage all modified files and commit with the given message.
///
/// # Errors
/// Returns `Err` if staging, tree writing, or the commit itself fails.
pub fn stage_and_commit(repo: &Repository, message: &str) -> Result<()> {
    let mut index = repo.index()?;
    index.add_all(std::iter::once("*"), git2::IndexAddOption::DEFAULT, None)?;
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
