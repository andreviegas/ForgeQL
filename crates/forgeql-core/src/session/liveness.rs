//! Cross-process worktree liveness.
//!
//! A worktree directory is created by one engine process and can be swept by
//! another: every process runs a reclaim pass over `{data_dir}/worktrees/` at
//! startup and deletes what looks abandoned. "Looks abandoned" used to be
//! answered from the sweeping process's own in-memory session maps, so a
//! worktree a *peer* process had just started checking out — present on disk,
//! not yet carrying a session sentinel, unknown to the sweeper — was
//! indistinguishable from an orphan left by a crash. The sweeper deleted it
//! mid-checkout, and the peer's `USE` failed with a "No such file or
//! directory" error raised from deep inside the checkout.
//!
//! Liveness is therefore recorded where every process can see it: an advisory
//! lock on a claim file beside the worktree directory.
//!
//! - An owner takes a **shared** lock ([`WorktreeClaim::acquire`]) before
//!   `git worktree add` runs, and holds it for as long as it owns the
//!   worktree. Shared, so two processes attached to the same session — a
//!   resumed alias handed from one agent to another — both hold it without
//!   contending.
//! - A sweeper takes an **exclusive** lock ([`claim_for_reclaim`]) and removes
//!   the worktree only while holding it. A live owner makes that fail, and the
//!   sweeper leaves the worktree alone; holding the lock across the whole
//!   teardown stops an owner from starting up into a directory being deleted.
//!
//! Owners block for their lock and so never fail on contention; sweepers never
//! block. The OS releases both when the holding process dies, which is exactly
//! what a crash needs: no lease to expire, no stale record to clean up, and the
//! dead owner's worktree is reclaimable immediately.
//!
//! The lock cannot see a peer that predates it, so the reclaim gate also takes
//! a grace window: a worktree directory younger than [`CREATION_GRACE_SECS`]
//! that carries no session sentinel is left alone, on the grounds that a
//! checkout may still be filling it. The cost is that a genuine orphan survives
//! one extra startup.
//!
//! Claim files are never unlinked. Removing one while another process is about
//! to open it would hand the two processes locks on different inodes for the
//! same worktree — precisely the failure this module exists to prevent. They
//! are empty files and cost one directory entry each.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use fd_lock::RwLock as FdRwLock;

/// How long a worktree directory carrying no session sentinel is left alone
/// before a reclaim sweep may treat it as an orphan.
///
/// This is the fallback, not the defence: the claim lock covers every peer that
/// takes one. The window covers the peer that does not — an older build
/// mid-checkout — and a checkout of a large source tree can legitimately run
/// for minutes before its sentinel is written.
pub const CREATION_GRACE_SECS: u64 = 300;

/// Why a reclaim sweep must leave a worktree directory alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protected {
    /// A live process holds the worktree's claim.
    Claimed,
    /// No claim, but the directory is younger than the caller's grace window,
    /// so a peer may still be checking it out.
    UnderConstruction,
    /// Liveness could not be established at all — the claim file could not be
    /// created or opened. A sweep that cannot prove a worktree is dead leaves
    /// it where it is.
    Unknown,
}

/// Path of the claim file guarding `worktree_path`.
///
/// It sits beside the worktree rather than inside it, so it survives the
/// `remove_dir_all` a reclaim performs — a lock file destroyed halfway through
/// the operation it arbitrates would arbitrate nothing. It is derived from the
/// directory name alone, so a caller that holds only a path agrees with one
/// that holds full session coordinates. The leading dot keeps it out of the
/// restore scan, which descends into directories only.
///
/// `None` for a path with no parent or no final component: not a worktree path,
/// and nothing to guard.
#[must_use]
pub fn claim_path(worktree_path: &Path) -> Option<PathBuf> {
    let parent = worktree_path.parent()?;
    let name = worktree_path.file_name()?.to_str()?;
    Some(parent.join(format!(".{name}.claim")))
}

/// Open (creating on demand) the claim file for a worktree.
fn open_claim_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating worktree claim dir {}", parent.display()))?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening worktree claim {}", path.display()))
}

/// `true` when `worktree_path` was last modified within `grace_secs`.
///
/// A directory whose age cannot be read is treated as young: a sweep that
/// cannot prove a directory is old leaves it. A path with nothing on disk is
/// not young — there is no directory there to protect.
#[must_use]
pub fn within_creation_grace(worktree_path: &Path, grace_secs: u64) -> bool {
    if grace_secs == 0 {
        return false;
    }
    let Ok(meta) = std::fs::metadata(worktree_path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    SystemTime::now()
        .duration_since(modified)
        // A modification time in the future means clock skew, not age.
        .map_or(true, |age| age < Duration::from_secs(grace_secs))
}

/// A live owner's shared claim on one worktree.
///
/// Held for as long as this process owns the worktree. Dropping it — or the
/// process dying — closes the descriptor and releases the lock, which is what
/// makes the worktree reclaimable again.
pub struct WorktreeClaim {
    /// Path to the on-disk claim file.
    path: PathBuf,
    /// Owns the locked file descriptor. Dropping the `RwLock` drops the inner
    /// `File`, closes the fd, and releases the OS-level lock.
    _rw: Box<FdRwLock<File>>,
}

impl WorktreeClaim {
    /// Claim `worktree_path` for this process, creating the claim file on
    /// demand.
    ///
    /// Blocks while a reclaim sweep holds the worktree, and so returns only
    /// once the directory is safe to create in or read. Callers must acquire
    /// **before** `git worktree add` starts: a claim taken after the checkout
    /// begins leaves exactly the window this module exists to close.
    ///
    /// # Errors
    /// Returns `Err` if the path cannot name a claim file, the claim file
    /// cannot be created, or the OS rejects the lock request.
    pub fn acquire(worktree_path: &Path) -> Result<Self> {
        let path = claim_path(worktree_path)
            .ok_or_else(|| anyhow!("cannot claim worktree path {}", worktree_path.display()))?;
        let file = open_claim_file(&path)?;

        // Heap-allocate so the address stays stable for the borrow the guard
        // takes below.
        let rw = Box::new(FdRwLock::new(file));
        {
            // Forgetting the guard skips its `Drop`, which would unlock the
            // file explicitly. The OS lock stays held until the descriptor is
            // closed — i.e. until `_rw` is dropped with this value.
            let guard = rw
                .read()
                .with_context(|| format!("claiming worktree {}", path.display()))?;
            std::mem::forget(guard);
        }

        Ok(Self { path, _rw: rw })
    }

    /// Path of the claim file this value holds.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A sweeper's exclusive lock on one worktree, held for the duration of a
/// reclaim.
pub struct ReclaimClaim {
    /// Path to the on-disk claim file.
    path: PathBuf,
    /// Owns the locked file descriptor; see [`WorktreeClaim`].
    _rw: Box<FdRwLock<File>>,
}

impl ReclaimClaim {
    /// Path of the claim file this value holds.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Decide whether a reclaim sweep may delete `worktree_path`.
///
/// `Ok` hands back the exclusive lock: hold it for the whole teardown, so an
/// owner cannot start up into a directory being deleted. `Err` names the reason
/// the worktree must survive this pass.
///
/// `grace_secs` is the caller's creation-grace window. Pass
/// [`CREATION_GRACE_SECS`] where a partially created worktree is plausible —
/// no sentinel has been written yet, so the directory's age is the only
/// evidence of what it is — and `0` where the worktree's own on-disk record
/// already proves it is old, as an expired session sentinel does.
///
/// # Errors
/// Returns [`Protected`] — never a failure, always a reason the worktree must
/// be left where it is: a live process holds it, it is too young to judge, or
/// its liveness could not be established at all.
pub fn claim_for_reclaim(
    worktree_path: &Path,
    grace_secs: u64,
) -> std::result::Result<ReclaimClaim, Protected> {
    if within_creation_grace(worktree_path, grace_secs) {
        return Err(Protected::UnderConstruction);
    }
    let Some(path) = claim_path(worktree_path) else {
        return Err(Protected::Unknown);
    };
    let Ok(file) = open_claim_file(&path) else {
        return Err(Protected::Unknown);
    };

    let mut rw = Box::new(FdRwLock::new(file));
    match rw.try_write() {
        // See `WorktreeClaim::acquire` for why the guard is forgotten.
        Ok(guard) => std::mem::forget(guard),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Err(Protected::Claimed),
        Err(_) => return Err(Protected::Unknown),
    }

    Ok(ReclaimClaim { path, _rw: rw })
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    /// A worktree directory in the real layout: `worktrees/{user}/{dir}`.
    fn worktree_dir(root: &Path) -> PathBuf {
        let wt = root.join("worktrees/anonymous/src.main.alias");
        std::fs::create_dir_all(&wt).unwrap();
        wt
    }

    /// The decision the whole module exists to make: a worktree a live process
    /// holds survives a sweep run by a process that has never heard of it.
    #[test]
    fn a_claimed_worktree_is_never_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = worktree_dir(tmp.path());

        let _claim = WorktreeClaim::acquire(&wt).expect("acquire");

        assert_eq!(claim_for_reclaim(&wt, 0).err(), Some(Protected::Claimed));
    }

    /// The control: without a claim, and past the grace window, the sweep still
    /// reclaims. A gate that only ever says "keep" would pass the test above.
    #[test]
    fn an_unclaimed_worktree_past_the_grace_window_is_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = worktree_dir(tmp.path());

        assert!(claim_for_reclaim(&wt, 0).is_ok());
    }

    /// A crashed owner leaves no lease to expire: the OS drops the lock when
    /// the descriptor closes, and the worktree is immediately reclaimable.
    #[test]
    fn releasing_the_claim_reopens_reclaim() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = worktree_dir(tmp.path());

        let claim = WorktreeClaim::acquire(&wt).expect("acquire");
        assert!(claim_for_reclaim(&wt, 0).is_err());
        drop(claim);

        assert!(claim_for_reclaim(&wt, 0).is_ok());
    }

    /// Two owners of the same session share the worktree; the claim is shared,
    /// so neither blocks the other.
    #[test]
    fn two_owners_can_claim_the_same_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = worktree_dir(tmp.path());

        let _first = WorktreeClaim::acquire(&wt).expect("first owner");
        let _second = WorktreeClaim::acquire(&wt).expect("second owner");
    }

    /// Two sweepers must not tear the same worktree down at once.
    #[test]
    fn two_sweepers_cannot_reclaim_the_same_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = worktree_dir(tmp.path());

        let _first = claim_for_reclaim(&wt, 0).expect("first sweeper");

        assert_eq!(claim_for_reclaim(&wt, 0).err(), Some(Protected::Claimed));
    }

    /// A newborn worktree with no claim — an older build mid-checkout — is left
    /// alone until the grace window passes.
    #[test]
    fn a_fresh_unclaimed_worktree_is_left_under_construction() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = worktree_dir(tmp.path());

        assert_eq!(
            claim_for_reclaim(&wt, CREATION_GRACE_SECS).err(),
            Some(Protected::UnderConstruction)
        );
    }

    /// The grace window protects a directory, not a bare path: a worktree whose
    /// checkout has not appeared yet is judged by its claim alone.
    #[test]
    fn a_path_with_nothing_on_disk_is_not_under_construction() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("worktrees/anonymous/never.created.here");

        assert!(!within_creation_grace(&missing, CREATION_GRACE_SECS));
        assert!(claim_for_reclaim(&missing, CREATION_GRACE_SECS).is_ok());
    }

    /// The claim file must live outside the directory it guards, or a reclaim
    /// would delete the lock the two processes are arbitrating on.
    #[test]
    fn the_claim_file_sits_beside_the_worktree_not_inside_it() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = worktree_dir(tmp.path());

        let claim = WorktreeClaim::acquire(&wt).expect("acquire");

        assert_eq!(
            claim.path(),
            wt.parent().unwrap().join(".src.main.alias.claim")
        );
        assert!(claim.path().exists());
        assert!(!claim.path().starts_with(&wt));
    }
}
