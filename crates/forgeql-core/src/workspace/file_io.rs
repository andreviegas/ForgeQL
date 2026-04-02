/// Atomic file write and helper I/O utilities.
///
/// RULE: Tempfile and target MUST be on the same filesystem so that the
/// rename syscall is atomic on Linux/macOS. Always use
/// `tempfile::Builder::new().tempfile_in(target.parent())`, never
/// `tempfile::tempfile()` (which creates in `/tmp`, potentially a different mount).
use std::io::Write;
use std::path::Path;

use anyhow::Result;

use crate::error::ForgeError;

/// Write `contents` to `target` atomically.
///
/// Creates a tempfile in the same directory as `target`, writes all bytes,
/// then renames it into place. If the process crashes between write and
/// rename, the tempfile is cleaned up by the OS — the target is never
/// left in a partially-written state.
///
/// # Errors
/// Returns `Err` if the parent directory is missing, the tempfile cannot be
/// created or written, or the rename fails.
pub fn write_atomic(target: &Path, contents: &[u8]) -> Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| ForgeError::io(target, std::io::Error::other("no parent directory")))?;

    // Ensure parent directories exist for new-file creation.
    std::fs::create_dir_all(dir).map_err(|e| ForgeError::io(dir, e))?;

    // Create tempfile IN THE SAME DIRECTORY (same filesystem = atomic rename).
    let mut tmp = tempfile::Builder::new()
        .tempfile_in(dir)
        .map_err(|e| ForgeError::io(dir, e))?;

    tmp.write_all(contents)
        .map_err(|e| ForgeError::io(target, e))?;

    // `persist` calls rename(2) — atomic on Linux/macOS.
    // The returned File handle is intentionally discarded — we only care about atomicity.
    let _file = tmp.persist(target).map_err(|_| ForgeError::AtomicPersist {
        path: target.to_path_buf(),
    })?;

    Ok(())
}

/// Read the raw bytes of a file.
///
/// # Errors
/// Returns `Err` if the file cannot be opened or read.
pub fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| ForgeError::io(path, e).into())
}

/// Read the UTF-8 text of a file.
///
/// # Errors
/// Returns `Err` if the file cannot be opened, read, or decoded as UTF-8.
pub fn read_text(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| ForgeError::io(path, e).into())
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_then_read_bytes_roundtrip() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("test.cpp");
        let content = b"void foo() {}";
        write_atomic(&target, content).unwrap();
        assert_eq!(read_bytes(&target).unwrap(), content);
    }

    #[test]
    fn write_atomic_overwrites_existing_file() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("file.cpp");
        write_atomic(&target, b"version 1").unwrap();
        write_atomic(&target, b"version 2").unwrap();
        assert_eq!(read_bytes(&target).unwrap(), b"version 2");
    }

    #[test]
    fn write_atomic_creates_file_if_missing() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("new_file.cpp");
        assert!(!target.exists());
        write_atomic(&target, b"hello").unwrap();
        assert!(target.exists());
    }

    #[test]
    fn read_text_returns_utf8_content() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("text.cpp");
        write_atomic(&target, b"// comment").unwrap();
        assert_eq!(read_text(&target).unwrap(), "// comment");
    }

    #[test]
    fn read_bytes_missing_file_is_error() {
        let result = read_bytes(Path::new("/nonexistent/path/file.cpp"));
        assert!(result.is_err());
    }

    #[test]
    fn write_atomic_preserves_binary_content() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("binary.bin");
        let content: Vec<u8> = (0u8..=255).collect();
        write_atomic(&target, &content).unwrap();
        assert_eq!(read_bytes(&target).unwrap(), content);
    }
}
