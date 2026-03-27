//! COPY LINES and MOVE LINES transform planning.
//!
//! Produces a [`TransformPlan`] that can be applied by the engine just like
//! any other mutation — the caller gets a unified diff preview and the writes
//! happen via the same atomic file-I/O path.

use std::path::Path;

use anyhow::{Result, bail};

use crate::transforms::change::lines_to_byte_range;
use crate::transforms::{ByteRangeEdit, FileEdit, TransformPlan};
use crate::workspace::file_io::read_bytes;

// -----------------------------------------------------------------------
// Public entry points
// -----------------------------------------------------------------------

/// Plan `COPY LINES start-end OF src TO dst [AT LINE at]`.
///
/// Reads lines `start..=end` (1-based, inclusive) from `src` and inserts
/// them into `dst` before line `at` (also 1-based).  When `at` is `None`
/// the payload is appended at the end of the file.
///
/// `src` and `dst` may be the same path; the function handles that case.
///
/// # Errors
/// Returns `Err` if lines are out of range, files cannot be read, or the
/// content is not valid UTF-8.
pub fn plan_copy_lines(
    src_rel: &str,
    src_abs: &Path,
    start: usize,
    end: usize,
    dst_abs: &Path,
) -> Result<TransformPlan> {
    // ── read source ──────────────────────────────────────────────────────
    let src_bytes = read_bytes(src_abs)?;
    let payload = extract_payload(&src_bytes, src_rel, start, end)?;

    // ── read destination (may be the same file or a new file) ────────────
    let dst_bytes = if dst_abs == src_abs {
        src_bytes
    } else if dst_abs.exists() {
        read_bytes(dst_abs)?
    } else {
        Vec::new()
    };

    Ok(insertion_plan(dst_abs, &dst_bytes, None, payload))
}

/// Plan `COPY LINES start-end OF src TO dst AT LINE at`.
///
/// Same as [`plan_copy_lines`] but with an explicit insertion line.
///
/// # Errors
/// Returns `Err` if lines are out of range, `at` is 0, files cannot be read,
/// or the content is not valid UTF-8.
pub fn plan_copy_lines_at(
    src_rel: &str,
    src_abs: &Path,
    start: usize,
    end: usize,
    dst_abs: &Path,
    at: usize,
) -> Result<TransformPlan> {
    if at == 0 {
        bail!("AT LINE is 1-based, got 0");
    }

    let src_bytes = read_bytes(src_abs)?;
    let payload = extract_payload(&src_bytes, src_rel, start, end)?;

    let dst_bytes = if dst_abs == src_abs {
        src_bytes
    } else if dst_abs.exists() {
        read_bytes(dst_abs)?
    } else {
        Vec::new()
    };

    Ok(insertion_plan(dst_abs, &dst_bytes, Some(at), payload))
}

/// Plan `MOVE LINES start-end OF src TO dst [AT LINE at]`.
///
/// Like COPY but also deletes the source lines (`start..=end` in `src`).
/// When `src == dst` (same-file move), both the insertion and deletion are
/// expressed as separate [`ByteRangeEdit`]s on the same file.
/// [`TransformPlan::apply`] applies them in reverse byte order, which
/// ensures correct results regardless of whether the move is up or down.
///
/// # Errors
/// Returns `Err` if lines are out of range, the content is not valid UTF-8,
/// or (for same-file moves) the destination line falls inside the moved range.
pub fn plan_move_lines(
    src_rel: &str,
    src_abs: &Path,
    start: usize,
    end: usize,
    dst_abs: &Path,
    at: Option<usize>,
) -> Result<TransformPlan> {
    if at == Some(0) {
        bail!("AT LINE is 1-based, got 0");
    }

    // ── read source ──────────────────────────────────────────────────────
    let src_bytes = read_bytes(src_abs)?;
    let payload = extract_payload(&src_bytes, src_rel, start, end)?;
    let (del_start, del_end) = lines_to_byte_range(&src_bytes, start, end)?;

    let same_file = src_abs == dst_abs;

    // ── compute insertion byte offset in dst ─────────────────────────────
    // For same-file moves we reuse src_bytes (no clone needed).
    // For cross-file moves we read dst only to locate the insertion point.
    let ins_byte = if same_file {
        insertion_byte_offset(&src_bytes, at)
    } else if dst_abs.exists() {
        let dst_bytes = read_bytes(dst_abs)?;
        insertion_byte_offset(&dst_bytes, at)
    } else {
        insertion_byte_offset(&[], at)
    };
    // Guard: for same-file moves the insertion point must not lie inside
    // the deleted range (that would be logically contradictory).
    if same_file && ins_byte > del_start && ins_byte < del_end {
        bail!(
            "AT LINE cannot point inside the moved range ({start}..={end}); \
             choose a line before {start} or after {end}"
        );
    }

    // ── build plan ───────────────────────────────────────────────────────
    let mut plan = TransformPlan::default();

    // Insertion edit (zero-length range = pure insert).
    plan.file_edits.push(FileEdit {
        path: dst_abs.to_path_buf(),
        edits: vec![ByteRangeEdit::new(ins_byte..ins_byte, payload)],
    });

    // Deletion edit.  For same-file moves this is a second FileEdit on the
    // same path; merge_by_file() will combine them and sort by descending
    // byte offset before apply(), which makes the operation self-consistent
    // regardless of move direction (up or down).
    plan.file_edits.push(FileEdit {
        path: src_abs.to_path_buf(),
        edits: vec![ByteRangeEdit::new(del_start..del_end, "")],
    });

    Ok(plan)
}

// -----------------------------------------------------------------------
// Private helpers
// -----------------------------------------------------------------------

/// Extract lines `start..=end` from `bytes` as a UTF-8 `String`.
///
/// Ensures the payload ends with a newline so the inserted block doesn't
/// merge with the following line.
fn extract_payload(bytes: &[u8], rel_path: &str, start: usize, end: usize) -> Result<String> {
    let (bs, be) = lines_to_byte_range(bytes, start, end)?;
    let raw = &bytes[bs..be];
    let text = std::str::from_utf8(raw)
        .map_err(|e| anyhow::anyhow!("{rel_path}: lines {start}-{end} are not valid UTF-8: {e}"))?
        .to_string();
    // lines_to_byte_range already includes the trailing newline when present;
    // ensure it is there even for the last line of a file missing a final newline.
    if text.ends_with('\n') {
        Ok(text)
    } else {
        Ok(format!("{text}\n"))
    }
}

/// Build a [`TransformPlan`] that inserts `payload` into `dst`.
fn insertion_plan(
    dst: &Path,
    dst_bytes: &[u8],
    at: Option<usize>,
    payload: String,
) -> TransformPlan {
    let ins_byte = insertion_byte_offset(dst_bytes, at);
    TransformPlan {
        file_edits: vec![FileEdit {
            path: dst.to_path_buf(),
            edits: vec![ByteRangeEdit::new(ins_byte..ins_byte, payload)],
        }],
        suggestions: Vec::new(),
    }
}

/// Return the byte offset at which to insert in `bytes` for a given target line.
///
/// - `None` → append at end of file (`bytes.len()`).
/// - `Some(k)` → start of line `k` (1-based).  If `k` is beyond the last
///   line, falls back to end of file.
fn insertion_byte_offset(bytes: &[u8], at: Option<usize>) -> usize {
    let Some(k) = at else {
        return bytes.len();
    };

    if k == 1 {
        return 0;
    }

    let mut current_line = 1usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            current_line += 1;
            if current_line == k {
                return i + 1; // byte just after the newline = start of line k
            }
        }
    }

    // k is beyond the last line → append at EOF
    bytes.len()
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::transforms::apply_edits_to_buffer;

    fn apply_plan_to_bytes(mut plan: TransformPlan, original: &[u8]) -> Vec<u8> {
        plan.merge_by_file().expect("merge");
        let fe = plan.file_edits.into_iter().next().unwrap();
        let mut buf = original.to_vec();
        apply_edits_to_buffer(&mut buf, &fe.edits);
        buf
    }

    // Helper: synthetic file bytes for "line1\nline2\n...lineN\n"
    fn make_file(n: usize) -> Vec<u8> {
        use std::fmt::Write;
        (1..=n)
            .fold(String::new(), |mut s, i| {
                writeln!(s, "line{i}").unwrap();
                s
            })
            .into_bytes()
    }

    #[test]
    fn copy_appends_when_no_at() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, make_file(5)).unwrap();
        let dst = tmp.path().join("dst.txt");
        std::fs::write(&dst, make_file(3)).unwrap();

        let plan = plan_copy_lines("src.txt", &src, 2, 3, &dst).unwrap();
        let original = std::fs::read(&dst).unwrap();
        let result = apply_plan_to_bytes(plan, &original);

        // dst had lines 1-3; copy appends lines 2-3 from src → lines 1-3, line2, line3
        let expected = b"line1\nline2\nline3\nline2\nline3\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn copy_at_line_inserts_before_target() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, make_file(5)).unwrap();
        let dst = tmp.path().join("dst.txt");
        std::fs::write(&dst, make_file(4)).unwrap();

        // Copy lines 4-5 of src before line 2 of dst
        let plan = plan_copy_lines_at("src.txt", &src, 4, 5, &dst, 2).unwrap();
        let original = std::fs::read(&dst).unwrap();
        let result = apply_plan_to_bytes(plan, &original);

        let expected = b"line1\nline4\nline5\nline2\nline3\nline4\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn move_different_files_removes_source_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, make_file(5)).unwrap();
        let dst = tmp.path().join("dst.txt");
        std::fs::write(&dst, make_file(3)).unwrap();

        let mut plan = plan_move_lines("src.txt", &src, 2, 4, &dst, None).unwrap();
        plan.merge_by_file().unwrap();

        // Apply each file's edits separately (apply() writes to disk; test manually)
        for fe in &mut plan.file_edits {
            fe.sort_reverse();
        }
        // Find dst edit and src edit
        let (src_edit, dst_edit): (Vec<_>, Vec<_>) = plan
            .file_edits
            .into_iter()
            .partition(|fe| fe.path == src.as_path());

        // Apply dst
        let mut dst_buf = std::fs::read(&dst).unwrap();
        if let Some(fe) = dst_edit.into_iter().next() {
            apply_edits_to_buffer(&mut dst_buf, &fe.edits);
        }
        // Apply src
        let mut src_buf = std::fs::read(&src).unwrap();
        if let Some(fe) = src_edit.into_iter().next() {
            apply_edits_to_buffer(&mut src_buf, &fe.edits);
        }

        assert_eq!(dst_buf, b"line1\nline2\nline3\nline2\nline3\nline4\n");
        assert_eq!(src_buf, b"line1\nline5\n");
    }

    #[test]
    fn move_same_file_down() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f.txt");
        // 5 lines: line1..line5
        std::fs::write(&f, make_file(5)).unwrap();

        // Move lines 1-2 to AT LINE 5 (before original line5)
        let mut plan = plan_move_lines("f.txt", &f, 1, 2, &f, Some(5)).unwrap();
        plan.merge_by_file().unwrap();

        let fe = plan.file_edits.into_iter().next().unwrap();
        let mut buf = std::fs::read(&f).unwrap();
        apply_edits_to_buffer(&mut buf, &fe.edits);

        // Expected: line3, line4, line1, line2, line5
        assert_eq!(buf, b"line3\nline4\nline1\nline2\nline5\n");
    }

    #[test]
    fn move_same_file_up() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f.txt");
        std::fs::write(&f, make_file(5)).unwrap();

        // Move lines 4-5 to AT LINE 2 (before original line2)
        let mut plan = plan_move_lines("f.txt", &f, 4, 5, &f, Some(2)).unwrap();
        plan.merge_by_file().unwrap();

        let fe = plan.file_edits.into_iter().next().unwrap();
        let mut buf = std::fs::read(&f).unwrap();
        apply_edits_to_buffer(&mut buf, &fe.edits);

        // Expected: line1, line4, line5, line2, line3
        assert_eq!(buf, b"line1\nline4\nline5\nline2\nline3\n");
    }

    #[test]
    fn move_same_file_inside_range_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f.txt");
        std::fs::write(&f, make_file(5)).unwrap();

        // Trying to move lines 2-4 AT LINE 3 (inside the range) must fail
        let result = plan_move_lines("f.txt", &f, 2, 4, &f, Some(3));
        assert!(result.is_err());
    }

    #[test]
    fn insertion_byte_offset_append() {
        let bytes = b"a\nb\nc\n";
        assert_eq!(insertion_byte_offset(bytes, None), 6);
    }

    #[test]
    fn insertion_byte_offset_line1() {
        let bytes = b"a\nb\nc\n";
        assert_eq!(insertion_byte_offset(bytes, Some(1)), 0);
    }

    #[test]
    fn insertion_byte_offset_line3() {
        let bytes = b"a\nb\nc\n";
        // line 3 starts after "a\nb\n" = byte 4
        assert_eq!(insertion_byte_offset(bytes, Some(3)), 4);
    }

    #[test]
    fn insertion_byte_offset_beyond_eof() {
        let bytes = b"a\nb\n";
        // line 99 is beyond the file → return len
        assert_eq!(insertion_byte_offset(bytes, Some(99)), 4);
    }

    #[test]
    fn copy_to_nonexistent_dst_creates_content() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src.txt");
        std::fs::write(&src, make_file(3)).unwrap();
        let dst = PathBuf::from("/nonexistent/path/dst.txt"); // doesn't exist → empty

        // plan_copy_lines reads dst_abs.exists() → false → dst_bytes = []
        // We can't call read_bytes on non-existent, but the plan should work for
        // the insertion offset calculation (appends to empty = offset 0).
        let plan = plan_copy_lines("src.txt", &src, 1, 2, &dst).unwrap();
        let result = apply_plan_to_bytes(plan, b"");
        assert_eq!(result, b"line1\nline2\n");
    }
}
