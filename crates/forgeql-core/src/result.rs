/// Typed result types for every `ForgeQL` operation.
///
/// These replace all `serde_json::Value` returns from the executor.  Transport
/// layers (MCP, REPL, pipe) serialize or format these as needed — the core
/// library never decides the wire format.
///
/// # Design
///
/// - Every operation returns a `ForgeQLResult` variant.
/// - Inner structs are `Serialize + Deserialize` for MCP JSON transport.
/// - `ForgeQLResult::to_display()` produces human-friendly terminal output.
/// - No `serde_json::Value` appears anywhere in this module.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod convert;
mod diff_patch;
mod display;
mod jobs;
mod mutation;
mod query;
mod show;
mod source_ops;
mod transaction;

pub use diff_patch::*;
pub use jobs::*;
pub use mutation::*;
pub use query::*;
pub use show::*;
pub use source_ops::*;
pub use transaction::*;

// -----------------------------------------------------------------------
// Top-level result enum
// -----------------------------------------------------------------------

/// The unified return type for all `ForgeQL` operations.
///
/// The engine's `execute()` method returns this; transport layers convert it
/// to JSON (MCP) or formatted text (REPL/pipe) without re-interpreting the
/// inner data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ForgeQLResult {
    /// Read-only queries: FIND symbols, FIND usages, FIND defines, etc.
    Query(QueryResult),
    /// Code exposure: SHOW body, SHOW outline, SHOW members, etc.
    Show(ShowResult),
    /// Mutations: RENAME, CHANGE, MIGRATE DEFINE, MIGRATE ENUM, etc.
    Mutation(MutationResult),
    /// Source and session lifecycle: CREATE SOURCE, USE, DISCONNECT, etc.
    SourceOp(SourceOpResult),
    /// Checkpoint: BEGIN TRANSACTION 'name'
    BeginTransaction(BeginTransactionResult),
    /// Commit: COMMIT MESSAGE 'msg'
    Commit(CommitResult),
    /// Plan preview: `DRY_RUN` and `EXPLAIN` (never writes files).
    Plan(PlanResult),
    /// Rollback: ROLLBACK [TRANSACTION 'name']
    Rollback(RollbackResult),
    /// Standalone verify: VERIFY build 'step'
    VerifyBuild(VerifyBuildResult),
    /// Output of a standalone `RUN '<step>' <args…>` command template.
    Run(RunResult),
    /// Node-addressed lookup: FIND NODE id
    FindNode(FindNodeResult),
    /// Background job submitted: `JOB START '<label>'`
    JobStarted(JobStartedResult),
    /// Background job status: `JOB STATUS '<id>'`
    JobStatus(crate::jobs::JobSnapshot),
    /// Background job list: `JOB LIST`
    JobList(JobListResult),
    /// Intermediate: a `VERIFY build` / `RUN` command submitted to the job
    /// pool. Never returned to end callers — transports wait on the job and
    /// convert it into `VerifyBuild` / `Run` (or `JobStarted` on wait timeout).
    PendingExec(PendingExecResult),
    /// Patch export: `EXPORT PATCH [LAST n]`
    ExportPatch(ExportPatchResult),
    /// Uncommitted worktree diff: `SHOW DIFF [STAT]`
    ShowDiff(ShowDiffResult),
}

/// Stage 2 block alias: when a row is a block member (it carries `block_ord` /
/// `block_off` fields written at index time), surface its handle as
/// `block_id(offset)` instead of the member's own node id. The member's segment
/// prefix is reused, so only the ordinal and offset change. Members still
/// resolve by their own node id; this only changes what FIND/SHOW display.
fn surface_block_alias(row: &SymbolMatch) -> Option<String> {
    let own = row.node_id.as_deref()?;
    Some(crate::node_id::surface_block_id(
        own,
        row.fields.get("block_ord").map(String::as_str),
        row.fields.get("block_off").map(String::as_str),
    ))
}

/// Serde default for [`VerifyBuildResult::summary_lines`].
const fn default_summary_lines() -> usize {
    40
}

// -----------------------------------------------------------------------
// Display helpers
// -----------------------------------------------------------------------

/// Compact a symbol name for display.  Multi-line names (e.g. block comments)
/// are replaced with `len:<bytes>` so they don't flood the output.
/// Single-line names longer than 120 chars are truncated with `…`.
/// A single-line orientation snippet of a (possibly multi-line) name: the first
/// line, trimmed, truncated to 120 chars, with a trailing `…` when any content
/// was dropped. Used so a comment name never spills raw multi-line text into the
/// name column while still hinting what the comment says.
/// A single-line orientation snippet of a (possibly multi-line) name: the first
/// line that carries real (alphanumeric) content, trimmed, truncated to 120
/// chars, with a trailing `…` when any content was dropped. Bare comment openers
/// like `/**`, `/*`, `//` are skipped so block comments surface their text, not a
/// delimiter. Used so a comment name never spills raw multi-line text into the
/// name column while still hinting what the comment says.
pub(crate) fn comment_snippet(name: &str) -> String {
    let max = 120usize;
    let full = name.trim();
    let chosen = name
        .lines()
        .map(str::trim)
        .find(|l| l.chars().any(char::is_alphanumeric))
        .or_else(|| name.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("");
    let dropped = chosen.len() < full.len();
    let mut snippet: String = chosen.chars().take(max).collect();
    if dropped || chosen.chars().count() > max {
        snippet.push('…');
    }
    snippet
}

pub(crate) fn compact_name(name: &str) -> std::borrow::Cow<'_, str> {
    if name.contains('\n') {
        std::borrow::Cow::Owned(comment_snippet(name))
    } else if name.len() > 120 {
        std::borrow::Cow::Owned(format!("{}…", &name[..120]))
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

// -----------------------------------------------------------------------
// Path relativization — strip worktree prefix from all paths
// -----------------------------------------------------------------------

/// Strip `root` from the front of `path`, returning a relative path.
/// Falls back to the original path if it doesn't start with root.
fn relativize(path: &mut PathBuf, root: &Path) {
    if let Ok(rel) = path.strip_prefix(root) {
        *path = rel.to_path_buf();
    }
}

impl ForgeQLResult {
    /// Strip absolute worktree prefixes from all paths in this result.
    ///
    /// Converts `/data/worktrees/s123/src/foo.cpp` → `src/foo.cpp`.
    /// Called by the engine after every `execute()` so that transport layers
    /// (MCP JSON, REPL, pipe) never see internal filesystem paths.
    pub fn relativize_paths(&mut self, worktree_root: &Path) {
        match self {
            Self::Query(q) => {
                for row in &mut q.results {
                    if let Some(ref mut p) = row.path {
                        relativize(p, worktree_root);
                    }
                }
            }
            Self::Show(s) => {
                if let Some(ref mut p) = s.file {
                    relativize(p, worktree_root);
                }
                match &mut s.content {
                    ShowContent::Outline { entries } => {
                        for entry in entries {
                            relativize(&mut entry.path, worktree_root);
                        }
                    }
                    ShowContent::CallGraph { entries, .. } => {
                        for entry in entries {
                            if let Some(ref mut p) = entry.path {
                                relativize(p, worktree_root);
                            }
                        }
                    }
                    ShowContent::FileList { files, .. } => {
                        for entry in files {
                            relativize(&mut entry.path, worktree_root);
                        }
                    }
                    ShowContent::Lines { .. }
                    | ShowContent::Signature { .. }
                    | ShowContent::Members { .. }
                    | ShowContent::Stats { .. }
                    | ShowContent::Paged { .. } => {}
                }
            }
            Self::Mutation(m) => {
                for p in &mut m.files_changed {
                    relativize(p, worktree_root);
                }
                for s in &mut m.suggestions {
                    relativize(&mut s.path, worktree_root);
                }
            }
            Self::Plan(p) => {
                for fe in &mut p.file_edits {
                    relativize(&mut fe.path, worktree_root);
                }
                for s in &mut p.suggestions {
                    relativize(&mut s.path, worktree_root);
                }
            }
            Self::FindNode(r) => relativize(&mut r.path, worktree_root),
            Self::BeginTransaction(_)
            | Self::JobStarted(_)
            | Self::JobStatus(_)
            | Self::JobList(_)
            | Self::PendingExec(_)
            | Self::Commit(_)
            | Self::SourceOp(_)
            | Self::VerifyBuild(_)
            | Self::Run(_)
            | Self::Rollback(_)
            // ExportPatch paths deliberately stay absolute: the patch files
            // are transfer artifacts the user fetches from outside the
            // session, so the full worktree path is the deliverable.
            | Self::ExportPatch(_)
            // ShowDiff paths arrive from git already relative to the worktree
            // root, so there is no prefix to strip.
            | Self::ShowDiff(_) => {}
        }
    }

    /// Count the number of raw source-code lines contained in this result.
    ///
    /// Used by the query logger to track how much source code was disclosed
    /// to the AI agent per operation.  Only `SHOW` results that return actual
    /// file lines contribute (`SHOW LINES`, `SHOW body`, `SHOW context`).
    /// Structured metadata results (outline, members, call graph) and all
    /// query / mutation results return `0` because they carry no raw source
    /// code.
    #[must_use]
    pub const fn source_lines_count(&self) -> usize {
        if let Self::Show(ShowResult {
            content: ShowContent::Lines { lines, .. },
            ..
        }) = self
        {
            lines.len()
        } else {
            0
        }
    }

    /// Whether the inline source-line output exceeds `cap` and will be windowed
    /// for `SHOW MORE` — computed from the line count at execute time so the
    /// coach observes capping before any transport renders it.
    #[must_use]
    pub const fn output_capped(&self, cap: usize) -> bool {
        matches!(
            self,
            Self::Show(ShowResult {
                content: ShowContent::Lines { lines, .. },
                ..
            }) if lines.len() > cap
        )
    }

    /// Whether a result set was returned only in part — more rows matched than
    /// were shown (a `FIND` capped by `LIMIT`).
    #[must_use]
    pub const fn output_truncated(&self) -> bool {
        if let Self::Query(q) = self {
            q.total > q.results.len()
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests;
