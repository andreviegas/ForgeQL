//! Unit tests for the COPY and MOVE transforms: appending, inserting before a
//! target line, and copying to a destination that does not yet exist; moving
//! across files and within one file in both directions, including a wider
//! delete-end that absorbs the source's trailing blanks; the errors for a
//! destination inside the source range and for a delete-end before the end;
//! and how an insertion line resolves to a byte offset.

use super::*;
use std::path::PathBuf;

use crate::transforms::apply_edits_to_buffer;

fn apply_plan_to_bytes(mut plan: TransformPlan, original: &[u8]) -> Vec<u8> {
    plan.merge_by_file().expect("merge");
    let fe = plan.file_edits.into_iter().next().unwrap();
    let mut buf = original.to_vec();
    apply_edits_to_buffer(&mut buf, &fe.edits);
    buf
}

// Helper: synthetic file bytes for "line1\nline2\n...lineN\n"
fn make_file(n: usize) -> Vec<u8> {
    use std::fmt::Write;
    (1..=n)
        .fold(String::new(), |mut s, i| {
            writeln!(s, "line{i}").unwrap();
            s
        })
        .into_bytes()
}

#[test]
fn copy_appends_when_no_at() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src.txt");
    std::fs::write(&src, make_file(5)).unwrap();
    let dst = tmp.path().join("dst.txt");
    std::fs::write(&dst, make_file(3)).unwrap();

    let plan = plan_copy_lines("src.txt", &src, 2, 3, &dst).unwrap();
    let original = std::fs::read(&dst).unwrap();
    let result = apply_plan_to_bytes(plan, &original);

    // dst had lines 1-3; copy appends lines 2-3 from src → lines 1-3, line2, line3
    let expected = b"line1\nline2\nline3\nline2\nline3\n";
    assert_eq!(result, expected);
}

#[test]
fn copy_at_line_inserts_before_target() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src.txt");
    std::fs::write(&src, make_file(5)).unwrap();
    let dst = tmp.path().join("dst.txt");
    std::fs::write(&dst, make_file(4)).unwrap();

    // Copy lines 4-5 of src before line 2 of dst
    let plan = plan_copy_lines_at("src.txt", &src, 4, 5, &dst, 2).unwrap();
    let original = std::fs::read(&dst).unwrap();
    let result = apply_plan_to_bytes(plan, &original);

    let expected = b"line1\nline4\nline5\nline2\nline3\nline4\n";
    assert_eq!(result, expected);
}

#[test]
fn move_different_files_removes_source_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src.txt");
    std::fs::write(&src, make_file(5)).unwrap();
    let dst = tmp.path().join("dst.txt");
    std::fs::write(&dst, make_file(3)).unwrap();

    let mut plan = plan_move_lines("src.txt", &src, 2, 4, 4, &dst, None).unwrap();
    plan.merge_by_file().unwrap();

    // Apply each file's edits separately (apply() writes to disk; test manually)
    for fe in &mut plan.file_edits {
        fe.sort_reverse();
    }
    // Find dst edit and src edit
    let (src_edit, dst_edit): (Vec<_>, Vec<_>) = plan
        .file_edits
        .into_iter()
        .partition(|fe| fe.path == src.as_path());

    // Apply dst
    let mut dst_buf = std::fs::read(&dst).unwrap();
    if let Some(fe) = dst_edit.into_iter().next() {
        apply_edits_to_buffer(&mut dst_buf, &fe.edits);
    }
    // Apply src
    let mut src_buf = std::fs::read(&src).unwrap();
    if let Some(fe) = src_edit.into_iter().next() {
        apply_edits_to_buffer(&mut src_buf, &fe.edits);
    }

    assert_eq!(dst_buf, b"line1\nline2\nline3\nline2\nline3\nline4\n");
    assert_eq!(src_buf, b"line1\nline5\n");
}

#[test]
fn move_same_file_down() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("f.txt");
    // 5 lines: line1..line5
    std::fs::write(&f, make_file(5)).unwrap();

    // Move lines 1-2 to AT LINE 5 (before original line5)
    let mut plan = plan_move_lines("f.txt", &f, 1, 2, 2, &f, Some(5)).unwrap();
    plan.merge_by_file().unwrap();

    let fe = plan.file_edits.into_iter().next().unwrap();
    let mut buf = std::fs::read(&f).unwrap();
    apply_edits_to_buffer(&mut buf, &fe.edits);

    // Expected: line3, line4, line1, line2, line5
    assert_eq!(buf, b"line3\nline4\nline1\nline2\nline5\n");
}

#[test]
fn move_same_file_up() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("f.txt");
    std::fs::write(&f, make_file(5)).unwrap();

    // Move lines 4-5 to AT LINE 2 (before original line2)
    let mut plan = plan_move_lines("f.txt", &f, 4, 5, 5, &f, Some(2)).unwrap();
    plan.merge_by_file().unwrap();

    let fe = plan.file_edits.into_iter().next().unwrap();
    let mut buf = std::fs::read(&f).unwrap();
    apply_edits_to_buffer(&mut buf, &fe.edits);

    // Expected: line1, line4, line5, line2, line3
    assert_eq!(buf, b"line1\nline4\nline5\nline2\nline3\n");
}

#[test]
fn move_same_file_inside_range_is_error() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("f.txt");
    std::fs::write(&f, make_file(5)).unwrap();

    // Trying to move lines 2-4 AT LINE 3 (inside the range) must fail
    let result = plan_move_lines("f.txt", &f, 2, 4, 4, &f, Some(3));
    assert!(result.is_err());
}

#[test]
fn move_wider_delete_end_absorbs_source_blanks() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src.txt");
    // A "node" on line 3 with a blank separator on line 4.
    std::fs::write(&src, b"line1\n\nline3\n\nline5\n").unwrap();
    let dst = tmp.path().join("dst.txt");
    std::fs::write(&dst, b"").unwrap();

    // Payload is line 3 only; the removed range 3..=4 absorbs the blank.
    let mut plan = plan_move_lines("src.txt", &src, 3, 3, 4, &dst, None).unwrap();
    plan.merge_by_file().unwrap();

    let (src_edit, dst_edit): (Vec<_>, Vec<_>) = plan
        .file_edits
        .into_iter()
        .partition(|fe| fe.path == src.as_path());

    let mut dst_buf = std::fs::read(&dst).unwrap();
    if let Some(fe) = dst_edit.into_iter().next() {
        apply_edits_to_buffer(&mut dst_buf, &fe.edits);
    }
    let mut src_buf = std::fs::read(&src).unwrap();
    if let Some(fe) = src_edit.into_iter().next() {
        apply_edits_to_buffer(&mut src_buf, &fe.edits);
    }

    // Payload stays the node's exact span — the blank does not travel.
    assert_eq!(dst_buf, b"line3\n");
    // No blank-line accumulation: exactly one separator survives.
    assert_eq!(src_buf, b"line1\n\nline5\n");
}

#[test]
fn move_delete_end_smaller_than_end_is_error() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("f.txt");
    std::fs::write(&f, make_file(5)).unwrap();
    assert!(plan_move_lines("f.txt", &f, 2, 4, 3, &f, None).is_err());
}

#[test]
fn insertion_byte_offset_append() {
    let bytes = b"a\nb\nc\n";
    assert_eq!(insertion_byte_offset(bytes, None), 6);
}

#[test]
fn insertion_byte_offset_line1() {
    let bytes = b"a\nb\nc\n";
    assert_eq!(insertion_byte_offset(bytes, Some(1)), 0);
}

#[test]
fn insertion_byte_offset_line3() {
    let bytes = b"a\nb\nc\n";
    // line 3 starts after "a\nb\n" = byte 4
    assert_eq!(insertion_byte_offset(bytes, Some(3)), 4);
}

#[test]
fn insertion_byte_offset_beyond_eof() {
    let bytes = b"a\nb\n";
    // line 99 is beyond the file → return len
    assert_eq!(insertion_byte_offset(bytes, Some(99)), 4);
}

#[test]
fn copy_to_nonexistent_dst_creates_content() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src.txt");
    std::fs::write(&src, make_file(3)).unwrap();
    let dst = PathBuf::from("/nonexistent/path/dst.txt"); // doesn't exist → empty

    // plan_copy_lines reads dst_abs.exists() → false → dst_bytes = []
    // We can't call read_bytes on non-existent, but the plan should work for
    // the insertion offset calculation (appends to empty = offset 0).
    let plan = plan_copy_lines("src.txt", &src, 1, 2, &dst).unwrap();
    let result = apply_plan_to_bytes(plan, b"");
    assert_eq!(result, b"line1\nline2\n");
}
