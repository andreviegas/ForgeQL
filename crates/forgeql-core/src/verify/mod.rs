/// Build verification pipeline — Phase 2.
///
/// Runs named build/test steps from `.forgeql.yaml`, streams output,
/// and triggers rollback on failure.
///
/// Phase 1 stub: enough to compile, real implementation in Phase 2.
use anyhow::Result;

use crate::config::VerifyStep;
use crate::transforms::TransformResult;

/// Result of running a verification pipeline.
#[derive(Debug)]
pub struct VerifyResult {
    pub step: String,
    pub success: bool,
    pub output: String,
}

/// Run a single named build step synchronously.
/// On failure, rolls back `result` and returns an error.
///
/// # Errors
/// Returns `Err` if the command cannot be spawned, or if it exits with a
/// non-zero status (in which case `result` is rolled back first).
pub fn run_step(step: &VerifyStep, result: TransformResult) -> Result<()> {
    use std::process::Command;

    let output = Command::new("sh")
        .args(["-c", &step.command])
        .output()
        .map_err(|e| crate::error::ForgeError::io(".", e))?;

    if output.status.success() {
        Ok(())
    } else {
        // Rollback before reporting failure.
        result.rollback()?;
        Err(crate::error::ForgeError::BuildFailed {
            step: step.name.clone(),
            code: output.status.code().unwrap_or(-1),
        }
        .into())
    }
}
