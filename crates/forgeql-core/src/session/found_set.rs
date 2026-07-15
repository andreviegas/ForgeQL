//! The `FOUND` set — the rows the previous FIND returned, and the master rev
//! that gates a bulk mutation over them.
//!
//! `FIND` *is* the set-selection syntax: a query with precise filters already
//! names the set, so the bulk verbs address it as `FOUND` rather than carrying a
//! second glob grammar. What FIND returned is what FOUND holds — never more.
//!
//! ## Invariant
//!
//! `file on disk == in-memory set` after every command that touches it: a FIND
//! replaces it, a mutation clears it. The set outlives the process because the
//! session does — the server may restart between the FIND and the mutation.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const FILE_VERSION: u32 = 1;
const FILE_NAME: &str = ".forgeql-foundset";

/// One row of the FIND result the set was armed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundMember {
    /// Stable handle, when the row carried one (`FIND symbols`, `FIND files`).
    ///
    /// `FIND usages` rows are call sites, not nodes: they have no handle, so
    /// the containing file's rev stands in for them in the master rev. That is
    /// coarser — any edit to the file breaks the gate — and deliberately so: a
    /// site is a line number, and line numbers move.
    pub node_id: Option<String>,
    /// Worktree-relative path. A row without one cannot be armed.
    pub path: String,
    /// 1-based line, for the site rows that carry no handle.
    pub line: Option<usize>,
}

impl FoundMember {
    /// The identity the master rev is computed over: the handle when there is
    /// one, else the site coordinates.
    #[must_use]
    pub fn key(&self) -> String {
        self.node_id.clone().unwrap_or_else(|| {
            self.line
                .map_or_else(|| self.path.clone(), |line| format!("{}:{line}", self.path))
        })
    }
}

/// The previous FIND's result set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundSet {
    /// Which FIND armed it — so an error can name the query to re-run.
    pub origin: String,
    /// Exactly the rows the agent was shown, in the order it saw them.
    pub members: Vec<FoundMember>,
    /// `false` when the FIND was truncated (`total > results.len()`).
    ///
    /// An incomplete set is still armed, but no master rev is issued for it and
    /// every FOUND mutation refuses. Acting on rows the agent never saw is the
    /// one mistake reading the diff afterwards cannot catch — for
    /// `DELETE NODES FOUND` it is a blind mass delete.
    pub complete: bool,
    /// The master rev issued for this set at FIND time, when one was issued.
    ///
    /// `None` for a truncated set — the gate has nothing to quote and every
    /// FOUND verb refuses. It is stored so the FIND response can hand it back,
    /// never trusted at mutation time: the gate re-derives the rev from the
    /// live members, so a set that has moved since cannot pass by quoting the
    /// rev it was armed with.
    pub master_rev: Option<String>,
}

impl FoundSet {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Master rev over the `(key, rev)` pairs of every member, in FIND order.
    ///
    /// Both ends read the per-member revs live — FIND to issue the rev, the
    /// mutation to re-derive it — so any change to a member's content, or to
    /// the membership itself, flips the hash. This is the set-level extension
    /// of the per-node `IF REV` contract, and unlike a directory's membership
    /// XOR it covers content too, because `CHANGE NODES FOUND` edits content.
    #[must_use]
    pub fn master_rev(pairs: &[(String, String)]) -> String {
        let mut hasher = Sha256::new();
        for (key, rev) in pairs {
            hasher.update(key.as_bytes());
            hasher.update([0u8]);
            hasher.update(rev.as_bytes());
            hasher.update([b'\n']);
        }
        let digest = hasher.finalize();
        crate::node_id::format_rev_exact(u64::from_le_bytes(
            digest[..8].try_into().unwrap_or([0u8; 8]),
        ))
    }
}

/// Root of the serialized set file.
#[derive(Serialize, Deserialize)]
struct FoundSetFile {
    version: u32,
    set: FoundSet,
}

/// Persist the set to `<worktree>/.forgeql-foundset`.
///
/// Called wherever the set is mutated — arming it and clearing it — rather than
/// at transaction boundaries: a session outlives the server, so state a later
/// command depends on has to reach disk the moment it changes.
///
/// # Errors
/// Returns `Err` if serialization or the atomic write fails.
pub fn save(set: &FoundSet, worktree_path: &Path) -> Result<()> {
    let bytes = bincode::serialize(&FoundSetFile {
        version: FILE_VERSION,
        set: set.clone(),
    })?;
    crate::workspace::file_io::write_atomic(&worktree_path.join(FILE_NAME), &bytes)?;
    Ok(())
}

/// Drop the persisted set. A mutation invalidates FOUND, and a file that outlived
/// it would re-arm a set whose line numbers have already shifted.
pub fn clear(worktree_path: &Path) {
    let path = worktree_path.join(FILE_NAME);
    if path.exists()
        && let Err(err) = std::fs::remove_file(&path)
    {
        tracing::warn!(error = %err, "could not remove the persisted FOUND set");
    }
}

/// Restore the set written by a previous process, if any.
///
/// A missing, corrupt, or version-mismatched file degrades to "no previous
/// FIND" — the state a fresh session is in, which every FOUND verb already
/// handles by telling the agent to run the FIND again.
#[must_use]
pub fn try_restore(worktree_path: &Path) -> Option<FoundSet> {
    let path = worktree_path.join(FILE_NAME);
    if !path.exists() {
        return None;
    }
    match crate::workspace::file_io::read_bytes(&path)
        .and_then(|bytes| Ok(bincode::deserialize::<FoundSetFile>(&bytes)?))
    {
        Ok(file) if file.version == FILE_VERSION => Some(file.set),
        Ok(file) => {
            tracing::warn!(
                version = file.version,
                expected = FILE_VERSION,
                "FOUND set file version mismatch — discarding"
            );
            None
        }
        Err(err) => {
            tracing::warn!(error = %err, "FOUND set file load failed (non-fatal)");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_rev_changes_when_a_member_rev_changes() {
        let before =
            FoundSet::master_rev(&[("na".into(), "h1".into()), ("nb".into(), "h2".into())]);
        let after = FoundSet::master_rev(&[("na".into(), "h1".into()), ("nb".into(), "h9".into())]);
        assert_ne!(before, after);
    }

    #[test]
    fn master_rev_changes_when_a_member_leaves() {
        let both = FoundSet::master_rev(&[("na".into(), "h1".into()), ("nb".into(), "h2".into())]);
        let one = FoundSet::master_rev(&[("na".into(), "h1".into())]);
        assert_ne!(both, one);
    }

    #[test]
    fn site_rows_key_on_path_and_line() {
        let site = FoundMember {
            node_id: None,
            path: "src/a.rs".into(),
            line: Some(42),
        };
        assert_eq!(site.key(), "src/a.rs:42");
    }

    #[test]
    fn roundtrips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let set = FoundSet {
            origin: "find_symbols".into(),
            members: vec![FoundMember {
                node_id: Some("nabc.0001".into()),
                path: "src/a.rs".into(),
                line: Some(10),
            }],
            complete: true,
            master_rev: Some("h0123456789abcdef".into()),
        };
        save(&set, dir.path()).expect("save");
        assert_eq!(try_restore(dir.path()), Some(set));

        clear(dir.path());
        assert_eq!(try_restore(dir.path()), None);
    }
}
