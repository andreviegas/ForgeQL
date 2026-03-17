pub mod change;
pub mod diff;

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;

use anyhow::Result;

use crate::ast::index::SymbolTable;
use crate::context::RequestContext;
use crate::workspace::Workspace;

// -----------------------------------------------------------------------
// Core edit primitives
// -----------------------------------------------------------------------

/// A single byte-range replacement in a file.
///
/// `start` and `end` are byte offsets (not character or line offsets).
/// An empty `replacement` string means deletion of the range.
///
/// CRITICAL: Multiple `ByteRangeEdit`s for the same file MUST be sorted in
/// REVERSE byte order before application. Applying a replacement changes the
/// byte length of the file, shifting all subsequent offsets. Reverse order
/// eliminates this problem entirely.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ByteRangeEdit {
    /// Byte offset of the start of the range to replace (inclusive).
    pub start: usize,
    /// Byte offset of the end of the range to replace (exclusive).
    pub end: usize,
    /// Text to write in place of `source[start..end]`.
    pub replacement: String,
}

impl ByteRangeEdit {
    pub fn new(range: Range<usize>, replacement: impl Into<String>) -> Self {
        Self {
            start: range.start,
            end: range.end,
            replacement: replacement.into(),
        }
    }

    /// Return the byte range as a `Range<usize>`.
    #[must_use]
    pub const fn range(&self) -> Range<usize> {
        self.start..self.end
    }
}

/// All edits for one file.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileEdit {
    pub path: PathBuf,
    pub edits: Vec<ByteRangeEdit>,
}

impl FileEdit {
    /// Sort edits in reverse byte order.
    /// MUST be called before `apply_to_buffer`.
    pub fn sort_reverse(&mut self) {
        self.edits.sort_by(|a, b| b.start.cmp(&a.start));
    }
}

// -----------------------------------------------------------------------
// String-literal candidate types
// -----------------------------------------------------------------------

/// A string literal in the workspace that contains the symbol name verbatim.
///
/// These are **never applied automatically**. They are surfaced to the caller
/// so the user can decide whether each occurrence is a structured
/// cross-reference (e.g. `[[deprecated("Use newName()")]]`) or an accidental
/// overlap that should be left untouched.
///
/// Language-specific advisors can enrich the `reason` field. The generic
/// layer always emits `CandidateReason::ExactStringMatch`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StringCandidate {
    /// File containing the match.
    pub path: PathBuf,
    /// Byte offset of the start of `symbol` inside the file.
    pub byte_offset: usize,
    /// Short excerpt of the surrounding line (trimmed), for display.
    pub snippet: String,
    /// Why this candidate was surfaced.
    pub reason: CandidateReason,
}

/// Why a `StringCandidate` was surfaced.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateReason {
    /// The exact symbol name appears inside a string literal.
    /// Intent unknown — could be a structured cross-reference or accidental.
    /// Language advisors may upgrade this to a more specific variant.
    ExactStringMatch,
    /// Inside a `[[deprecated("...")]]` / `__attribute__((deprecated(...)))`
    /// argument — a deliberate, structured cross-reference.
    /// (Set by the C/C++ language advisor.)
    DeprecatedAttribute,
    /// Argument to a language-binding `.def("name", ...)` call
    /// (pybind11, SWIG, Boost.Python, Qt `SIGNAL`/`SLOT`).
    /// (Set by the C/C++ language advisor.)
    BindingName,
}

// -----------------------------------------------------------------------
// TransformPlan
// -----------------------------------------------------------------------

/// The full plan produced by `plan()` — pure analysis, no file I/O.
///
/// `plan()` must never write files. It reads the `SymbolTable` and the
/// workspace file contents, then returns a `TransformPlan` describing
/// every byte-range replacement needed.
///
/// `suggestions` lists string literals that contain the symbol name but were
/// not automatically renamed. The caller should surface these to the user.
#[derive(Debug, Default)]
pub struct TransformPlan {
    pub file_edits: Vec<FileEdit>,
    pub suggestions: Vec<StringCandidate>,
}

impl TransformPlan {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.file_edits.is_empty()
    }

    #[must_use]
    pub fn edit_count(&self) -> usize {
        self.file_edits.iter().map(|fe| fe.edits.len()).sum()
    }

    /// Merge all `FileEdit` entries that target the same path into a single
    /// `FileEdit` per file, then validate that no edits overlap.
    ///
    /// This is critical for transactions that combine multiple ops (e.g.
    /// RENAME + CHANGE both touching the same file).  Without merging, the
    /// second `FileEdit` would be applied against already-modified bytes
    /// using stale offsets.
    ///
    /// After merging, all edits within each file are sorted in descending
    /// byte order and checked for overlaps (edit N's `start` < previous
    /// edit's `end`).  An overlap indicates conflicting edits that cannot
    /// be applied in a single pass — the caller should split them into
    /// separate transactions.
    ///
    /// # Errors
    /// Returns `Err` with a descriptive message if any two edits within the
    /// same file overlap.
    pub fn merge_by_file(&mut self) -> anyhow::Result<()> {
        use std::collections::HashMap;

        let mut by_path: HashMap<PathBuf, Vec<ByteRangeEdit>> = HashMap::new();
        for fe in self.file_edits.drain(..) {
            by_path.entry(fe.path).or_default().extend(fe.edits);
        }

        for (path, mut edits) in by_path {
            // Sort descending by start offset.
            edits.sort_by(|a, b| b.start.cmp(&a.start));

            // Overlap check: after descending sort, for consecutive edits
            // [i] and [i+1] where [i].start > [i+1].start, an overlap
            // exists when [i+1].end > [i].start (the earlier edit's range
            // extends into the later edit's range).
            for w in edits.windows(2) {
                if w[1].end > w[0].start {
                    anyhow::bail!(
                        "Conflicting edits in {}: byte ranges {}..{} and {}..{} overlap. \
                         Split into separate transactions.",
                        path.display(),
                        w[1].start,
                        w[1].end,
                        w[0].start,
                        w[0].end,
                    );
                }
            }

            self.file_edits.push(FileEdit { path, edits });
        }

        Ok(())
    }

    /// Produce a human-readable unified diff for all file edits without
    /// writing anything to disk.
    ///
    /// Reads each target file to get the original content, applies the
    /// planned edits in memory, and returns the combined diff string.
    ///
    /// # Errors
    /// Returns `Err` if any source file cannot be read or edits overlap.
    pub fn diff_str(&mut self) -> anyhow::Result<String> {
        self.merge_by_file()?;
        diff::diff_plan(self)
    }

    /// Apply all edits atomically and return the original file contents
    /// (needed for rollback).
    ///
    /// Edits within each file are re-sorted in reverse byte order before
    /// application to handle the offset-shift problem.
    ///
    /// # Errors
    /// Returns `Err` if any file cannot be read or written.
    pub fn apply(mut self) -> Result<TransformResult> {
        self.merge_by_file()?;

        let mut originals = HashMap::new();

        for file_edit in &mut self.file_edits {
            // Edits are already reverse-sorted by merge_by_file();
            // sort_reverse is a no-op here but kept for safety.
            file_edit.sort_reverse();

            // For new file creation (e.g. CHANGE FILE WITH content on a
            // non-existent file), treat the original as empty bytes.
            let original = if file_edit.path.exists() {
                crate::workspace::file_io::read_bytes(&file_edit.path)?
            } else {
                Vec::new()
            };
            let mut buf = original.clone();
            apply_edits_to_buffer(&mut buf, &file_edit.edits);
            crate::workspace::file_io::write_atomic(&file_edit.path, &buf)?;

            drop(originals.insert(file_edit.path.clone(), original));
        }

        Ok(TransformResult {
            transaction_name: None,
            originals,
        })
    }
}

/// What `apply()` returns — enough data to undo every write.
#[derive(Debug)]
pub struct TransformResult {
    pub transaction_name: Option<String>,
    /// Original raw bytes of each modified file.
    pub originals: HashMap<PathBuf, Vec<u8>>,
}

impl TransformResult {
    /// Restore every modified file to its original content.
    ///
    /// # Errors
    /// Returns `Err` if any file cannot be written back to its original bytes.
    pub fn rollback(self) -> Result<()> {
        for (path, original) in self.originals {
            crate::workspace::file_io::write_atomic(&path, &original)?;
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------
// plan_from_ir
// -----------------------------------------------------------------------

/// Produce a `TransformPlan` from a `ChangeContent` IR operation.
///
/// # Errors
/// Returns `Err` if the operation is not `ChangeContent`, or if planning fails.
pub fn plan_from_ir(
    op: &crate::ir::ForgeQLIR,
    ctx: &RequestContext,
    ws: &Workspace,
    idx: &SymbolTable,
) -> anyhow::Result<TransformPlan> {
    use crate::ir::ForgeQLIR;
    use change::ChangeFiles;

    match op {
        ForgeQLIR::ChangeContent { files, target, .. } => {
            ChangeFiles::new(files.clone(), target.clone()).plan(ctx, ws, idx)
        }
        other => anyhow::bail!("op {other:?} is not a mutation and cannot be planned"),
    }
}

// -----------------------------------------------------------------------
// Buffer editing utility
// -----------------------------------------------------------------------

/// Apply a sorted (reverse byte order!) list of edits to a byte buffer.
///
/// Edits MUST be sorted by `start` descending before calling this.
/// Panics in debug builds if edits overlap.
pub fn apply_edits_to_buffer(buf: &mut Vec<u8>, edits: &[ByteRangeEdit]) {
    for edit in edits {
        debug_assert!(edit.end <= buf.len(), "edit end beyond buffer length");
        drop(buf.splice(edit.start..edit.end, edit.replacement.bytes()));
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // --- apply_edits_to_buffer -------------------------------------------

    #[test]
    fn single_replacement() {
        let mut buf = b"void acenderLuz() {}".to_vec();
        // "acenderLuz" is at bytes 5..15
        let edits = vec![ByteRangeEdit::new(5..15, "turnOnLight")];
        apply_edits_to_buffer(&mut buf, &edits);
        assert_eq!(buf, b"void turnOnLight() {}");
    }

    #[test]
    fn deletion_empty_replacement() {
        let mut buf = b"foo bar baz".to_vec();
        let edits = vec![ByteRangeEdit::new(4..8, "")]; // delete "bar "
        apply_edits_to_buffer(&mut buf, &edits);
        assert_eq!(buf, b"foo baz");
    }

    #[test]
    fn insertion_zero_length_range() {
        let mut buf = b"foo baz".to_vec();
        let edits = vec![ByteRangeEdit::new(4..4, "bar ")]; // insert before "baz"
        apply_edits_to_buffer(&mut buf, &edits);
        assert_eq!(buf, b"foo bar baz");
    }

    #[test]
    fn multiple_edits_already_reverse_sorted() {
        // "old foo old" → "new foo new"
        // Edits MUST arrive in reverse byte order (caller responsibility).
        let mut buf = b"old foo old".to_vec();
        let edits = vec![
            ByteRangeEdit::new(8..11, "new"), // second "old" — higher offset first
            ByteRangeEdit::new(0..3, "new"),  // first "old"
        ];
        apply_edits_to_buffer(&mut buf, &edits);
        assert_eq!(buf, b"new foo new");
    }

    #[test]
    fn replacement_with_different_length() {
        // Replace a short token with a longer one; verify no offset drift.
        // "a + b" → "alpha + beta"
        let mut buf = b"a + b".to_vec();
        let edits = vec![
            ByteRangeEdit::new(4..5, "beta"), // 'b' → "beta" (higher offset first)
            ByteRangeEdit::new(0..1, "alpha"), // 'a' → "alpha"
        ];
        apply_edits_to_buffer(&mut buf, &edits);
        assert_eq!(buf, b"alpha + beta");
    }

    // --- FileEdit::sort_reverse ------------------------------------------

    #[test]
    fn sort_reverse_orders_descending() {
        let mut fe = FileEdit {
            path: "fake.cpp".into(),
            edits: vec![
                ByteRangeEdit::new(0..3, "a"),
                ByteRangeEdit::new(8..11, "b"),
                ByteRangeEdit::new(4..6, "c"),
            ],
        };
        fe.sort_reverse();
        assert_eq!(fe.edits[0].start, 8);
        assert_eq!(fe.edits[1].start, 4);
        assert_eq!(fe.edits[2].start, 0);
    }

    #[test]
    fn sort_then_apply_two_renames_same_file() {
        // Realistic scenario: rename "acenderLuz" in two locations.
        // Positions in "void acenderLuz(); acenderLuz();"
        //   first:  5..15
        //   second: 19..29
        let source = b"void acenderLuz(); acenderLuz();";
        let mut fe = FileEdit {
            path: "fake.cpp".into(),
            edits: vec![
                ByteRangeEdit::new(5..15, "turnOnLight"), // first occurrence
                ByteRangeEdit::new(19..29, "turnOnLight"), // second occurrence
            ],
        };
        fe.sort_reverse(); // puts 19..29 before 5..15
        let mut buf = source.to_vec();
        apply_edits_to_buffer(&mut buf, &fe.edits);
        assert_eq!(buf, b"void turnOnLight(); turnOnLight();");
    }

    // --- TransformPlan::merge_by_file ------------------------------------

    #[test]
    fn merge_by_file_combines_edits_for_same_path() {
        let mut plan = TransformPlan {
            file_edits: vec![
                FileEdit {
                    path: "file.cpp".into(),
                    edits: vec![ByteRangeEdit::new(0..3, "AAA")],
                },
                FileEdit {
                    path: "other.cpp".into(),
                    edits: vec![ByteRangeEdit::new(0..5, "BBB")],
                },
                FileEdit {
                    path: "file.cpp".into(),
                    edits: vec![ByteRangeEdit::new(10..15, "CCC")],
                },
            ],
            suggestions: vec![],
        };
        plan.merge_by_file().unwrap();
        // Two unique paths.
        assert_eq!(plan.file_edits.len(), 2);
        // The file.cpp entry should have 2 edits merged.
        let fe = plan
            .file_edits
            .iter()
            .find(|fe| fe.path == Path::new("file.cpp"))
            .unwrap();
        assert_eq!(fe.edits.len(), 2);
        // Edits must be sorted descending by start.
        assert!(fe.edits[0].start > fe.edits[1].start);
    }

    #[test]
    fn merge_by_file_detects_overlap() {
        let mut plan = TransformPlan {
            file_edits: vec![
                FileEdit {
                    path: "file.cpp".into(),
                    edits: vec![ByteRangeEdit::new(5..15, "X")],
                },
                FileEdit {
                    path: "file.cpp".into(),
                    edits: vec![ByteRangeEdit::new(10..20, "Y")], // overlaps 5..15
                },
            ],
            suggestions: vec![],
        };
        let result = plan.merge_by_file();
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("overlap"), "error must mention overlap: {msg}");
        assert!(msg.contains("file.cpp"), "error must mention file: {msg}");
    }

    #[test]
    fn merge_by_file_allows_adjacent_non_overlapping() {
        // Edits [0..5) and [5..10) are adjacent but not overlapping.
        let mut plan = TransformPlan {
            file_edits: vec![
                FileEdit {
                    path: "file.cpp".into(),
                    edits: vec![ByteRangeEdit::new(0..5, "A")],
                },
                FileEdit {
                    path: "file.cpp".into(),
                    edits: vec![ByteRangeEdit::new(5..10, "B")],
                },
            ],
            suggestions: vec![],
        };
        plan.merge_by_file().unwrap();
        let fe = &plan.file_edits[0];
        assert_eq!(fe.edits.len(), 2);
    }
}
