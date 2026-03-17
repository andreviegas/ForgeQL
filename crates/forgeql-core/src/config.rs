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
