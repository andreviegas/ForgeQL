#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
//! `GitSha1Provider` — git SHA-1 backed [`SourceProvider`].
//!
//! Uses `git2` to walk commits, read blobs, and hash content using
//! git's blob-object algorithm: `"blob {len}\0{content}"`.
//!
//! Phase 01: instantiated at session creation but not yet used by the
//! legacy storage engine for any data-access operations.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use sha1::{Digest, Sha1};

use super::source_provider::{ContentId, SnapshotId, SourceProvider};

// -----------------------------------------------------------------------
// Content and Snapshot ID types
// -----------------------------------------------------------------------

/// 20-byte SHA-1 blob hash.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GitSha1(pub [u8; 20]);

impl ContentId for GitSha1 {
    fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self>
    where
        Self: Sized,
    {
        let arr: [u8; 20] = bytes.try_into().map_err(|_| {
            anyhow!(
                "GitSha1::from_bytes: expected 20 bytes, got {}",
                bytes.len()
            )
        })?;
        Ok(Self(arr))
    }

    fn hex(&self) -> String {
        use std::fmt::Write as _;
        self.0.iter().fold(String::with_capacity(40), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    }

    fn byte_len() -> usize
    where
        Self: Sized,
    {
        20
    }
}

/// Git SHA-1 commit hash (same width as blob hash).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GitSha1Commit(pub [u8; 20]);

impl SnapshotId for GitSha1Commit {
    fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    fn hex(&self) -> String {
        use std::fmt::Write as _;
        self.0.iter().fold(String::with_capacity(40), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
    }
}

// -----------------------------------------------------------------------
// GitSha1Provider
// -----------------------------------------------------------------------

/// `SourceProvider` for standard git repositories using SHA-1 object hashes.
///
/// Opened lazily — the `repo` path is validated at construction time;
/// actual git operations happen on demand.
pub struct GitSha1Provider {
    repo_path: PathBuf,
}

impl GitSha1Provider {
    /// Open the bare (or non-bare) git repository at `repo_path`.
    ///
    /// # Errors
    /// Returns an error if the path is not a valid git repository.
    pub fn new(repo_path: PathBuf) -> Result<Self> {
        // Validate by opening; we don't keep the handle because git2 is not Send.
        let _repo = git2::Repository::open_bare(&repo_path)
            .or_else(|_| git2::Repository::open(&repo_path))
            .map_err(|e| {
                anyhow!(
                    "GitSha1Provider: cannot open repo at {}: {e}",
                    repo_path.display()
                )
            })?;
        Ok(Self { repo_path })
    }
}

impl SourceProvider for GitSha1Provider {
    type Content = GitSha1;
    type Snapshot = GitSha1Commit;

    fn provider_id(&self) -> &'static str {
        "git-sha1"
    }

    fn hash_content(&self, bytes: &[u8]) -> GitSha1 {
        // git blob object hash: SHA1("blob {len}\0{content}")
        let mut hasher = Sha1::new();
        hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
        hasher.update(bytes);
        let result = hasher.finalize();
        GitSha1(result.into())
    }

    fn walk_snapshot(
        &self,
        snap: &GitSha1Commit,
    ) -> Result<Box<dyn Iterator<Item = Result<(PathBuf, GitSha1)>> + Send>> {
        let repo = git2::Repository::open_bare(&self.repo_path)
            .or_else(|_| git2::Repository::open(&self.repo_path))
            .map_err(|e| anyhow!("walk_snapshot: cannot open repo: {e}"))?;

        let oid = git2::Oid::from_bytes(&snap.0)
            .map_err(|e| anyhow!("walk_snapshot: invalid oid: {e}"))?;
        let commit = repo
            .find_commit(oid)
            .map_err(|e| anyhow!("walk_snapshot: commit not found: {e}"))?;
        let tree = commit
            .tree()
            .map_err(|e| anyhow!("walk_snapshot: tree not found: {e}"))?;

        let mut entries: Vec<Result<(PathBuf, GitSha1)>> = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |root, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                let path = if root.is_empty() {
                    PathBuf::from(entry.name().unwrap_or(""))
                } else {
                    PathBuf::from(root).join(entry.name().unwrap_or(""))
                };
                let oid = entry.id();
                let oid_bytes = oid.as_bytes();
                match <[u8; 20]>::try_from(oid_bytes) {
                    Ok(arr) => entries.push(Ok((path, GitSha1(arr)))),
                    Err(_) => {
                        entries.push(Err(anyhow!("unexpected oid byte length")));
                    }
                }
            }
            git2::TreeWalkResult::Ok
        })
        .map_err(|e| anyhow!("walk_snapshot: tree walk failed: {e}"))?;

        Ok(Box::new(entries.into_iter()))
    }

    fn read_content(&self, id: &GitSha1) -> Result<Vec<u8>> {
        let repo = git2::Repository::open_bare(&self.repo_path)
            .or_else(|_| git2::Repository::open(&self.repo_path))
            .map_err(|e| anyhow!("read_content: cannot open repo: {e}"))?;
        let oid =
            git2::Oid::from_bytes(&id.0).map_err(|e| anyhow!("read_content: invalid oid: {e}"))?;
        let blob = repo
            .find_blob(oid)
            .map_err(|e| anyhow!("read_content: blob not found: {e}"))?;
        Ok(blob.content().to_vec())
    }

    fn current_snapshot(&self, worktree: &Path) -> Result<GitSha1Commit> {
        let repo = git2::Repository::open(worktree).map_err(|e| {
            anyhow!(
                "current_snapshot: cannot open repo at {}: {e}",
                worktree.display()
            )
        })?;
        let head = repo
            .head()
            .map_err(|e| anyhow!("current_snapshot: HEAD not found: {e}"))?;
        let oid = head
            .target()
            .ok_or_else(|| anyhow!("current_snapshot: HEAD is not a direct ref"))?;
        let bytes: [u8; 20] = oid
            .as_bytes()
            .try_into()
            .map_err(|_| anyhow!("current_snapshot: unexpected oid length"))?;
        Ok(GitSha1Commit(bytes))
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements
)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::process::Command;

    use super::*;

    /// Canonical path to the workspace root (the git repo under test).
    fn workspace_root() -> PathBuf {
        // CARGO_MANIFEST_DIR = crates/forgeql-core
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..") // -> crates/
            .join("..") // -> workspace root
            .canonicalize()
            .expect("canonicalize workspace root")
    }

    /// Run `git ls-tree -r HEAD` in the workspace root and return a map of
    /// `path -> 40-char blob SHA`.  Skips non-blob entries (submodules).
    fn ls_tree(root: &std::path::Path) -> HashMap<PathBuf, String> {
        let output = Command::new("git")
            .args(["ls-tree", "-r", "HEAD"])
            .current_dir(root)
            .output()
            .expect("failed to run `git ls-tree -r HEAD`");
        assert!(
            output.status.success(),
            "`git ls-tree` exited non-zero: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("non-utf8 ls-tree output");
        stdout
            .lines()
            .filter(|l| l.contains(" blob "))
            .map(|l| {
                // format: "<mode> blob <sha40>\t<path>"
                let (meta, path_str) = l.split_once('\t').expect("no tab in ls-tree line");
                let sha = meta
                    .split_ascii_whitespace()
                    .nth(2)
                    .expect("no sha field in ls-tree line")
                    .to_string();
                (PathBuf::from(path_str), sha)
            })
            .collect()
    }

    /// Task 4b gate test: `GitSha1Provider::walk_snapshot` matches `git ls-tree -r HEAD`.
    ///
    /// Opens the repo that contains this crate, resolves the current HEAD via
    /// `current_snapshot`, walks the tree with `walk_snapshot`, and asserts that
    /// every (path, blob-id) pair matches the output of `git ls-tree -r HEAD`.
    #[test]
    fn walk_snapshot_matches_git_ls_tree() {
        let root = workspace_root();

        let provider = GitSha1Provider::new(root.clone()).expect("failed to open fixture repo");

        // Resolve HEAD.
        let snap = provider
            .current_snapshot(&root)
            .expect("failed to resolve HEAD snapshot");

        // Walk the tree via the provider.
        let walked: HashMap<PathBuf, String> = provider
            .walk_snapshot(&snap)
            .expect("walk_snapshot failed")
            .map(|r| {
                let (path, id) = r.expect("tree entry error");
                (path, id.hex())
            })
            .collect();

        // Walk via `git ls-tree` (ground truth).
        let expected = ls_tree(&root);

        // Every entry in `expected` must appear in `walked` with the same SHA.
        let mut mismatches: Vec<String> = Vec::new();
        for (path, sha) in &expected {
            match walked.get(path) {
                None => mismatches.push(format!("missing: {}", path.display())),
                Some(got) if got != sha => mismatches.push(format!(
                    "sha mismatch for {}: walk={got} ls-tree={sha}",
                    path.display()
                )),
                Some(_) => {}
            }
        }
        // Entries in `walked` that are absent from `expected` are also a bug.
        for path in walked.keys() {
            if !expected.contains_key(path) {
                mismatches.push(format!("extra entry not in ls-tree: {}", path.display()));
            }
        }

        assert!(
            mismatches.is_empty(),
            "GitSha1Provider::walk_snapshot diverges from git ls-tree:\n{}",
            mismatches.join("\n")
        );
    }
}
