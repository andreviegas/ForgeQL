/// Build verification pipeline.
///
/// Runs named build/test steps from `.forgeql.yaml`, streams output,
/// and triggers rollback on failure.
use std::path::Path;

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
pub fn run_standalone(step: &VerifyStep, workdir: &Path, env: &[(&str, String)]) -> VerifyResult {
    run_shell(&step.name, &step.command, workdir, env, None)
}

/// Run an arbitrary shell `command` in `workdir` with `env`.
///
/// Optionally writes `stdin` to the child's standard input — `RUN` templates use
/// this to feed a `String` param to the subprocess safely, so it never touches
/// the shell. Captures stdout+stderr combined; `name` labels the result.
#[must_use]
pub fn run_shell(
    name: &str,
    command: &str,
    workdir: &Path,
    env: &[(&str, String)],
    stdin: Option<&str>,
) -> VerifyResult {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let spawned = Command::new("sh")
        .args(["-c", command])
        .current_dir(workdir)
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match spawned {
        Err(e) => {
            return VerifyResult {
                step: name.to_string(),
                success: false,
                output: format!("failed to spawn: {e}"),
            };
        }
        Ok(c) => c,
    };

    // Write the stdin payload, then drop the pipe to signal EOF. Inputs are small
    // (a single FQL query), so writing before reading output cannot deadlock.
    if let Some(input) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        let _ = pipe.write_all(input.as_bytes());
    }

    match child.wait_with_output() {
        Err(e) => VerifyResult {
            step: name.to_string(),
            success: false,
            output: format!("failed to wait: {e}"),
        },
        Ok(output) => VerifyResult {
            step: name.to_string(),
            success: output.status.success(),
            output: format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        },
    }
}

/// Run a single named build step synchronously inside a transaction.
/// On failure, rolls back `result` and returns an error.
///
/// # Errors
/// Returns `Err` if the command cannot be spawned, or if it exits with a
/// non-zero status (in which case `result` is rolled back first).
pub fn run_step(step: &VerifyStep, workdir: &Path, result: TransformResult) -> Result<()> {
    use std::process::Command;

    let output = Command::new("sh")
        .args(["-c", &step.command])
        .current_dir(workdir)
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
