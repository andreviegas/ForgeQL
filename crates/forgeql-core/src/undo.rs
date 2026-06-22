//! Per-session UNDO ring: snapshots of the pre-edit bytes of every file a
//! mutation touched, so `UNDO` can restore the previous state mechanically.
//!
//! Every mutation already produces `transforms::TransformResult { originals }`
//! — the raw bytes of each modified file before the edit, which `apply()`
//! captures specifically so a write can be reversed. Instead of dropping it,
//! the engine writes it here as the most-recent ring slot. `UNDO [LAST-n]`
//! reads slot `n` back and rewrites those bytes: restoring `LAST-0` reverses the
//! most recent mutation, `LAST-1` reverses the two most recent, and so on.
//!
//! # Storage and lifecycle
//!
//! Each slot is a single file `.forgeql-undo-<n>` in the session worktree root,
//! beside the SHOW MORE ring and the columnar delta. Like them it is excluded
//! from user-facing commits (`git::is_clean_commit_excluded`, by prefix) and is
//! denied to user paths by `Workspace::safe_path` (the `.forgeql*` denylist), so
//! it never reaches published history and cannot be targeted by a query. It dies
//! with the worktree (TTL GC), giving the right lifetime for free.
//!
//! # Format (length-prefixed, lossless, binary-safe)
//!
//! The header line is `FQLUNDO<TAB>v1<TAB><op>`, then one record per file: a
//! `<path_len><TAB><byte_len>` line followed by the worktree-relative path bytes
//! and then the content bytes. Paths are stored relative to the worktree root so
//! the ring is relocatable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Filename prefix of the per-session UNDO ring slots, written in the worktree
/// root as `<prefix>-<n>` (`LAST-<n>`, 0 = most recent).
pub const UNDO_FILE_NAME: &str = ".forgeql-undo";

/// Depth of the `LAST-n` ring: slots `LAST-0` .. `LAST-<RING_SIZE-1>`.
pub const RING_SIZE: usize = 10;

const HEADER_MARKER: &str = "FQLUNDO\tv1";

/// One restorable file: its path relative to the worktree root and the bytes to
/// write back.
pub struct UndoFile {
    /// Path relative to the worktree root.
    pub rel_path: PathBuf,
    /// Pre-edit bytes to restore.
    pub bytes: Vec<u8>,
}

/// A decoded snapshot: the op that produced it and the files it can restore.
pub struct Snapshot {
    /// Name of the mutation op whose pre-edit state this slot holds.
    pub op: String,
    /// The files this slot can restore.
    pub files: Vec<UndoFile>,
}

/// Absolute path of ring slot `n` for a session worktree.
#[must_use]
pub fn slot_path(worktree_root: &Path, n: usize) -> PathBuf {
    worktree_root.join(format!("{UNDO_FILE_NAME}-{n}"))
}

/// Shift every ring slot up by one (dropping the oldest) so a fresh `LAST-0` can
/// be written. Best-effort: a missing slot is skipped.
fn rotate_ring(worktree_root: &Path) {
    let _ = std::fs::remove_file(slot_path(worktree_root, RING_SIZE - 1));
    for n in (0..RING_SIZE - 1).rev() {
        let from = slot_path(worktree_root, n);
        if from.exists() {
            let _ = std::fs::rename(&from, slot_path(worktree_root, n + 1));
        }
    }
}

/// Encode a snapshot of `originals` as the session's most recent UNDO slot.
///
/// `originals` maps absolute paths to their pre-edit bytes; paths outside
/// `worktree_root` are skipped, and an empty map is a no-op.
///
/// # Errors
/// Returns the underlying I/O error if the slot file cannot be written.
pub fn write_snapshot<S: std::hash::BuildHasher>(
    worktree_root: &Path,
    op: &str,
    originals: &HashMap<PathBuf, Vec<u8>, S>,
) -> std::io::Result<()> {
    if originals.is_empty() {
        return Ok(());
    }
    rotate_ring(worktree_root);

    let safe_op = op.replace(['\n', '\r', '\t'], " ");
    let mut buf = format!("{HEADER_MARKER}\t{safe_op}\n").into_bytes();

    for (abs_path, bytes) in originals {
        let Ok(rel) = abs_path.strip_prefix(worktree_root) else {
            continue;
        };
        let rel_str = rel.to_string_lossy();
        let rel_b = rel_str.as_bytes();
        buf.extend_from_slice(format!("{}\t{}\n", rel_b.len(), bytes.len()).as_bytes());
        buf.extend_from_slice(rel_b);
        buf.extend_from_slice(bytes);
    }

    std::fs::write(slot_path(worktree_root, 0), buf)
}

/// Decode ring slot `n`, if present and well-formed.
///
/// Returns `Ok(None)` when the slot file is absent or lacks the header marker
/// (treated as "no snapshot" rather than an error).
///
/// # Errors
/// Returns the underlying I/O error for failures other than a missing file.
pub fn read_snapshot(worktree_root: &Path, n: usize) -> std::io::Result<Option<Snapshot>> {
    let raw = match std::fs::read(slot_path(worktree_root, n)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };

    let Some(nl) = raw.iter().position(|&b| b == b'\n') else {
        return Ok(None);
    };
    let header = String::from_utf8_lossy(&raw[..nl]);
    let mut hparts = header.split('\t');
    if hparts.next() != Some("FQLUNDO") || hparts.next() != Some("v1") {
        return Ok(None);
    }
    let op = hparts.next().unwrap_or("").to_string();

    let mut files = Vec::new();
    let mut pos = nl + 1;
    while pos < raw.len() {
        let Some(rel_nl) = raw[pos..].iter().position(|&b| b == b'\n') else {
            break;
        };
        let line = String::from_utf8_lossy(&raw[pos..pos + rel_nl]);
        let mut lp = line.split('\t');
        let (Some(pl), Some(bl)) = (lp.next(), lp.next()) else {
            break;
        };
        let (Ok(path_len), Ok(byte_len)) = (pl.parse::<usize>(), bl.parse::<usize>()) else {
            break;
        };
        pos += rel_nl + 1;
        if pos + path_len + byte_len > raw.len() {
            break;
        }
        let rel_path =
            PathBuf::from(String::from_utf8_lossy(&raw[pos..pos + path_len]).into_owned());
        pos += path_len;
        let bytes = raw[pos..pos + byte_len].to_vec();
        pos += byte_len;
        files.push(UndoFile { rel_path, bytes });
    }

    Ok(Some(Snapshot { op, files }))
}

#[cfg(test)]
mod tests {
    use super::*;

    static TMP_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn tmp_dir() -> PathBuf {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("fql-undo-test-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        tmp
    }

    #[test]
    fn snapshot_roundtrips_paths_and_bytes() {
        let root = tmp_dir();
        let mut originals: HashMap<PathBuf, Vec<u8>> = HashMap::new();
        drop(originals.insert(root.join("src/a.rs"), b"fn a() {}\n".to_vec()));
        drop(originals.insert(root.join("b.json"), b"{}".to_vec()));
        write_snapshot(&root, "change_node", &originals).unwrap();

        let snap = read_snapshot(&root, 0).unwrap().expect("snapshot present");
        assert_eq!(snap.op, "change_node");
        assert_eq!(snap.files.len(), 2);
        let mut got: HashMap<PathBuf, Vec<u8>> = HashMap::new();
        for f in snap.files {
            drop(got.insert(f.rel_path, f.bytes));
        }
        assert_eq!(
            got.get(Path::new("src/a.rs")).map(Vec::as_slice),
            Some(b"fn a() {}\n".as_slice())
        );
        assert_eq!(
            got.get(Path::new("b.json")).map(Vec::as_slice),
            Some(b"{}".as_slice())
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ring_pages_previous_snapshots_as_last_n() {
        let root = tmp_dir();
        for i in 0..3u8 {
            let mut originals: HashMap<PathBuf, Vec<u8>> = HashMap::new();
            drop(originals.insert(root.join("f.rs"), vec![i]));
            write_snapshot(&root, "op", &originals).unwrap();
        }
        for (slot, expected) in [(0usize, 2u8), (1, 1), (2, 0)] {
            let snap = read_snapshot(&root, slot).unwrap().unwrap();
            assert_eq!(snap.files[0].bytes, vec![expected]);
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_slot_is_none() {
        let root = tmp_dir();
        assert!(read_snapshot(&root, 0).unwrap().is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn empty_originals_writes_nothing() {
        let root = tmp_dir();
        let originals: HashMap<PathBuf, Vec<u8>> = HashMap::new();
        write_snapshot(&root, "op", &originals).unwrap();
        assert!(read_snapshot(&root, 0).unwrap().is_none());
        std::fs::remove_dir_all(&root).ok();
    }
}
