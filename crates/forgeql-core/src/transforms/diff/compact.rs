//! Token-bounded compact diff preview.
//!
//! Renders a plan — or a set of addressed edits — as a small number of hunks
//! with truncated lines, sized by [`CompactDiffConfig`].

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;

use super::apply::apply_in_memory;
use super::lcs::{ChangeRange, byte_offset_to_line, line_start_offsets, merge_change_ranges};
use crate::transforms::{ByteRangeEdit, FileEdit, TransformPlan};
use crate::workspace::file_io;

#[cfg(test)]
use super::lcs::change_ranges;

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
    /// Number of unchanged context lines shown *before* the first changed line
    /// in each hunk. Surfaces the line a mechanical edit landed against — e.g.
    /// the prior collection element that now needs a trailing separator after
    /// an INSERT (BUG-022), which the bare `+` line alone would hide.
    pub context_before: usize,
    /// Number of unchanged context lines shown after the last changed line
    /// in each hunk (helps the agent detect merge errors).
    pub context_after: usize,
}

impl Default for CompactDiffConfig {
    fn default() -> Self {
        Self {
            max_lines_per_file: 14,
            max_line_width: 120,
            context_before: 2,
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

        let hunks = build_compact_hunks(&old_lines, &new_lines, &ranges, cfg, None);
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

/// Per-line node handles for a post-edit line range: `(node_id, node_start)`,
/// or `None` for a line covered by no indexed node.
type LineNodeRefs = Vec<Option<(String, usize)>>;

/// Build the post-edit compact diff with inline node addresses on present lines.
///
/// Must be called **after** the edit is applied and the session reindexed: it
/// reads the post-edit content from disk and the pre-edit content from
/// `originals` (as returned by `TransformPlan::apply`), so node ordinals for the
/// new lines already exist. `edits` is the snapshot of the applied file edits,
/// taken before `apply` consumed the plan.
///
/// `node_refs(path, lo, hi)` returns, for each 1-based post-edit line in
/// `lo..=hi`, the innermost `(node_id, node_start_line)` or `None`; an empty Vec
/// (unindexed file) renders that file without addresses. Present lines (added +
/// context) are addressed `node_id(offset)`; removed lines have no post-edit
/// position and stay unaddressed.
///
/// # Errors
/// Returns `Err` if a post-edit file cannot be read.
pub fn compact_diff_addressed<S: std::hash::BuildHasher>(
    edits: &[FileEdit],
    originals: &std::collections::HashMap<std::path::PathBuf, Vec<u8>, S>,
    cfg: &CompactDiffConfig,
    node_refs: &mut dyn FnMut(&Path, usize, usize) -> LineNodeRefs,
) -> Result<String> {
    let mut out = String::new();
    for fe in edits {
        let Some(old_bytes) = originals.get(&fe.path) else {
            continue;
        };
        let new_bytes = file_io::read_bytes(&fe.path)?;
        let old_str = String::from_utf8_lossy(old_bytes);
        let new_str = String::from_utf8_lossy(&new_bytes);
        if old_str == new_str {
            continue;
        }

        let old_lines: Vec<&str> = old_str.split('\n').collect();
        let new_lines: Vec<&str> = new_str.split('\n').collect();
        let ranges = edit_based_change_ranges(&old_str, &new_str, &fe.edits);
        if ranges.is_empty() {
            continue;
        }

        // 1-based post-edit line span the hunks touch (including context).
        let mut lo = usize::MAX;
        let mut hi = 0_usize;
        for cr in &ranges {
            lo = lo.min(cr.new_start.saturating_sub(cfg.context_before) + 1);
            hi = hi.max((cr.new_end + cfg.context_after).min(new_lines.len()));
        }
        let refs = if lo >= 1 && lo <= hi {
            node_refs(&fe.path, lo, hi)
        } else {
            Vec::new()
        };

        let addr_fn = |new_line: usize| -> Option<String> {
            if new_line < lo {
                return None;
            }
            let (node_id, node_start) = refs.get(new_line - lo)?.as_ref()?;
            let offset = new_line.saturating_sub(*node_start) + 1;
            Some(format!("{node_id}({offset})"))
        };
        let addr: Option<&dyn Fn(usize) -> Option<String>> = if refs.iter().any(Option::is_some) {
            Some(&addr_fn)
        } else {
            None
        };

        let hunks = build_compact_hunks(&old_lines, &new_lines, &ranges, cfg, addr);
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
pub(super) fn compact_diff_preview(
    old: &str,
    new: &str,
    path: &Path,
    cfg: &CompactDiffConfig,
) -> String {
    let old_lines: Vec<&str> = old.split('\n').collect();
    let new_lines: Vec<&str> = new.split('\n').collect();

    let ranges = change_ranges(&old_lines, &new_lines);
    if ranges.is_empty() {
        return String::new();
    }

    // Build per-hunk display blocks (each hunk = changed lines + context-after).
    let hunks = build_compact_hunks(&old_lines, &new_lines, &ranges, cfg, None);
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
        // Multiple oversized hunks: emit whole regions in order until the line
        // budget is exhausted, then a one-line summary of the remainder.
        // Unlike the old first+last-only render (which silently dropped every
        // middle region of a large multi-region edit), no region is dropped
        // without being counted — each is either shown in full or summarised.
        let mut emitted_lines = 0_usize;
        let mut emitted_hunks = 0_usize;
        for hunk in hunks {
            // Always emit at least one region; otherwise stop before overrunning.
            if emitted_hunks > 0 && emitted_lines + hunk.lines.len() > cfg.max_lines_per_file {
                break;
            }
            for line in &hunk.lines {
                let _ = writeln!(out, "{}", truncate_line(line, cfg.max_line_width));
            }
            emitted_lines += hunk.lines.len();
            emitted_hunks += 1;
        }
        if emitted_hunks < hunks.len() {
            let remaining_hunks = hunks.len() - emitted_hunks;
            let remaining_lines: usize = hunks[emitted_hunks..].iter().map(|h| h.lines.len()).sum();
            let _ = writeln!(
                out,
                "(\u{2026} {remaining_hunks} more region(s) \u{b7} {remaining_lines} lines elided \
                 \u{2014} narrow the CHANGE or read the file for the rest \u{2026})"
            );
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
/// Each hunk shows up to `cfg.context_before` unchanged lines before the
/// change (surfacing the line a mechanical edit landed against), the `-`/`+`
/// change lines, then up to `cfg.context_after` unchanged lines after it.
fn build_compact_hunks(
    old: &[&str],
    new: &[&str],
    ranges: &[ChangeRange],
    cfg: &CompactDiffConfig,
    addr: Option<&dyn Fn(usize) -> Option<String>>,
) -> Vec<CompactHunk> {
    // Render a present (post-edit) line, prefixing its node address when the
    // addresser resolves one for the line's 1-based post-edit number.
    let present = |prefix: char, new_line: usize, text: &str| -> String {
        addr.and_then(|f| f(new_line)).map_or_else(
            || format!("{prefix}{text}"),
            |a| format!("{prefix}{a}  {text}"),
        )
    };

    let mut hunks = Vec::new();

    for cr in ranges {
        let mut lines = Vec::new();

        // Context-before: unchanged lines just above the change (from old),
        // mapped to their post-edit line numbers so they can be addressed.
        // Surfaces the line a mechanical edit landed against (BUG-022).
        let ctx_b_start = cr.old_start.saturating_sub(cfg.context_before);
        for idx in ctx_b_start..cr.old_start {
            let text = old.get(idx).copied().unwrap_or("");
            let new_line = cr.new_start.saturating_sub(cr.old_start - idx) + 1;
            lines.push(present(' ', new_line, text));
        }

        // Removed lines: gone post-edit, so no address.
        for idx in cr.old_start..cr.old_end {
            let text = old.get(idx).copied().unwrap_or("");
            lines.push(format!("-{text}"));
        }
        // Added lines.
        for idx in cr.new_start..cr.new_end {
            let text = new.get(idx).copied().unwrap_or("");
            lines.push(present('+', idx + 1, text));
        }
        // Context-after: unchanged lines just below the change (from new).
        let ctx_start = cr.new_end;
        let ctx_end = (ctx_start + cfg.context_after).min(new.len());
        for idx in ctx_start..ctx_end {
            let text = new.get(idx).copied().unwrap_or("");
            lines.push(present(' ', idx + 1, text));
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
        let start_isize = isize::try_from(edit.start).unwrap_or(isize::MAX);
        let repl_len_isize = isize::try_from(edit.replacement.len()).unwrap_or(isize::MAX);
        let new_byte_start = usize::try_from((start_isize + byte_shift).max(0)).unwrap_or(0);
        let new_byte_end =
            usize::try_from((start_isize + byte_shift + repl_len_isize).max(0)).unwrap_or(0);

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
        let edit_span_isize = isize::try_from(edit.end - edit.start).unwrap_or(isize::MAX);
        byte_shift += repl_len_isize - edit_span_isize;

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

/// Truncate a display line to `max_w` visible characters.
///
/// Lines that fit are returned as-is. Longer lines keep the first and last
/// portions separated by `…` (U+2026). The 1-char prefix (`-`, `+`, ` `)
/// is preserved and does not count toward the width budget.
pub(super) fn truncate_line(line: &str, max_w: usize) -> Cow<'_, str> {
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
