/// `CHANGE FILE[S] ...` — universal mutation transform plugin.
///
/// Resolves every `ChangeTarget` variant to one or more `ByteRangeEdit`s
/// against the listed files.  Supports creation, overwrite, line-range
/// replacement, and deletion.
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use tracing::debug;

use crate::ir::ChangeTarget;
use crate::transforms::{ByteRangeEdit, FileEdit, TransformPlan};
use crate::workspace::Workspace;

/// The `CHANGE` transform.  Constructed from parsed IR fields.
pub struct ChangeFiles {
    /// Workspace-relative file paths (from the DSL `file_list`).
    pub files: Vec<String>,
    /// The targeting mode and associated data.
    pub target: ChangeTarget,
}

impl ChangeFiles {
    #[must_use]
    pub const fn new(files: Vec<String>, target: ChangeTarget) -> Self {
        Self { files, target }
    }

    /// # Errors
    /// Returns an error if multi-file validation fails, a target file cannot
    /// be read, or the targeting mode cannot be resolved to edits.
    /// # Errors
    /// Returns an error if multi-file validation fails, a target file cannot
    /// be read, or the targeting mode cannot be resolved to edits.
    pub fn plan(&self, workspace: &Workspace) -> Result<TransformPlan> {
        let (resolved, from_glob) = resolve_file_globs(&self.files, workspace)?;

        // Security guard: .forgeql.yaml is a protected file.
        // Allowing CHANGE to overwrite it would let an AI agent inject arbitrary
        // shell commands that VERIFY build would then execute.
        for rel_path in &resolved {
            if std::path::Path::new(rel_path).file_name()
                == Some(std::ffi::OsStr::new(".forgeql.yaml"))
            {
                bail!(
                    "'.forgeql.yaml' is a protected file and cannot be modified by CHANGE commands"
                );
            }
        }

        validate_multi_file(&resolved, &self.target)?;

        let mut plan = TransformPlan::default();
        for rel_path in &resolved {
            let abs_path = workspace.safe_path(rel_path)?;
            let fe = resolve_target(rel_path, &abs_path, &self.target)?;
            // For literal (non-glob) paths, an empty edit means the pattern
            // was not found — that is an error the user should see.
            if !from_glob
                && fe.edits.is_empty()
                && let ChangeTarget::Matching { pattern, .. } = &self.target
            {
                bail!("{rel_path}: pattern not found: '{pattern}'");
            }
            plan.file_edits.push(fe);
        }

        // When files came from glob expansion the pattern may legitimately be
        // absent in some of them.  Drop no-op edits but error if nothing
        // matched anywhere.
        if from_glob {
            plan.file_edits.retain(|fe| !fe.edits.is_empty());
            if plan.file_edits.is_empty() {
                bail!("pattern not found in any file matched by the glob(s)");
            }
        }

        Ok(plan)
    }
}

// -----------------------------------------------------------------------
// Glob expansion
// -----------------------------------------------------------------------

/// Return `true` when a path string contains glob metacharacters.
fn is_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Expand glob patterns in the file list against the workspace.
///
/// Entries without wildcards are kept as-is.  Entries with `*`, `?`, or `[`
/// are matched against every file in the workspace using the same glob engine
/// as `IN` / `EXCLUDE`.
fn resolve_file_globs(raw: &[String], workspace: &Workspace) -> Result<(Vec<String>, bool)> {
    let mut out = Vec::new();
    let mut any_glob = false;
    for entry in raw {
        if is_glob(entry) {
            any_glob = true;
            let matched: Vec<String> = workspace
                .files()
                .filter(|p| crate::ast::query::relative_glob_matches(p, entry, workspace.root()))
                .map(|p| workspace.relative(&p).display().to_string())
                .collect();
            if matched.is_empty() {
                bail!("glob '{entry}' matched no files in the workspace");
            }
            out.extend(matched);
        } else {
            out.push(entry.clone());
        }
    }
    Ok((out, any_glob))
}

// -----------------------------------------------------------------------
// Multi-file validation
// -----------------------------------------------------------------------

/// Reject multi-file targets for modes that require a single file.
fn validate_multi_file(files: &[String], target: &ChangeTarget) -> Result<()> {
    if files.len() <= 1 {
        return Ok(());
    }
    match target {
        ChangeTarget::Matching { .. } | ChangeTarget::Delete => Ok(()),
        ChangeTarget::WithContent { .. } => {
            bail!(
                "CHANGE WITH content requires a single file, got {}",
                files.len()
            );
        }
        ChangeTarget::Lines { .. } => {
            bail!("CHANGE LINES requires a single file, got {}", files.len());
        }
    }
}

// -----------------------------------------------------------------------
// Per-mode resolution to FileEdit
// -----------------------------------------------------------------------

/// Dispatch to the appropriate per-mode resolver.
fn resolve_target(rel_path: &str, abs_path: &Path, target: &ChangeTarget) -> Result<FileEdit> {
    match target {
        ChangeTarget::WithContent { content } => resolve_with_content(abs_path, content),
        ChangeTarget::Matching {
            pattern,
            replacement,
            word_boundary,
        } => resolve_matching(rel_path, abs_path, pattern, replacement, *word_boundary),
        ChangeTarget::Lines {
            start,
            end,
            content,
        } => resolve_lines(rel_path, abs_path, *start, *end, content),
        ChangeTarget::Delete => resolve_delete(rel_path, abs_path),
    }
}

fn resolve_with_content(abs_path: &Path, content: &str) -> Result<FileEdit> {
    let len = if abs_path.exists() {
        crate::workspace::file_io::read_bytes(abs_path)?.len()
    } else {
        0
    };
    Ok(FileEdit {
        path: abs_path.to_path_buf(),
        edits: vec![ByteRangeEdit::new(0..len, content)],
        delete: false,
    })
}

fn resolve_matching(
    rel_path: &str,
    abs_path: &Path,
    pattern: &str,
    replacement: &str,
    word_boundary: bool,
) -> Result<FileEdit> {
    let source = crate::workspace::file_io::read_bytes(abs_path)?;
    if let Some(encoding) = wide_encoding(&source) {
        bail!("{rel_path}: {}", wide_encoding_refusal(encoding));
    }
    let text =
        std::str::from_utf8(&source).map_err(|e| anyhow!("{rel_path}: not valid UTF-8: {e}"))?;

    // Collect every occurrence of the pattern (byte offsets).
    let mut ranges: Vec<std::ops::Range<usize>> = if word_boundary {
        let escaped = regex::escape(pattern);
        let re = regex::Regex::new(&format!(r"\b{escaped}\b"))
            .map_err(|e| anyhow!("{rel_path}: invalid WORD pattern: {e}"))?;
        re.find_iter(text).map(|m| m.start()..m.end()).collect()
    } else {
        text.match_indices(pattern)
            .map(|(start, _)| start..start + pattern.len())
            .collect()
    };

    if ranges.is_empty() {
        // Return an empty FileEdit — the caller decides whether to skip or error.
        return Ok(FileEdit {
            path: abs_path.to_path_buf(),
            edits: vec![],
            delete: false,
        });
    }

    // Apply edits in REVERSE byte order so earlier offsets stay valid.
    ranges.sort_by(|a, b| b.start.cmp(&a.start));

    let count = ranges.len();
    debug!(%rel_path, count, "MATCHING resolved");

    let edits = ranges
        .into_iter()
        .map(|r| ByteRangeEdit::new(r, replacement))
        .collect();

    Ok(FileEdit {
        path: abs_path.to_path_buf(),
        edits,
        delete: false,
    })
}

/// Collect replacement edits for `pattern` restricted to `range` (byte
/// offsets) of `source` — the node- and line-scoped mechanical rename.
///
/// Mirrors [`resolve_matching`]'s match collection; returns an empty vec
/// when the pattern does not occur inside the range (the caller decides
/// whether that is an error).
pub(super) fn matching_edits_in_range(
    source: &[u8],
    pattern: &str,
    replacement: &str,
    word_boundary: bool,
    range: std::ops::Range<usize>,
) -> Result<Vec<ByteRangeEdit>> {
    // `source` is the whole file, so the mark is visible here even when
    // `range` starts past it. Same reason as `lines_to_byte_range`: the
    // replacement would be UTF-8 bytes spliced into UTF-16.
    if let Some(encoding) = wide_encoding(source) {
        bail!("{}", wide_encoding_refusal(encoding));
    }
    let slice = source
        .get(range.clone())
        .ok_or_else(|| anyhow!("byte range {range:?} out of bounds"))?;
    let text = std::str::from_utf8(slice).map_err(|e| anyhow!("not valid UTF-8: {e}"))?;
    let base = range.start;

    let ranges: Vec<std::ops::Range<usize>> = if word_boundary {
        let escaped = regex::escape(pattern);
        let re = regex::Regex::new(&format!(r"\b{escaped}\b"))
            .map_err(|e| anyhow!("invalid WORD pattern: {e}"))?;
        re.find_iter(text)
            .map(|m| base + m.start()..base + m.end())
            .collect()
    } else {
        text.match_indices(pattern)
            .map(|(start, _)| base + start..base + start + pattern.len())
            .collect()
    };

    Ok(ranges
        .into_iter()
        .map(|r| ByteRangeEdit::new(r, replacement))
        .collect())
}

fn resolve_lines(
    rel_path: &str,
    abs_path: &Path,
    start: usize,
    end: usize,
    content: &str,
) -> Result<FileEdit> {
    let source = crate::workspace::file_io::read_bytes(abs_path)?;
    let (byte_start, byte_end) = lines_to_byte_range(&source, start, end)?;
    debug!(%rel_path, byte_start, byte_end, "LINES resolved");

    // LINES is a line-oriented command: the replaced range includes the
    // trailing newline (from lines_to_byte_range), so the replacement text
    // must also end with one.  Without this, the last replacement line
    // merges with the next existing line.
    let content = if !content.is_empty() && !content.ends_with('\n') {
        format!("{content}\n")
    } else {
        content.to_string()
    };

    Ok(FileEdit {
        path: abs_path.to_path_buf(),
        edits: vec![ByteRangeEdit::new(byte_start..byte_end, content)],
        delete: false,
    })
}

fn resolve_delete(rel_path: &str, abs_path: &Path) -> Result<FileEdit> {
    if !abs_path.exists() {
        bail!("{rel_path}: file does not exist, cannot delete");
    }
    let source = crate::workspace::file_io::read_bytes(abs_path)?;
    Ok(FileEdit {
        path: abs_path.to_path_buf(),
        // Full-content removal edit: kept so the boundary diff shows the
        // deleted lines; `delete: true` makes apply() unlink the file.
        edits: vec![ByteRangeEdit::new(0..source.len(), "")],
        delete: true,
    })
}

// -----------------------------------------------------------------------
// Line-range → byte-range helper
// -----------------------------------------------------------------------

/// The encoding these bytes declare, when it is one whose line boundaries are
/// not byte boundaries.
///
/// Only a byte-order mark counts. Guessing from NUL density would misread a
/// compiled object, and a wrong answer here either refuses a legitimate edit
/// or performs a destructive one.
pub(crate) fn wide_encoding(source: &[u8]) -> Option<&'static str> {
    match source {
        [0xFF, 0xFE, 0x00, 0x00, ..] => Some("UTF-32LE"),
        [0x00, 0x00, 0xFE, 0xFF, ..] => Some("UTF-32BE"),
        [0xFF, 0xFE, ..] => Some("UTF-16LE"),
        [0xFE, 0xFF, ..] => Some("UTF-16BE"),
        _ => None,
    }
}

/// Why a partial edit into such a file is refused, and what to do instead.
///
/// Stated in full because the agent reaching this has just been shown a site
/// in the file by `FIND usages`: without the reason, a refusal on a line the
/// query quoted back reads as a defect rather than as the boundary it is.
pub(crate) fn wide_encoding_refusal(encoding: &str) -> String {
    format!(
        "refusing to edit: this file is {encoding}, where a line boundary is not a byte \
         boundary — splicing UTF-8 text into it by byte offset would shift every byte after \
         the edit and destroy the file. `FIND usages` reads {encoding} text, so a site in one \
         can be found but not rewritten in place. That includes replacing the file whole: a \
         whole-file `CHANGE NODE` is lowered to a line range like any other, and a line range \
         over {encoding} does not even reach the last byte. To convert it, delete the file and \
         write it again — DELETE NODE, then INSERT NODE FOR, then INSERT AFTER NODE — or edit \
         it outside ForgeQL and re-index."
    )
}
/// Convert 1-based inclusive line range to byte offsets.
///
/// Returns `(byte_start, byte_end)` where `byte_start` is the offset of the
/// first byte of `start_line` and `byte_end` is the offset just past the last
/// byte of `end_line` (including the trailing newline, if present).
///
/// # Errors
/// Returns an error if lines are out of range or `end < start`.
pub(crate) fn lines_to_byte_range(
    source: &[u8],
    start_line: usize,
    end_line: usize,
) -> Result<(usize, usize)> {
    // A line boundary is only a byte boundary if the bytes are ASCII-
    // compatible. In UTF-16 a newline is two bytes and so is every letter, so
    // scanning for `0x0A` lands inside a code unit: the range would look
    // spliceable, the UTF-8 text written into it would shift everything after
    // the edit by a byte, and the file would be silently destroyed. `FIND
    // usages` now reads UTF-16 text and hands back the file's own handle, so a
    // site in one can be quoted straight into an edit — refuse the write.
    if let Some(encoding) = wide_encoding(source) {
        bail!("{}", wide_encoding_refusal(encoding));
    }
    if start_line == 0 {
        bail!("line numbers are 1-based, got start=0");
    }
    if end_line < start_line {
        bail!("end line ({end_line}) < start line ({start_line})");
    }

    // A 0-byte file has no lines to map, but line 1 of it is still a valid
    // target: it is where INSERT BEFORE/AFTER writes the first content into a
    // freshly created file. Without this the create-then-write bootstrap fails
    // with "start line 1 out of range".
    if source.is_empty() && start_line == 1 && end_line <= 1 {
        return Ok((0, 0));
    }

    let mut line = 1usize;
    let mut byte_start = None;
    let mut byte_end = None;

    for (i, &b) in source.iter().enumerate() {
        if byte_start.is_none() && line == start_line {
            byte_start = Some(i);
        }
        if b == b'\n' {
            if line == end_line {
                byte_end = Some(i + 1);
                break;
            }
            line += 1;
        }
    }

    // Handle last line without trailing newline.
    if byte_end.is_none() && line == end_line {
        byte_end = Some(source.len());
    }

    let bs = byte_start
        .ok_or_else(|| anyhow!("start line {start_line} out of range (file has {line} lines)"))?;
    let be = byte_end
        .ok_or_else(|| anyhow!("end line {end_line} out of range (file has {line} lines)"))?;

    Ok((bs, be))
}

/// Extend a 1-based inclusive `end_line` forward over the contiguous run of
/// blank lines that immediately follow it in `content`, so deleting a node also
/// removes its trailing blank separator (avoids blank-line accumulation).
/// Whitespace is not part of a node's span/rev, so this only widens the DELETE
/// extent. Returns `end_line` unchanged when the next line is non-blank or out
/// of range.
pub(crate) fn absorb_trailing_blank_lines(content: &str, end_line: usize) -> usize {
    let lines: Vec<&str> = content.lines().collect();
    let mut end = end_line;
    // The 1-based line `end + 1` sits at 0-based index `end`.
    while end < lines.len() && lines[end].trim().is_empty() {
        end += 1;
    }
    end
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests;
