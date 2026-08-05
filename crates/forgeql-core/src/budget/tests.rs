//! Unit tests for line-budget tracking: construction and the snapshot it
//! exposes, deduction and recovery, the warning and critical thresholds and the
//! status line they drive, mutation rewards, and the on-disk round-trip with
//! its sweep of expired budget files.

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
