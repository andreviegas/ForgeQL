/// `CHANGE FILE[S] ...` — universal mutation transform plugin.
///
/// Resolves every `ChangeTarget` variant to one or more `ByteRangeEdit`s
/// against the listed files.  Supports creation, overwrite, line-range
/// replacement, and deletion.
use std::path::Path;

use anyhow::{Result, anyhow, bail};
use tracing::debug;

use crate::ast::index::SymbolTable;
use crate::context::RequestContext;
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
    pub fn plan(
        &self,
        _ctx: &RequestContext,
        workspace: &Workspace,
        _index: &SymbolTable,
    ) -> Result<TransformPlan> {
        // Expand any glob patterns in the file list before validation.
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
        } => resolve_matching(rel_path, abs_path, pattern, replacement),
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
    })
}

fn resolve_matching(
    rel_path: &str,
    abs_path: &Path,
    pattern: &str,
    replacement: &str,
) -> Result<FileEdit> {
    let source = crate::workspace::file_io::read_bytes(abs_path)?;
    let text =
        std::str::from_utf8(&source).map_err(|e| anyhow!("{rel_path}: not valid UTF-8: {e}"))?;

    // Collect every occurrence of the pattern (byte offsets).
    let mut ranges: Vec<std::ops::Range<usize>> = text
        .match_indices(pattern)
        .map(|(start, _)| start..start + pattern.len())
        .collect();

    if ranges.is_empty() {
        // Return an empty FileEdit — the caller decides whether to skip or error.
        return Ok(FileEdit {
            path: abs_path.to_path_buf(),
            edits: vec![],
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
    })
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
    })
}

fn resolve_delete(rel_path: &str, abs_path: &Path) -> Result<FileEdit> {
    if !abs_path.exists() {
        bail!("{rel_path}: file does not exist, cannot delete");
    }
    let source = crate::workspace::file_io::read_bytes(abs_path)?;
    Ok(FileEdit {
        path: abs_path.to_path_buf(),
        edits: vec![ByteRangeEdit::new(0..source.len(), "")],
    })
}

// -----------------------------------------------------------------------
// Line-range → byte-range helper
// -----------------------------------------------------------------------

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
    if start_line == 0 {
        bail!("line numbers are 1-based, got start=0");
    }
    if end_line < start_line {
        bail!("end line ({end_line}) < start line ({start_line})");
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

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_to_byte_range_basic() {
        let source = b"line1\nline2\nline3\nline4\n";
        // Lines 2-3 → "line2\nline3\n"
        let (s, e) = lines_to_byte_range(source, 2, 3).unwrap();
        assert_eq!(&source[s..e], b"line2\nline3\n");
    }

    #[test]
    fn lines_to_byte_range_single_line() {
        let source = b"aaa\nbbb\nccc\n";
        let (s, e) = lines_to_byte_range(source, 1, 1).unwrap();
        assert_eq!(&source[s..e], b"aaa\n");
    }

    #[test]
    fn lines_to_byte_range_last_line_no_newline() {
        let source = b"first\nsecond";
        let (s, e) = lines_to_byte_range(source, 2, 2).unwrap();
        assert_eq!(&source[s..e], b"second");
    }

    #[test]
    fn lines_to_byte_range_zero_start_error() {
        let source = b"hello\n";
        assert!(lines_to_byte_range(source, 0, 1).is_err());
    }

    #[test]
    fn lines_to_byte_range_end_before_start_error() {
        let source = b"hello\nworld\n";
        assert!(lines_to_byte_range(source, 3, 1).is_err());
    }

    #[test]
    fn lines_to_byte_range_out_of_range_error() {
        let source = b"only one line\n";
        assert!(lines_to_byte_range(source, 5, 6).is_err());
    }

    #[test]
    fn validate_multi_file_matching_ok() {
        let files: Vec<String> = vec!["a.cpp".into(), "b.cpp".into()];
        let target = ChangeTarget::Matching {
            pattern: "x".into(),
            replacement: "y".into(),
        };
        assert!(validate_multi_file(&files, &target).is_ok());
    }

    #[test]
    fn validate_multi_file_with_content_error() {
        let files: Vec<String> = vec!["a.cpp".into(), "b.cpp".into()];
        let target = ChangeTarget::WithContent {
            content: "x".into(),
        };
        assert!(validate_multi_file(&files, &target).is_err());
    }

    #[test]
    fn validate_multi_file_delete_ok() {
        let files: Vec<String> = vec!["a.cpp".into(), "b.cpp".into()];
        assert!(validate_multi_file(&files, &ChangeTarget::Delete).is_ok());
    }

    #[test]
    fn validate_multi_file_lines_error() {
        let files: Vec<String> = vec!["a.cpp".into(), "b.cpp".into()];
        let target = ChangeTarget::Lines {
            start: 1,
            end: 5,
            content: "x".into(),
        };
        assert!(validate_multi_file(&files, &target).is_err());
    }

    #[test]
    fn validate_single_file_always_ok() {
        let files: Vec<String> = vec!["file.cpp".into()];
        for target in [
            ChangeTarget::WithContent {
                content: "x".into(),
            },
            ChangeTarget::Matching {
                pattern: "a".into(),
                replacement: "b".into(),
            },
            ChangeTarget::Lines {
                start: 1,
                end: 1,
                content: "x".into(),
            },
            ChangeTarget::Delete,
        ] {
            assert!(validate_multi_file(&files, &target).is_ok());
        }
    }

    // ── BUG #1 regression: MATCHING must replace ALL occurrences ─────────────

    #[test]
    fn resolve_matching_replaces_all_occurrences() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("buttons.cpp");
        std::fs::write(&path, "Button a;\nButton b;\nint Button_count = 2;\n").expect("write");

        let fe = resolve_matching("buttons.cpp", &path, "Button", "PushButton").unwrap();

        // All three occurrences should produce three edits.
        assert_eq!(
            fe.edits.len(),
            3,
            "expected 3 edits, got {}: {fe:?}",
            fe.edits.len()
        );

        // Edits must be in reverse byte order so earlier offsets stay valid.
        for w in fe.edits.windows(2) {
            assert!(w[0].start > w[1].start, "edits not in reverse byte order");
        }
    }

    #[test]
    fn resolve_matching_single_occurrence_still_works() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("single.cpp");
        std::fs::write(&path, "void oldName() {}").expect("write");

        let fe = resolve_matching("single.cpp", &path, "oldName", "newName").unwrap();
        assert_eq!(fe.edits.len(), 1);
        assert_eq!(fe.edits[0].replacement, "newName");
    }

    #[test]
    fn resolve_matching_not_found_returns_empty_edits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("miss.cpp");
        std::fs::write(&path, "nothing here").expect("write");
        let fe = resolve_matching("miss.cpp", &path, "nonexistent", "x")
            .expect("should succeed with empty edits");
        assert!(fe.edits.is_empty());
    }

    // ── CHANGE LINES trailing newline ────────────────────────────────────

    #[test]
    fn resolve_lines_appends_trailing_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lines.c");
        std::fs::write(&path, "aaa\nbbb\nccc\nddd\n").expect("write");

        // Replace line 2 with text that lacks a trailing newline.
        let fe = resolve_lines("lines.c", &path, 2, 2, "BBB").unwrap();
        assert_eq!(fe.edits.len(), 1);
        assert_eq!(fe.edits[0].replacement, "BBB\n", "should auto-append \\n");
    }

    #[test]
    fn resolve_lines_preserves_existing_trailing_newline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lines2.c");
        std::fs::write(&path, "aaa\nbbb\nccc\n").expect("write");

        let fe = resolve_lines("lines2.c", &path, 2, 2, "BBB\n").unwrap();
        assert_eq!(fe.edits[0].replacement, "BBB\n", "should not double \\n");
    }

    #[test]
    fn resolve_lines_no_newline_for_empty_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lines3.c");
        std::fs::write(&path, "aaa\nbbb\nccc\n").expect("write");

        // Deleting lines (empty content) should stay empty.
        let fe = resolve_lines("lines3.c", &path, 2, 2, "").unwrap();
        assert_eq!(fe.edits[0].replacement, "", "empty content = line deletion");
    }
}
