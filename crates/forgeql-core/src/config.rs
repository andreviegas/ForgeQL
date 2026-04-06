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
    200
}
const fn default_ceiling() -> usize {
    2000
}
const fn default_recovery_base() -> usize {
    20
}
const fn default_recovery_window_secs() -> u64 {
    60
}
const fn default_warning_threshold() -> usize {
    100
}
const fn default_critical_threshold() -> usize {
    50
}
const fn default_critical_max_lines() -> usize {
    15
}
const fn default_idle_reset_secs() -> u64 {
    300
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
        Ok(config)
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
}
