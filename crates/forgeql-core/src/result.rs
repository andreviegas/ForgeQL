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

/// Names are truncated to this many **characters**, never bytes: slicing a
/// `&str` at a byte index that is not a character boundary panics, and a name
/// here is arbitrary source text — a documentation line ending in an em dash
/// put byte 120 inside that dash and took the process down.
///
/// Not purely a display bound: [`comment_snippet`] is also called while
/// indexing, to label a block row, so a value below the 40 characters that
/// label is re-cut to would change stored index output and would need an
/// `ENRICH_VER` bump. At 120 it is invisible there.
pub(crate) const NAME_DISPLAY_CHARS: usize = 120;

/// A single-line orientation snippet of a (possibly multi-line) name: the first
/// line that carries real (alphanumeric) content, trimmed, truncated to
/// `NAME_DISPLAY_CHARS` characters, with a trailing `…` when any content was
/// dropped. Bare comment openers like `/**`, `/*`, `//` are skipped so block
/// comments surface their text, not a delimiter. Used so a comment name never
/// spills raw multi-line text into the name column while still hinting what the
/// comment says.
pub(crate) fn comment_snippet(name: &str) -> String {
    let max = NAME_DISPLAY_CHARS;
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

/// Compact a symbol name for display. A multi-line name (e.g. a block comment)
/// is rendered as the single-line snippet [`comment_snippet`] builds from it;
/// a single-line name longer than `NAME_DISPLAY_CHARS` characters is truncated
/// with `…`. This is a display bound only — the untruncated name is what
/// filtering, ordering, dedup and every count are computed from.
pub(crate) fn compact_name(name: &str) -> std::borrow::Cow<'_, str> {
    if name.contains('\n') {
        std::borrow::Cow::Owned(comment_snippet(name))
    } else if name.chars().count() > NAME_DISPLAY_CHARS {
        std::borrow::Cow::Owned(format!(
            "{}…",
            name.chars().take(NAME_DISPLAY_CHARS).collect::<String>()
        ))
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

    /// Append the notice that `paths` were re-indexed before this answer
    /// because they had changed on disk outside `ForgeQL`. Carried by every
    /// `Show` result — each `ShowContent` variant's renderer emits the `hint`
    /// row, pinned by `every_show_content_renders_the_hint_it_carries` — and by
    /// `FIND NODE`; the `Query` arm is defensive, since no gated verb returns a
    /// `Query` result today. A mutation carries none: a stale rev is refused
    /// before it runs, a matching one names bytes the gate has just verified,
    /// and the forms that quote no rev at all — an EOF append to a whole-file
    /// handle, `COPY NODE … TO`, `COPY NODES FOUND TO` — are gated the same way
    /// but have nothing to refuse, so they act on the re-indexed bytes without
    /// announcing it. Every mutation's boundary diff shows the lines it wrote.
    pub fn note_reindexed_outside_forgeql(&mut self, paths: &[PathBuf]) {
        let notice = Self::reindexed_outside_forgeql_notice(paths);
        let slot = match self {
            Self::Show(s) => &mut s.hint,
            Self::Query(q) => &mut q.hint,
            Self::FindNode(n) => &mut n.hint,
            _ => return,
        };
        *slot = Some(match slot.take() {
            Some(existing) => format!("{existing} {notice}"),
            None => notice,
        });
    }

    /// The wording of that notice for an answer that carries lines and revs.
    /// A refusal issued right after the gate re-indexed a file carries the
    /// notice too, but with a different tail — see
    /// [`Self::reindexed_before_refusal_notice`].
    pub(crate) fn reindexed_outside_forgeql_notice(paths: &[PathBuf]) -> String {
        Self::reindexed_notice_with(
            paths,
            "Handles are stable; the lines and revs in this answer are current, \
             and a rev read before the change is refused.",
        )
    }

    /// The same notice for a command that was REFUSED. A refusal carries no
    /// lines and no revs, so it cannot claim they are current; all it can say
    /// is that the re-index happened first, and that handles still resolve.
    pub(crate) fn reindexed_before_refusal_notice(paths: &[PathBuf]) -> String {
        Self::reindexed_notice_with(
            paths,
            "Handles are stable; this happened before the command was refused, \
             so the refusal describes the file as it is now.",
        )
    }

    /// The shared body: which files, capped, and why — `tail` says what the
    /// answer carrying it can promise.
    fn reindexed_notice_with(paths: &[PathBuf], tail: &str) -> String {
        /// Paths named in full before the notice starts counting instead. A
        /// `FOUND` sweep can name every file in the worktree, and a notice is
        /// one row of an answer, not a listing.
        const NAMED: usize = 5;
        let listed = paths
            .iter()
            .take(NAMED)
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let listed = match paths.len().saturating_sub(NAMED) {
            0 => listed,
            more => format!("{listed} and {more} more"),
        };
        let subject = if paths.len() == 1 { "the file" } else { "they" };
        format!(
            "re-indexed {listed}: {subject} changed on disk outside ForgeQL after being \
             indexed (a formatter, a build step, an editor). {tail}"
        )
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
