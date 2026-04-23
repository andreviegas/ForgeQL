/// `.forgeql.yaml` configuration file deserialization.
///
/// Example `.forgeql.yaml`:
/// ```yaml
/// workspace_root: .
/// verify_steps:
///   - name: build_producao
///     command: "./scripts/Build.sh producao"
///     timeout_secs: 120
///   - name: run_tests
///     command: "ctest --test-dir build"
///     timeout_secs: 120
/// ```
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Top-level `.forgeql.yaml` structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgeConfig {
    /// Path to the workspace root (relative to the config file location).
    #[serde(default = "default_workspace_root")]
    pub workspace_root: PathBuf,

    /// Named build/test steps used by `VERIFY build '<name>'`.
    #[serde(default)]
    pub verify_steps: Vec<VerifyStep>,

    /// Extra glob patterns to ignore on top of `.forgeql-ignore`.
    #[serde(default)]
    pub ignore_patterns: Vec<String>,

    /// Line-budget configuration.  When present, the server enforces a
    /// rolling budget that limits how many source lines an agent may read.
    #[serde(default)]
    pub line_budget: Option<LineBudgetConfig>,
}

/// Configuration for the line-budget system that limits how many source
/// lines an agent can read per session window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineBudgetConfig {
    /// Starting line budget for a new session.
    #[serde(default = "default_initial")]
    pub initial: usize,
    /// Maximum the effective budget can grow to over time.
    #[serde(default = "default_ceiling")]
    pub ceiling: usize,
    /// Base number of lines recovered per recovery event.
    #[serde(default = "default_recovery_base")]
    pub recovery_base: usize,
    /// Time window in seconds — recovery halves within the same window.
    #[serde(default = "default_recovery_window_secs")]
    pub recovery_window_secs: u64,
    /// Budget level below which a warning is emitted.
    #[serde(default = "default_warning_threshold")]
    pub warning_threshold: usize,
    /// Budget level below which SHOW LINES is capped.
    #[serde(default = "default_critical_threshold")]
    pub critical_threshold: usize,
    /// Maximum lines returned by SHOW LINES when in critical state.
    #[serde(default = "default_critical_max_lines")]
    pub critical_max_lines: usize,
    /// Seconds of inactivity after which the persisted budget file is
    /// considered stale and deleted on the next `USE`, giving the agent
    /// a fresh budget.  Set to 0 to disable expiry.  Default: 300 (5 min).
    #[serde(default = "default_idle_reset_secs")]
    pub idle_reset_secs: u64,
}

const fn default_initial() -> usize {
    1000
}
const fn default_ceiling() -> usize {
    3000
}
const fn default_recovery_base() -> usize {
    50
}
const fn default_recovery_window_secs() -> u64 {
    30
}
const fn default_warning_threshold() -> usize {
    250
}
const fn default_critical_threshold() -> usize {
    50
}
const fn default_critical_max_lines() -> usize {
    20
}
const fn default_idle_reset_secs() -> u64 {
    200
}

impl Default for LineBudgetConfig {
    fn default() -> Self {
        Self {
            initial: default_initial(),
            ceiling: default_ceiling(),
            recovery_base: default_recovery_base(),
            recovery_window_secs: default_recovery_window_secs(),
            warning_threshold: default_warning_threshold(),
            critical_threshold: default_critical_threshold(),
            critical_max_lines: default_critical_max_lines(),
            idle_reset_secs: default_idle_reset_secs(),
        }
    }
}

/// One named build or test step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyStep {
    pub name: String,
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_workspace_root() -> PathBuf {
    PathBuf::from(".")
}

const fn default_timeout() -> u64 {
    120
}

impl ForgeConfig {
    /// Load config from a `.forgeql.yaml` file.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read or cannot be parsed as YAML.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file '{}'", path.display()))?;
        let config: Self = serde_yml::from_str(&text)
            .with_context(|| format!("parsing config file '{}'", path.display()))?;
        config.validate(path)?;
        Ok(config)
    }

    /// Validate the config for semantic errors (e.g. duplicate step names).
    fn validate(&self, path: &Path) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for step in &self.verify_steps {
            if !seen.insert(&step.name) {
                anyhow::bail!(
                    "{}: duplicate verify_steps name '{}' — each step must have a unique name",
                    path.display(),
                    step.name
                );
            }
        }
        Ok(())
    }

    /// Search for `.forgeql.yaml` by walking up from `start` to the filesystem root.
    /// Returns `None` when no config file is found (valid — config is optional).
    #[must_use]
    pub fn find(start: &Path) -> Option<PathBuf> {
        let mut dir = start;
        loop {
            let candidate = dir.join(".forgeql.yaml");
            if candidate.exists() {
                return Some(candidate);
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => return None,
            }
        }
    }

    /// Find the step with the given name.
    #[must_use]
    pub fn step(&self, name: &str) -> Option<&VerifyStep> {
        self.verify_steps.iter().find(|s| s.name == name)
    }

    /// Write a commented template sidecar config file at
    /// `<data_dir>/<source_name>.forgeql.yaml`.
    ///
    /// Does nothing when the file already exists (idempotent).  Returns the
    /// path when the file was newly created, `None` when it already existed
    /// or when a write error occurred (non-fatal).
    #[must_use]
    pub fn write_sidecar_template(
        data_dir: &std::path::Path,
        source_name: &str,
    ) -> Option<std::path::PathBuf> {
        let path = data_dir.join(format!("{source_name}.forgeql.yaml"));
        if path.exists() {
            return None;
        }
        let template = format!(
            "\
# .forgeql.yaml — ForgeQL configuration for source '{source_name}'
#
# This sidecar file lives next to the bare repo in the ForgeQL data directory
# (not inside the repository itself), so it never needs to be committed.
# All fields shown here are the defaults — delete any line you do not need to
# override, or remove a whole block to disable that subsystem.
#
# Documentation: https://forgeql.dev/docs/configuration

workspace_root: .

# ── Line Budget ───────────────────────────────────────────────────────────────
# Controls how many source lines an agent may read per rolling session window.
# Remove the entire `line_budget:` block to disable the budget system entirely,
# which will leave the `line_budget` column empty in the CSV query log.
line_budget:
  # Starting line allowance for a brand-new session.
  initial: 1000
  # The budget can never grow above this ceiling, no matter how many
  # recovery events occur.
  ceiling: 3000
  # Lines credited back per recovery event.  The first recovery in a window
  # grants the full base; subsequent ones in the same window are halved.
  recovery_base: 50
  # Duration of the recovery window in seconds.
  recovery_window_secs: 30
  # When the remaining budget drops below this level, every response will
  # include a warning so the agent knows to be more selective.
  warning_threshold: 250
  # When the remaining budget drops below this level, SHOW LINES output is
  # capped to `critical_max_lines` to slow consumption automatically.
  critical_threshold: 50
  # Hard cap on lines returned by SHOW LINES while in critical state.
  critical_max_lines: 20
  # Seconds of inactivity after which the persisted budget file is treated as
  # stale and deleted, giving the next session a fresh budget.
  # Set to 0 to disable automatic expiry.
  idle_reset_secs: 200

# ── Verify Steps ──────────────────────────────────────────────────────────────
# Named build/test commands executed by `VERIFY build '<name>'`.
# The VERIFY command will fail with \"step not found\" until at least one step
# is defined here.  Uncomment and adapt the examples below to your project.
#
# verify_steps:
#   - name: test
#     command: \"cargo test\"
#     timeout_secs: 120
#
#   - name: build
#     command: \"cargo build --release\"
#     timeout_secs: 300
"
        );
        std::fs::write(&path, template).ok()?;
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_rejects_duplicate_verify_step_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".forgeql.yaml");
        std::fs::write(
            &path,
            "workspace_root: .\nverify_steps:\n  - name: build\n    command: make\n  - name: build\n    command: make all\n",
        )
        .expect("write");
        let err = ForgeConfig::load(&path).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("duplicate verify_steps name 'build'"),
            "expected duplicate error, got: {msg}"
        );
    }

    #[test]
    fn load_accepts_unique_verify_step_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".forgeql.yaml");
        std::fs::write(
            &path,
            "workspace_root: .\nverify_steps:\n  - name: build\n    command: make\n  - name: test\n    command: make test\n",
        )
        .expect("write");
        let config = ForgeConfig::load(&path).expect("should load successfully");
        assert_eq!(config.verify_steps.len(), 2);
    }
}
