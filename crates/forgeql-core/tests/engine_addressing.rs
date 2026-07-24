//! Integration tests for bare-hex file/directory handles and node addressing.
//!
//! Split out of `engine_integration`: the `n<hex>` file-handle and directory-rev
//! addressing cases — resolving, reading, and mutating files and directories by
//! their stable hex handles.
//!
//! Run with: `cargo test -p forgeql-core --test engine_addressing`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    // panic! is the normal way to fail a test assertion
    clippy::panic,
    // helper functions defined inside test bodies after let-statements
    clippy::items_after_statements,
    // doc comments in tests don't need exhaustive backtick coverage
    clippy::doc_markdown
)]

use std::fs;

use forgeql_core::result::ForgeQLResult;

mod common;

use common::{engine_with_session, execute_fql, node_rev, try_fql};

#[test]
fn bare_hex_resolves_the_whole_file() {
    let (mut engine, sid, dir) = engine_with_session();
    fs::write(dir.path().join("notes.txt"), "one\ntwo\nthree\n").unwrap();

    let handle = common::path_handle("notes.txt");
    match execute_fql(&mut engine, &sid, &format!("FIND NODE '{handle}'")) {
        ForgeQLResult::FindNode(node) => {
            assert_eq!(node.fql_kind, "file");
            assert_eq!(node.name, "notes.txt");
            assert_eq!((node.line, node.end_line), (1, 3));
            assert!(node.rev.starts_with('h'), "file rows carry a rev: {node:?}");
        }
        other => panic!("expected FindNode, got {other:?}"),
    }
}

#[test]
fn bare_hex_under_twelve_chars_is_not_a_handle() {
    let (mut engine, sid, _dir) = engine_with_session();
    // Otherwise an ordinary all-hex symbol name (`nadd`, `nbeef`) would parse as
    // a file handle wherever a name and a node_id are both accepted.
    let err = try_fql(&mut engine, &sid, "FIND NODE 'nbeef'").unwrap_err();
    assert!(
        err.to_string().contains("invalid node_id format"),
        "got: {err}"
    );
}

#[test]
fn bare_hex_offset_reads_a_line_range_of_the_file() {
    let (mut engine, sid, dir) = engine_with_session();
    fs::write(dir.path().join("notes.txt"), "one\ntwo\nthree\n").unwrap();

    let handle = common::path_handle("notes.txt");
    match execute_fql(&mut engine, &sid, &format!("SHOW NODE '{handle}(2-3)'")) {
        ForgeQLResult::Show(show) => {
            let text = format!("{show}");
            assert!(text.contains("two") && text.contains("three"), "{text}");
            assert!(!text.contains("one"), "offset must not read line 1: {text}");
        }
        other => panic!("expected Show, got {other:?}"),
    }
}

#[test]
fn delete_node_bare_hex_requires_if_rev() {
    let (mut engine, sid, dir) = engine_with_session();
    let path = dir.path().join("notes.txt");
    fs::write(&path, "one\n").unwrap();

    let handle = common::path_handle("notes.txt");
    let err = try_fql(&mut engine, &sid, &format!("DELETE NODE '{handle}'")).unwrap_err();
    assert!(err.to_string().contains("requires IF REV"), "got: {err}");
    assert!(path.exists(), "the refused delete must not touch the file");
}

#[test]
fn delete_node_bare_hex_unlinks_the_file_rather_than_emptying_it() {
    let (mut engine, sid, dir) = engine_with_session();
    let path = dir.path().join("notes.txt");
    fs::write(&path, "one\ntwo\n").unwrap();

    let handle = common::path_handle("notes.txt");
    let rev = node_rev(&mut engine, &sid, &handle);
    let _ = execute_fql(
        &mut engine,
        &sid,
        &format!("DELETE NODE '{handle}' IF REV '{rev}'"),
    );

    // The node-delete lowering (replace the line span with "") would leave a
    // 0-byte ghost here. The file has to be gone from disk and from the index.
    assert!(!path.exists(), "whole-file DELETE must unlink the file");
    let err = try_fql(&mut engine, &sid, &format!("FIND NODE '{handle}'")).unwrap_err();
    assert!(err.to_string().contains("not found"), "got: {err}");
}

#[test]
fn change_node_bare_hex_requires_if_rev() {
    let (mut engine, sid, dir) = engine_with_session();
    let path = dir.path().join("notes.txt");
    fs::write(&path, "one\ntwo\n").unwrap();

    let handle = common::path_handle("notes.txt");
    let err = try_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{handle}' WITH 'wiped'"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("requires IF REV"), "got: {err}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");

    let rev = node_rev(&mut engine, &sid, &handle);
    let _ = execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{handle}' IF REV '{rev}' WITH 'wiped'"),
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "wiped\n");
}

#[test]
fn insert_around_bare_hex_prepends_at_bof_and_appends_at_eof() {
    let (mut engine, sid, dir) = engine_with_session();
    let path = dir.path().join("notes.txt");
    fs::write(&path, "middle\n").unwrap();

    let handle = common::path_handle("notes.txt");
    // Creating is not destructive, so neither form needs a rev.
    let _ = execute_fql(
        &mut engine,
        &sid,
        &format!("INSERT BEFORE NODE '{handle}' WITH 'first'"),
    );
    let _ = execute_fql(
        &mut engine,
        &sid,
        &format!("INSERT AFTER NODE '{handle}' WITH 'last'"),
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "first\nmiddle\nlast\n",
        "BEFORE is BOF, AFTER is EOF"
    );
}

#[test]
fn insert_after_bare_hex_writes_into_an_empty_file() {
    let (mut engine, sid, dir) = engine_with_session();
    let path = dir.path().join("empty.txt");
    fs::write(&path, "").unwrap();

    // The create-then-write bootstrap: a 0-byte file has no lines to map, but
    // line 1 of it must still be a valid insert target.
    let handle = common::path_handle("empty.txt");
    let _ = execute_fql(
        &mut engine,
        &sid,
        &format!("INSERT AFTER NODE '{handle}' WITH '# Title'"),
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "# Title\n");
}

#[test]
fn dir_rev_tracks_membership_not_content() {
    let (mut engine, sid, dir) = engine_with_session();
    fs::create_dir_all(dir.path().join("pkg/deep")).unwrap();
    fs::write(dir.path().join("pkg/a.txt"), "a\n").unwrap();

    let handle = common::path_handle("pkg");
    let before = node_rev(&mut engine, &sid, &handle);
    assert!(before.starts_with('h'));

    // Editing a file underneath does not move the directory rev: content
    // staleness is what the per-file rev is for.
    fs::write(dir.path().join("pkg/a.txt"), "a much longer body\n").unwrap();
    assert_eq!(node_rev(&mut engine, &sid, &handle), before);

    // Adding a file deep in the subtree does move it — otherwise a recursive
    // delete could destroy a file the agent never saw listed.
    fs::write(dir.path().join("pkg/deep/b.txt"), "b\n").unwrap();
    assert_ne!(node_rev(&mut engine, &sid, &handle), before);
}

#[test]
fn delete_node_dir_hex_removes_the_whole_subtree() {
    let (mut engine, sid, dir) = engine_with_session();
    fs::create_dir_all(dir.path().join("pkg/deep")).unwrap();
    fs::write(dir.path().join("pkg/a.txt"), "a\n").unwrap();
    fs::write(dir.path().join("pkg/deep/b.txt"), "b\n").unwrap();

    let handle = common::path_handle("pkg");
    let err = try_fql(&mut engine, &sid, &format!("DELETE NODE '{handle}'")).unwrap_err();
    assert!(err.to_string().contains("requires IF REV"), "got: {err}");

    let rev = node_rev(&mut engine, &sid, &handle);
    let _ = execute_fql(
        &mut engine,
        &sid,
        &format!("DELETE NODE '{handle}' IF REV '{rev}'"),
    );
    assert!(
        !dir.path().join("pkg").exists(),
        "the emptied directories go too"
    );
}
