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
}
