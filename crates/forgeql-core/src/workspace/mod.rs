pub mod file_io;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use tracing::debug;

use crate::error::ForgeError;

/// Represents a resolved workspace on disk.
///
/// All file enumeration MUST go through `Workspace` — never use
/// `std::fs::read_dir` directly. This ensures `.forgeql-ignore` and
/// `.gitignore` patterns are always respected.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Create a workspace rooted at `root`.
    ///
    /// # Errors
    /// Returns `Err` if `root` does not exist or is not a directory.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.is_dir() {
            return Err(ForgeError::WorkspaceRootNotFound(root).into());
        }
        Ok(Self { root })
    }

    /// Discover the workspace root by looking for `.forgeql.yaml` or `.git`
    /// starting from `start` and walking up to the filesystem root.
    ///
    /// # Errors
    /// Returns `Err` if no workspace root is found by the time the filesystem
    /// root is reached.
    pub fn discover(start: &Path) -> Result<Self> {
        let mut dir = start;
        loop {
            if dir.join(".forgeql.yaml").exists() || dir.join(".git").exists() {
                debug!(root = %dir.display(), "workspace root discovered");
                return Self::new(dir);
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => return Err(ForgeError::WorkspaceRootNotFound(start.to_path_buf()).into()),
            }
        }
    }

    /// The absolute path to the workspace root.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Enumerate all source files in the workspace, respecting:
    ///   - `.gitignore`
    ///   - `.ignore`
    ///   - `.forgeql-ignore`
    ///
    /// Yields absolute `PathBuf`s for regular files only (no directories).
    pub fn files(&self) -> impl Iterator<Item = PathBuf> + '_ {
        WalkBuilder::new(&self.root)
            .add_custom_ignore_filename(".forgeql-ignore")
            .hidden(false) // include dot-files unless ignored
            .git_ignore(true)
            .build()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                if entry.file_type().is_some_and(|t| t.is_file()) {
                    Some(entry.into_path())
                } else {
                    None
                }
            })
    }

    /// Enumerate all directories in the workspace, respecting the same ignore
    /// files as [`Workspace::files`].
    ///
    /// Returns absolute paths; the workspace root itself is not included.
    /// Directories are addressable nodes (a bare-hex `n<hex>` handle), and an
    /// empty directory is implied by no file path — so walking is the only way
    /// to see one.
    #[must_use]
    pub fn dirs(&self) -> Vec<PathBuf> {
        WalkBuilder::new(&self.root)
            .add_custom_ignore_filename(".forgeql-ignore")
            .hidden(false)
            .git_ignore(true)
            .build()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                if entry.file_type().is_some_and(|t| t.is_dir())
                    && entry.path() != self.root
                    && !crate::result::FileEntry::is_runtime_artifact(entry.path())
                {
                    Some(entry.into_path())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Enumerate source files matching a given extension (e.g. `"cpp"`, `"h"`).
    pub fn files_with_extension<'a>(&'a self, ext: &'a str) -> impl Iterator<Item = PathBuf> + 'a {
        self.files().filter(move |p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == ext)
        })
    }

    /// Return `true` if the given path is inside the workspace root.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }

    /// Make a path relative to the workspace root for display purposes.
    #[must_use]
    pub fn relative(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.root).unwrap_or(path).to_path_buf()
    }

    /// Resolve `user_path` relative to the workspace root, guarding against
    /// path traversal and confinement attacks:
    ///
    /// - **Absolute paths** — Rust's `PathBuf::join` silently replaces the
    ///   base when the right-hand side is absolute.  We reject them early.
    /// - **`..` escape sequences** — normalised without touching the
    ///   filesystem (so new-file targets for CHANGE work), then checked to
    ///   still start with the worktree root.
    /// - **Protected internals** — a root-level `.git` or `.forgeql*` entry is
    ///   denied so the repo's git store and `ForgeQL`'s own runtime/control files
    ///   are never readable or writable through a query.
    /// - **Symlink escapes** — the deepest existing ancestor is canonicalised
    ///   and verified to stay inside the canonical root, so a symlinked
    ///   directory inside the worktree cannot point out of it.
    ///
    /// # Errors
    /// Returns an error if `user_path` is absolute, escapes the root (lexically
    /// or via a symlink), or targets a protected internal path.
    pub fn safe_path(&self, user_path: &str) -> anyhow::Result<std::path::PathBuf> {
        let p = std::path::Path::new(user_path);
        if p.is_absolute() {
            anyhow::bail!(
                "path '{user_path}' is absolute; all paths must be relative to the worktree"
            );
        }
        let joined = self.root.join(p);
        let normalised = normalise_path(&joined);
        if !normalised.starts_with(&self.root) {
            anyhow::bail!("path '{user_path}' escapes the worktree root");
        }

        // Denylist: never expose the repo's own `.git` directory or ForgeQL's
        // runtime/control files (`.forgeql*`), even though they live inside the
        // worktree root. Only the first (root-level) component is checked — that
        // is where these protected entries live.
        if let Ok(rel) = normalised.strip_prefix(&self.root)
            && let Some(std::path::Component::Normal(first)) = rel.components().next()
        {
            let name = first.to_string_lossy();
            if name == ".git" || name.starts_with(".forgeql") {
                anyhow::bail!("path '{user_path}' targets a protected internal path ('{name}')");
            }
        }

        // Symlink-safe containment: lexical normalisation does not follow
        // symlinks, so a symlinked directory inside the worktree could point out
        // of it. Canonicalize the deepest existing ancestor (the target itself
        // may not exist yet, e.g. a new-file CHANGE) and verify it is still
        // inside the canonical root. Skipped when the root cannot be
        // canonicalised (e.g. a virtual root in unit tests).
        if let Ok(canon_root) = std::fs::canonicalize(&self.root) {
            let mut ancestor = normalised.as_path();
            let canon_ancestor = loop {
                match std::fs::canonicalize(ancestor) {
                    Ok(c) => break Some(c),
                    Err(_) => match ancestor.parent() {
                        Some(parent) => ancestor = parent,
                        None => break None,
                    },
                }
            };
            if let Some(canon_ancestor) = canon_ancestor
                && !canon_ancestor.starts_with(&canon_root)
            {
                anyhow::bail!("path '{user_path}' escapes the worktree root via a symlink");
            }
        }

        Ok(normalised)
    }

    /// Return `true` when the workspace has no working directory — i.e. it is
    /// a bare clone or a worktree whose checked-out files are not present on
    /// disk.
    ///
    /// Detection: a normal working tree always has a `.git` entry (file or
    /// directory) at its root; a bare repository has neither.
    ///
    /// Used by the SHOW path to decide whether a git blob fallback is needed
    /// when `file_io::read_bytes` fails.
    #[must_use]
    pub fn is_bare(&self) -> bool {
        !self.root.join(".git").exists() && !self.root.is_file()
    }

    /// Read the content of a git blob by its 20-byte SHA-1 object ID.
    ///
    /// Discovers the git repository via `git2::Repository::discover` starting
    /// from `self.root`.  Works for both normal and bare repositories.
    ///
    /// # Errors
    /// Returns `Err` if the repository cannot be opened, the OID is invalid,
    /// or no blob with that SHA-1 exists in the object store.
    pub fn read_blob_by_sha(&self, sha: &[u8; 20]) -> Result<Vec<u8>> {
        let repo = git2::Repository::discover(&self.root)
            .context("cannot open git repo for blob fallback")?;
        let oid = git2::Oid::from_bytes(sha).context("invalid blob SHA")?;
        let blob = repo.find_blob(oid).context("blob not found in git")?;
        Ok(blob.content().to_vec())
    }
}

/// Collapse `.` and `..` components without touching the filesystem.
///
/// `std::fs::canonicalize` would also work for *existing* paths but fails on
/// paths that do not yet exist (e.g. new-file targets in CHANGE commands).
fn normalise_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut parts: Vec<Component> = Vec::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = parts.pop();
            }
            c => parts.push(c),
        }
    }
    parts.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::normalise_path;

    fn make_ws() -> super::Workspace {
        // Bypasses the `is_dir` check — unit-testing path logic only.
        super::Workspace {
            root: PathBuf::from("/worktree"),
        }
    }

    #[test]
    fn normalise_dot_segments() {
        let result = normalise_path(&PathBuf::from("/a/./b/../c"));
        assert_eq!(result, PathBuf::from("/a/c"));
    }

    #[test]
    fn safe_path_relative_ok() {
        let p = make_ws().safe_path("src/main.rs").unwrap();
        assert_eq!(p, PathBuf::from("/worktree/src/main.rs"));
    }

    #[test]
    fn safe_path_dot_dot_within_root_ok() {
        let p = make_ws().safe_path("src/../lib.rs").unwrap();
        assert_eq!(p, PathBuf::from("/worktree/lib.rs"));
    }

    #[test]
    fn safe_path_absolute_rejected() {
        let err = make_ws().safe_path("/etc/passwd").unwrap_err();
        assert!(err.to_string().contains("absolute"), "got: {err}");
    }

    #[test]
    fn safe_path_dot_dot_escape_rejected() {
        let err = make_ws().safe_path("../../etc/passwd").unwrap_err();
        assert!(err.to_string().contains("escapes"), "got: {err}");
    }

    #[test]
    fn safe_path_sibling_escape_rejected() {
        let err = make_ws().safe_path("../other/file.rs").unwrap_err();
        assert!(err.to_string().contains("escapes"), "got: {err}");
    }

    #[test]
    fn safe_path_rejects_dot_git() {
        let err = make_ws().safe_path(".git/config").unwrap_err();
        assert!(err.to_string().contains("protected"), "got: {err}");
    }

    #[test]
    fn safe_path_rejects_forgeql_runtime_files() {
        for p in [".forgeql.yaml", ".forgeql-showmore-0", ".forgeql-index"] {
            let err = make_ws().safe_path(p).unwrap_err();
            assert!(
                err.to_string().contains("protected"),
                "{p} must be denied: {err}"
            );
        }
    }

    #[test]
    fn safe_path_allows_gitignore_not_dot_git() {
        // `.gitignore` is a normal file; only the `.git` directory is denied.
        let p = make_ws().safe_path(".gitignore").unwrap();
        assert_eq!(p, PathBuf::from("/worktree/.gitignore"));
    }

    #[test]
    fn safe_path_dot_dot_into_dot_git_rejected() {
        // `..` tricks that resolve back to the root-level `.git` are still denied.
        let err = make_ws().safe_path("src/../.git/config").unwrap_err();
        assert!(err.to_string().contains("protected"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn safe_path_rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("wt");
        std::fs::create_dir(&root).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        // A symlink inside the worktree pointing outside it.
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let ws = super::Workspace { root };
        let err = ws.safe_path("link/secret.txt").unwrap_err();
        assert!(
            err.to_string().contains("symlink") || err.to_string().contains("escapes"),
            "symlinked path must be rejected: {err}"
        );
    }
}
