//! Rendering a plan as a unified diff, and applying edits in memory.
//!
//! The entry points read each file a plan touches, apply that file's edits to
//! an in-memory copy, and render old-vs-new as a unified diff. Nothing here
//! writes to disk.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;

use super::lcs::{build_hunks, change_ranges};
use crate::transforms::{ByteRangeEdit, FileEdit, TransformPlan};
use crate::workspace::file_io;

// -----------------------------------------------------------------------
// Public API
// -----------------------------------------------------------------------

/// Generate a unified diff string for one [`FileEdit`].
///
/// Reads `fe.path` from disk, applies all edits in memory, and returns the
/// textual diff.  Returns an empty `String` when the file is unaffected.
///
/// # Errors
/// Returns `Err` if the source file cannot be read.
pub fn diff_file_edit(fe: &FileEdit) -> Result<String> {
    let original = file_io::read_bytes(&fe.path)?;
    let modified = apply_in_memory(&original, &fe.edits);

    let old_str = String::from_utf8_lossy(&original);
    let new_str = String::from_utf8_lossy(&modified);

    if old_str == new_str {
        return Ok(String::new());
    }
    Ok(unified_diff(&old_str, &new_str, &fe.path))
}

/// Generate a combined unified diff for **all** files in a [`TransformPlan`].
///
/// Files that are unaffected are silently skipped.  The output is a
/// concatenation of per-file diffs in the order they appear in `plan`.
///
/// # Errors
/// Stops at the first file that cannot be read.
pub fn diff_plan(plan: &TransformPlan) -> Result<String> {
    let mut out = String::new();
    for fe in &plan.file_edits {
        let d = diff_file_edit(fe)?;
        if !d.is_empty() {
            out.push_str(&d);
        }
    }
    Ok(out)
}

// -----------------------------------------------------------------------
// In-memory apply
// -----------------------------------------------------------------------

/// Apply `edits` to `original` bytes without writing any files.
///
/// Edits are sorted in reverse byte order before application — identical
/// to the on-disk `apply()` path — to prevent offset drift.
pub(super) fn apply_in_memory(original: &[u8], edits: &[ByteRangeEdit]) -> Vec<u8> {
    let mut sorted: Vec<&ByteRangeEdit> = edits.iter().collect();
    sorted.sort_by(|a, b| b.start.cmp(&a.start));

    let mut buf: Vec<u8> = original.to_vec();
    for edit in sorted {
        let start = edit.start.min(buf.len());
        let end = edit.end.min(buf.len());
        drop(buf.splice(start..end, edit.replacement.bytes()));
    }
    buf
}

// -----------------------------------------------------------------------
// Unified diff
// -----------------------------------------------------------------------

/// Format a unified diff between `old` and `new` for `path`.
pub(super) fn unified_diff(old: &str, new: &str, path: &Path) -> String {
    // Split on '\n'.  We intentionally keep the trailing empty string created
    // by a file ending with '\n' so line numbers stay consistent.
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();

    let ranges = change_ranges(&old_lines, &new_lines);
    if ranges.is_empty() {
        return String::new();
    }

    let path_str = path.display().to_string();
    let mut out = String::new();
    let _ = writeln!(out, "--- a/{path_str}");
    let _ = writeln!(out, "+++ b/{path_str}");

    for hunk in build_hunks(&old_lines, &new_lines, &ranges) {
        out.push_str(&hunk);
    }
    out
}
