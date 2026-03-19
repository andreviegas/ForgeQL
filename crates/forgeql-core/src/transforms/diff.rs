//! Unified diff generation for [`TransformPlan`]s.
//!
//! Applies a [`FileEdit`] to an in-memory buffer (no disk writes) and
//! generates a standard unified diff (`--- a/…`, `+++ b/…`, `@@ … @@`
//! hunks) suitable for human review.
//!
//! The diff algorithm is a line-level LCS (O(m·n)) which is acceptable
//! for typical source files.  Very large files (product m·n > 4 000 000)
//! fall back to a simple "replace everything" representation — still correct,
//! just without common-context compression.
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;

use super::{ByteRangeEdit, FileEdit, TransformPlan};
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
fn apply_in_memory(original: &[u8], edits: &[ByteRangeEdit]) -> Vec<u8> {
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

/// Number of context lines around each change block.
const CONTEXT: usize = 3;

/// Format a unified diff between `old` and `new` for `path`.
fn unified_diff(old: &str, new: &str, path: &Path) -> String {
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

// -----------------------------------------------------------------------
// LCS — line-level longest common subsequence
// -----------------------------------------------------------------------

/// A contiguous block that differs between `old` and `new`.
struct ChangeRange {
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
}

/// Compute the diff as a list of [`ChangeRange`]s via line-level LCS.
fn change_ranges(old: &[&str], new: &[&str]) -> Vec<ChangeRange> {
    let matches = lcs_matches(old, new);
    gaps_from_matches(old.len(), new.len(), &matches)
}

/// Return `(old_idx, new_idx)` pairs of matching lines (the LCS).
///
/// Complexity: O(m·n) time and space.  For very large files the function
/// returns an empty vec (→ whole-file replacement diff).
fn lcs_matches(old: &[&str], new: &[&str]) -> Vec<(usize, usize)> {
    /// Maximum O(m·n) cell count before falling back to whole-file diff.
    const MYERS_CELL_CAP: usize = 4_000_000;

    let m = old.len();
    let n = new.len();

    // Guard against pathological inputs (O(m·n) space).
    if m.saturating_mul(n) > MYERS_CELL_CAP {
        return Vec::new();
    }

    // dp[i][j] = LCS length of old[i..] and new[j..]
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            dp[i][j] = if old[i] == new[j] {
                dp[i + 1][j + 1].saturating_add(1)
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    // Trace back the LCS.
    let mut result = Vec::new();
    let (mut i, mut j) = (0_usize, 0_usize);
    while i < m && j < n {
        if old[i] == new[j] {
            result.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    result
}

/// Convert LCS matches into [`ChangeRange`]s (the gaps between matches).
fn gaps_from_matches(
    old_len: usize,
    new_len: usize,
    matches: &[(usize, usize)],
) -> Vec<ChangeRange> {
    let mut ranges = Vec::new();
    let mut prev_old = 0_usize;
    let mut prev_new = 0_usize;

    for &(oi, ni) in matches {
        if oi > prev_old || ni > prev_new {
            ranges.push(ChangeRange {
                old_start: prev_old,
                old_end: oi,
                new_start: prev_new,
                new_end: ni,
            });
        }
        prev_old = oi + 1;
        prev_new = ni + 1;
    }

    if prev_old < old_len || prev_new < new_len {
        ranges.push(ChangeRange {
            old_start: prev_old,
            old_end: old_len,
            new_start: prev_new,
            new_end: new_len,
        });
    }
    ranges
}

// -----------------------------------------------------------------------
// Hunk rendering
// -----------------------------------------------------------------------

/// Merge nearby [`ChangeRange`]s into unified diff hunk strings.
fn build_hunks(old: &[&str], new: &[&str], ranges: &[ChangeRange]) -> Vec<String> {
    if ranges.is_empty() {
        return Vec::new();
    }

    let mut hunks = Vec::new();
    // Each "group" is a slice of ChangeRanges that belong to the same hunk.
    let mut group_start = 0_usize;

    for i in 1..=ranges.len() {
        let last = &ranges[i - 1];
        let flush = i == ranges.len() || {
            let next = &ranges[i];
            next.old_start.saturating_sub(last.old_end) > CONTEXT * 2
        };

        if flush {
            hunks.push(render_hunk(old, new, &ranges[group_start..i]));
            group_start = i;
        }
    }
    hunks
}

/// Format one hunk from a slice of [`ChangeRange`]s.
fn render_hunk(old: &[&str], new: &[&str], group: &[ChangeRange]) -> String {
    let first = &group[0];
    let last = &group[group.len() - 1];

    // Compute old/new start lines with context (1-based for the @@ header).
    let old_ctx_start = first.old_start.saturating_sub(CONTEXT);
    let new_ctx_start = first.new_start.saturating_sub(CONTEXT);

    let old_ctx_end = (last.old_end + CONTEXT).min(old.len());
    let new_ctx_end = (last.new_end + CONTEXT).min(new.len());

    let old_count = old_ctx_end - old_ctx_start;
    let new_count = new_ctx_end - new_ctx_start;

    let mut hunk = String::new();
    let _ = writeln!(
        hunk,
        "@@ -{},{} +{},{} @@",
        old_ctx_start + 1,
        old_count,
        new_ctx_start + 1,
        new_count,
    );

    // Walk through the context/change spans and emit +/- lines.
    let mut oi = old_ctx_start;
    let mut ni = new_ctx_start;

    for cr in group {
        // Context lines before this change range.
        while oi < cr.old_start && ni < cr.new_start {
            let line = old.get(oi).copied().unwrap_or("");
            let _ = writeln!(hunk, " {line}");
            oi += 1;
            ni += 1;
        }
        // Removed lines (only in old).
        for idx in cr.old_start..cr.old_end {
            let line = old.get(idx).copied().unwrap_or("");
            let _ = writeln!(hunk, "-{line}");
        }
        // Added lines (only in new).
        for idx in cr.new_start..cr.new_end {
            let line = new.get(idx).copied().unwrap_or("");
            let _ = writeln!(hunk, "+{line}");
        }
        oi = cr.old_end;
        ni = cr.new_end;
    }

    // Trailing context lines.
    while oi < old_ctx_end && ni < new_ctx_end {
        let line = old.get(oi).copied().unwrap_or("");
        let _ = writeln!(hunk, " {line}");
        oi += 1;
        ni += 1;
    }

    hunk
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transforms::{ByteRangeEdit, FileEdit};

    // --- apply_in_memory --------------------------------------------------

    #[test]
    fn apply_in_memory_single_replacement() {
        let src = b"hello world";
        let edit = ByteRangeEdit::new(6..11, "Rust");
        let result = apply_in_memory(src, &[edit]);
        assert_eq!(result, b"hello Rust");
    }

    #[test]
    fn apply_in_memory_multiple_reverse_order() {
        // Two non-overlapping edits — must be applied in reverse order.
        let src = b"foo bar baz";
        let e1 = ByteRangeEdit::new(0..3, "ONE");
        let e2 = ByteRangeEdit::new(4..7, "TWO");
        let result = apply_in_memory(src, &[e1, e2]);
        assert_eq!(result, b"ONE TWO baz");
    }

    #[test]
    fn apply_in_memory_deletion() {
        // Remove "me" (bytes 7..9), leaving the surrounding spaces intact →
        // "remove  please" (two spaces where "me" was).
        let src = b"remove me please";
        let edit = ByteRangeEdit::new(7..9, "");
        let result = apply_in_memory(src, &[edit]);
        assert_eq!(result, b"remove  please");
    }

    // --- unified_diff -----------------------------------------------------

    #[test]
    fn diff_identical_content_is_empty() {
        let content = "line one\nline two\nline three\n";
        let d = unified_diff(content, content, Path::new("src/test.cpp"));
        assert!(d.is_empty(), "identical files must produce empty diff");
    }

    #[test]
    fn diff_single_line_change_contains_markers() {
        let old = "int foo() { return 1; }\n";
        let new = "int bar() { return 1; }\n";
        let d = unified_diff(old, new, Path::new("src/test.cpp"));
        assert!(d.contains("--- a/src/test.cpp"), "must have --- header");
        assert!(d.contains("+++ b/src/test.cpp"), "must have +++ header");
        assert!(d.contains("-int foo()"), "must show removed line");
        assert!(d.contains("+int bar()"), "must show added line");
    }

    #[test]
    fn diff_plan_reads_file_and_applies_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.cpp");
        std::fs::write(&path, "int acenderLuz() {}\n").unwrap();

        let fe = FileEdit {
            path,
            edits: vec![ByteRangeEdit::new(4..14, "turnOnLight")],
        };
        let plan = TransformPlan {
            file_edits: vec![fe],
            suggestions: vec![],
        };
        let d = diff_plan(&plan).unwrap();
        assert!(d.contains("-int acenderLuz()"), "diff must show old name");
        assert!(d.contains("+int turnOnLight()"), "diff must show new name");
    }

    #[test]
    fn lcs_matches_identical_sequences() {
        let lines = vec!["a", "b", "c"];
        let m = lcs_matches(&lines, &lines);
        assert_eq!(m, vec![(0, 0), (1, 1), (2, 2)]);
    }

    #[test]
    fn lcs_matches_completely_different() {
        let old = vec!["a", "b"];
        let new = vec!["x", "y"];
        let m = lcs_matches(&old, &new);
        assert!(m.is_empty());
    }
}
