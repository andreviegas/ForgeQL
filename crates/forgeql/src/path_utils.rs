/// Path resolution utilities for `--data-dir`.
///
/// Handles tilde expansion and `..`/`.` normalization across Linux, macOS
/// and Windows without requiring the target path to exist on disk.
use std::path::{Component, Path, PathBuf};

/// Expand a leading `~` and lexically normalize `.`/`..` components.
///
/// Works identically on Linux, macOS, and Windows:
///   - Uses [`dirs::home_dir()`] for home-directory resolution (handles
///     `$HOME`, `USERPROFILE`, `FOLDERID_Profile`).
///   - Lexical normalization means no `stat(2)` calls — safe to call before
///     the directory is created.
///
/// # Examples
/// ```
/// // ~/forgeql-data           → /home/user/forgeql-data
/// // ~/../../datadir/test     → /datadir/test
/// // ~/./subdir/../data       → /home/user/data
/// // /absolute/path           → /absolute/path
/// // C:\Users\user\data       → C:\Users\user\data  (Windows)
/// ```
pub(crate) fn resolve_data_dir(path: &Path) -> PathBuf {
    normalize_lexically(&expand_tilde(path))
}

/// Replace a leading `~` component with the platform home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    path.strip_prefix("~").map_or_else(
        |_| path.to_path_buf(),
        |rest| {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
            home.join(rest)
        },
    )
}

/// Resolve `.` and `..` components without touching the filesystem.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {} // drop `.`
            Component::ParentDir => {
                let _ = out.pop();
            } // resolve `..`
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
    }

    #[test]
    fn test_bare_tilde() {
        assert_eq!(resolve_data_dir(Path::new("~")), home());
    }

    #[test]
    fn test_tilde_subdir() {
        assert_eq!(
            resolve_data_dir(Path::new("~/forgeql-data")),
            home().join("forgeql-data")
        );
    }

    #[test]
    fn test_tilde_dot_normalization() {
        assert_eq!(
            resolve_data_dir(Path::new("~/./subdir/../data")),
            home().join("data")
        );
    }

    #[test]
    fn test_tilde_escape_traversal() {
        // `~/../../datadir/test` must be absolute and contain no `..`
        let result = resolve_data_dir(Path::new("~/../../datadir/test"));
        assert!(result.is_absolute());
        assert!(!result.components().any(|c| c == Component::ParentDir));
    }

    #[test]
    fn test_absolute_path_unchanged() {
        let p = Path::new("/tmp/forgeql-workspace");
        assert_eq!(resolve_data_dir(p), p);
    }
}
