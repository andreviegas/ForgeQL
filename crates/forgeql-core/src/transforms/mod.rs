pub mod change;
pub mod copy_move;
pub mod diff;
use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;

use anyhow::Result;

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
    /// When true, `apply()` removes the file from disk instead of writing the
    /// edited buffer (`CHANGE FILE … WITH NOTHING`). The `edits` still carry
    /// the full-content removal so the boundary diff shows what was deleted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delete: bool,
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

    /// Total number of lines in all replacement texts across every edit.
    ///
    /// Used by the budget system to grant proportional recovery: the agent
    /// earns back lines for productive work (writing code), not just flat
    /// recovery from the rolling window.
    #[must_use]
    pub fn lines_written(&self) -> usize {
        self.file_edits
            .iter()
            .flat_map(|fe| &fe.edits)
            .map(|edit| {
                if edit.replacement.is_empty() {
                    0
                } else {
                    edit.replacement.lines().count()
                }
            })
            .sum()
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

        let mut by_path: HashMap<PathBuf, (Vec<ByteRangeEdit>, bool)> = HashMap::new();
        for fe in self.file_edits.drain(..) {
            let entry = by_path.entry(fe.path).or_default();
            entry.0.extend(fe.edits);
            // A whole-file deletion must survive the merge (BUG-014): any
            // FileEdit flagged delete keeps the merged edit a deletion.
            entry.1 |= fe.delete;
        }

        for (path, (mut edits, delete)) in by_path {
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

            self.file_edits.push(FileEdit {
                path,
                edits,
                delete,
            });
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
        let mut created = Vec::new();

        for file_edit in &mut self.file_edits {
            // Edits are already reverse-sorted by merge_by_file();
            // sort_reverse is a no-op here but kept for safety.
            file_edit.sort_reverse();

            // For new file creation (e.g. CHANGE FILE WITH content on a
            // non-existent file), treat the original as empty bytes.
            let existed = file_edit.path.exists();
            let original = if existed {
                crate::workspace::file_io::read_bytes(&file_edit.path)?
            } else {
                Vec::new()
            };
            if file_edit.delete {
                // `CHANGE FILE … WITH NOTHING`: remove the file instead of
                // writing an emptied buffer. `original` is kept for rollback.
                std::fs::remove_file(&file_edit.path)?;
            } else {
                if !existed {
                    // Ancestor directories the write is about to bring into
                    // existence are as much this plan's creation as the file
                    // itself — ROLLBACK must remove both, and only these
                    // (a directory that already existed is not ours to delete).
                    let mut missing = Vec::new();
                    let mut dir = file_edit.path.parent();
                    while let Some(d) = dir {
                        if d.as_os_str().is_empty() || d.exists() {
                            break;
                        }
                        missing.push(d.to_path_buf());
                        dir = d.parent();
                    }
                    created.extend(missing.into_iter().rev());
                    created.push(file_edit.path.clone());
                }
                let mut buf = original.clone();
                apply_edits_to_buffer(&mut buf, &file_edit.edits);
                crate::workspace::file_io::write_atomic(&file_edit.path, &buf)?;
            }

            drop(originals.insert(file_edit.path.clone(), original));
        }

        Ok(TransformResult {
            transaction_name: None,
            originals,
            created,
        })
    }
}

/// Total number of lines in the original spans replaced or deleted by a set of
/// edits, counted against the pre-edit `originals` captured by [`TransformPlan::apply`].
///
/// Symmetric to [`TransformPlan::lines_written`]: that counts replacement lines,
/// this counts the original lines each edit overwrote. The pair lets a caller
/// surface the net line delta of a mutation with no language knowledge — the
/// loudest mechanical signal that an edit destroyed more than it wrote.
#[must_use]
pub fn lines_removed<S: std::hash::BuildHasher>(
    edits: &[FileEdit],
    originals: &HashMap<PathBuf, Vec<u8>, S>,
) -> usize {
    edits
        .iter()
        .flat_map(|fe| fe.edits.iter().map(move |edit| (fe.path.as_path(), edit)))
        .map(|(path, edit)| {
            originals.get(path).map_or(0, |original| {
                let end = edit.end.min(original.len());
                let start = edit.start.min(end);
                let span = &original[start..end];
                if span.is_empty() {
                    0
                } else {
                    String::from_utf8_lossy(span).lines().count()
                }
            })
        })
        .sum()
}

/// Collect replacement edits for `pattern` restricted to a byte `range` of
/// `source` — the node- and line-scoped mechanical rename primitive.
///
/// Returns an empty vec when the pattern does not occur inside the range;
/// the caller decides whether that is an error.
///
/// # Errors
/// Returns `Err` when the range is out of bounds, the slice is not valid
/// UTF-8, or the WORD pattern cannot be compiled.
pub fn matching_edits_in_range(
    source: &[u8],
    pattern: &str,
    replacement: &str,
    word_boundary: bool,
    range: std::ops::Range<usize>,
) -> anyhow::Result<Vec<ByteRangeEdit>> {
    change::matching_edits_in_range(source, pattern, replacement, word_boundary, range)
}
/// What `apply()` returns — enough data to undo every write.
#[derive(Debug)]
pub struct TransformResult {
    pub transaction_name: Option<String>,
    /// Original raw bytes of each modified file.
    pub originals: HashMap<PathBuf, Vec<u8>>,
    /// Files this plan brought into existence.
    ///
    /// `git reset --hard` restores only tracked paths, and staging is deferred
    /// to COMMIT — so a file created inside a transaction is untracked when
    /// ROLLBACK runs and the reset walks straight past it. Surfacing the fact
    /// here lets the transaction layer remove them itself. (A blanket
    /// `git clean` would be wrong: it would also delete the user's unrelated
    /// untracked files.)
    pub created: Vec<PathBuf>,
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
pub fn plan_from_ir(op: &crate::ir::ForgeQLIR, ws: &Workspace) -> anyhow::Result<TransformPlan> {
    use crate::ir::ForgeQLIR;
    use change::ChangeFiles;

    match op {
        ForgeQLIR::ChangeContent { files, target, .. } => {
            ChangeFiles::new(files.clone(), target.clone()).plan(ws)
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
            delete: false,
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
            delete: false,
        };
        fe.sort_reverse(); // puts 19..29 before 5..15
        let mut buf = source.to_vec();
        apply_edits_to_buffer(&mut buf, &fe.edits);
        assert_eq!(buf, b"void turnOnLight(); turnOnLight();");
    }

    // --- lines_removed ---------------------------------------------------

    #[test]
    fn lines_removed_counts_original_span_lines() {
        // A node spanning 4 source lines replaced by a 1-line signature: the
        // signal must report the 4 original lines regardless of how few were
        // written back — the footgun that silently deletes a function body.
        let original = b"fn f() {\n    a;\n    b;\n}\n".to_vec();
        let span_len = original.len() - 1; // exclude the trailing newline
        let edits = vec![FileEdit {
            path: PathBuf::from("fake.rs"),
            edits: vec![ByteRangeEdit::new(0..span_len, "fn f() {")],
            delete: false,
        }];
        let mut originals: HashMap<PathBuf, Vec<u8>> = HashMap::new();
        drop(originals.insert(PathBuf::from("fake.rs"), original));
        assert_eq!(lines_removed(&edits, &originals), 4);
    }

    #[test]
    fn lines_removed_is_zero_for_pure_insertion() {
        // A zero-length range inserts without overwriting: nothing is removed.
        let mut originals: HashMap<PathBuf, Vec<u8>> = HashMap::new();
        drop(originals.insert(PathBuf::from("fake.rs"), b"a\nb\n".to_vec()));
        let edits = vec![FileEdit {
            path: PathBuf::from("fake.rs"),
            edits: vec![ByteRangeEdit::new(2..2, "x\n")],
            delete: false,
        }];
        assert_eq!(lines_removed(&edits, &originals), 0);
    }

    // --- TransformPlan::merge_by_file ------------------------------------

    #[test]
    fn merge_by_file_combines_edits_for_same_path() {
        let mut plan = TransformPlan {
            file_edits: vec![
                FileEdit {
                    path: "file.cpp".into(),
                    edits: vec![ByteRangeEdit::new(0..3, "AAA")],
                    delete: false,
                },
                FileEdit {
                    path: "other.cpp".into(),
                    edits: vec![ByteRangeEdit::new(0..5, "BBB")],
                    delete: false,
                },
                FileEdit {
                    path: "file.cpp".into(),
                    edits: vec![ByteRangeEdit::new(10..15, "CCC")],
                    delete: false,
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
    fn merge_by_file_preserves_delete_flag() {
        // BUG-014 regression: merge_by_file used to rebuild FileEdits with a
        // hardcoded `delete: false`, silently downgrading a whole-file
        // deletion into a truncation.
        let mut plan = TransformPlan {
            file_edits: vec![FileEdit {
                path: "gone.cpp".into(),
                edits: vec![ByteRangeEdit::new(0..5, "")],
                delete: true,
            }],
            suggestions: Vec::new(),
        };
        plan.merge_by_file().expect("merge should succeed");
        assert_eq!(plan.file_edits.len(), 1);
        assert!(
            plan.file_edits[0].delete,
            "delete flag must survive merge_by_file"
        );
    }

    #[test]
    fn merge_by_file_detects_overlap() {
        let mut plan = TransformPlan {
            file_edits: vec![
                FileEdit {
                    path: "file.cpp".into(),
                    edits: vec![ByteRangeEdit::new(5..15, "X")],
                    delete: false,
                },
                FileEdit {
                    path: "file.cpp".into(),
                    edits: vec![ByteRangeEdit::new(10..20, "Y")], // overlaps 5..15
                    delete: false,
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
                    delete: false,
                },
                FileEdit {
                    path: "file.cpp".into(),
                    edits: vec![ByteRangeEdit::new(5..10, "B")],
                    delete: false,
                },
            ],
            suggestions: vec![],
        };
        plan.merge_by_file().unwrap();
        let fe = &plan.file_edits[0];
        assert_eq!(fe.edits.len(), 2);
    }

    #[test]
    fn lines_written_counts_replacement_lines() {
        let plan = TransformPlan {
            file_edits: vec![FileEdit {
                path: "file.cpp".into(),
                edits: vec![
                    ByteRangeEdit::new(0..10, "line1\nline2\nline3\n"),
                    ByteRangeEdit::new(20..30, "single_line"),
                ],
                delete: false,
            }],
            suggestions: vec![],
        };
        // 3 lines from first edit + 1 from second = 4
        assert_eq!(plan.lines_written(), 4);
    }

    #[test]
    fn lines_written_deletion_is_zero() {
        let plan = TransformPlan {
            file_edits: vec![FileEdit {
                path: "file.cpp".into(),
                edits: vec![ByteRangeEdit::new(0..10, "")],
                delete: false,
            }],
            suggestions: vec![],
        };
        assert_eq!(plan.lines_written(), 0);
    }
}
