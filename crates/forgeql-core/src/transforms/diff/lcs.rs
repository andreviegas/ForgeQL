//! Line-level LCS, change-range merging, and unified-diff hunk assembly.
//!
//! Pure computation — no I/O and no plan/edit types. Both the compact
//! preview and the apply layer build their output on top of these.

use std::fmt::Write as _;

/// Number of context lines around each change block.
const CONTEXT: usize = 3;

// -----------------------------------------------------------------------
// LCS — line-level longest common subsequence
// -----------------------------------------------------------------------

/// A contiguous block that differs between `old` and `new`.
#[derive(Copy, Clone)]
pub(super) struct ChangeRange {
    pub(super) old_start: usize,
    pub(super) old_end: usize,
    pub(super) new_start: usize,
    pub(super) new_end: usize,
}

/// Compute the diff as a list of [`ChangeRange`]s via line-level LCS.
pub(super) fn change_ranges(old: &[&str], new: &[&str]) -> Vec<ChangeRange> {
    let matches = lcs_matches(old, new);
    gaps_from_matches(old.len(), new.len(), &matches)
}

/// Return `(old_idx, new_idx)` pairs of matching lines (the LCS).
///
/// Complexity: O(m·n) time and space.  For very large files the function
/// returns an empty vec (→ whole-file replacement diff).
pub(super) fn lcs_matches(old: &[&str], new: &[&str]) -> Vec<(usize, usize)> {
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
pub(super) fn gaps_from_matches(
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
pub(super) fn build_hunks(old: &[&str], new: &[&str], ranges: &[ChangeRange]) -> Vec<String> {
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
pub(super) fn render_hunk(old: &[&str], new: &[&str], group: &[ChangeRange]) -> String {
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

/// Build a sorted table of byte offsets where each line begins.
/// `offsets[0] == 0` (first line starts at byte 0).
pub(super) fn line_start_offsets(text: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            offsets.push(i + 1);
        }
    }
    offsets
}

/// Binary-search the line-start table to find which line contains `byte_pos`.
pub(super) fn byte_offset_to_line(offsets: &[usize], byte_pos: usize) -> usize {
    match offsets.binary_search(&byte_pos) {
        Ok(exact) => exact,
        Err(ins) => ins.saturating_sub(1),
    }
}

/// Merge overlapping or adjacent [`ChangeRange`]s in-place.
pub(super) fn merge_change_ranges(ranges: &mut Vec<ChangeRange>) {
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
