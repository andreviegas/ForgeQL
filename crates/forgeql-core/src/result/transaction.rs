//! Result types for transaction lifecycle (begin, commit, rollback).

use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------
// Transaction results
// -----------------------------------------------------------------------

/// Result of a `BEGIN TRANSACTION 'name'` — checkpoint created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeginTransactionResult {
    /// Checkpoint label.
    pub name: String,
    /// Git commit OID recorded as the checkpoint.
    pub checkpoint_oid: String,
}

/// Result of a `COMMIT MESSAGE 'msg'` — git commit created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitResult {
    /// Commit message.
    pub message: String,
    /// Git commit hash of the new commit.
    pub commit_hash: String,
}

// -----------------------------------------------------------------------
// Rollback result
// -----------------------------------------------------------------------

/// Result of a `ROLLBACK [TRANSACTION 'name']` operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackResult {
    /// The checkpoint label (or `"last"` if none was specified).
    pub name: String,
    /// Git commit OID that was reset to.
    pub reset_to_oid: String,
}
