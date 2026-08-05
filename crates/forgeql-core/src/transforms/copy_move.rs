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

/// Plan `MOVE LINES start-end OF src TO dst [AT LINE at]` — and the
/// node-addressed moves lowered onto it.
///
/// Like COPY but also deletes the source range. The payload is always
/// `start..=end`; the removed range is `start..=delete_end`. Whole-node moves
/// widen `delete_end` over the node's trailing blank separator (via
/// `absorb_trailing_blank_lines`) so the source file does not accumulate
/// blank lines — the same policy as `DELETE NODE`. The line-addressed
/// `MOVE LINES` verb passes `delete_end == end` and stays exact.
///
/// When `src == dst` (same-file move), both the insertion and deletion are
/// expressed as separate [`ByteRangeEdit`]s on the same file.
/// [`TransformPlan::apply`] applies them in reverse byte order, which
/// ensures correct results regardless of whether the move is up or down.
///
/// # Errors
/// Returns `Err` if lines are out of range, `delete_end < end`, the content
/// is not valid UTF-8, or (for same-file moves) the destination line falls
/// inside the removed range.
pub fn plan_move_lines(
    src_rel: &str,
    src_abs: &Path,
    start: usize,
    end: usize,
    delete_end: usize,
    dst_abs: &Path,
    at: Option<usize>,
) -> Result<TransformPlan> {
    if at == Some(0) {
        bail!("AT LINE is 1-based, got 0");
    }
    if delete_end < end {
        bail!("delete_end ({delete_end}) < end ({end}): the removed range must cover the payload");
    }

    // ── read source ──────────────────────────────────────────────────────
    let src_bytes = read_bytes(src_abs)?;
    let payload = extract_payload(&src_bytes, src_rel, start, end)?;
    let (del_start, del_end) = lines_to_byte_range(&src_bytes, start, delete_end)?;

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
    // the removed range (that would be logically contradictory).
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
        delete: false,
    });

    // Deletion edit.  For same-file moves this is a second FileEdit on the
    // same path; merge_by_file() will combine them and sort by descending
    // byte offset before apply(), which makes the operation self-consistent
    // regardless of move direction (up or down).
    plan.file_edits.push(FileEdit {
        path: src_abs.to_path_buf(),
        edits: vec![ByteRangeEdit::new(del_start..del_end, "")],
        delete: false,
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
            delete: false,
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
mod tests;
