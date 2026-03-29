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
//!
//! ## Compact diff preview
//!
//! [`compact_diff_plan`] produces a token-bounded summary of each file's
//! changes.  Parameters live in [`CompactDiffConfig`] and can be overridden
//! at the call site or — in the future — via CLI flags / config file.
use std::borrow::Cow;
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
// Compact diff preview
// -----------------------------------------------------------------------

/// Tuneable parameters for the compact diff preview.
///
/// Defaults: K=14 content lines per file, W=40 chars per line, C=2
/// context-after lines.  These can be overridden at the call site or —
/// in the future — via CLI flags or `.forgeql.yaml`.
#[derive(Debug, Clone)]
pub struct CompactDiffConfig {
    /// Maximum *content* lines emitted per file (excluding the header).
    pub max_lines_per_file: usize,
    /// Maximum visible characters per line before truncation.
    pub max_line_width: usize,
    /// Number of unchanged context lines shown after the last changed line
    /// in each hunk (helps the agent detect merge errors).
    pub context_after: usize,
}

impl Default for CompactDiffConfig {
    fn default() -> Self {
        Self {
            max_lines_per_file: 14,
            max_line_width: 40,
            context_after: 2,
        }
    }
}

/// Produce a compact, token-bounded diff preview for all files in a plan.
///
/// Uses the known edit byte ranges to build a focused diff around just the
/// changed lines — no LCS required.  This is O(edits) regardless of file
/// size, fixing the problem where files exceeding the LCS cell cap
/// (4 000 000) fell back to a whole-file replacement diff that showed only
/// the file header/tail instead of the actual edited region.
///
/// # Errors
/// Returns `Err` if a source file cannot be read.
pub fn compact_diff_plan(plan: &TransformPlan, cfg: &CompactDiffConfig) -> Result<String> {
    let mut out = String::new();
    for fe in &plan.file_edits {
        let original = file_io::read_bytes(&fe.path)?;
        let modified = apply_in_memory(&original, &fe.edits);
        let old_str = String::from_utf8_lossy(&original);
        let new_str = String::from_utf8_lossy(&modified);
        if old_str == new_str {
            continue;
        }

        let old_lines: Vec<&str> = old_str.split('\n').collect();
        let new_lines: Vec<&str> = new_str.split('\n').collect();

        // Convert byte-range edits to line-level change ranges using the
        // known edit positions, then build the compact preview from those.
        let ranges = edit_based_change_ranges(&old_str, &new_str, &fe.edits);
        if ranges.is_empty() {
            continue;
        }

        let hunks = build_compact_hunks(&old_lines, &new_lines, &ranges, cfg);
        if hunks.is_empty() {
            continue;
        }

        let preview = render_compact_hunks(&fe.path, &hunks, cfg);
        if !preview.is_empty() {
            out.push_str(&preview);
        }
    }
    Ok(out)
}

/// Produce a compact preview of the diff between `old` and `new` for one file.
///
/// The output is bounded: at most `cfg.max_lines_per_file` content lines,
/// each truncated to `cfg.max_line_width` characters. Multi-hunk changes
/// show the first and last hunks with `…` elision in between.
///
/// NOTE: Uses LCS internally — only suitable for small files.  The production
/// path (`compact_diff_plan`) uses `edit_based_change_ranges` instead.
#[cfg(test)]
fn compact_diff_preview(old: &str, new: &str, path: &Path, cfg: &CompactDiffConfig) -> String {
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();

    let ranges = change_ranges(&old_lines, &new_lines);
    if ranges.is_empty() {
        return String::new();
    }

    // Build per-hunk display blocks (each hunk = changed lines + context-after).
    let hunks = build_compact_hunks(&old_lines, &new_lines, &ranges, cfg);
    if hunks.is_empty() {
        return String::new();
    }

    render_compact_hunks(path, &hunks, cfg)
}

/// Render pre-built compact hunks into the final display string with
/// line/hunk elision when the total exceeds the line budget.
fn render_compact_hunks(path: &Path, hunks: &[CompactHunk], cfg: &CompactDiffConfig) -> String {
    if hunks.is_empty() {
        return String::new();
    }

    let path_str = path.display();
    let mut out = format!("── {path_str} ──\n");

    let total_content_lines: usize = hunks.iter().map(|h| h.lines.len()).sum();

    if total_content_lines <= cfg.max_lines_per_file {
        // Everything fits — emit all hunks verbatim.
        for hunk in hunks {
            for line in &hunk.lines {
                let _ = writeln!(out, "{}", truncate_line(line, cfg.max_line_width));
            }
        }
    } else if hunks.len() == 1 {
        // Single oversized hunk: line-level head/tail elision.
        // Show first K/2 lines, elision marker, then last K/2 lines.
        let lines = &hunks[0].lines;
        let first_budget = cfg.max_lines_per_file / 2;
        let last_budget = cfg.max_lines_per_file - first_budget;
        let elided = lines.len().saturating_sub(first_budget + last_budget);

        for line in lines.iter().take(first_budget) {
            let _ = writeln!(out, "{}", truncate_line(line, cfg.max_line_width));
        }
        let _ = writeln!(out, "(\u{2026} {elided} lines elided \u{2026})");
        let skip = lines.len().saturating_sub(last_budget);
        for line in lines.iter().skip(skip) {
            let _ = writeln!(out, "{}", truncate_line(line, cfg.max_line_width));
        }
    } else {
        // Multiple oversized hunks: hunk-level head/tail elision.
        // Show first hunk head lines, elision marker, then last hunk tail lines.
        let first = &hunks[0];
        let last = &hunks[hunks.len() - 1];

        // Split the budget: half to the first hunk, half to the last.
        let first_budget = cfg.max_lines_per_file / 2;
        let last_budget = cfg.max_lines_per_file - first_budget;

        // First hunk (head lines).
        for line in first.lines.iter().take(first_budget) {
            let _ = writeln!(out, "{}", truncate_line(line, cfg.max_line_width));
        }

        // Elision marker.
        let elided_hunks = hunks.len() - 2;
        let elided_lines: usize = hunks[1..hunks.len() - 1]
            .iter()
            .map(|h| h.lines.len())
            .sum();
        let _ = writeln!(
            out,
            "(\u{2026} {elided_hunks} hunks, {elided_lines} lines elided \u{2026})"
        );

        // Last hunk (tail lines).
        let skip = last.lines.len().saturating_sub(last_budget);
        for line in last.lines.iter().skip(skip) {
            let _ = writeln!(out, "{}", truncate_line(line, cfg.max_line_width));
        }
    }
    out
}

/// A block of display lines for one hunk in the compact preview.
struct CompactHunk {
    lines: Vec<String>,
}

/// Build compact display hunks from change ranges.
///
/// Each hunk contains `-`/`+` lines for the change, plus up to
/// `cfg.context_after` unchanged lines after the last change.
fn build_compact_hunks(
    old: &[&str],
    new: &[&str],
    ranges: &[ChangeRange],
    cfg: &CompactDiffConfig,
) -> Vec<CompactHunk> {
    let mut hunks = Vec::new();

    for cr in ranges {
        let mut lines = Vec::new();

        // Removed lines.
        for idx in cr.old_start..cr.old_end {
            let text = old.get(idx).copied().unwrap_or("");
            lines.push(format!("-{text}"));
        }
        // Added lines.
        for idx in cr.new_start..cr.new_end {
            let text = new.get(idx).copied().unwrap_or("");
            lines.push(format!("+{text}"));
        }
        // Context-after: unchanged lines right after this change in the new file.
        let ctx_start = cr.new_end;
        let ctx_end = (ctx_start + cfg.context_after).min(new.len());
        for idx in ctx_start..ctx_end {
            let text = new.get(idx).copied().unwrap_or("");
            lines.push(format!(" {text}"));
        }

        hunks.push(CompactHunk { lines });
    }

    hunks
}

/// Convert byte-range edits to line-level [`ChangeRange`]s by mapping each
/// edit's byte span to old-file and new-file line numbers.
///
/// This is O(edits · log(lines)) and works on any file size, avoiding the
/// O(m·n) LCS entirely.  The resulting ranges can be passed directly to
/// [`build_compact_hunks`].
fn edit_based_change_ranges(
    old_text: &str,
    new_text: &str,
    edits: &[ByteRangeEdit],
) -> Vec<ChangeRange> {
    if edits.is_empty() {
        return Vec::new();
    }

    // Build line-start byte-offset tables for old and new content.
    let old_offsets = line_start_offsets(old_text);
    let new_offsets = line_start_offsets(new_text);

    // Sort edits by start offset (ascending) to process in order.
    let mut sorted: Vec<&ByteRangeEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| e.start);

    // Track cumulative byte shift caused by replacements so we can map
    // old-file byte positions to new-file byte positions.
    let mut byte_shift: isize = 0;
    let mut ranges = Vec::new();

    for edit in &sorted {
        let old_start_line = byte_offset_to_line(&old_offsets, edit.start);
        let old_end_line = byte_offset_to_line(&old_offsets, edit.end);
        // Include the line that contains edit.end (unless it's at the line start boundary).
        let old_end = if edit.end > edit.start
            && old_end_line < old_offsets.len()
            && old_offsets[old_end_line] == edit.end
        {
            old_end_line
        } else {
            (old_end_line + 1).min(old_offsets.len())
        };

        // Map to new-file lines via the cumulative byte shift.
        #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
        let new_byte_start = (edit.start as isize + byte_shift).max(0) as usize;
        #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
        let new_byte_end =
            (edit.start as isize + byte_shift + edit.replacement.len() as isize).max(0) as usize;

        let new_start_line = byte_offset_to_line(&new_offsets, new_byte_start);
        let new_end_line = byte_offset_to_line(&new_offsets, new_byte_end);
        let new_end = if new_byte_end > new_byte_start
            && new_end_line < new_offsets.len()
            && new_offsets[new_end_line] == new_byte_end
        {
            new_end_line
        } else {
            (new_end_line + 1).min(new_offsets.len())
        };

        // Update cumulative shift: replacement_len - original_span_len.
        #[allow(clippy::cast_possible_wrap)]
        {
            byte_shift += edit.replacement.len() as isize - (edit.end - edit.start) as isize;
        }

        ranges.push(ChangeRange {
            old_start: old_start_line,
            old_end,
            new_start: new_start_line,
            new_end,
        });
    }

    // Merge overlapping or adjacent ranges.
    merge_change_ranges(&mut ranges);
    ranges
}

/// Build a sorted table of byte offsets where each line begins.
/// `offsets[0] == 0` (first line starts at byte 0).
fn line_start_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            offsets.push(i + 1);
        }
    }
    offsets
}

/// Binary-search the line-start table to find which line contains `byte_pos`.
fn byte_offset_to_line(offsets: &[usize], byte_pos: usize) -> usize {
    match offsets.binary_search(&byte_pos) {
        Ok(exact) => exact,
        Err(ins) => ins.saturating_sub(1),
    }
}

/// Merge overlapping or adjacent [`ChangeRange`]s in-place.
fn merge_change_ranges(ranges: &mut Vec<ChangeRange>) {
    if ranges.len() <= 1 {
        return;
    }
    ranges.sort_by_key(|r| (r.old_start, r.new_start));
    let mut write = 0;
    for read in 1..ranges.len() {
        if ranges[read].old_start <= ranges[write].old_end
            && ranges[read].new_start <= ranges[write].new_end
        {
            ranges[write].old_end = ranges[write].old_end.max(ranges[read].old_end);
            ranges[write].new_end = ranges[write].new_end.max(ranges[read].new_end);
        } else {
            write += 1;
            ranges[write] = ranges[read];
        }
    }
    ranges.truncate(write + 1);
}

/// Truncate a display line to `max_w` visible characters.
///
/// Lines that fit are returned as-is. Longer lines keep the first and last
/// portions separated by `…` (U+2026). The 1-char prefix (`-`, `+`, ` `)
/// is preserved and does not count toward the width budget.
fn truncate_line(line: &str, max_w: usize) -> Cow<'_, str> {
    // The first character is the diff marker (-/+/ ), keep it intact.
    if line.len() <= 1 {
        return Cow::Borrowed(line);
    }
    let prefix = &line[..1];
    let content = &line[1..];

    // char_count for correct Unicode handling.
    let char_count = content.chars().count();
    if char_count <= max_w {
        return Cow::Borrowed(line);
    }

    // Split budget: half minus 1 for the ellipsis on each side.
    // E.g. max_w=40 → keep 19 head + … + 20 tail = 40 chars.
    let head = (max_w - 1) / 2;
    let tail = max_w - 1 - head;

    let head_str: String = content.chars().take(head).collect();
    let tail_str: String = content.chars().skip(char_count - tail).collect();

    Cow::Owned(format!("{prefix}{head_str}\u{2026}{tail_str}"))
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
#[derive(Copy, Clone)]
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

    // --- compact diff preview ---------------------------------------------

    #[test]
    fn compact_preview_single_hunk() {
        let old = "line1\nold line\nline3\nline4\n";
        let new = "line1\nnew line\nline3\nline4\n";
        let cfg = CompactDiffConfig::default();
        let preview = compact_diff_preview(old, new, Path::new("test.cpp"), &cfg);
        assert!(preview.contains("── test.cpp ──"), "must have file header");
        assert!(preview.contains("-old line"), "must show removed line");
        assert!(preview.contains("+new line"), "must show added line");
        // Context-after: should show lines after the change.
        assert!(preview.contains(" line3"), "must show context-after line");
    }

    #[test]
    fn compact_preview_truncates_long_lines() {
        let long = format!("line1\n{}\nline3\n", "x".repeat(80));
        let new_long = format!("line1\n{}\nline3\n", "y".repeat(80));
        let cfg = CompactDiffConfig {
            max_line_width: 20,
            ..CompactDiffConfig::default()
        };
        let preview = compact_diff_preview(&long, &new_long, Path::new("t.cpp"), &cfg);
        // The changed lines should contain the ellipsis.
        assert!(
            preview.contains('\u{2026}'),
            "long lines must be truncated with …"
        );
    }

    #[test]
    fn compact_preview_multi_hunk_elides_middle() {
        // Many hunks far apart — middle should be elided when total exceeds K.
        let mut old_lines = Vec::new();
        let mut new_lines = Vec::new();
        for i in 0..100 {
            old_lines.push(format!("line{i}"));
            new_lines.push(format!("line{i}"));
        }
        // Create 5 changes spread far apart → 5 hunks.
        for &idx in &[2, 20, 40, 60, 80] {
            old_lines[idx] = format!("old_{idx}");
            new_lines[idx] = format!("new_{idx}");
        }

        let old = old_lines.join("\n");
        let new = new_lines.join("\n");
        let cfg = CompactDiffConfig {
            max_lines_per_file: 10,
            ..CompactDiffConfig::default()
        };
        let preview = compact_diff_preview(&old, &new, Path::new("big.cpp"), &cfg);
        assert!(preview.contains("new_2"), "first hunk must be visible");
        assert!(preview.contains("new_80"), "last hunk must be visible");
        assert!(
            preview.contains("\u{2026}"),
            "must have elision marker for middle hunks: {preview}"
        );
    }

    #[test]
    fn compact_preview_single_oversized_hunk_uses_head_tail_elision() {
        // One hunk with many added lines — should get line-level head/tail
        // elision, NOT naive truncation with "(… truncated …)".
        let old = "fn foo() {\n    old_body();\n}\n";
        // New body has 30 lines — well over the K=14 default.
        let mut new_body = String::new();
        for i in 0..30 {
            use std::fmt::Write as _;
            let _ = writeln!(new_body, "    line_{i}();");
        }
        let new = format!("fn foo() {{\n{new_body}}}\n");

        let cfg = CompactDiffConfig::default(); // K=14
        let preview = compact_diff_preview(old, &new, Path::new("foo.cpp"), &cfg);

        // Must use the proportional elision marker, not the naive "(… truncated …)".
        assert!(
            preview.contains("\u{2026}") && preview.contains("lines elided"),
            "single oversized hunk must use head/tail elision: {preview}"
        );
        assert!(
            !preview.contains("truncated"),
            "must not fall back to naive truncation: {preview}"
        );

        // The preview must not exceed K + 1 (elision line) + header content lines.
        let content_lines: usize = preview
            .lines()
            .filter(|l| l.starts_with('-') || l.starts_with('+') || l.starts_with(' '))
            .count();
        assert!(
            content_lines <= cfg.max_lines_per_file,
            "content lines {content_lines} must not exceed K={}: {preview}",
            cfg.max_lines_per_file
        );

        // Head should be present (first changed lines visible).
        assert!(
            preview.contains("-    old_body()"),
            "head must show removed line"
        );
        // Tail should be present (last added line visible).
        assert!(
            preview.contains("+    line_29()"),
            "tail must show last added line"
        );
    }

    #[test]
    fn compact_preview_identical_content_is_empty() {
        let text = "same\n";
        let cfg = CompactDiffConfig::default();
        let preview = compact_diff_preview(text, text, Path::new("f.cpp"), &cfg);
        assert!(preview.is_empty());
    }

    #[test]
    fn compact_diff_plan_large_file_shows_edited_region() {
        // Regression: files over ~2000 lines exceeded the LCS cell cap
        // (4_000_000), causing compact_diff_plan to fall back to a
        // whole-file replacement diff that showed the file header/tail
        // instead of the actual edited region.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.c");

        // Build a 3000-line file — 3000×3000 = 9M > 4M LCS cap.
        let lines: Vec<String> = (0..3000).map(|i| format!("line_{i:04}();")).collect();
        let original = lines.join("\n") + "\n";
        std::fs::write(&path, &original).unwrap();

        // Edit line 1500 (roughly in the middle).
        let target = "line_1500();";
        let byte_start = original.find(target).unwrap();
        let byte_end = byte_start + target.len();
        let replacement = "replaced_line_1500();";

        let plan = TransformPlan {
            file_edits: vec![FileEdit {
                path,
                edits: vec![ByteRangeEdit::new(byte_start..byte_end, replacement)],
            }],
            suggestions: vec![],
        };

        let cfg = CompactDiffConfig::default();
        let result = compact_diff_plan(&plan, &cfg).unwrap();

        // The diff MUST show the actual edited lines, not the file header/tail.
        assert!(
            result.contains("-line_1500();"),
            "must show removed line near edit site, got: {result}"
        );
        assert!(
            result.contains("+replaced_line_1500();"),
            "must show added line near edit site, got: {result}"
        );
        // Must NOT show lines from the file header (line 0) or tail (line 2999).
        assert!(
            !result.contains("line_0000"),
            "must not show file header, got: {result}"
        );
        assert!(
            !result.contains("line_2999"),
            "must not show file tail, got: {result}"
        );
    }

    #[test]
    fn truncate_line_short_unchanged() {
        assert_eq!(truncate_line("+short", 40), "+short");
    }

    #[test]
    fn truncate_line_exact_width() {
        // 40-char content after prefix → should not truncate.
        let line = format!("+{}", "a".repeat(40));
        assert_eq!(truncate_line(&line, 40).as_ref(), line.as_str());
    }

    #[test]
    fn truncate_line_over_width() {
        let line = format!("+{}", "a".repeat(60));
        let result = truncate_line(&line, 40);
        assert!(result.contains('\u{2026}'), "must contain ellipsis");
        // Prefix '+' + 40 visible chars (19 head + … + 20 tail).
        // Total char count: 1 (prefix) + 19 + 1 (…) + 20 = 41.
        assert_eq!(result.chars().count(), 41);
    }
}
