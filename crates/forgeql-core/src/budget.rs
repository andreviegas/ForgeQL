//! Line-budget tracking for agent sessions.
//!
//! The budget limits how many source lines an agent can read within a
//! rolling time window.  Every `SHOW` or `FIND` response that discloses
//! source code decrements the budget by the number of lines returned.
//!
//! Recovery is granted on each command that finds the budget below the
//! ceiling, but successive recoveries within the same time
//! window yield diminishing returns (halving).
//!
//! The budget state is persisted under the `ForgeQL` data directory at
//! `.budgets/{source}@{branch}.json` so reconnecting via `USE … AS 'alias'`
//! cannot reset the budget — the key is the real branch, not the alias.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::config::LineBudgetConfig;

/// Filename used to persist budget state inside the worktree.
/// Subdirectory inside `data_dir` where per-branch budget files are stored.
pub const BUDGET_DIR: &str = ".budgets";

/// Snapshot of the budget returned after each command.
#[derive(Debug, Clone)]
pub struct BudgetSnapshot {
    /// Lines remaining in the current budget.
    pub remaining: usize,
    /// Delta from the last operation (negative = deduction, positive = recovery).
    pub delta: isize,
    /// `true` when remaining < `warning_threshold`.
    pub warning: bool,
    /// `true` when remaining < `critical_threshold`.
    pub critical: bool,
    /// Configuration ceiling — used to compute fixed-width log formatting.
    pub ceiling: usize,
}
impl BudgetSnapshot {
    /// Format the budget as a compact status line: `"remaining (delta)"`.
    #[must_use]
    pub fn status_line(&self) -> String {
        let sign = if self.delta >= 0 { "+" } else { "" };
        let base = format!("{} ({}{})", self.remaining, sign, self.delta);
        if self.critical {
            format!(
                "{base} \
                 \u{26a0}\u{fe0f} CRITICAL: output is capped. \
                 Use FIND+WHERE to locate symbols before reading source. \
                 Avoid SHOW body DEPTH 99; prefer SHOW LINES n-m with narrow ranges."
            )
        } else if self.warning {
            format!(
                "{base} \
                 \u{26a0}\u{fe0f} Budget low. \
                 Use FIND symbols WHERE name = '...' to get file+line, \
                 then SHOW LINES n-m for only the lines needed. \
                 GROUP BY file + HAVING to find hotspots before reading."
            )
        } else {
            base
        }
    }

    /// Format the budget as a fixed-width string suitable for the CSV log.
    ///
    /// The column width is derived from `ceiling` so every row in a given log
    /// file aligns, because column width is fixed to the ceiling digit count.
    ///
    /// Example output (ceiling = 2000, width = 4): `"0009/0295 (-0015)"`
    #[must_use]
    pub fn fixed_status_line(&self) -> String {
        let w = self.ceiling.to_string().len();
        let sign = if self.delta >= 0 { '+' } else { '-' };
        let abs_delta = self.delta.unsigned_abs();
        format!(
            "{:0>w$} ({}{:0>w$})",
            self.remaining,
            sign,
            abs_delta,
            w = w
        )
    }
}

/// Persistent portion of the budget state (serialized to disk).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedBudget {
    remaining: usize,
    /// Number of recovery halvings applied in the current window.
    recovery_halvings: u32,
    /// Epoch seconds when the current recovery window started.
    window_start_epoch: u64,
    /// Absolute epoch-second deadline: if `now > expires_at_epoch` the file
    /// is stale and should be deleted (budget resets to initial).
    expires_at_epoch: u64,
}

/// Runtime budget tracker for one session.
#[derive(Debug)]
pub struct BudgetState {
    config: LineBudgetConfig,
    remaining: usize,
    /// Number of recovery halvings already applied in the current window.
    recovery_halvings: u32,
    /// When the current recovery window started (monotonic).
    window_start: Instant,
    /// Wall-clock epoch of window start (for persistence).
    window_start_epoch: u64,
    /// Delta from the most recent operation.
    last_delta: isize,
    /// Seconds of idle time before the persisted file expires.
    /// Captured from config at `USE` time; never re-read from disk.
    idle_reset_secs: u64,
}

fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl BudgetState {
    /// Create a fresh budget from configuration.
    #[must_use]
    pub fn new(config: &LineBudgetConfig) -> Self {
        let now = epoch_now();
        Self {
            idle_reset_secs: config.idle_reset_secs,
            config: config.clone(),
            remaining: config.initial,
            recovery_halvings: 0,
            window_start: Instant::now(),
            window_start_epoch: now,
            last_delta: 0,
        }
    }

    /// Compute the budget file path for a given source + branch.
    ///
    /// Stored at `data_dir/.budgets/{source}@{branch}.json` (sanitized).
    /// This ensures the budget survives worktree alias changes — it is
    /// always keyed on the actual git branch being worked on.
    #[must_use]
    pub fn budget_path(data_dir: &Path, source: &str, branch: &str) -> PathBuf {
        let sanitize = |s: &str| {
            s.chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect::<String>()
        };
        data_dir
            .join(BUDGET_DIR)
            .join(format!("{}@{}.json", sanitize(source), sanitize(branch)))
    }

    /// Restore budget from a persisted file, applying the given config.
    ///
    /// If the file is missing or corrupt, returns a fresh budget.
    #[must_use]
    pub fn load(config: &LineBudgetConfig, data_dir: &Path, source: &str, branch: &str) -> Self {
        let path = Self::budget_path(data_dir, source, branch);
        let Ok(data) = std::fs::read_to_string(&path) else {
            return Self::new(config);
        };
        let Ok(persisted) = serde_json::from_str::<PersistedBudget>(&data) else {
            return Self::new(config);
        };

        // Check expiry: if now is past the deadline, delete the stale file
        // and return a fresh budget so the agent starts with a full allowance.
        let now_epoch = epoch_now();
        if persisted.expires_at_epoch > 0 && now_epoch > persisted.expires_at_epoch {
            let _ = std::fs::remove_file(&path);
            return Self::new(config);
        }

        // Clamp remaining to current config ceiling.
        let remaining = persisted.remaining.min(config.ceiling);

        // Reconstruct window_start from persisted epoch.
        let elapsed_secs = now_epoch.saturating_sub(persisted.window_start_epoch);
        let window_start = Instant::now()
            .checked_sub(Duration::from_secs(elapsed_secs))
            .unwrap_or_else(Instant::now);

        Self {
            idle_reset_secs: config.idle_reset_secs,
            config: config.clone(),
            remaining,
            recovery_halvings: persisted.recovery_halvings,
            window_start,
            window_start_epoch: persisted.window_start_epoch,
            last_delta: 0,
        }
    }

    /// Reset the last-delta to zero without touching any other state.
    ///
    /// Called by the engine after commands that consume no source lines
    /// (FIND, mutations, source ops) so that the status line always
    /// reflects the current command's cost rather than a stale value.
    pub const fn reset_delta(&mut self) {
        self.last_delta = 0;
    }

    /// Persist current state to `data_dir/.budgets/`.
    ///
    /// Always called after every command (the system is stateless) so the
    /// `expires_at_epoch` timestamp stays rolling.
    pub fn save(&self, data_dir: &Path, source: &str, branch: &str) {
        let path = Self::budget_path(data_dir, source, branch);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let expires_at_epoch = if self.idle_reset_secs > 0 {
            epoch_now() + self.idle_reset_secs
        } else {
            0 // 0 means "never expire"
        };
        let persisted = PersistedBudget {
            remaining: self.remaining,
            recovery_halvings: self.recovery_halvings,
            window_start_epoch: self.window_start_epoch,
            expires_at_epoch,
        };
        if let Ok(json) = serde_json::to_string(&persisted) {
            let _ = std::fs::write(&path, json);
        }
    }

    /// Deduct `lines` from the budget.  Returns the snapshot after deduction.
    pub fn deduct(&mut self, lines: usize) -> BudgetSnapshot {
        let before = self.remaining;

        if lines == 0 {
            // No source lines disclosed — eligible for recovery credit.
            self.try_recover();
        } else {
            // Source lines disclosed — deduct, no recovery.
            self.remaining = self.remaining.saturating_sub(lines);
        }

        self.last_delta = self.remaining.cast_signed() - before.cast_signed();

        debug!(
            lines,
            remaining = self.remaining,
            delta = self.last_delta,
            "budget: deducted"
        );

        self.snapshot()
    }

    /// Grant proportional recovery based on lines actually written by a
    /// mutation (CHANGE / COPY / MOVE).
    ///
    /// This bypasses the rolling-window halving entirely: the agent earns
    /// back lines 1:1 for every line it writes, capped at the ceiling.
    /// The philosophy is that productive work (writing code) deserves full
    /// credit, whereas passive reads are subject to the normal diminishing
    /// recovery.
    pub fn reward_mutation(&mut self, lines_written: usize) -> BudgetSnapshot {
        if lines_written == 0 {
            return self.snapshot();
        }

        let before = self.remaining;
        self.remaining = (self.remaining + lines_written).min(self.config.ceiling);
        self.last_delta = self.remaining.cast_signed() - before.cast_signed();

        debug!(
            lines_written,
            remaining = self.remaining,
            delta = self.last_delta,
            "budget: mutation reward"
        );

        self.snapshot()
    }

    /// Check whether the budget is in critical state (SHOW LINES should be capped).
    #[must_use]
    pub const fn is_critical(&self) -> bool {
        self.remaining < self.config.critical_threshold
    }

    /// Maximum lines allowed for SHOW LINES when in critical state.
    #[must_use]
    pub const fn critical_max_lines(&self) -> usize {
        self.config.critical_max_lines
    }

    /// Build a snapshot of the current state.
    #[must_use]
    pub const fn snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            remaining: self.remaining,
            delta: self.last_delta,
            warning: self.remaining < self.config.warning_threshold,
            critical: self.remaining < self.config.critical_threshold,
            ceiling: self.config.ceiling,
        }
    }

    /// Attempt to recover budget.  Recovery halves with each successive
    /// call within the same time window.  A new window resets the halving
    /// counter.  The ceiling is the hard cap — no reward pushes `remaining`
    /// above it.
    fn try_recover(&mut self) {
        if self.remaining >= self.config.ceiling {
            return;
        }

        let window = Duration::from_secs(self.config.recovery_window_secs);
        if self.window_start.elapsed() >= window {
            // New window — reset halving counter.
            self.recovery_halvings = 0;
            self.window_start = Instant::now();
            self.window_start_epoch = epoch_now();
        }

        let recovery = self.config.recovery_base >> self.recovery_halvings.min(31);
        if recovery == 0 {
            return;
        }

        let new_remaining = (self.remaining + recovery).min(self.config.ceiling);
        let actual = new_remaining - self.remaining;
        if actual > 0 {
            self.remaining = new_remaining;
            self.recovery_halvings += 1;
            debug!(
                recovery = actual,
                remaining = self.remaining,
                halvings = self.recovery_halvings,
                "budget: recovered"
            );
        }
    }
}

/// Delete all expired budget files under `data_dir/.budgets/`.
///
/// Called on every `USE` command so stale files from abandoned branches are
/// cleaned up automatically, without any external cron.
pub fn sweep_expired(data_dir: &Path) {
    let dir = data_dir.join(BUDGET_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let now = epoch_now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(data) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(persisted) = serde_json::from_str::<PersistedBudget>(&data) else {
            // Corrupt file — delete it.
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if persisted.expires_at_epoch > 0 && now > persisted.expires_at_epoch {
            let _ = std::fs::remove_file(&path);
        }
    }
}
// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> LineBudgetConfig {
        LineBudgetConfig {
            initial: 100,
            ceiling: 500,
            recovery_base: 20,
            recovery_window_secs: 60,
            warning_threshold: 40,
            critical_threshold: 20,
            critical_max_lines: 15,
            idle_reset_secs: 300,
        }
    }

    #[test]
    fn new_budget_starts_at_initial() {
        let cfg = test_config();
        let state = BudgetState::new(&cfg);
        assert_eq!(state.remaining, 100);
    }

    #[test]
    fn deduct_reduces_remaining() {
        let cfg = test_config();
        let mut state = BudgetState::new(&cfg);
        let snap = state.deduct(30);
        // lines > 0 → no recovery, just subtract.
        assert_eq!(snap.remaining, 70);
        assert_eq!(snap.delta, -30);
    }

    #[test]
    fn deduct_saturates_at_zero() {
        let cfg = test_config();
        let mut state = BudgetState::new(&cfg);
        let snap = state.deduct(200);
        assert_eq!(snap.remaining, 0);
    }

    #[test]
    fn recovery_fires_when_below_max() {
        let cfg = test_config();
        let mut state = BudgetState::new(&cfg);
        state.remaining = 50; // below ceiling (500)
        let snap = state.deduct(0);
        // Recovery: base=20, halvings=0 → recover 20.
        // remaining: 50 + 20 = 70.
        assert_eq!(snap.remaining, 70);
    }

    #[test]
    fn recovery_halves_within_window() {
        let cfg = test_config();
        let mut state = BudgetState::new(&cfg);
        state.remaining = 10;
        // First recovery: +20 → remaining=30
        state.deduct(0);
        assert_eq!(state.remaining, 30);
        state.remaining = 10;
        // Second recovery (same window): +10 → remaining=20
        state.deduct(0);
        assert_eq!(state.remaining, 20);
        state.remaining = 10;
        // Third: +5
        state.deduct(0);
        assert_eq!(state.remaining, 15);
    }

    #[test]
    fn critical_state() {
        let cfg = test_config();
        let mut state = BudgetState::new(&cfg);
        state.remaining = 15;
        assert!(state.is_critical());
        let snap = state.snapshot();
        assert!(snap.critical);
        assert!(snap.warning);
    }

    #[test]
    fn warning_state() {
        let cfg = test_config();
        let mut state = BudgetState::new(&cfg);
        state.remaining = 35;
        let snap = state.snapshot();
        assert!(snap.warning);
        assert!(!snap.critical);
    }

    #[test]
    fn status_line_format() {
        // Normal state — no suffix.
        let snap = BudgetSnapshot {
            remaining: 70,
            delta: -30,
            warning: false,
            critical: false,
            ceiling: 2000,
        };
        assert!(snap.status_line().starts_with("70 (-30)"));
        assert!(
            !snap.status_line().contains('\u{26a0}'),
            "no warning suffix"
        );

        // Warning state — suffix present.
        let warn = BudgetSnapshot {
            warning: true,
            ..snap
        };
        assert!(warn.status_line().contains('\u{26a0}'));
        assert!(warn.status_line().starts_with("70 (-30)"));

        // Critical state — different suffix.
        let crit = BudgetSnapshot {
            critical: true,
            ..snap
        };
        assert!(crit.status_line().contains("CRITICAL"));
    }

    #[test]
    fn persistence_round_trip() {
        let cfg = test_config();
        let mut state = BudgetState::new(&cfg);
        state.deduct(40);
        let tmp = tempfile::tempdir().unwrap();
        state.save(tmp.path(), "test-source", "test-branch");
        let loaded = BudgetState::load(&cfg, tmp.path(), "test-source", "test-branch");
        assert_eq!(loaded.remaining, state.remaining);
    }

    #[test]
    fn load_missing_file_returns_fresh() {
        let cfg = test_config();
        let tmp = tempfile::tempdir().unwrap();
        let state = BudgetState::load(&cfg, tmp.path(), "test-source", "test-branch");
        assert_eq!(state.remaining, cfg.initial);
    }

    #[test]
    fn reward_mutation_adds_lines_written() {
        let cfg = test_config();
        let mut state = BudgetState::new(&cfg);
        // Drain budget.
        state.deduct(80);
        assert_eq!(state.remaining, 20);
        // Write 50 lines → earn back 50.
        let snap = state.reward_mutation(50);
        assert_eq!(snap.remaining, 70);
        assert_eq!(snap.delta, 50);
    }

    #[test]
    fn reward_mutation_capped_at_ceiling() {
        let cfg = test_config();
        let mut state = BudgetState::new(&cfg);
        state.remaining = 490;
        // Writing 100 lines should cap at ceiling (500), not reach 590.
        let snap = state.reward_mutation(100);
        assert_eq!(snap.remaining, 500);
        assert_eq!(snap.delta, 10);
    }

    #[test]
    fn reward_mutation_zero_is_noop() {
        let cfg = test_config();
        let mut state = BudgetState::new(&cfg);
        state.deduct(50);
        let before = state.remaining;
        let snap = state.reward_mutation(0);
        assert_eq!(snap.remaining, before);
    }

    // -- snapshot fields -----------------------------------------------

    #[test]
    fn snapshot_remaining_correct() {
        let cfg = test_config();
        let mut state = BudgetState::new(&cfg);
        state.deduct(30);
        let snap = state.snapshot();
        assert_eq!(snap.remaining, 70);
        assert_eq!(snap.ceiling, cfg.ceiling);
    }

    #[test]
    fn snapshot_warning_flag_set_at_threshold() {
        let cfg = test_config(); // warning_threshold = 40
        let mut state = BudgetState::new(&cfg);
        state.deduct(61); // remaining = 39 < 40 → warning
        let snap = state.snapshot();
        assert!(
            snap.warning,
            "warning must be set when remaining < warning_threshold"
        );
        assert!(!snap.critical, "critical must not be set at warning level");
    }

    #[test]
    fn snapshot_critical_flag_set_at_threshold() {
        let cfg = test_config(); // critical_threshold = 20
        let mut state = BudgetState::new(&cfg);
        state.deduct(81); // remaining = 19 < 20 → critical
        let snap = state.snapshot();
        assert!(
            snap.critical,
            "critical must be set when remaining < critical_threshold"
        );
        assert!(snap.warning, "warning must also be set when critical");
    }

    #[test]
    fn snapshot_no_flags_above_warning() {
        let cfg = test_config();
        let state = BudgetState::new(&cfg); // remaining = 100, well above thresholds
        let snap = state.snapshot();
        assert!(!snap.warning);
        assert!(!snap.critical);
    }

    // -- critical_max_lines -------------------------------------------

    #[test]
    fn critical_max_lines_returns_config_value() {
        let cfg = test_config(); // critical_max_lines = 15
        let state = BudgetState::new(&cfg);
        assert_eq!(state.critical_max_lines(), 15);
    }

    // -- sweep_expired ------------------------------------------------

    #[test]
    fn sweep_expired_removes_expired_files() {
        let tmp = tempfile::tempdir().unwrap();
        let budget_dir = tmp.path().join(BUDGET_DIR);
        std::fs::create_dir_all(&budget_dir).unwrap();

        // Create an expired file (expires_at_epoch = 1, now >> 1).
        let expired = serde_json::json!({
            "remaining": 50,
            "recovery_halvings": 0,
            "window_start_epoch": 0,
            "expires_at_epoch": 1
        });
        let expired_path = budget_dir.join("expired.json");
        std::fs::write(&expired_path, expired.to_string()).unwrap();

        // Create a fresh file (expires_at_epoch = now + 10000).
        let fresh_epoch = epoch_now() + 10_000;
        let fresh = serde_json::json!({
            "remaining": 100,
            "recovery_halvings": 0,
            "window_start_epoch": 0,
            "expires_at_epoch": fresh_epoch
        });
        let fresh_path = budget_dir.join("fresh.json");
        std::fs::write(&fresh_path, fresh.to_string()).unwrap();

        sweep_expired(tmp.path());

        assert!(!expired_path.exists(), "expired file must be deleted");
        assert!(fresh_path.exists(), "fresh file must be kept");
    }

    #[test]
    fn sweep_expired_all_fresh_none_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let budget_dir = tmp.path().join(BUDGET_DIR);
        std::fs::create_dir_all(&budget_dir).unwrap();

        let fresh_epoch = epoch_now() + 10_000;
        for i in 0..3_u32 {
            let data = serde_json::json!({
                "remaining": 100,
                "recovery_halvings": 0,
                "window_start_epoch": 0,
                "expires_at_epoch": fresh_epoch
            });
            std::fs::write(budget_dir.join(format!("{i}.json")), data.to_string()).unwrap();
        }

        sweep_expired(tmp.path());

        let count = std::fs::read_dir(&budget_dir).unwrap().count();
        assert_eq!(count, 3, "all fresh files must be kept");
    }

    #[test]
    fn sweep_expired_corrupt_json_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let budget_dir = tmp.path().join(BUDGET_DIR);
        std::fs::create_dir_all(&budget_dir).unwrap();

        let corrupt_path = budget_dir.join("corrupt.json");
        std::fs::write(&corrupt_path, "not valid json {{{").unwrap();

        sweep_expired(tmp.path());

        assert!(
            !corrupt_path.exists(),
            "corrupt JSON file must be deleted by sweep_expired"
        );
    }

    #[test]
    fn sweep_expired_non_json_files_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let budget_dir = tmp.path().join(BUDGET_DIR);
        std::fs::create_dir_all(&budget_dir).unwrap();

        // Write a .txt file — should not be touched.
        let txt_path = budget_dir.join("notes.txt");
        std::fs::write(&txt_path, "hello").unwrap();

        sweep_expired(tmp.path());

        assert!(txt_path.exists(), ".txt files must not be touched");
    }
}
