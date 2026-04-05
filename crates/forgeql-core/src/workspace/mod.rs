pub mod file_io;

use std::path::{Path, PathBuf};

use anyhow::Result;
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
    /// path traversal attacks:
    ///
    /// - **Absolute paths** — Rust's `PathBuf::join` silently replaces the
    ///   base when the right-hand side is absolute.  We reject them early.
    /// - **`..` escape sequences** — normalised without touching the
    ///   filesystem (so new-file targets for CHANGE work), then checked to
    ///   still start with the worktree root.
    ///
    /// # Errors
    /// Returns an error if `user_path` is absolute or escapes the root.
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
        Ok(normalised)
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
}
