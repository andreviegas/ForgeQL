//! Removal-range tombstones — staging derived from the byte range a mutation
//! removes, not from the verb that removed it. Any path that deletes or
//! replaces a line span calls `stage_removed_span`, so a byte-identical sibling
//! can never adopt the handle of a construct whose whole span was removed (the
//! `IF REV` blind spot, previously reached through verbs that skipped the
//! tombstone).

use anyhow::Result;

use crate::engine::{ForgeQLEngine, require_session_id};

impl ForgeQLEngine {
    /// Tombstone the ordinal of every root node whose whole span lies inside the
    /// removed line range `[start, end]` (1-based, inclusive) in `rel_path`.
    /// Staged into the session so the reindex this mutation triggers retires
    /// those handles instead of letting a byte-identical sibling adopt them.
    pub(in crate::engine) fn stage_removed_span(
        &mut self,
        session_id: Option<&str>,
        rel_path: &str,
        start: usize,
        end: usize,
    ) -> Result<()> {
        let sid = require_session_id(session_id)?;
        let ordinals = {
            let session = self.require_session(sid)?;
            session
                .engine_for(&crate::ir::Backend::Default)?
                .root_ordinals_within(rel_path, &session.worktree_path, start, end)
        };
        if ordinals.is_empty() {
            return Ok(());
        }
        if let Some(session) = self.sessions.get_mut(sid) {
            session
                .pending_tombstones
                .entry(rel_path.into())
                .or_default()
                .extend(ordinals);
        }
        Ok(())
    }
}
