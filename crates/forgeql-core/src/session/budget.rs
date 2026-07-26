//! The per-session line budget.
//!
//! Reading costs: every source line a read discloses is deducted, and once the
//! budget is critical `SHOW` output is capped rather than refused. Writing
//! credits back — `reward_budget` grants one line per line a mutation wrote, up
//! to the configured ceiling — so the budget prices reading against the work it
//! leads to.
//!
//! The persisted fields are written on every change rather than held in memory,
//! and `init_budget` restores them when a session is resumed, so a budget
//! survives a server restart. Two deliberate exceptions: `last_delta` is
//! per-query scratch and is never written, and a budget left idle past its
//! expiry is discarded and started fresh rather than restored.

use crate::budget::{BudgetSnapshot, BudgetState};
use crate::config::LineBudgetConfig;
use crate::session::Session;

impl Session {
    /// Initialise the line-budget for this session.
    ///
    /// `data_dir` is the `ForgeQL` data root (`~/.forgeql`).
    /// `budget_branch` is the computed budget key — the feature branch name,
    /// derived by the engine from the `USE` command (see `use_source`).
    /// When `resumed` is `true` the persisted budget is restored from disk;
    /// otherwise a fresh budget is created.
    pub fn init_budget(
        &mut self,
        config: &LineBudgetConfig,
        resumed: bool,
        data_dir: &std::path::Path,
        budget_branch: &str,
    ) {
        self.budget_data_dir = Some(data_dir.to_path_buf());
        self.budget_branch = Some(budget_branch.to_string());
        self.budget = Some(if resumed {
            BudgetState::load(config, data_dir, &self.source_name, budget_branch)
        } else {
            BudgetState::new(config)
        });
    }

    /// Deduct `lines` from the budget and persist the new state.
    /// Returns `None` when no budget is configured.
    pub fn deduct_budget(&mut self, lines: usize) -> Option<BudgetSnapshot> {
        let data_dir = self.budget_data_dir.clone()?;
        let budget_branch = self.budget_branch.clone()?;
        let budget = self.budget.as_mut()?;
        let snap = budget.deduct(lines);
        budget.save(&data_dir, &self.source_name, &budget_branch);
        Some(snap)
    }

    /// Grant proportional budget recovery for a mutation that wrote code.
    ///
    /// Unlike `deduct_budget(0)` which triggers the rolling-window recovery,
    /// this rewards the agent 1:1 for every line written.
    pub fn reward_budget(&mut self, lines_written: usize) -> Option<BudgetSnapshot> {
        let data_dir = self.budget_data_dir.clone()?;
        let budget_branch = self.budget_branch.clone()?;
        let budget = self.budget.as_mut()?;
        let snap = budget.reward_mutation(lines_written);
        budget.save(&data_dir, &self.source_name, &budget_branch);
        Some(snap)
    }

    /// Reset the budget delta to zero for non-consuming commands.
    pub const fn reset_budget_delta(&mut self) {
        if let Some(ref mut b) = self.budget {
            b.reset_delta();
        }
    }

    /// Return `true` if a budget is active and in critical state.
    #[must_use]
    pub fn is_budget_critical(&self) -> bool {
        self.budget.as_ref().is_some_and(BudgetState::is_critical)
    }
    /// Maximum lines allowed when in critical state.
    #[must_use]
    pub fn budget_critical_max_lines(&self) -> Option<usize> {
        self.budget
            .as_ref()
            .filter(|b| b.is_critical())
            .map(BudgetState::critical_max_lines)
    }

    /// Current budget snapshot (without deducting).
    #[must_use]
    pub fn budget_snapshot(&self) -> Option<BudgetSnapshot> {
        self.budget.as_ref().map(BudgetState::snapshot)
    }
}
