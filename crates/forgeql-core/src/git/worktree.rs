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
    /// Full hash of the commit actually checked out in the worktree.
    ///
    /// For a fresh worktree this is the resolved USE base. For a REUSED
    /// worktree or session branch it is that branch's real tip — which may
    /// differ from the requested base (session commits, or a base that moved
    /// under REFRESH). Callers must report THIS value, never re-resolve the
    /// requested base: reporting a resolution the checkout does not match is
    /// how an agent ends up told "base = new head" while reading old files.
    pub base_commit: Option<String>,
    /// Full hash the requested base (`branch` param) resolved to at creation
    /// time. Recorded so the session can later detect that the base moved
    /// (e.g. after `REFRESH SOURCE`) and refuse to silently resume.
    pub upstream_head: Option<String>,
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
        let checkout_oid = resume_checkout_oid(worktree_path, name, branch, origin_commit.id())?;
        info!(name, branch, session_branch = %session_branch_name,
              path = %worktree_path.display(), "worktree already on disk — resuming");
        return Ok(WorktreeInfo {
            name: name.to_string(),
            path: worktree_path.to_path_buf(),
            branch: Some(branch.to_string()),
            base_commit: Some(checkout_oid.to_string()),
            upstream_head: Some(origin_commit.id().to_string()),
            is_locked: false,
        });
    }

    // No worktree on disk, so there is no local state to preserve — but a
    // reused session branch may sit at an older commit than the freshly
    // resolved base (typical when the base moved under REFRESH after the
    // branch was left behind). Fast-forward the ref when its tip is a strict
    // ancestor of the base so the new checkout starts where the caller asked;
    // a tip with its own session commits is not an ancestor and is kept —
    // resuming committed work at its real tip is correct, and `base_commit`
    // reports that tip rather than pretending the requested base was used.
    let mut reference = reference;
    let mut checkout_oid = reference.peel_to_commit()?.id();
    if checkout_oid != origin_commit.id()
        && repo
            .merge_base(checkout_oid, origin_commit.id())
            .is_ok_and(|mb| mb == checkout_oid)
    {
        info!(branch = %session_branch_name, from = %checkout_oid, to = %origin_commit.id(),
              "fast-forwarding stale session branch to the requested base");
        reference = reference.set_target(
            origin_commit.id(),
            "forgeql: fast-forward stale session branch to requested base",
        )?;
        checkout_oid = origin_commit.id();
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
        base_commit: Some(checkout_oid.to_string()),
        upstream_head: Some(origin_commit.id().to_string()),
        is_locked: false,
    })
}

/// Decide what commit a RESUMED worktree checkout stands on, healing the one
/// unambiguous stale case.
///
/// The existing checkout's HEAD is the truth about what the session will
/// read — it may sit behind a base that moved (REFRESH while the session
/// idled) or ahead of it (session commits). A CLEAN checkout whose HEAD is a
/// strict ancestor of the requested base fast-forwards to it, so a re-USE
/// after REFRESH actually picks up the new base instead of silently resuming
/// the old one. Anything else (local modifications, or a tip with its own
/// commits) is preserved as-is and reported truthfully via the returned oid.
/// Untracked files survive a hard reset, so only tracked modifications block
/// the heal.
fn resume_checkout_oid(
    worktree_path: &Path,
    name: &str,
    branch: &str,
    requested: git2::Oid,
) -> Result<git2::Oid> {
    let wt_repo = Repository::open(worktree_path)?;
    let head_oid = wt_repo.head()?.peel_to_commit()?.id();
    if head_oid == requested {
        return Ok(head_oid);
    }
    let is_ancestor = wt_repo
        .merge_base(head_oid, requested)
        .is_ok_and(|mb| mb == head_oid);
    let mut status_opts = git2::StatusOptions::new();
    let _ = status_opts.include_untracked(false).include_ignored(false);
    let clean = wt_repo
        .statuses(Some(&mut status_opts))
        .is_ok_and(|s| s.is_empty());
    if is_ancestor && clean {
        let target = wt_repo.find_commit(requested)?;
        wt_repo.reset(target.as_object(), git2::ResetType::Hard, None)?;
        info!(name, branch, from = %head_oid, to = %requested,
              "worktree resumed clean behind the requested base — fast-forwarded");
        Ok(requested)
    } else {
        info!(name, branch, head = %head_oid, %requested,
              "worktree resumed at its own tip — differs from the requested base");
        Ok(head_oid)
    }
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
            upstream_head: None,
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
mod tests;
