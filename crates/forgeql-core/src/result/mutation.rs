//! Result types for mutation, plan, and pending-exec operations.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------
// Mutation results
// -----------------------------------------------------------------------

/// Result of a mutation operation (RENAME, CHANGE, MIGRATE).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationResult {
    /// Operation name: `"rename_symbol"`, `"change_content"`, etc.
    pub op: String,
    /// Whether the changes were written to disk.
    pub applied: bool,
    /// Files that were (or would be) modified.
    pub files_changed: Vec<PathBuf>,
    /// Total number of individual byte-range edits.
    pub edit_count: usize,
    /// Total number of lines in all replacement texts.
    ///
    /// Used by the budget system: the agent earns proportional recovery
    /// based on how many lines it actually wrote.
    pub lines_written: usize,
    /// Total number of lines in the original spans replaced or deleted by this
    /// mutation. Paired with `lines_written`, it is the loudest mechanical
    /// signal of a destructive edit: replacing a 60-line node with a 6-line
    /// body reports `lines_removed: 54, lines_written: 6`. The engine stays
    /// mechanical — it reports the line arithmetic and leaves the judgement to
    /// the agent.
    #[serde(default)]
    pub lines_removed: usize,
    /// Unified diff (populated for dry-run and explain modes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Advisory notes (e.g. string literals containing the symbol name)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<SuggestionEntry>,
    /// New node ID of the mutated/inserted symbol after reindex.
    ///
    /// * `CHANGE NODE`: same as the input `node_id` (ordinal is stable after
    ///   body replacement); confirmed by a post-reindex `find_node` lookup.
    /// * `INSERT BEFORE|AFTER NODE`: `node_id` of the first symbol found at
    ///   the insertion line after reindex, or `None` if the inserted content
    ///   contained no addressable symbol at that line.
    /// * `DELETE NODE`: always `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_node_id: Option<String>,
    /// Content rev of `new_node_id` after the edit.
    ///
    /// Returned so a follow-up mutation on the same node needs no re-read: with
    /// `IF REV` mandatory, a mutation that handed back a handle but not its new
    /// rev would force a `FIND NODE` round trip before every chained edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_rev: Option<String>,
    /// Structured-text files this mutation left unparseable under a strict format
    /// grammar, each with the parser's diagnostic. Empty when every touched file
    /// still parses (or has no strict validator). See [`StructuralError`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structural_errors: Vec<StructuralError>,
}

/// A structured-text file left unparseable by a mutation.
///
/// When an edit leaves a touched file invalid under its own strict grammar (a
/// broken `.json`, say), the engine reports the breakage and the parser's
/// message; it never repairs it (mechanical tool). One entry per touched file
/// that is invalid *after* the edit; a mutation whose touched files all still
/// parse (or have no strict validator) carries none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuralError {
    /// The touched file that no longer parses (workspace-relative path).
    pub path: PathBuf,
    /// Whether the file parsed cleanly *before* this edit. `Some(true)` — this
    /// edit introduced the error. `Some(false)` — it was already broken and the
    /// edit did not cause it (the defect being chased may be the breakage
    /// itself). `None` — the pre-edit state was unknown (e.g. a file this
    /// mutation created).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_before: Option<bool>,
    /// The strict parser's diagnostic, ideally with a line and column
    /// (e.g. `"expected ',' or '}' at line 1 column 10"`).
    pub message: String,
}

/// An advisory note about a potential issue found during planning.
///
/// For example, a string literal that contains the renamed symbol name
/// but was intentionally left unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionEntry {
    /// Source file path.
    pub path: PathBuf,
    /// Byte offset in the file.
    pub byte_offset: usize,
    /// Short excerpt of the surrounding code.
    pub snippet: String,
    /// Why this candidate was flagged.
    pub reason: String,
}

/// Which command submitted a [`PendingExecResult`] job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingExecKind {
    /// `VERIFY build '<step>'`.
    Verify,
    /// `RUN '<step>'`.
    Run,
}

/// Intermediate result of `VERIFY build` / `RUN` — the command now runs on
/// the background job pool.
///
/// The engine lock is never held while the subprocess runs. Transports wait
/// on `job_id` (up to `wait_secs`) and convert the finished job into a
/// [`VerifyBuildResult`] / [`RunResult`]; a job still running at the deadline
/// is surfaced as `JobStarted` for `JOB STATUS` polling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingExecResult {
    /// Job id in the background registry.
    pub job_id: String,
    /// The verify/run step name being executed.
    pub step: String,
    /// Which command produced this pending job.
    pub kind: PendingExecKind,
    /// Longest time a synchronous caller should wait before falling back to
    /// `JobStarted` — the step's `timeout_secs`.
    pub wait_secs: u64,
    /// Inline summary window carried through to the final result.
    pub summary_lines: usize,
    /// Which end of the output to show inline (tail by default).
    pub summary_direction: crate::config::SummaryDirection,
}

// -----------------------------------------------------------------------
// Plan results (DRY_RUN, EXPLAIN)
// -----------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanResult {
    /// Operation name: `"dry_run"` or `"explain"`.
    pub op: String,
    /// Unified diff showing what would change.
    pub diff: String,
    /// Summary of edits per file.
    pub file_edits: Vec<FileEditSummary>,
    /// Advisory notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<SuggestionEntry>,
}

/// Summary of edits planned for one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEditSummary {
    /// Source file path.
    pub path: PathBuf,
    /// Number of byte-range edits in this file.
    pub edit_count: usize,
}
