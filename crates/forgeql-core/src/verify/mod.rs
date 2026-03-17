/// Build verification pipeline.
///
/// Runs named build/test steps from `.forgeql.yaml`, streams output,
/// and triggers rollback on failure.
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

/// Run a single named build step and return its result.
/// Does **not** roll back anything — use this for standalone `VERIFY build`.
#[must_use]
pub fn run_standalone(step: &VerifyStep) -> VerifyResult {
    use std::process::Command;

    match Command::new("sh").args(["-c", &step.command]).output() {
        Err(e) => VerifyResult {
            step: step.name.clone(),
            success: false,
            output: format!("failed to spawn: {e}"),
        },
        Ok(output) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            VerifyResult {
                step: step.name.clone(),
                success: output.status.success(),
                output: combined,
            }
        }
    }
}

/// Run a single named build step synchronously inside a transaction.
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
