//! Per-overlay advisory file locking (Phase 05 R7).
//!
//! When two processes (or threads) call `USE` on the same `(source, branch)`
//! at roughly the same commit, both can race to build the workspace overlay
//! file at `<bare>/forgeql/overlays/<provider>/<commit>.bin`.  Without
//! coordination they would either:
//!
//! - Build the same overlay twice (wasted CPU on a large repo), or
//! - Race on `tempfile.persist(overlay_path)` and one rename clobbers the
//!   other mid-mmap on a third reader.
//!
//! [`OverlayLock`] takes an exclusive advisory lock on a sibling
//! `<commit>.lock` file before returning.  The caller then re-checks
//! `overlay_path.exists()` inside the critical section so a peer that
//! finished the build while we waited is observed and respected.
//!
//! The lock is **process-level** (POSIX `flock` / Windows `LockFileEx`) and
//! is released when the [`OverlayLock`] is dropped: the underlying file
//! descriptor is closed, and both POSIX `flock` and Windows `LockFileEx`
//! release any locks held against a file when its last fd is closed.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fd_lock::RwLock as FdRwLock;

/// Advisory write-lock guarding a single overlay file.
///
/// The lock file lives next to the overlay (`<commit>.lock`) and is created
/// on demand.  Holding the value keeps the lock alive; dropping it releases
/// the lock immediately.
pub struct OverlayLock {
    /// Path to the on-disk lock file (kept for diagnostics).
    #[expect(dead_code, reason = "retained as metadata for diagnostics")]
    lock_path: PathBuf,
    /// Owns the locked file descriptor.  Dropping the `RwLock` drops the
    /// inner `File`, closes the fd, and releases the OS-level lock.
    _rw: Box<FdRwLock<std::fs::File>>,
}

impl OverlayLock {
    /// Acquire an exclusive write-lock on the lock file beside `overlay_path`.
    ///
    /// Blocks until the lock is granted.  The lock file is created on
    /// demand (the parent directory is created too if missing).
    ///
    /// # Errors
    /// Returns `Err` if the lock-file path cannot be created or the OS
    /// rejects the lock request.
    pub fn acquire(overlay_path: &Path) -> Result<Self> {
        let lock_path = overlay_path.with_extension("lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating overlay lock dir {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening overlay lock {}", lock_path.display()))?;

        // Heap-allocate so the address is stable for the borrow that the
        // write-guard takes below.
        let mut rw = Box::new(FdRwLock::new(file));
        {
            // Take an exclusive lock and immediately `forget` the guard.
            // Forgetting the guard skips its `Drop`, which would have
            // explicitly unlocked the file.  The OS lock remains held
            // until the file descriptor is closed (i.e. when `_rw` is
            // dropped at the end of `OverlayLock`'s lifetime).
            //
            // The borrow ends here because `forget` consumes the guard,
            // so we can move `rw` into the returned struct freely.
            let guard = rw
                .write()
                .with_context(|| format!("locking overlay lock {}", lock_path.display()))?;
            std::mem::forget(guard);
        }

        Ok(Self { lock_path, _rw: rw })
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    /// Acquire releases on drop and the lock file is created on demand.
    #[test]
    fn lock_creates_file_and_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let overlay = tmp.path().join("nested").join("dir").join("abc.bin");
        {
            let _l = OverlayLock::acquire(&overlay).expect("acquire");
            assert!(overlay.with_extension("lock").exists());
        }
        // After drop we can re-acquire immediately.
        let _l2 = OverlayLock::acquire(&overlay).expect("re-acquire");
    }

    /// A second acquirer must wait for the first to release.
    /// Only meaningful on POSIX (intra-process flock semantics).
    #[cfg(unix)]
    #[test]
    fn second_acquirer_blocks_until_release() {
        let tmp = tempfile::tempdir().unwrap();
        let overlay = tmp.path().join("blocking.bin");
        let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

        let order_a = Arc::clone(&order);
        let path_a = overlay.clone();
        let a = thread::spawn(move || {
            let l = OverlayLock::acquire(&path_a).expect("a acquire");
            order_a.lock().unwrap().push("a-locked");
            thread::sleep(Duration::from_millis(150));
            order_a.lock().unwrap().push("a-release");
            drop(l);
        });

        // Give A a head start.
        thread::sleep(Duration::from_millis(20));

        let order_b = Arc::clone(&order);
        let path_b = overlay;
        let b = thread::spawn(move || {
            let start = Instant::now();
            let _l = OverlayLock::acquire(&path_b).expect("b acquire");
            let waited = start.elapsed();
            order_b.lock().unwrap().push("b-locked");
            assert!(
                waited >= Duration::from_millis(80),
                "b should have waited for a; waited={waited:?}"
            );
        });

        a.join().unwrap();
        b.join().unwrap();
        let final_order = order.lock().unwrap().clone();
        assert_eq!(
            final_order,
            vec!["a-locked", "a-release", "b-locked"],
            "lock did not serialise the two threads"
        );
    }
}
