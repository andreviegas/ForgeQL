//! Result types for build/verify and background-job operations.

use serde::{Deserialize, Serialize};

use super::default_summary_lines;

// -----------------------------------------------------------------------
// Verify build result
// -----------------------------------------------------------------------

/// Result of a standalone `VERIFY build 'step'` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyBuildResult {
    /// The verify step name that was run.
    pub step: String,
    /// Whether the step command exited successfully.
    pub success: bool,
    /// Combined stdout + stderr output from the command.
    pub output: String,
    /// Number of output lines to show inline before buffering the rest for
    /// `SHOW MORE`. Resolved from the step's `summary` config at run time.
    #[serde(default = "default_summary_lines")]
    pub summary_lines: usize,
    /// Which end of the output to show inline (tail by default).
    #[serde(default)]
    pub summary_direction: crate::config::SummaryDirection,
}

/// Result of `JOB START '<label>'` — the submitted job's id and label.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStartedResult {
    /// Opaque job id to poll with `JOB STATUS`.
    pub id: String,
    /// The verify-step label this job runs.
    pub label: String,
}

/// Result of `JOB LIST` — summaries of all known background jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobListResult {
    /// Jobs in submission order (newest last).
    pub jobs: Vec<crate::jobs::JobSummary>,
}

/// Result of a standalone `RUN '<step>' <args…>` command.
///
/// The output of an allowlisted `run_steps` template. Shape mirrors
/// [`VerifyBuildResult`]; the distinct type lets the renderer label it `RUN`
/// and buffer its output for `SHOW MORE`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    /// The run step (template) name that was executed.
    pub step: String,
    /// Whether the command exited successfully.
    pub success: bool,
    /// Combined stdout + stderr output from the command.
    pub output: String,
    /// Number of output lines to show inline before buffering the rest for
    /// `SHOW MORE`. Resolved from the step's `summary` config at run time.
    #[serde(default = "default_summary_lines")]
    pub summary_lines: usize,
    /// Which end of the output to show inline (tail by default).
    #[serde(default)]
    pub summary_direction: crate::config::SummaryDirection,
}
