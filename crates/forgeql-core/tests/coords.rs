//! Tests for [`SessionCoords`] — session identity and derived values.
//!
//! Run with: `cargo test -p forgeql-core --test coords`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown
)]

use std::path::{Path, PathBuf};

use forgeql_core::session::SessionCoords;

fn coords() -> SessionCoords {
    SessionCoords::anonymous("pisco-firmware", "main", "research")
}

// --- constructors ---

#[test]
fn anonymous_sets_user() {
    assert_eq!(coords().user, "anonymous");
}

#[test]
fn new_preserves_all_fields() {
    let c = SessionCoords::new("alice", "zephyr", "fix/null-check", "task-42");
    assert_eq!(c.user, "alice");
    assert_eq!(c.source, "zephyr");
    assert_eq!(c.branch, "fix/null-check");
    assert_eq!(c.alias, "task-42");
}

// --- map_key ---

#[test]
fn map_key_combines_user_and_alias() {
    assert_eq!(coords().map_key(), "anonymous:research");
}

#[test]
fn map_key_different_users_same_alias_are_distinct() {
    let a = SessionCoords::new("alice", "repo", "main", "review");
    let b = SessionCoords::new("bob", "repo", "main", "review");
    assert_ne!(a.map_key(), b.map_key());
}

// --- git_branch ---

#[test]
fn git_branch_includes_all_four_components() {
    assert_eq!(
        coords().git_branch(),
        "fql/anonymous/pisco-firmware/main/research"
    );
}

#[test]
fn git_branch_with_slash_in_branch_preserves_slash() {
    let c = SessionCoords::new("bob", "zephyr", "fix/null-check", "task-42");
    assert_eq!(c.git_branch(), "fql/bob/zephyr/fix/null-check/task-42");
}

#[test]
fn git_branch_different_sources_same_user_and_alias_are_distinct() {
    let a = SessionCoords::new("alice", "pisco-firmware", "main", "r");
    let b = SessionCoords::new("alice", "zephyr-andre", "main", "r");
    assert_ne!(a.git_branch(), b.git_branch());
}

#[test]
fn git_branch_different_users_same_rest_are_distinct() {
    let a = SessionCoords::new("alice", "repo", "main", "r");
    let b = SessionCoords::new("bob", "repo", "main", "r");
    assert_ne!(a.git_branch(), b.git_branch());
}

// --- worktree_dir ---

#[test]
fn worktree_dir_basic() {
    assert_eq!(coords().worktree_dir(), "pisco-firmware.main.research");
}

#[test]
fn worktree_dir_replaces_slashes_in_branch_with_dashes() {
    let c = SessionCoords::anonymous("repo", "fix/null-check", "task");
    assert_eq!(c.worktree_dir(), "repo.fix-null-check.task");
}

#[test]
fn worktree_dir_replaces_slashes_in_source_with_dashes() {
    // Pathological but must not create nested directories.
    let c = SessionCoords::anonymous("org/repo", "main", "r");
    assert_eq!(c.worktree_dir(), "org-repo.main.r");
}

#[test]
fn worktree_dir_different_sources_same_alias_are_distinct() {
    let a = SessionCoords::anonymous("pisco-firmware", "main", "r");
    let b = SessionCoords::anonymous("zephyr-andre", "main", "r");
    assert_ne!(a.worktree_dir(), b.worktree_dir());
}

// --- worktree_path ---

#[test]
fn worktree_path_includes_user_subdir() {
    let data = PathBuf::from("/data/forgeql");
    let path = coords().worktree_path(&data);
    assert_eq!(
        path,
        PathBuf::from("/data/forgeql/worktrees/anonymous/pisco-firmware.main.research")
    );
}

#[test]
fn worktree_path_different_users_same_rest_are_distinct() {
    let data = PathBuf::from("/data");
    let a = SessionCoords::new("alice", "repo", "main", "r").worktree_path(&data);
    let b = SessionCoords::new("bob", "repo", "main", "r").worktree_path(&data);
    assert_ne!(a, b);
}

// --- worktrees_root / user_worktrees_root ---

#[test]
fn worktrees_root_appends_worktrees_dir() {
    assert_eq!(
        SessionCoords::worktrees_root(Path::new("/data")),
        PathBuf::from("/data/worktrees")
    );
}

#[test]
fn user_worktrees_root_appends_user() {
    assert_eq!(
        SessionCoords::user_worktrees_root(Path::new("/data"), "alice"),
        PathBuf::from("/data/worktrees/alice")
    );
}

// --- is_sha_ref ---

#[test]
fn is_sha_ref_false_for_named_branch() {
    assert!(!SessionCoords::anonymous("r", "main", "a").is_sha_ref());
    assert!(!SessionCoords::anonymous("r", "fix/null-check", "a").is_sha_ref());
}

#[test]
fn is_sha_ref_false_for_short_string() {
    // 6 hex chars — too short
    assert!(!SessionCoords::anonymous("r", "a3f9b2", "a").is_sha_ref());
}

#[test]
fn is_sha_ref_true_for_7_char_hex() {
    assert!(SessionCoords::anonymous("r", "a3f9b2c", "a").is_sha_ref());
}

#[test]
fn is_sha_ref_true_for_full_40_char_sha() {
    let sha = "a3f9b2c7e91f3d2b4c5e6f7a8b9c0d1e2f3a4b5c";
    assert!(SessionCoords::anonymous("r", sha, "a").is_sha_ref());
}

#[test]
fn is_sha_ref_false_when_contains_non_hex_letter() {
    // 'g' is not a hex digit
    assert!(!SessionCoords::anonymous("r", "a3f9b2g", "a").is_sha_ref());
}

// --- budget_branch ---

#[test]
fn budget_branch_returns_alias_for_main() {
    let c = SessionCoords::anonymous("repo", "main", "my-feature");
    assert_eq!(c.budget_branch(), "my-feature");
}

#[test]
fn budget_branch_returns_alias_for_master() {
    let c = SessionCoords::anonymous("repo", "master", "my-feature");
    assert_eq!(c.budget_branch(), "my-feature");
}

#[test]
fn budget_branch_returns_branch_for_feature_branch() {
    let c = SessionCoords::anonymous("repo", "fix/null-check", "task-42");
    assert_eq!(c.budget_branch(), "fix/null-check");
}

// --- validate ---

#[test]
fn validate_ok_when_alias_differs_from_branch() {
    assert!(coords().validate().is_ok());
}

#[test]
fn validate_err_when_alias_equals_branch() {
    let c = SessionCoords::anonymous("repo", "main", "main");
    assert!(c.validate().is_err());
    let msg = c.validate().unwrap_err();
    assert!(msg.contains("'main'"));
}

// --- from_dir_name ---

#[test]
fn from_dir_name_roundtrip() {
    let c = SessionCoords::anonymous("pisco-firmware", "main", "research");
    let dir = c.worktree_dir();
    let recovered = SessionCoords::from_dir_name("anonymous", "pisco-firmware", "research", &dir);
    assert_eq!(recovered, Some(c));
}

#[test]
fn from_dir_name_feature_branch_loses_slashes() {
    // slash in branch becomes dash in dir name — recovered branch has dashes
    let c = SessionCoords::anonymous("repo", "fix/null-check", "task");
    let dir = c.worktree_dir(); // "repo.fix-null-check.task"
    let recovered = SessionCoords::from_dir_name("anonymous", "repo", "task", &dir).unwrap();
    assert_eq!(recovered.branch, "fix-null-check"); // lossy — dashes, not slashes
}

#[test]
fn from_dir_name_returns_none_on_mismatch() {
    assert!(SessionCoords::from_dir_name("anonymous", "repo", "alias", "unrelated").is_none());
    assert!(
        SessionCoords::from_dir_name("anonymous", "repo", "alias", "repo.main.other").is_none()
    );
}

#[test]
fn from_dir_name_returns_none_when_prefix_only() {
    // "repo." present but no suffix
    assert!(SessionCoords::from_dir_name("anonymous", "repo", "alias", "repo.main").is_none());
}
