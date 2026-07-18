/// Per-session git worktree lifecycle — Phase B of the v2 architecture.
///
/// Each user session owns exactly one git worktree checked out from a
/// `Source`. Worktrees are isolated from each other (separate filesystem
/// paths) so concurrent sessions never clobber each other's working copies.
///
/// SQL analogy:
///   `USE source.branch`   →  `create()`
///   `SHOW WORKTREES`      →  `list()`
///   (session ends)        →  `remove()`
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use git2::{BranchType, Repository, WorktreeLockStatus};
use tracing::{debug, info};

// -----------------------------------------------------------------------
// WorktreeInfo
// -----------------------------------------------------------------------

/// Metadata about a git worktree (does not hold an open `Repository`).
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    /// Name used to identify the worktree in git (e.g. `"session-abc123"`).
    pub name: String,
    /// Absolute path to the worktree's working directory.
    pub path: PathBuf,
    /// Local branch name that is checked out in the worktree, if any.
    pub branch: Option<String>,
    /// Whether the worktree has been locked via `git worktree lock`.
    pub is_locked: bool,
    /// Full hash of the commit the session branch was born at (USE base).
    pub base_commit: Option<String>,
}

// -----------------------------------------------------------------------
// Public API
// -----------------------------------------------------------------------

/// Check out `branch` into a new worktree at `worktree_path`.
///
/// The worktree is added to the repository located at `repo_path` (which
/// may be a bare repo or the `.git` directory of a normal repo).
///
/// `custom_branch` overrides the git branch name created for this worktree.
/// When `None` the branch is auto-named `forgeql/<name>` (the default).
/// When `Some("agent/refactor-signal-api")` that exact name is used, allowing
/// `git fetch <remote>` to surface a human-readable branch to reviewers.
///
/// # Errors
/// Returns `Err` if:
/// - the repository cannot be opened at `repo_path`
/// - `branch` does not exist as a local branch in that repository
/// - git is unable to add the worktree (e.g. path already in use)
// The resume-or-create flow has inherent branching (check existing branch,
// check existing worktree) that pushes past the default complexity limit.
#[allow(clippy::cognitive_complexity)]
pub fn create(
    repo_path: &Path,
    name: &str,
    branch: &str,
    worktree_path: &Path,
    custom_branch: Option<&str>,
) -> Result<WorktreeInfo> {
    let repo = open_repo(repo_path)?;

    // Create a per-session local branch at the same commit that `branch`
    // currently points to.  When a custom_branch name is provided (via
    // `USE … AS 'name'`) we use it directly; otherwise we auto-name it
    // `forgeql/<name>`.  This allows multiple simultaneous sessions based on
    // the same upstream branch without git complaining the branch is "already
    // checked out" in another worktree.
    let session_branch_name =
        custom_branch.map_or_else(|| format!("forgeql/{name}"), str::to_string);
    let origin_commit = resolve_commit(&repo, branch)?;

    // If the branch already exists (e.g. server restarted and the previous
    // session's branch was never cleaned up), reuse it instead of failing.
    // With `force = false` git2 would return an "already exists" error.
    let existing_branch = repo
        .find_branch(&session_branch_name, BranchType::Local)
        .ok();
    let reference = match existing_branch {
        Some(branch_ref) => {
            debug!(branch = %session_branch_name, "session branch already exists — reusing");
            branch_ref.into_reference()
        }
        None => repo
            .branch(&session_branch_name, &origin_commit, false)?
            .into_reference(),
    };

    // If the worktree directory already exists on disk (stale from a previous
    // server lifecycle), verify it really belongs to *this* bare repo before
    // reusing it.  Without this check, two sources whose worktree paths
    // happened to collide (legacy layout pre-0.38.2) could silently hand a
    // worktree from one source to a session for another — corrupting both.
    if worktree_path.exists() {
        let belongs_here = Repository::open(worktree_path).is_ok_and(|existing| {
            existing
                .path()
                .canonicalize()
                .ok()
                .and_then(|p| p.parent().and_then(Path::parent).map(Path::to_path_buf))
                .zip(repo_path.canonicalize().ok())
                .is_some_and(|(found_bare, expected_bare)| found_bare == expected_bare)
        });
        if !belongs_here {
            bail!(
                "worktree directory '{}' exists but does not belong to bare repo '{}' \
                 — refusing to reuse to avoid cross-source corruption. \
                 Remove the stale directory or pick a different alias.",
                worktree_path.display(),
                repo_path.display(),
            );
        }
        info!(name, branch, session_branch = %session_branch_name,
              path = %worktree_path.display(), "worktree already on disk — resuming");
        return Ok(WorktreeInfo {
            name: name.to_string(),
            path: worktree_path.to_path_buf(),
            branch: Some(branch.to_string()),
            base_commit: Some(origin_commit.id().to_string()),
            is_locked: false,
        });
    }

    let mut opts = git2::WorktreeAddOptions::new();
    let _ = opts.reference(Some(&reference));

    // If stale git-internal worktree metadata exists (the checkout path was
    // removed but `git worktree remove` was never called), libgit2 will fail
    // with "directory exists" when trying to create the new gitdir entry.
    // Prune the orphaned metadata first so the add can proceed cleanly.
    if let Ok(stale) = repo.find_worktree(name) {
        let mut prune_opts = git2::WorktreePruneOptions::new();
        // Default flags prune worktrees whose checkout path no longer exists,
        // which is exactly the case here (worktree_path.exists() was false).
        if let Err(e) = stale.prune(Some(&mut prune_opts)) {
            debug!(name, error = %e, "could not prune stale worktree metadata (continuing)");
        } else {
            debug!(name, "pruned stale worktree metadata before re-adding");
        }
    }

    info!(name, branch, session_branch = %session_branch_name,
          path = %worktree_path.display(), "creating worktree");
    drop(repo.worktree(name, worktree_path, Some(&opts))?);
    debug!(name, "worktree created");

    Ok(WorktreeInfo {
        name: name.to_string(),
        path: worktree_path.to_path_buf(),
        branch: Some(branch.to_string()), // conceptual branch (what the user requested)
        base_commit: Some(origin_commit.id().to_string()),
        is_locked: false,
    })
}

/// Resolve a USE base token to a commit.
///
/// A local branch of that name wins; otherwise the token is treated as a
/// revision - a commit hash (full or abbreviated), tag, or any
/// `revparse_single` input - and peeled to a commit. This lets
/// `USE source.<commit-hash> AS '...'` base a session directly on an immutable
/// commit, not only on a branch head.
///
/// # Errors
/// Returns an error if `rev` is neither a local branch nor a resolvable commit.
pub fn resolve_commit<'r>(repo: &'r Repository, rev: &str) -> Result<git2::Commit<'r>> {
    if let Ok(branch) = repo.find_branch(rev, BranchType::Local) {
        return Ok(branch.into_reference().peel_to_commit()?);
    }
    let object = repo.revparse_single(rev).map_err(|e| {
        anyhow::anyhow!("USE base '{rev}' is neither a local branch nor a resolvable commit: {e}")
    })?;
    object
        .peel_to_commit()
        .map_err(|e| anyhow::anyhow!("USE base '{rev}' resolved to a non-commit object: {e}"))
}

/// List all worktrees in the repository at `repo_path`.
///
/// # Errors
/// Returns `Err` if the repository cannot be opened or worktree iteration
/// fails.
pub fn list(repo_path: &Path) -> Result<Vec<WorktreeInfo>> {
    let repo = open_repo(repo_path)?;
    let names = repo.worktrees()?;
    let mut result = Vec::with_capacity(names.len());

    for name_opt in &names {
        let Some(name) = name_opt else { continue };
        let wt = match repo.find_worktree(name) {
            Ok(w) => w,
            Err(e) => {
                debug!(name, error = %e, "skipping unreadable worktree");
                continue;
            }
        };
        let path = wt.path().to_path_buf();
        let is_locked = matches!(wt.is_locked(), Ok(WorktreeLockStatus::Locked(_)));
        let branch = branch_of_worktree(&path);
        result.push(WorktreeInfo {
            name: name.to_string(),
            path,
            branch,
            is_locked,
            base_commit: None,
        });
    }

    Ok(result)
}

/// Remove the worktree named `name` from the repository at `repo_path`.
///
/// The worktree's directory is deleted from the filesystem. The worktree
/// must not be locked.
///
/// # Errors
/// Returns `Err` if:
/// - the repository cannot be opened
/// - no worktree named `name` exists
/// - the worktree is locked (`git worktree lock` was called)
/// - the git prune or directory removal fail
pub fn remove(repo_path: &Path, name: &str) -> Result<()> {
    let repo = open_repo(repo_path)?;
    let wt = repo.find_worktree(name)?;

    if matches!(wt.is_locked(), Ok(WorktreeLockStatus::Locked(_))) {
        bail!("worktree '{name}' is locked and cannot be removed");
    }

    // Record path before pruning strips the metadata.
    let wt_path = wt.path().to_path_buf();

    // `valid(true)` forces pruning even when the worktree directory still
    // exists on disk.
    let mut prune_opts = git2::WorktreePruneOptions::new();
    let _ = prune_opts.valid(true);
    wt.prune(Some(&mut prune_opts))?;

    if wt_path.exists() {
        info!(name, path = %wt_path.display(), "removing worktree directory");
        std::fs::remove_dir_all(&wt_path)?;
    }

    debug!(name, "worktree removed");
    Ok(())
}

/// Delete the per-session local branch `forgeql/<session_id>` from the
/// repository at `repo_path`.
///
/// This is the cleanup counterpart to the branch created by `create()`. If the
/// branch no longer exists (already deleted, or server restarted), returns `Ok`
/// without error.
///
/// # Errors
/// Returns `Err` if the repository cannot be opened or branch deletion fails.
pub fn delete_session_branch(repo_path: &Path, session_id: &str) -> Result<()> {
    let repo = open_repo(repo_path)?;
    let branch_name = format!("forgeql/{session_id}");
    match repo.find_branch(&branch_name, BranchType::Local) {
        Ok(mut branch) => {
            branch.delete()?;
            debug!(branch = %branch_name, "deleted session branch");
        }
        Err(_) => {
            debug!(branch = %branch_name, "session branch not found — already deleted");
        }
    }
    Ok(())
}

/// Delete a branch by its full name (no prefix added).
///
/// # Errors
///
/// Returns `Err` if the repository cannot be opened or branch deletion fails.
pub fn delete_branch(repo_path: &Path, branch_name: &str) -> Result<()> {
    let repo = open_repo(repo_path)?;
    match repo.find_branch(branch_name, BranchType::Local) {
        Ok(mut branch) => {
            branch.delete()?;
            debug!(branch = %branch_name, "deleted branch");
        }
        Err(_) => {
            debug!(branch = %branch_name, "branch not found — already deleted");
        }
    }
    Ok(())
}

/// Resolve the branch a worktree teardown should delete.
///
/// Resolution order, most authoritative first:
///   1. `known` — the exact branch the caller already holds (a warm worktree's
///      `fql/__warm__/…` name, or [`WorktreeInfo::branch`], which survives the
///      checkout directory already being gone);
///   2. the live HEAD branch read from `wt_path`, while the checkout still
///      exists — covers custom `fql/{user}/{source}/{branch}/{alias}` names that
///      cannot be reconstructed from `name`;
///   3. the legacy auto name `forgeql/{name}` as a last resort (HEAD detached or
///      unreadable).
fn resolve_teardown_branch(wt_path: &Path, name: &str, known: Option<&str>) -> String {
    known
        .map(str::to_owned)
        .or_else(|| branch_of_worktree(wt_path))
        .unwrap_or_else(|| format!("forgeql/{name}"))
}

/// Maintain a compatibility symlink at the pre-user-segment worktree path.
///
/// Worktrees moved from `worktrees/{dir}` to `worktrees/{user}/{dir}`, but
/// host tooling (container runners, mount scripts) built against the old
/// layout still resolves the un-nested path. This creates
/// `legacy_path -> {user}/{dir}` (relative target, so a relocated data dir
/// stays consistent) unless something already exists there — a real
/// directory from the old layout, or another user's link, is never
/// clobbered. Best-effort; Unix only (symlink creation needs no privilege
/// there).
pub fn ensure_legacy_link(legacy_path: &Path, wt_path: &Path) {
    #[cfg(unix)]
    {
        if legacy_path.symlink_metadata().is_ok() {
            return; // occupied: old-layout worktree, or an existing link
        }
        let Some(user_dir) = wt_path.parent().and_then(Path::file_name) else {
            return;
        };
        let Some(wt_name) = wt_path.file_name() else {
            return;
        };
        let target = PathBuf::from(user_dir).join(wt_name);
        if let Err(e) = std::os::unix::fs::symlink(&target, legacy_path) {
            tracing::debug!(
                legacy = %legacy_path.display(),
                error = %e,
                "legacy worktree link not created (non-fatal)"
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (legacy_path, wt_path);
    }
}

/// Remove the compatibility symlink left by [`ensure_legacy_link`], if any.
///
/// Only removes `worktrees/{dir}` when it is a symlink whose target resolves
/// to `wt_path` (`worktrees/{user}/{dir}`); a real directory or a link owned
/// by another session is left untouched. Best-effort.
pub fn remove_legacy_link(wt_path: &Path) {
    let Some(user_root) = wt_path.parent() else {
        return;
    };
    let Some(worktrees_root) = user_root.parent() else {
        return;
    };
    let Some(wt_name) = wt_path.file_name() else {
        return;
    };
    let legacy = worktrees_root.join(wt_name);
    let Ok(meta) = legacy.symlink_metadata() else {
        return;
    };
    if !meta.file_type().is_symlink() {
        return;
    }
    let Ok(target) = std::fs::read_link(&legacy) else {
        return;
    };
    // Relative targets resolve against the link's own directory.
    let resolved = if target.is_absolute() {
        target
    } else {
        worktrees_root.join(target)
    };
    if resolved == wt_path {
        let _ = std::fs::remove_file(&legacy);
    }
}
/// Remove a worktree **and** delete its backing branch in one call.
///
/// This is the single teardown entry point for every worktree the server
/// creates — live sessions, TTL eviction of auto sessions, startup
/// stale-pruning, and background warming. Going through it guarantees a
/// worktree is never removed while its branch is left orphaned in the bare repo
/// (the leak that accumulated `fql/__warm__/…` and `fql/anonymous/…` refs).
///
/// The branch is resolved via [`resolve_teardown_branch`]; pass `known_branch`
/// whenever the exact name is available, since it is the only source that
/// survives the checkout directory already being gone.
///
/// Best-effort: both the worktree removal and the branch deletion are always
/// attempted; the first error is returned for the caller to log. Callers that
/// intentionally keep a branch (e.g. a named session retained for review) must
/// call [`remove`] directly instead. The compatibility symlink left by
/// [`ensure_legacy_link`] is removed alongside the worktree.
///
/// # Errors
/// Returns the first of the worktree-removal or branch-deletion errors, if any.
pub fn remove_with_branch(
    repo_path: &Path,
    wt_path: &Path,
    name: &str,
    known_branch: Option<&str>,
) -> Result<()> {
    // Resolve the branch BEFORE remove() deletes the checkout — once the
    // directory is gone, HEAD can no longer be read.
    let branch = resolve_teardown_branch(wt_path, name, known_branch);
    remove_legacy_link(wt_path);
    let remove_res = remove(repo_path, name);
    let branch_res = delete_branch(repo_path, &branch);
    remove_res.and(branch_res)
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// Open `path` as either a bare repository or a normal repo (by looking
/// for a `.git` directory / file).
fn open_repo(path: &Path) -> Result<Repository> {
    let repo = Repository::open_bare(path).or_else(|_| Repository::open(path))?;
    Ok(repo)
}

/// Return the local branch name currently checked out in the worktree whose
/// working directory is `wt_path`, or `None` if the HEAD is detached or the
/// repository cannot be opened.
pub(crate) fn branch_of_worktree(wt_path: &Path) -> Option<String> {
    let repo = Repository::open(wt_path).ok()?;
    let head = repo.head().ok()?;
    if !head.is_branch() {
        return None;
    }
    head.shorthand().map(str::to_owned)
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
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
        let info = create(&bare, "s-auto", &branch, &wt_path, None)
            .expect("auto-branch resume must succeed");
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
}
