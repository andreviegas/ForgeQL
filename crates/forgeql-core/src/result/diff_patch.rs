//! Result types for diff and patch-export operations.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One patch file produced by `EXPORT PATCH`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchFileEntry {
    /// Absolute path of the mbox file inside the session worktree.
    pub path: PathBuf,
    /// File size in bytes.
    pub bytes: u64,
    /// SHA-256 of the file contents (hex). Verify after transferring the
    /// file — or after copying the inline text — before running `git am`.
    pub sha256: String,
}

/// Result of `EXPORT PATCH [LAST n]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportPatchResult {
    /// The commit range that was exported (e.g. `a1b2c3d..HEAD` or
    /// `last 3 commit(s)`).
    pub range: String,
    /// Patch files in series order (apply with `git am` in this order).
    pub files: Vec<PatchFileEntry>,
    /// Concatenated mbox content of every patch file, inlined for copying;
    /// over-cap output is windowed and pageable via `SHOW MORE`.
    pub content: String,
    /// Caution the agent should surface (uncommitted changes not exported,
    /// or a range whose commits carried only runtime files).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// One file's entry in a [`ShowDiffResult`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffFileEntry {
    /// Path relative to the worktree root.
    pub path: std::path::PathBuf,
    /// Single-letter git status: `A`dded (incl. untracked), `M`odified,
    /// `D`eleted, `R`enamed, `T`ypechange.
    pub status: String,
    /// Count of added (`+`) lines.
    pub added: usize,
    /// Count of removed (`-`) lines.
    pub removed: usize,
}

/// Result of `SHOW DIFF [STAT]` — the session worktree's **uncommitted** diff.
///
/// `EXPORT PATCH` covers committed work only, so this is the one way an agent
/// (in particular a pre-commit reviewer that cannot read the worktree from the
/// filesystem) can see a pending change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowDiffResult {
    /// The file map — one row per changed file, after clause filtering.
    pub files: Vec<DiffFileEntry>,
    /// Unified-diff text for the surviving files; empty for `STAT`.
    /// Over-cap output is windowed and pageable via `SHOW MORE`.
    pub content: String,
    /// Caution the agent should surface (e.g. a clean worktree).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}
