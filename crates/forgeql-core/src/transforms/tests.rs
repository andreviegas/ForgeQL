//! Unit tests for the shared transform plumbing: applying an edit set to a
//! buffer, reverse-sorting edits so earlier offsets stay valid while later ones
//! are rewritten, merging edits by file and detecting overlaps, and the
//! `lines_written` / `lines_removed` counts a mutation reports back.

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
