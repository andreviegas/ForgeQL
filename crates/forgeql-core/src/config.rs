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

    /// Named external command templates used by `RUN '<name>' <args…>`.
    /// Allowlisted + typed (see [`RunStep`]); frozen at `USE` like verify steps.
    #[serde(default)]
    pub run_steps: Vec<RunStep>,

    /// Extra glob patterns to ignore on top of `.forgeql-ignore`.
    #[serde(default)]
    pub ignore_patterns: Vec<String>,

    /// Line-budget configuration.  When present, the server enforces a
    /// rolling budget that limits how many source lines an agent may read.
    #[serde(default)]
    pub line_budget: Option<LineBudgetConfig>,

    /// Columnar storage engine configuration (Phase 03+).
    ///
    /// Enables shadow-writing of columnar segment files alongside the legacy
    /// Controls optional background warming policies.
    #[serde(default)]
    pub columnar: ColumnarConfig,

    /// Inline output caps for non-VERIFY commands.  Full results are buffered
    /// for `SHOW MORE`; these only bound the inline window.
    #[serde(default)]
    pub output: OutputConfig,
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

/// Configuration for the columnar storage engine.
///
/// Columnar indexing is always enabled. This section controls optional
/// background warming policies.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ColumnarConfig {
    /// Background segment + overlay warming after `CREATE SOURCE`.
    ///
    /// Defaults to disabled. When enabled, a background thread walks the
    /// chosen snapshots and pre-builds segments/overlays so the first `USE`
    /// is fast (overlay cache hit rather than full build).
    #[serde(default)]
    pub warm_on_create: WarmPolicy,

    /// Background segment + overlay warming after `REFRESH SOURCE`.
    ///
    /// Only snapshots whose branch HEADs moved are re-warmed, preventing
    /// unnecessary CPU drain on no-change polling refreshes.
    ///
    /// Defaults to disabled.
    #[serde(default)]
    pub warm_on_refresh: WarmPolicy,
}

/// Controls which snapshots are pre-warmed in the background.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WarmPolicy {
    /// Whether background warming is enabled for this hook.
    #[serde(default)]
    pub enabled: bool,

    /// Which snapshots to warm.  Ignored when `enabled = false`.
    #[serde(default)]
    pub policy: WarmPolicyKind,

    /// Snapshot refs to warm when `policy = "pinned"`.
    #[serde(default)]
    pub pinned: Vec<String>,

    /// Maximum number of snapshots to warm concurrently.
    ///
    /// Defaults to 2. Set to 1 to serialize warming and reduce I/O pressure.
    #[serde(default = "default_warm_concurrency")]
    pub max_concurrent: usize,
}

/// Which snapshots a [`WarmPolicy`] should target.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum WarmPolicyKind {
    /// Do not warm any snapshots (even if `WarmPolicy::enabled` is true).
    #[default]
    Off,
    /// Warm only the HEAD of the registered default branch.
    DefaultBranch,
    /// Warm the HEAD of every branch in the source.
    AllBranches,
    /// Warm exactly the refs listed in `WarmPolicy::pinned`.
    Pinned,
}

const fn default_warm_concurrency() -> usize {
    2
}
/// One named build or test step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyStep {
    pub name: String,
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Inline summary window for this step's output.
    ///
    /// Build/test logs have no universal pass/fail grammar, so `ForgeQL` never
    /// parses or summarizes them — it returns the last (or first) `lines` of
    /// output inline and buffers the full log for `SHOW MORE`. Absent → the
    /// default tail window (see [`SummaryConfig::default`]).
    #[serde(default)]
    pub summary: SummaryConfig,
    /// When `true`, `COMMIT` is refused until this step has passed since the
    /// most recent mutation. Several steps may set this — every gated step must
    /// pass (logical AND). Absent/false → this step never gates commits.
    #[serde(default)]
    pub commit_gate: bool,
    /// Typed positional parameters this step accepts. `VERIFY build '<step>'
    /// '<arg>'…` validates argument count and type, then substitutes each
    /// `$name` occurrence in `command`. Empty → the step takes no arguments.
    #[serde(default)]
    pub params: Vec<ParamSpec>,
    /// Resource footprint of this step, consumed by the `JOB` scheduler. Either
    /// a tier (`light|medium|heavy`) or an explicit
    /// `{cores, memory_mb, max_seconds}` map. Absent → `medium`.
    #[serde(default)]
    pub weight: Weight,
}

/// Resource footprint of a build job: what the `JOB` scheduler accounts for when
/// admitting work. Slice 1 records it on each job but does not yet schedule by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceCost {
    /// CPU cores the command is expected to occupy.
    pub cores: u32,
    /// Peak resident memory, in megabytes.
    pub memory_mb: u64,
    /// Soft wall-clock budget in seconds (enforced as a timeout in a later slice).
    pub max_seconds: u64,
}

/// Coarse cost tier for a verify step — sugar over [`ResourceCost`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeightLevel {
    /// Cheap lint / check.
    Light,
    /// A normal build or test.
    Medium,
    /// A heavy whole-workspace build.
    Heavy,
}

impl WeightLevel {
    /// Map a tier to its resource footprint. Presets are tunable; the scheduler
    /// always reasons in cores/memory/time, never in tiers.
    #[must_use]
    pub const fn cost(self) -> ResourceCost {
        match self {
            Self::Light => ResourceCost {
                cores: 1,
                memory_mb: 1024,
                max_seconds: 120,
            },
            Self::Medium => ResourceCost {
                cores: 4,
                memory_mb: 4096,
                max_seconds: 600,
            },
            Self::Heavy => ResourceCost {
                cores: 8,
                memory_mb: 16384,
                max_seconds: 1800,
            },
        }
    }
}

/// Declared cost of a verify step in `.forgeql.yaml`: either a tier
/// (`light|medium|heavy`) or an explicit `{cores, memory_mb, max_seconds}` map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Weight {
    /// A coarse tier.
    Level(WeightLevel),
    /// Exact resource numbers.
    Explicit(ResourceCost),
}

impl Default for Weight {
    fn default() -> Self {
        Self::Level(WeightLevel::Medium)
    }
}

impl Weight {
    /// Resolve to concrete resource numbers.
    #[must_use]
    pub const fn resolve(self) -> ResourceCost {
        match self {
            Self::Level(level) => level.cost(),
            Self::Explicit(cost) => cost,
        }
    }
}

/// Type of a [`VerifyStep`] parameter — constrains how an argument is
/// validated and bound. Only `Ident` exists today (a safe shell token);
/// `String` (stdin-bound) will arrive with the RUN command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    /// `[A-Za-z0-9_.-]+` — safe to splice directly into the shell command.
    #[default]
    Ident,
    /// Arbitrary text — never spliced into the command; bound to the
    /// subprocess **stdin** instead, so quotes/spaces/metacharacters in the
    /// argument cannot inject shell syntax. Used by `RUN` templates.
    String,
}

/// One declared positional parameter of a [`VerifyStep`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamSpec {
    /// Placeholder name; `$name` in the step `command` is replaced with the arg.
    pub name: String,
    /// Validation kind for the supplied argument.
    #[serde(default, rename = "type")]
    pub kind: ParamType,
}

/// One named external command runnable via `RUN '<name>' <args…>`.
///
/// Unlike [`VerifyStep`] (an open, vetted allowlist), `run_steps` are
/// allowlisted *templates*: the engine substitutes only declared `Ident`
/// params into `command` and binds `String` params to the subprocess stdin,
/// so an agent can parameterise a template but never free-form a command.
/// Frozen at `USE` like verify steps, so a later CHANGE cannot tamper them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStep {
    pub name: String,
    pub command: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Inline output window; the full output is buffered for `SHOW MORE`.
    #[serde(default)]
    pub summary: SummaryConfig,
    /// Typed positional parameters. `Ident` → substituted as `$name` in
    /// `command`; `String` → bound to the subprocess stdin (never the command).
    #[serde(default)]
    pub params: Vec<ParamSpec>,
}

/// Validate `args` against `params` and resolve a command template.
///
/// `Ident` args (must match `[A-Za-z0-9_.-]+`) are substituted for `$name` in
/// `command` — injection-safe, since no shell metacharacter can appear. `String`
/// args are NEVER substituted into the command; they are newline-joined in
/// declared order and returned as the subprocess **stdin** payload. Returns
/// `(resolved_command, stdin)` or a human-readable error.
pub(crate) fn resolve_template(
    step_name: &str,
    command: &str,
    params: &[ParamSpec],
    args: &[String],
) -> Result<(String, Option<String>), String> {
    if args.len() != params.len() {
        return Err(format!(
            "step '{step_name}' expects {} argument(s), got {}",
            params.len(),
            args.len()
        ));
    }
    let mut resolved = command.to_string();
    // Substitute longest ident names first so `$t` never clobbers `$target`.
    let mut order: Vec<usize> = (0..params.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(params[i].name.len()));
    for i in order {
        let param = &params[i];
        let arg = &args[i];
        match param.kind {
            ParamType::Ident => {
                let ok = !arg.is_empty()
                    && arg
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
                if !ok {
                    return Err(format!(
                        "argument for '{}' must be an identifier [A-Za-z0-9_.-], got '{arg}'",
                        param.name
                    ));
                }
                resolved = resolved.replace(&format!("${}", param.name), arg);
            }
            ParamType::String => {}
        }
    }
    // String params are bound to stdin, collected in DECLARED order (independent
    // of the longest-first ident substitution order above).
    let stdin_parts: Vec<String> = params
        .iter()
        .zip(args)
        .filter(|(p, _)| matches!(p.kind, ParamType::String))
        .map(|(_, a)| a.clone())
        .collect();
    let stdin = (!stdin_parts.is_empty()).then(|| stdin_parts.join("\n"));
    Ok((resolved, stdin))
}

/// Resolve a [`VerifyStep`] command (ident params only; no stdin). Thin wrapper
/// over [`resolve_template`] preserving the original `VERIFY build` call site.
pub(crate) fn resolve_command(step: &VerifyStep, args: &[String]) -> Result<String, String> {
    let (command, _stdin) = resolve_template(&step.name, &step.command, &step.params, args)?;
    Ok(command)
}

/// Inline output window for a [`VerifyStep`].
///
/// `direction` chooses which end of the log is shown inline; `lines` is how
/// many. The full log is always available via `SHOW MORE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryConfig {
    /// Which end of the output to show inline.
    #[serde(default)]
    pub direction: SummaryDirection,
    /// Number of lines to show inline before buffering the rest.
    #[serde(default = "default_summary_lines")]
    pub lines: usize,
}

impl Default for SummaryConfig {
    fn default() -> Self {
        Self {
            direction: SummaryDirection::default(),
            lines: default_summary_lines(),
        }
    }
}

/// Which end of a [`VerifyStep`]'s output is shown inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryDirection {
    /// Show the last `lines` of output (default — verdicts/errors land last).
    #[default]
    Tail,
    /// Show the first `lines` of output.
    Head,
}

const fn default_summary_lines() -> usize {
    40
}

fn default_workspace_root() -> PathBuf {
    PathBuf::from(".")
}

const fn default_timeout() -> u64 {
    120
}

/// Inline output caps for non-`VERIFY` commands.
///
/// Each value is the number of rows/lines returned inline before the full
/// result is buffered for `SHOW MORE`.  Replaces the former hard-coded
/// `DEFAULT_QUERY_LIMIT` / `DEFAULT_SHOW_LINE_LIMIT` constants so deployments
/// can tune verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputConfig {
    /// Inline row cap for `FIND` / list results when no explicit `LIMIT` is given.
    #[serde(default = "default_find_limit")]
    pub find_limit: usize,
    /// Inline source-line cap for `SHOW LINES` / `SHOW body` / `SHOW context`.
    #[serde(default = "default_show_lines")]
    pub show_lines: usize,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            find_limit: default_find_limit(),
            show_lines: default_show_lines(),
        }
    }
}

const fn default_find_limit() -> usize {
    20
}

const fn default_show_lines() -> usize {
    40
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
# .forgeql.yaml — ForgeQL config for source '{source_name}'
#
# Sidecar file: lives in the ForgeQL data dir, not inside the repo.  A repo
# may also carry its own .forgeql.yaml in the workspace root.  Steps and
# templates are frozen at USE, so a later edit cannot tamper with a command
# the gate will run.
# Defaults shown — delete lines you don't need, remove a block to disable it.
# Docs: https://github.com/andreviegas/ForgeQL

workspace_root: .

# ── Line Budget ───────────────────────────────────────────────────────────────
# Source-line allowance per session. Remove block to disable the budget system.
line_budget:
  initial: 3000             # starting allowance
  ceiling: 9000             # hard ceiling; budget never exceeds this
  recovery_base: 200         # lines credited per recovery (halved on repeats)
  recovery_window_secs: 30  # recovery window in seconds
  warning_threshold: 250    # warn agent when budget falls below this
  critical_threshold: 50    # cap SHOW LINES output when budget is critical
  critical_max_lines: 20    # max lines returned in critical state
  idle_reset_secs: 120      # idle seconds before budget resets; 0 = never

# ── Verify Steps ──────────────────────────────────────────────────────────────
# Named commands run synchronously by `VERIFY build '<name>' ['arg']…` or as
# background jobs by `JOB START '<name>'`. Uncomment and adapt to your project.
#
# Every step's process receives the session environment contract:
#   FORGEQL_SESSION_ID  full session token        FORGEQL_SOURCE   source name
#   FORGEQL_BRANCH      base branch               FORGEQL_ALIAS    session alias
#   FORGEQL_WORKTREE    absolute path of the session worktree — build THIS
#                       tree, never a hardcoded checkout
#   FORGEQL_BUILD_DIR   per-worktree build dir so concurrent sessions never
#                       share build artifacts (e.g. cargo --target-dir)
#
# verify_steps:
#   - name: test
#     command: \"cargo test\"
#     timeout_secs: 120
#     commit_gate: true     # COMMIT refused until this passes after the last
#                           # edit; several gated steps must ALL pass
#     weight: medium        # JOB scheduler cost: light | medium | heavy, or
#                           # explicit {{cores: 4, memory_mb: 4096, max_seconds: 600}}
#     summary:              # inline output window; full log kept for SHOW MORE
#       direction: tail     # head | tail
#       lines: 40
#
#   - name: build-one       # typed args: VERIFY build 'build-one' 'core_b1'
#     command: \"cmake -U PROJECT -D PROJECT=$project -B build && cmake --build build\"
#     params:
#       - name: project
#         type: ident       # ident = [A-Za-z0-9_.-]+, substituted for $project;
#                           # string = bound to stdin, never spliced
#     timeout_secs: 1800
#     weight: heavy

# ── Run Templates ─────────────────────────────────────────────────────────────
# Allowlisted command templates run by `RUN '<name>' ['arg']…`. The agent can
# parameterise a template but never free-form a command: ident params are
# validated and substituted, string params are newline-joined onto stdin.
# Same environment contract as verify steps.
#
# run_steps:
#   - name: grep-cache
#     command: \"grep -m1 $key $FORGEQL_BUILD_DIR/CMakeCache.txt\"
#     params:
#       - name: key
#         type: ident
#     timeout_secs: 30

# ── Columnar Storage ──────────────────────────────────────────────────────────
# Columnar indexing is always on. The section below controls optional
# background warming — pre-builds overlays so the first USE is instant.
# columnar:
#   warm_on_create:
#     enabled: false
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

    /// The generated sidecar template must stay loadable (its uncommented
    /// YAML parses) and document every step feature an agent can use.
    #[test]
    fn sidecar_template_parses_and_documents_step_features() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = ForgeConfig::write_sidecar_template(dir.path(), "demo")
            .expect("template should be created");
        let config = ForgeConfig::load(&path).expect("generated template must load");
        assert!(
            config.verify_steps.is_empty(),
            "examples stay commented out"
        );

        let text = std::fs::read_to_string(&path).expect("read template");
        for needle in [
            "run_steps:",
            "commit_gate:",
            "params:",
            "type: ident",
            "weight:",
            "summary:",
            "FORGEQL_WORKTREE",
            "FORGEQL_BUILD_DIR",
        ] {
            assert!(text.contains(needle), "template must document '{needle}'");
        }

        // Idempotent: a second call must not overwrite an existing file.
        assert!(ForgeConfig::write_sidecar_template(dir.path(), "demo").is_none());
    }

    #[test]
    fn verify_step_weight_levels_and_explicit_resolve() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".forgeql.yaml");
        std::fs::write(
            &path,
            "workspace_root: .\nverify_steps:\n  - name: lite\n    command: lint\n    weight: light\n  - name: heavy\n    command: build\n    weight: heavy\n  - name: exact\n    command: x\n    weight:\n      cores: 2\n      memory_mb: 2048\n      max_seconds: 90\n  - name: plain\n    command: y\n",
        )
        .expect("write");
        let config = ForgeConfig::load(&path).expect("should load");
        let cost = |n: &str| config.step(n).expect("step exists").weight.resolve();
        assert_eq!(cost("lite"), WeightLevel::Light.cost());
        assert_eq!(cost("heavy"), WeightLevel::Heavy.cost());
        assert_eq!(
            cost("exact"),
            ResourceCost {
                cores: 2,
                memory_mb: 2048,
                max_seconds: 90,
            }
        );
        // An absent `weight:` defaults to medium.
        assert_eq!(cost("plain"), WeightLevel::Medium.cost());
    }

    fn ident(name: &str) -> ParamSpec {
        ParamSpec {
            name: name.to_string(),
            kind: ParamType::Ident,
        }
    }

    fn string(name: &str) -> ParamSpec {
        ParamSpec {
            name: name.to_string(),
            kind: ParamType::String,
        }
    }

    #[test]
    fn resolve_template_substitutes_ident_into_command() {
        let params = vec![ident("target")];
        let (cmd, stdin) =
            resolve_template("build", "make $target", &params, &["server".to_string()])
                .expect("resolve");
        assert_eq!(cmd, "make server");
        assert!(stdin.is_none());
    }

    #[test]
    fn resolve_template_substitutes_longest_name_first() {
        // `$t` must not clobber `$target`.
        let params = vec![ident("t"), ident("target")];
        let (cmd, _) = resolve_template(
            "x",
            "$t-$target",
            &params,
            &["A".to_string(), "B".to_string()],
        )
        .expect("resolve");
        assert_eq!(cmd, "A-B");
    }

    #[test]
    fn resolve_template_binds_string_to_stdin_never_command() {
        let params = vec![string("QUERY")];
        let (cmd, stdin) = resolve_template(
            "run_fql",
            "forgeql --data-dir d",
            &params,
            &["FIND symbols; rm -rf /".to_string()],
        )
        .expect("resolve");
        // The string arg is NEVER spliced into the command — only stdin.
        assert_eq!(cmd, "forgeql --data-dir d");
        assert_eq!(stdin.as_deref(), Some("FIND symbols; rm -rf /"));
    }

    #[test]
    fn resolve_template_rejects_wrong_arity() {
        let params = vec![ident("a")];
        let err = resolve_template("x", "$a", &params, &[]).unwrap_err();
        assert!(err.contains("expects 1 argument"), "got: {err}");
    }

    #[test]
    fn resolve_template_rejects_ident_injection() {
        let params = vec![ident("a")];
        let err = resolve_template("x", "$a", &params, &["foo; rm -rf /".to_string()]).unwrap_err();
        assert!(err.contains("must be an identifier"), "got: {err}");
    }
}
