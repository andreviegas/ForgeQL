//! Startup session-restore must never traverse the compatibility symlinks
//! that `USE` leaves at `worktrees/{dir}`, and must never prune a directory
//! that is not actually a git worktree.
//!
//! Both properties guard real damage: a restore scan that followed such a
//! symlink walked a worktree's own contents as if they were session dirs and
//! pruned every subdirectory that lacked a session sentinel.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use forgeql_core::engine::ForgeQLEngine;
use std::fs;

mod common;

#[cfg(unix)]
#[test]
fn restore_scan_ignores_legacy_symlinks_and_non_worktrees() {
    let tmp = tempfile::tempdir().expect("test setup");
    let data_dir = tmp.path().join("data");
    let wt = data_dir.join("worktrees/anonymous/src.main.alias");
    fs::create_dir_all(wt.join("sub")).expect("test setup");
    fs::write(wt.join(".git"), "gitdir: /nonexistent").expect("test setup");
    fs::write(wt.join("sub/keep.txt"), "content").expect("test setup");
    // Fresh timestamp-only sentinel: an old-format session that the restore
    // scan leaves in place for the agent to reconnect.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("test setup")
        .as_secs();
    fs::write(wt.join(".forgeql-session"), format!("timestamp={now}\n")).expect("test setup");
    // Compatibility symlink at the pre-user-segment location, exactly as
    // ensure_legacy_link creates it.
    std::os::unix::fs::symlink(
        std::path::Path::new("anonymous").join("src.main.alias"),
        data_dir.join("worktrees/src.main.alias"),
    )
    .expect("test setup");
    // A stray directory that is not a git worktree must survive untouched.
    let stray = data_dir.join("worktrees/anonymous/random-data");
    fs::create_dir_all(&stray).expect("test setup");
    fs::write(stray.join("precious.txt"), "do not delete").expect("test setup");

    let mut engine =
        ForgeQLEngine::new(data_dir.clone(), common::make_registry()).expect("test setup");
    engine.restore_sessions_from_disk();

    // The symlink was not traversed: the worktree's contents are intact.
    assert!(wt.join("sub/keep.txt").exists());
    // The non-worktree directory was not pruned.
    assert!(stray.join("precious.txt").exists());
    // The symlink itself is still in place.
    assert!(
        data_dir
            .join("worktrees/src.main.alias")
            .symlink_metadata()
            .expect("test setup")
            .file_type()
            .is_symlink()
    );
}
