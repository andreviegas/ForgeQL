#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    unused_results
)]
//! `MockProvider` — in-memory [`SourceProvider`] for unit tests.
//!
//! Backed by `HashMap<MockId, Vec<u8>>` + `HashMap<MockSnapshot, Vec<...>>`.
//! No git dependency; usable without a real repository on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};

use super::source_provider::{ContentId, SnapshotId, SourceProvider};

// -----------------------------------------------------------------------
// ID types
// -----------------------------------------------------------------------

/// Opaque blob identifier for the mock provider (arbitrary bytes).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MockId(pub Vec<u8>);

impl ContentId for MockId {
    fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Ok(Self(bytes.to_vec()))
    }

    fn hex(&self) -> String {
        use std::fmt::Write as _;
        self.0.iter().fold(String::new(), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    }

    fn byte_len() -> usize {
        // Variable-length mock IDs; return 0 as sentinel.
        0
    }
}

/// Opaque snapshot identifier for the mock provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MockSnapshot(pub String);

impl SnapshotId for MockSnapshot {
    fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }

    fn hex(&self) -> String {
        use std::fmt::Write as _;
        self.0.as_bytes().iter().fold(String::new(), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    }
}

// -----------------------------------------------------------------------
// MockProvider
// -----------------------------------------------------------------------

/// In-memory source provider for unit tests.
///
/// Manually populate `blobs` and `snapshots` before running tests.
///
/// ```rust,ignore
/// let mut provider = MockProvider::default();
/// let id = provider.insert(b"fn foo() {}");
/// provider.add_snapshot("snap-1", vec![(PathBuf::from("foo.rs"), id)]);
/// ```
#[derive(Default)]
pub struct MockProvider {
    /// Map from content ID to file bytes.
    pub blobs: HashMap<MockId, Vec<u8>>,
    /// Map from snapshot label to list of (path, content_id) pairs.
    pub snapshots: HashMap<MockSnapshot, Vec<(PathBuf, MockId)>>,
    /// The "current" snapshot for `current_snapshot()`.
    pub current: Option<MockSnapshot>,
}

impl MockProvider {
    /// Insert `bytes` into the blob store and return the generated ID.
    ///
    /// The ID is the hash of the bytes using a simple FNV-1a algorithm.
    /// Deterministic for the same byte sequence.
    pub fn insert(&mut self, bytes: &[u8]) -> MockId {
        let id = MockId(fnv1a(bytes).to_le_bytes().to_vec());
        self.blobs.insert(id.clone(), bytes.to_vec());
        id
    }

    /// Register a snapshot with a list of `(path, content_id)` pairs.
    pub fn add_snapshot(&mut self, label: &str, entries: Vec<(PathBuf, MockId)>) {
        self.snapshots
            .insert(MockSnapshot(label.to_string()), entries);
    }

    /// Set the current snapshot label returned by `current_snapshot()`.
    pub fn set_current(&mut self, label: &str) {
        self.current = Some(MockSnapshot(label.to_string()));
    }
}

impl SourceProvider for MockProvider {
    type Content = MockId;
    type Snapshot = MockSnapshot;

    fn provider_id(&self) -> &'static str {
        "mock"
    }

    fn hash_content(&self, bytes: &[u8]) -> MockId {
        MockId(fnv1a(bytes).to_le_bytes().to_vec())
    }

    #[allow(clippy::needless_collect)] // collect breaks borrow on self.snapshots → needed for Send bound
    fn walk_snapshot(
        &self,
        snap: &MockSnapshot,
    ) -> Result<Box<dyn Iterator<Item = Result<(PathBuf, MockId)>> + Send>> {
        let entries: Vec<_> = self
            .snapshots
            .get(snap)
            .ok_or_else(|| anyhow!("MockProvider: snapshot '{}' not found", snap.0))?
            .iter()
            .map(|(p, id)| Ok::<_, anyhow::Error>((p.clone(), id.clone())))
            .collect();
        Ok(Box::new(entries.into_iter()))
    }

    fn read_content(&self, id: &MockId) -> Result<Vec<u8>> {
        self.blobs
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("MockProvider: blob not found"))
    }

    fn current_snapshot(&self, _worktree: &Path) -> Result<MockSnapshot> {
        self.current
            .clone()
            .ok_or_else(|| anyhow!("MockProvider: no current snapshot set"))
    }
}

// -----------------------------------------------------------------------
// Simple FNV-1a hash (no external dep needed for tests)
// -----------------------------------------------------------------------

fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 14_695_981_039_346_656_037;
    const PRIME: u64 = 1_099_511_628_211;
    bytes
        .iter()
        .fold(OFFSET, |acc, &b| (acc ^ u64::from(b)).wrapping_mul(PRIME))
}
