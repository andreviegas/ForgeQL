/// Persistent checkpoint stack — serialized to `.forgeql-checkpoints` in the
/// worktree so the stack survives server restarts.
///
/// ## Invariant (holds after every completed operation)
///
/// `file on disk == in-memory stack`  AND
/// `checkpoints.last().oid` (or `last_clean_oid` when stack is empty) == `HEAD`
///
/// The invariant is maintained by:
/// - **BEGIN**: `stage_and_commit` → push → `save`  (file = full new stack)
/// - **ROLLBACK**: pop in-memory → `reset_hard` → `save`  (overwrites whatever
///   git restored the file to with the correct popped stack)
/// - **COMMIT**: squash → `checkpoints.clear()` → `remove_file`
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::session::{Checkpoint, Session};

// v2 adds `PersistedCheckpoint.created`. bincode is positional, so a v1 file
// cannot be read as v2 — `load` bails on the version and `try_restore` degrades
// to an empty stack with a warning, which is the same graceful path a corrupt
// file takes. The only loss is an in-flight transaction across an upgrade.
const FILE_VERSION: u32 = 2;
const FILE_NAME: &str = ".forgeql-checkpoints";

/// Root of the serialized checkpoint file.
#[derive(Serialize, Deserialize)]
struct CheckpointFile {
    version: u32,
    last_clean_oid: Option<String>,
    checkpoints: Vec<PersistedCheckpoint>,
}

/// One entry in the persisted stack — mirrors [`Checkpoint`].
#[derive(Serialize, Deserialize)]
struct PersistedCheckpoint {
    name: String,
    oid: String,
    pre_txn_oid: String,
    created: Vec<std::path::PathBuf>,
}

// -----------------------------------------------------------------------
// Public surface
// -----------------------------------------------------------------------

/// Serialize the current session's checkpoint stack to `<worktree>/.forgeql-checkpoints`.
///
/// # Errors
/// Returns `Err` if bincode serialization or the atomic write fails.
pub fn save(session: &Session, worktree_path: &Path) -> Result<()> {
    let cf = CheckpointFile {
        version: FILE_VERSION,
        last_clean_oid: session.last_clean_oid.clone(),
        checkpoints: session
            .checkpoints
            .iter()
            .map(|cp| PersistedCheckpoint {
                name: cp.name.clone(),
                oid: cp.oid.clone(),
                pre_txn_oid: cp.pre_txn_oid.clone(),
                created: cp.created.clone(),
            })
            .collect(),
    };
    let bytes = bincode::serialize(&cf)?;
    let path = worktree_path.join(FILE_NAME);
    crate::workspace::file_io::write_atomic(&path, &bytes)?;
    debug!(
        checkpoints = cf.checkpoints.len(),
        path = %path.display(),
        "checkpoint stack saved to disk"
    );
    Ok(())
}

/// Remove the checkpoint file (called after `COMMIT MESSAGE`).
///
/// Silently ignores `NotFound` — idempotent.
pub fn remove(worktree_path: &Path) {
    let path = worktree_path.join(FILE_NAME);
    let _ = std::fs::remove_file(&path);
}

/// Attempt to restore the checkpoint stack from disk into `session`.
///
/// Validates that the stored HEAD matches `current_head` before restoring.
/// If the file is missing, corrupt, or stale, the session is left with an
/// empty stack (graceful degradation — same behaviour as before FT6).
pub fn try_restore(session: &mut Session, worktree_path: &Path, current_head: &str) {
    let path = worktree_path.join(FILE_NAME);
    if !path.exists() {
        return;
    }
    match load(&path) {
        Ok(cf) => {
            // Combined HEAD validation per Q2 answer:
            //   - active checkpoints  → top checkpoint OID must equal HEAD
            //   - empty stack         → last_clean_oid must equal HEAD
            let expected = cf
                .checkpoints
                .last()
                .map(|c| c.oid.as_str())
                .or(cf.last_clean_oid.as_deref());
            if expected == Some(current_head) {
                let n = cf.checkpoints.len();
                restore_into(cf, session);
                debug!(checkpoints = n, "restored checkpoint stack from disk");
            } else {
                tracing::warn!(
                    expected = ?expected,
                    actual = current_head,
                    "checkpoint file HEAD mismatch — discarding stale stack"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "checkpoint file load failed (non-fatal)");
        }
    }
}

// -----------------------------------------------------------------------
// Private helpers
// -----------------------------------------------------------------------

fn load(path: &Path) -> Result<CheckpointFile> {
    let bytes = crate::workspace::file_io::read_bytes(path)?;
    let cf: CheckpointFile = bincode::deserialize(&bytes)?;
    if cf.version != FILE_VERSION {
        anyhow::bail!(
            "checkpoint file version mismatch: file has v{}, expected v{}",
            cf.version,
            FILE_VERSION
        );
    }
    Ok(cf)
}

fn restore_into(cf: CheckpointFile, session: &mut Session) {
    session.last_clean_oid = cf.last_clean_oid;
    session.checkpoints = cf
        .checkpoints
        .into_iter()
        .map(|p| Checkpoint {
            name: p.name,
            oid: p.oid,
            pre_txn_oid: p.pre_txn_oid,
            created: p.created,
        })
        .collect();
}
