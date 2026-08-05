//! Unit tests for the `CHANGE FILE[S]` transform: line-to-byte-range conversion
//! including CRLF and mixed endings, trailing-blank absorption, the multi-file
//! validation rules, `MATCHING` resolution with and without word boundaries,
//! line splices and their trailing newline, glob detection, and DELETE.

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
fn absorb_trailing_blank_lines_extends_over_blank_run() {
    // No trailing blank → unchanged.
    assert_eq!(super::absorb_trailing_blank_lines("a\nb\nc", 1), 1);
    // One trailing blank after line 1 → absorbs it (end 1 → 2).
    assert_eq!(super::absorb_trailing_blank_lines("a\n\nc", 1), 2);
    // Multiple trailing blanks → absorbs the whole run.
    assert_eq!(super::absorb_trailing_blank_lines("a\n\n\n\nc", 1), 4);
    // Node is the last line → nothing to absorb.
    assert_eq!(super::absorb_trailing_blank_lines("a", 1), 1);
    // Trailing blank at EOF.
    assert_eq!(super::absorb_trailing_blank_lines("a\n\n", 1), 2);
    // Whitespace-only line counts as blank.
    assert_eq!(super::absorb_trailing_blank_lines("a\n   \nc", 1), 2);
}

#[test]
fn validate_multi_file_matching_ok() {
    let files: Vec<String> = vec!["a.cpp".into(), "b.cpp".into()];
    let target = ChangeTarget::Matching {
        pattern: "x".into(),
        replacement: "y".into(),
        word_boundary: false,
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
            word_boundary: false,
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

    let fe = resolve_matching("buttons.cpp", &path, "Button", "PushButton", false).unwrap();

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

    let fe = resolve_matching("single.cpp", &path, "oldName", "newName", false).unwrap();
    assert_eq!(fe.edits.len(), 1);
    assert_eq!(fe.edits[0].replacement, "newName");
}

#[test]
fn resolve_matching_not_found_returns_empty_edits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("miss.cpp");
    std::fs::write(&path, "nothing here").expect("write");
    let fe = resolve_matching("miss.cpp", &path, "nonexistent", "x", false)
        .expect("should succeed with empty edits");
    assert!(fe.edits.is_empty());
}

// ── MATCHING WORD boundary ─────────────────────────────────────────

#[test]
fn resolve_matching_word_boundary_skips_compound_terms() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("compound.cpp");
    std::fs::write(
        &path,
        "field_declaration foo;\nvoid declaration() {}\nint declaration = 0;\n",
    )
    .expect("write");

    let fe = resolve_matching("compound.cpp", &path, "declaration", "variable", true).unwrap();

    // Only the standalone "declaration" occurrences should be replaced,
    // NOT the "declaration" inside "field_declaration".
    assert_eq!(
        fe.edits.len(),
        2,
        "expected 2 edits (standalone only), got {}: {fe:?}",
        fe.edits.len()
    );
}

#[test]
fn resolve_matching_word_boundary_false_replaces_all() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("compound2.cpp");
    std::fs::write(&path, "field_declaration foo;\nvoid declaration() {}\n").expect("write");

    let fe = resolve_matching("compound2.cpp", &path, "declaration", "variable", false).unwrap();

    // Without WORD, all 3 occurrences (including inside compound) are replaced.
    assert_eq!(
        fe.edits.len(),
        2,
        "expected 2 edits (all substrings), got {}: {fe:?}",
        fe.edits.len()
    );
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
// -- is_glob ----------------------------------------------------------

#[test]
fn is_glob_double_star_is_glob() {
    assert!(is_glob("src/**/*.rs"), "** must be recognised as a glob");
}

#[test]
fn is_glob_question_mark_is_glob() {
    assert!(is_glob("file?.rs"), "? must be recognised as a glob");
}

#[test]
fn is_glob_bracket_char_class_is_glob() {
    assert!(is_glob("[abc].rs"), "[ must be recognised as a glob");
}

#[test]
fn is_glob_plain_path_is_not_glob() {
    assert!(!is_glob("src/foo/bar.rs"), "plain path is not a glob");
}

#[test]
fn is_glob_empty_string_is_not_glob() {
    assert!(!is_glob(""), "empty string is not a glob");
}

// -- lines_to_byte_range CRLF -----------------------------------------

#[test]
fn lines_to_byte_range_crlf_endings() {
    // Windows CRLF: each line ends with \r\n
    let source = b"first\r\nsecond\r\nthird\r\n";
    // Line 2 should cover "second\r\n"
    let (start, end) = lines_to_byte_range(source, 2, 2).unwrap();
    let slice = &source[start..end];
    assert_eq!(slice, b"second\r\n");
}

#[test]
fn lines_to_byte_range_mixed_lf_crlf() {
    // Mixed line endings — only \n triggers line count increment
    let source = b"lf\ncrlf\r\nend\n";
    // Line 1 = "lf\n", line 2 = "crlf\r\n", line 3 = "end\n"
    let (start, end) = lines_to_byte_range(source, 2, 2).unwrap();
    assert_eq!(&source[start..end], b"crlf\r\n");
}

// -- resolve_delete ---------------------------------------------------

#[test]
fn resolve_delete_creates_full_clear_edit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("target.cpp");
    let content = "first line\nsecond line\n";
    std::fs::write(&path, content).expect("write");

    let fe = resolve_delete("target.cpp", &path).unwrap();
    assert_eq!(fe.edits.len(), 1);
    // The single edit should span the whole file (0..content.len()).
    assert_eq!(fe.edits[0].start, 0);
    assert_eq!(fe.edits[0].end, content.len());
    assert_eq!(fe.edits[0].replacement, "");
}

#[test]
fn resolve_delete_nonexistent_file_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("ghost.cpp");
    // File was never created.
    let result = resolve_delete("ghost.cpp", &path);
    assert!(result.is_err(), "missing file must return an error");
}
