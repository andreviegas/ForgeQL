//! Read-only views onto engine and session state.
//!
//! Everything here answers a question without changing anything: how many
//! commands have run, what a session's line budget looks like, where its
//! worktree is. Transports call these to build status output and footers, so
//! they must stay cheap and side-effect free.

use std::path::Path;

use crate::{engine::ForgeQLEngine, ir::ForgeQLIR, session::Session};

impl ForgeQLEngine {
    /// Number of commands served since engine creation.
    #[must_use]
    pub const fn commands_served(&self) -> u64 {
        self.commands_served
    }

    /// Number of active sessions (in-memory) plus pending sessions (on-disk, not yet loaded).
    #[must_use]
    pub fn session_count(&self) -> usize {
        self.sessions.len() + self.pending_sessions.len()
    }

    /// Return the current budget snapshot for a session.
    /// Returns `None` if no budget is active OR if the last operation was an
    /// admin-exempt command (`CreateSource`, `RefreshSource`, `ShowSources`, `ShowBranches`, `ShowVersion`)
    /// — those commands should not appear in the budget log.
    #[must_use]
    pub fn budget_status(&self, session_id: &str) -> Option<crate::budget::BudgetSnapshot> {
        self.sessions
            .get(session_id)
            .and_then(Session::budget_snapshot)
    }

    /// Worktree root for a loaded session, used by transports to locate the
    /// session's `SHOW MORE` buffer. `None` when the session is not in memory.
    #[must_use]
    pub fn session_worktree(&self, session_id: &str) -> Option<std::path::PathBuf> {
        self.sessions
            .get(session_id)
            .map(|s| s.worktree_path.clone())
    }

    /// Inline output cap (lines) for a loaded session, used by transports to
    /// window over-cap CSV output into the `SHOW MORE` buffer. Falls back to
    /// the configured default when the session is not resident in memory.
    #[must_use]
    pub fn session_inline_cap(&self, session_id: &str) -> usize {
        self.sessions.get(session_id).map_or_else(
            || crate::config::OutputConfig::default().show_lines,
            |s| s.output_config().show_lines,
        )
    }

    /// Return `Some(snapshot)` only for non-admin ops, `None` for admin-exempt commands.
    #[must_use]
    pub fn budget_status_for_op(
        &self,
        session_id: &str,
        op: &ForgeQLIR,
    ) -> Option<crate::budget::BudgetSnapshot> {
        let is_admin = matches!(
            op,
            ForgeQLIR::CreateSource { .. }
                | ForgeQLIR::RefreshSource { .. }
                | ForgeQLIR::ShowSources
                | ForgeQLIR::ShowBranches
                | ForgeQLIR::ShowVersion
        );
        if is_admin {
            None
        } else {
            self.budget_status(session_id)
        }
    }
    /// Number of registered sources.
    #[must_use]
    pub fn source_count(&self) -> usize {
        self.registry.len()
    }

    /// The data directory path.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)] // PathBuf::as_path is not const
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}
