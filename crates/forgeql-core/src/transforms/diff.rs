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
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;

use super::{ByteRangeEdit, FileEdit, TransformPlan};
use crate::workspace::file_io;

mod compact;
mod lcs;

pub use compact::{CompactDiffConfig, compact_diff_addressed, compact_diff_plan};

use lcs::{build_hunks, change_ranges};

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
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::compact::{compact_diff_preview, truncate_line};
    use super::lcs::{
        ChangeRange, byte_offset_to_line, gaps_from_matches, lcs_matches, line_start_offsets,
        merge_change_ranges, render_hunk,
    };
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
            delete: false,
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
    fn compact_preview_context_before_surfaces_prior_line() {
        // BUG-022: an INSERT after the last collection element must reveal the
        // prior element (which now lacks a trailing separator) via context-
        // before — the bare inserted `+` line alone would hide the breakage.
        let old = "[\n  { \"a\": 1 },\n  { \"b\": 2 }\n]\n";
        let new = "[\n  { \"a\": 1 },\n  { \"b\": 2 }\n  { \"c\": 3 }\n]\n";
        let cfg = CompactDiffConfig::default();
        let preview = compact_diff_preview(old, new, Path::new("data.json"), &cfg);
        assert!(
            preview.contains("+  { \"c\": 3 }"),
            "must show the inserted line: {preview}"
        );
        assert!(
            preview.contains("{ \"b\": 2 }"),
            "context-before must reveal the prior element missing a separator: {preview}"
        );
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
    fn compact_preview_multi_hunk_shows_leading_regions_and_summary() {
        // Many hunks far apart, exceeding the budget. The render shows leading
        // regions in order and SUMMARISES the remainder — it must never drop a
        // middle region silently (the old first+last-only behaviour did).
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
        // The leading region is shown in full...
        assert!(preview.contains("new_2"), "leading region must be visible");
        // ...and the remainder is explicitly counted, not silently dropped.
        assert!(
            preview.contains("more region(s)"),
            "remaining regions must be summarised, never dropped: {preview}"
        );
        assert!(
            preview.contains('\u{2026}'),
            "must have an elision marker: {preview}"
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
                delete: false,
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
    fn compact_diff_addressed_prefixes_present_lines_with_handles() {
        // After an INSERT (disk holds the post-edit content, `originals` the
        // pre-edit), present lines — the inserted line and its context — carry
        // inline `node_id(offset)` handles. Removed lines stay unaddressed.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        let old = "[\n  { \"a\": 1 },\n  { \"b\": 2 }\n]\n";
        let new = "[\n  { \"a\": 1 },\n  { \"b\": 2 }\n  { \"c\": 3 }\n]\n";
        std::fs::write(&path, new).unwrap(); // disk = post-edit content

        let mut originals = std::collections::HashMap::new();
        let _ = originals.insert(path.clone(), old.as_bytes().to_vec());

        // The edit that produced `new`: insert the new element before `]`.
        let insert_at = old.find(']').unwrap();
        let edits = vec![FileEdit {
            path,
            edits: vec![ByteRangeEdit::new(insert_at..insert_at, "  { \"c\": 3 }\n")],
            delete: false,
        }];

        // Mock addresser: every queried line resolves to node "nABC.0009"
        // starting at `lo` (so offset = line - lo + 1).
        let mut node_refs = |_p: &Path, lo: usize, hi: usize| -> Vec<Option<(String, usize)>> {
            (lo..=hi)
                .map(|_| Some(("nABC.0009".to_string(), lo)))
                .collect()
        };

        let cfg = CompactDiffConfig::default();
        let out = compact_diff_addressed(&edits, &originals, &cfg, &mut node_refs).unwrap();
        assert!(
            out.contains("+nABC.0009("),
            "the inserted line must carry an inline node address: {out}"
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
    // --- line_start_offsets -----------------------------------------------

    #[test]
    fn line_start_offsets_empty_string() {
        // Empty text: only one "line" starting at byte 0.
        let offsets = line_start_offsets("");
        assert_eq!(offsets, vec![0]);
    }

    #[test]
    fn line_start_offsets_single_line_no_newline() {
        let offsets = line_start_offsets("hello");
        assert_eq!(offsets, vec![0]);
    }

    #[test]
    fn line_start_offsets_two_lines() {
        // "one\ntwo" → lines start at 0 and 4.
        let offsets = line_start_offsets("one\ntwo");
        assert_eq!(offsets, vec![0, 4]);
    }

    #[test]
    fn line_start_offsets_trailing_newline() {
        // "abc\n" → offsets are 0 and 4 (empty trailing "line").
        let offsets = line_start_offsets("abc\n");
        assert_eq!(offsets, vec![0, 4]);
    }

    #[test]
    fn line_start_offsets_three_lines() {
        let offsets = line_start_offsets("a\nbb\nccc");
        assert_eq!(offsets, vec![0, 2, 5]);
    }

    // --- byte_offset_to_line -------------------------------------------------

    #[test]
    fn byte_offset_to_line_start() {
        let offsets = vec![0, 5, 10];
        assert_eq!(byte_offset_to_line(&offsets, 0), 0);
    }

    #[test]
    fn byte_offset_to_line_exact_boundary() {
        let offsets = vec![0, 5, 10];
        // Byte 5 is exactly the start of line 1 → line index 1.
        assert_eq!(byte_offset_to_line(&offsets, 5), 1);
    }

    #[test]
    fn byte_offset_to_line_within_line() {
        let offsets = vec![0, 5, 10];
        // Byte 7 is inside line 1 (started at 5) → line index 1.
        assert_eq!(byte_offset_to_line(&offsets, 7), 1);
    }

    #[test]
    fn byte_offset_to_line_past_end() {
        let offsets = vec![0, 5, 10];
        // Past the last known start → clamps to last line index (2).
        assert_eq!(byte_offset_to_line(&offsets, 100), 2);
    }

    // --- merge_change_ranges ------------------------------------------------

    #[test]
    fn merge_change_ranges_single_unchanged() {
        let mut r = vec![ChangeRange {
            old_start: 2,
            old_end: 5,
            new_start: 2,
            new_end: 5,
        }];
        merge_change_ranges(&mut r);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn merge_change_ranges_non_overlapping() {
        let mut r = vec![
            ChangeRange {
                old_start: 0,
                old_end: 2,
                new_start: 0,
                new_end: 2,
            },
            ChangeRange {
                old_start: 10,
                old_end: 12,
                new_start: 10,
                new_end: 12,
            },
        ];
        merge_change_ranges(&mut r);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn merge_change_ranges_overlapping() {
        let mut r = vec![
            ChangeRange {
                old_start: 0,
                old_end: 5,
                new_start: 0,
                new_end: 5,
            },
            ChangeRange {
                old_start: 3,
                old_end: 8,
                new_start: 3,
                new_end: 8,
            },
        ];
        merge_change_ranges(&mut r);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].old_end, 8);
        assert_eq!(r[0].new_end, 8);
    }

    #[test]
    fn merge_change_ranges_adjacent() {
        // Adjacent ranges (end == start of next): the condition uses <=, so
        // the second range's old_start (3) <= the first range's old_end (3) → MERGED.
        let mut r = vec![
            ChangeRange {
                old_start: 0,
                old_end: 3,
                new_start: 0,
                new_end: 3,
            },
            ChangeRange {
                old_start: 3,
                old_end: 6,
                new_start: 3,
                new_end: 6,
            },
        ];
        merge_change_ranges(&mut r);
        // Adjacent ranges are merged into one by the <= check.
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].old_end, 6);
    }

    // --- gaps_from_matches --------------------------------------------------

    #[test]
    fn gaps_from_matches_no_matches_one_range() {
        // No common lines → entire span is one change range.
        let ranges = gaps_from_matches(3, 3, &[]);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].old_start, 0);
        assert_eq!(ranges[0].old_end, 3);
        assert_eq!(ranges[0].new_end, 3);
    }

    #[test]
    fn gaps_from_matches_full_coverage_no_ranges() {
        // All lines match → no change ranges.
        let matches = vec![(0, 0), (1, 1), (2, 2)];
        let ranges = gaps_from_matches(3, 3, &matches);
        assert!(ranges.is_empty());
    }

    #[test]
    fn gaps_from_matches_gap_at_start() {
        // First line differs, rest match: gap [0..1, 0..1].
        let matches = vec![(1, 1), (2, 2)];
        let ranges = gaps_from_matches(3, 3, &matches);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].old_start, 0);
        assert_eq!(ranges[0].old_end, 1);
    }

    #[test]
    fn gaps_from_matches_gap_at_end() {
        // First lines match, last line differs.
        let matches = vec![(0, 0), (1, 1)];
        let ranges = gaps_from_matches(3, 3, &matches);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].old_start, 2);
        assert_eq!(ranges[0].old_end, 3);
    }

    // --- change_ranges (integration of lcs + gaps) --------------------------

    #[test]
    fn change_ranges_identical_is_empty() {
        let lines = vec!["a", "b", "c"];
        let ranges = change_ranges(&lines, &lines);
        assert!(ranges.is_empty());
    }

    #[test]
    fn change_ranges_completely_different() {
        let old = vec!["a", "b"];
        let new = vec!["x", "y"];
        let ranges = change_ranges(&old, &new);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].old_start, 0);
        assert_eq!(ranges[0].old_end, 2);
    }

    #[test]
    fn change_ranges_single_line_change_middle() {
        let old = vec!["a", "b", "c"];
        let new = vec!["a", "B", "c"];
        let ranges = change_ranges(&old, &new);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].old_start, 1);
        assert_eq!(ranges[0].old_end, 2);
        assert_eq!(ranges[0].new_start, 1);
        assert_eq!(ranges[0].new_end, 2);
    }

    // --- build_hunks / render_hunk ------------------------------------------

    #[test]
    fn build_hunks_empty_ranges_returns_empty() {
        let old: Vec<&str> = vec!["a"];
        let new: Vec<&str> = vec!["a"];
        let hunks = build_hunks(&old, &new, &[]);
        assert!(hunks.is_empty());
    }

    #[test]
    fn render_hunk_header_format() {
        // Single-line change: old line "foo" replaced by "bar".
        let old = vec!["ctx1", "ctx2", "ctx3", "foo", "ctx4", "ctx5", "ctx6"];
        let new = vec!["ctx1", "ctx2", "ctx3", "bar", "ctx4", "ctx5", "ctx6"];
        let cr = ChangeRange {
            old_start: 3,
            old_end: 4,
            new_start: 3,
            new_end: 4,
        };
        let hunk = render_hunk(&old, &new, &[cr]);
        assert!(hunk.starts_with("@@"), "hunk must start with @@: {hunk}");
        assert!(hunk.contains("-foo"), "removed line must have - prefix");
        assert!(hunk.contains("+bar"), "added line must have + prefix");
        assert!(
            hunk.contains(" ctx1") || hunk.contains(" ctx2"),
            "context lines must have space prefix"
        );
    }

    #[test]
    fn render_hunk_add_only() {
        // Line inserted between existing lines.
        let old = vec!["a", "b"];
        let new = vec!["a", "NEW", "b"];
        let cr = ChangeRange {
            old_start: 1,
            old_end: 1,
            new_start: 1,
            new_end: 2,
        };
        let hunk = render_hunk(&old, &new, &[cr]);
        assert!(hunk.contains("+NEW"), "added line must appear");
        // The @@ header contains '-' so check diff-body lines specifically.
        assert!(
            !hunk.lines().any(|l| l.starts_with('-')),
            "no removal lines in add-only hunk: {hunk}"
        );
    }

    #[test]
    fn render_hunk_delete_only() {
        // One line removed.
        let old = vec!["a", "REMOVE", "b"];
        let new = vec!["a", "b"];
        let cr = ChangeRange {
            old_start: 1,
            old_end: 2,
            new_start: 1,
            new_end: 1,
        };
        let hunk = render_hunk(&old, &new, &[cr]);
        assert!(hunk.contains("-REMOVE"), "removed line must appear");
        // The @@ header contains '+' so check diff-body lines specifically.
        assert!(
            !hunk.lines().any(|l| l.starts_with('+')),
            "no addition lines in delete-only hunk: {hunk}"
        );
    }

    #[test]
    fn unified_diff_delete_only_line() {
        let old = "a\nDELETED\nb\n";
        let new = "a\nb\n";
        let d = unified_diff(old, new, Path::new("f.c"));
        assert!(d.contains("-DELETED"));
    }

    #[test]
    fn unified_diff_path_appears_in_headers() {
        let old = "x\n";
        let new = "y\n";
        let d = unified_diff(old, new, Path::new("src/myfile.cpp"));
        assert!(d.contains("--- a/src/myfile.cpp"));
        assert!(d.contains("+++ b/src/myfile.cpp"));
    }

    #[test]
    fn truncate_line_prefix_only_unchanged() {
        // A line with only a prefix character and no content should not truncate.
        let result = truncate_line("+", 40);
        assert_eq!(result.as_ref(), "+");
    }

    #[test]
    fn truncate_line_unicode_content() {
        // Unicode chars (multi-byte): truncation must count chars, not bytes.
        // Build a 41-char content (all 'é' which is 2 UTF-8 bytes each).
        let line = format!("+{}", "é".repeat(41));
        let result = truncate_line(&line, 40);
        assert!(result.contains('\u{2026}'), "must be truncated");
        // char count: prefix(1) + 19 + ellipsis(1) + 20 = 41 chars
        assert_eq!(result.chars().count(), 41);
    }
}
