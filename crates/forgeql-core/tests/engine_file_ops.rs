//! Integration tests for file and directory lifecycle operations.
//!
//! Split out of `engine_integration`: creating files and directories
//! (`INSERT NODE FOR`), renaming and moving them (`MOVE NODE … TO`), copying
//! them (`COPY NODE … TO`), and listing them (`FIND files`) — the mutations
//! that add, relocate, or enumerate whole files and directories as nodes.
//!
//! Run with: `cargo test -p forgeql-core --test engine_file_ops`
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
fn find_files_hands_out_a_handle_and_a_rev_per_row() {
    let (mut engine, sid, dir) = engine_with_session();
    fs::create_dir_all(dir.path().join("pkg")).unwrap();
    fs::write(dir.path().join("pkg/a.txt"), "a\n").unwrap();

    match execute_fql(&mut engine, &sid, "FIND files WHERE path LIKE 'pkg%'") {
        ForgeQLResult::Show(show) => {
            let text = format!("{show}");
            // Every listed row is actionable as it stands — the handle and the
            // rev are both there, so DELETE NODE ... IF REV is one round trip.
            assert!(text.contains(&common::path_handle("pkg/a.txt")), "{text}");
            assert!(text.contains(&common::path_handle("pkg")), "{text}");
            // Directories are marked by a trailing slash on the path — no extra
            // column, and `WHERE path LIKE '%/'` selects them.
            assert!(text.contains("pkg/\n") || text.contains("pkg/ "), "{text}");
        }
        other => panic!("expected Show, got {other:?}"),
    }
}

#[test]
fn insert_node_for_creates_an_empty_file_and_returns_its_handle() {
    let (mut engine, sid, dir) = engine_with_session();

    match execute_fql(&mut engine, &sid, "INSERT NODE FOR 'notes/readme.md'") {
        ForgeQLResult::Mutation(m) => {
            assert_eq!(
                m.new_node_id.as_deref(),
                Some(common::path_handle("notes/readme.md").as_str())
            );
        }
        other => panic!("expected Mutation, got {other:?}"),
    }
    let path = dir.path().join("notes/readme.md");
    assert_eq!(fs::read_to_string(&path).unwrap(), "", "created empty");

    // The bootstrap: create, then write into it by handle.
    let handle = common::path_handle("notes/readme.md");
    let _ = execute_fql(
        &mut engine,
        &sid,
        &format!("INSERT AFTER NODE '{handle}' WITH '# Title'"),
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "# Title\n");

    // It will not clobber an existing path.
    let err = try_fql(&mut engine, &sid, "INSERT NODE FOR 'notes/readme.md'").unwrap_err();
    assert!(err.to_string().contains("already exists"), "got: {err}");
}

#[test]
fn insert_node_for_trailing_slash_creates_a_directory() {
    let (mut engine, sid, dir) = engine_with_session();
    let _ = execute_fql(&mut engine, &sid, "INSERT NODE FOR 'docs/'");
    assert!(dir.path().join("docs").is_dir());
}

#[test]
fn move_node_to_path_renames_the_file_and_keeps_its_rev() {
    let (mut engine, sid, dir) = engine_with_session();
    fs::write(dir.path().join("old.txt"), "body\n").unwrap();

    let handle = common::path_handle("old.txt");
    let rev = node_rev(&mut engine, &sid, &handle);
    match execute_fql(
        &mut engine,
        &sid,
        &format!("MOVE NODE '{handle}' IF REV '{rev}' TO 'new.txt'"),
    ) {
        ForgeQLResult::Mutation(m) => {
            // The handle is path-derived, so a rename earns a new one.
            assert_eq!(
                m.new_node_id.as_deref(),
                Some(common::path_handle("new.txt").as_str())
            );
        }
        other => panic!("expected Mutation, got {other:?}"),
    }
    assert!(
        !dir.path().join("old.txt").exists(),
        "the source is unlinked, not emptied"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("new.txt")).unwrap(),
        "body\n"
    );
    // Same bytes at the new path, so the same rev.
    assert_eq!(
        node_rev(&mut engine, &sid, &common::path_handle("new.txt")),
        rev
    );
}

#[test]
fn move_node_to_requires_if_rev_and_refuses_an_existing_destination() {
    let (mut engine, sid, dir) = engine_with_session();
    fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    fs::write(dir.path().join("b.txt"), "b\n").unwrap();

    let handle = common::path_handle("a.txt");
    let err = try_fql(
        &mut engine,
        &sid,
        &format!("MOVE NODE '{handle}' TO 'c.txt'"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("requires IF REV"), "got: {err}");

    let rev = node_rev(&mut engine, &sid, &handle);
    let err = try_fql(
        &mut engine,
        &sid,
        &format!("MOVE NODE '{handle}' IF REV '{rev}' TO 'b.txt'"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("already exists"), "got: {err}");
    assert_eq!(fs::read_to_string(dir.path().join("b.txt")).unwrap(), "b\n");
}

#[test]
fn copy_node_to_a_directory_handle_keeps_the_basename() {
    let (mut engine, sid, dir) = engine_with_session();
    fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    fs::create_dir_all(dir.path().join("pkg")).unwrap();

    let file = common::path_handle("a.txt");
    let pkg = common::path_handle("pkg");
    // Creation only, so no rev needed.
    let _ = execute_fql(&mut engine, &sid, &format!("COPY NODE '{file}' TO '{pkg}'"));
    assert_eq!(
        fs::read_to_string(dir.path().join("pkg/a.txt")).unwrap(),
        "a\n"
    );
    assert!(
        dir.path().join("a.txt").exists(),
        "COPY leaves the source alone"
    );
}

#[test]
fn copy_node_to_a_trailing_slash_path_is_the_same_as_a_directory_handle() {
    let (mut engine, sid, dir) = engine_with_session();
    fs::write(dir.path().join("a.txt"), "a\n").unwrap();

    // `api/v2/` does not exist yet — a path is the only way to name it, which is
    // exactly why the TO argument accepts one.
    let file = common::path_handle("a.txt");
    let _ = execute_fql(
        &mut engine,
        &sid,
        &format!("COPY NODE '{file}' TO 'api/v2/'"),
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("api/v2/a.txt")).unwrap(),
        "a\n"
    );
}
