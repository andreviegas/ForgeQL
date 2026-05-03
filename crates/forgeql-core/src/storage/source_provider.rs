#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::type_complexity
)]
//! [`SourceProvider`] trait — decouples storage from git internals.
//!
//! Every operation that needs to identify source content, walk a workspace
//! snapshot, or read a content blob goes through this trait. The storage
//! engine never calls git directly; it calls the provider.
//!
//! See `ForgeQL-StorageEngine-Plan/source-provider.md` for the full design.
//!
//! # Phase 01 scope
//!
//! The trait is defined and `GitSha1Provider` is wired into `Session::new`,
//! but `LegacyMemoryStorage` does not call any provider methods — it still
//! uses `Workspace` + `git2` for index building as before.
//!
//! The trait exists now so Phase 03 writes against an abstraction from day one
//! instead of fighting a git-coupled storage engine later.

use std::hash::Hash;
use std::path::{Path, PathBuf};

use anyhow::Result;

// -----------------------------------------------------------------------
// ContentId and SnapshotId
// -----------------------------------------------------------------------

/// Identifies a single version of a source file in a provider-specific way.
///
/// For `GitSha1Provider` this is a 20-byte SHA-1 blob hash.
/// For `GitSha256Provider` it would be 32 bytes.
pub trait ContentId: Hash + Eq + Clone + Send + Sync + 'static {
    /// Raw bytes of the content identifier.
    fn as_bytes(&self) -> &[u8];
    /// Parse from raw bytes. Returns an error if `bytes.len()` is wrong.
    fn from_bytes(bytes: &[u8]) -> Result<Self>
    where
        Self: Sized;
    /// Hex string representation (lowercase), used in on-disk paths.
    fn hex(&self) -> String;
    /// Fixed byte length for this content-ID type.
    fn byte_len() -> usize
    where
        Self: Sized;
}

/// Identifies a workspace snapshot (commit) in a provider-specific way.
pub trait SnapshotId: Hash + Eq + Clone + Send + Sync + 'static {
    /// Raw bytes of the snapshot identifier.
    fn as_bytes(&self) -> &[u8];
    /// Hex string representation (lowercase).
    fn hex(&self) -> String;
}

// -----------------------------------------------------------------------
// SourceProvider
// -----------------------------------------------------------------------

/// Abstracts all access to source content addresses and workspace snapshots.
///
/// Storage code calls only these methods — never `gix` / `git2` directly.
/// This lets the storage engine support SHA-1 git, SHA-256 git, Jujutsu,
/// or any future SCM without code changes.
pub trait SourceProvider: Send + Sync {
    /// The content-ID type produced by this provider.
    type Content: ContentId;
    /// The snapshot-ID type produced by this provider.
    type Snapshot: SnapshotId;

    /// Stable short string embedded in segment paths and headers.
    ///
    /// Must never change across releases for the same underlying SCM.
    /// Examples: `"git-sha1"`, `"git-sha256"`, `"mock"`.
    fn provider_id(&self) -> &'static str;

    /// Hash `bytes` using the provider's content-addressing algorithm.
    ///
    /// Must be deterministic and must match how the SCM hashes the same bytes
    /// (so worktree files and tracked blobs yield identical IDs).
    fn hash_content(&self, bytes: &[u8]) -> Self::Content;

    /// Enumerate every `(path, content_id)` pair reachable from `snap`.
    fn walk_snapshot(
        &self,
        snap: &Self::Snapshot,
    ) -> Result<Box<dyn Iterator<Item = Result<(PathBuf, Self::Content)>> + Send>>;

    /// Read content bytes for a given content ID.
    fn read_content(&self, id: &Self::Content) -> Result<Vec<u8>>;

    /// Resolve the snapshot currently checked out in `worktree`.
    fn current_snapshot(&self, worktree: &Path) -> Result<Self::Snapshot>;

    /// Compute paths that changed between two snapshots.
    ///
    /// The default implementation walks both snapshots and diffs by content ID.
    /// SCMs with fast native diff (git pack deltas) may override.
    fn changed_paths(&self, from: &Self::Snapshot, to: &Self::Snapshot) -> Result<Vec<PathBuf>> {
        let from_map: std::collections::HashMap<PathBuf, Self::Content> =
            self.walk_snapshot(from)?.filter_map(Result::ok).collect();
        let to_map: std::collections::HashMap<PathBuf, Self::Content> =
            self.walk_snapshot(to)?.filter_map(Result::ok).collect();

        let mut changed = Vec::new();
        for (path, id) in &to_map {
            if from_map.get(path) != Some(id) {
                changed.push(path.clone());
            }
        }
        for path in from_map.keys() {
            if !to_map.contains_key(path) {
                changed.push(path.clone());
            }
        }
        Ok(changed)
    }
}
