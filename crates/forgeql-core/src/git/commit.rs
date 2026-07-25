//! Staging and committing, in the four shapes `ForgeQL` needs.
//!
//! An internal checkpoint keeps the index cache so `git reset --hard` can
//! restore it; a user-facing commit strips it. Each entry point pairs a
//! staging filter from [`super::excludes`] with a commit, so which files a
//! given commit shape drops is decided in one place per shape.

use std::path::{Path, PathBuf};

use anyhow::Result;
use git2::Repository;
use tracing::debug;

use super::excludes::{
    CHECKPOINT_EXCLUDED, is_checkpoint_excluded, is_clean_commit_excluded,
    purge_excluded_index_entries,
};

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
