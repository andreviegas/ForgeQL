//! Integration tests for `ForgeQLEngine::execute()`.
//!
//! These tests exercise the full engine dispatch path — parser → IR → engine
//! → result — using the `motor_control` C++ fixtures in a temp workspace.
//!
//! Run with: `cargo test -p forgeql-core --test engine_integration`
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

use forgeql_core::auth::{AuthContext, auth};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::ir::{Backend, Clauses, ForgeQLIR};
use forgeql_core::parser;
use forgeql_core::result::{ForgeQLResult, ShowContent};
use forgeql_core::session::SessionCoords;
use tempfile::tempdir;

mod common;

// -----------------------------------------------------------------------
// Helpers — the shared harness lives in `tests/common`; these thin adapters
// keep the `(engine, session_id, TempDir)` tuple idiom this suite's bodies use.
// -----------------------------------------------------------------------

/// Create a temp workspace with `motor_control` fixtures (plus any extras) and
/// boot a columnar session over it. Returns `(engine, session_id, TempDir)`;
/// the `TempDir` must stay alive.
fn engine_with_session_with_extra_files(
    extra_files: &[&str],
) -> (ForgeQLEngine, String, tempfile::TempDir) {
    let mut fixtures = vec!["motor_control.h", "motor_control.cpp"];
    fixtures.extend_from_slice(extra_files);
    common::columnar_session_real(&fixtures).into_parts()
}

fn engine_with_session() -> (ForgeQLEngine, String, tempfile::TempDir) {
    engine_with_session_with_extra_files(&[])
}

/// Legacy-backend variant of `engine_with_session`. Pins tests that document
/// known legacy/columnar behaviour divergences; flip each caller back to
/// `engine_with_session` once the columnar side is fixed.
fn engine_with_session_legacy() -> (ForgeQLEngine, String, tempfile::TempDir) {
    common::legacy_session_real(&["motor_control.h", "motor_control.cpp"]).into_parts()
}
/// Parse FQL and execute the first op against the engine.
fn execute_fql(engine: &mut ForgeQLEngine, session_id: &str, fql: &str) -> ForgeQLResult {
    let ops = parser::parse(fql).expect("parse");
    let op = ops.first().expect("at least one op");
    let coords = SessionCoords::from_session_id(session_id).expect("valid session_id");
    engine
        .execute(auth(AuthContext::Tester), Some(&coords), op)
        .result
        .expect("execute")
}

/// Parse and execute, returning the error instead of panicking on it.
fn try_fql(
    engine: &mut ForgeQLEngine,
    session_id: &str,
    fql: &str,
) -> anyhow::Result<ForgeQLResult> {
    let ops = parser::parse(fql).expect("parse");
    let op = ops.first().expect("at least one op");
    let coords = SessionCoords::from_session_id(session_id).expect("valid session_id");
    engine
        .execute(auth(AuthContext::Tester), Some(&coords), op)
        .result
}

/// Run a statement that must be refused, and hand back the message it was
/// refused with — the message is the contract for every LAST gate.
fn fql_err(engine: &mut ForgeQLEngine, session_id: &str, fql: &str) -> String {
    try_fql(engine, session_id, fql)
        .expect_err("must be refused")
        .to_string()
}

// -----------------------------------------------------------------------
// Files and directories as addressable nodes (bare-hex `n<hex>` handles)
// -----------------------------------------------------------------------

fn node_rev(engine: &mut ForgeQLEngine, session_id: &str, handle: &str) -> String {
    match execute_fql(engine, session_id, &format!("FIND NODE '{handle}'")) {
        ForgeQLResult::FindNode(node) => node.rev,
        other => panic!("expected FindNode, got {other:?}"),
    }
}

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

// -----------------------------------------------------------------------
// Creation, relocation, and ROLLBACK of created files
// -----------------------------------------------------------------------

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

// -----------------------------------------------------------------------
// Engine lifecycle
// -----------------------------------------------------------------------

#[test]
fn engine_starts_with_zero_state() {
    let tmp = tempdir().unwrap();
    let engine =
        ForgeQLEngine::new(tmp.path().to_path_buf(), common::make_registry_real()).unwrap();
    assert_eq!(engine.session_count(), 0);
    assert_eq!(engine.source_count(), 0);
    assert_eq!(engine.commands_served(), 0);
}

#[test]
fn show_sources_on_empty_engine() {
    let tmp = tempdir().unwrap();
    let mut engine =
        ForgeQLEngine::new(tmp.path().to_path_buf(), common::make_registry_real()).unwrap();
    let result = engine
        .execute(auth(AuthContext::Tester), None, &ForgeQLIR::ShowSources)
        .result
        .unwrap();
    match result {
        ForgeQLResult::Query(qr) => {
            assert_eq!(qr.op, "show_sources");
            assert!(qr.results.is_empty());
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

// -----------------------------------------------------------------------
// FIND symbols
// -----------------------------------------------------------------------

#[test]
fn find_symbols_returns_known_functions() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name LIKE 'encender%'",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            assert_eq!(qr.op, "find_symbols");
            let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
            assert!(
                names.contains(&"encenderMotor"),
                "expected encenderMotor in {names:?}"
            );
            assert!(
                names.contains(&"encenderSistema"),
                "expected encenderSistema in {names:?}"
            );
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

#[test]
fn find_symbols_with_limit() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name LIKE '%' LIMIT 2",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            assert!(qr.results.len() <= 2, "LIMIT 2 should cap results");
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

#[test]
fn find_symbols_no_match_returns_empty() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name LIKE 'zzz_nonexistent_%'",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            assert!(qr.results.is_empty());
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

// -----------------------------------------------------------------------
// FIND usages
// -----------------------------------------------------------------------

#[test]
fn find_usages_returns_sites() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    match result {
        ForgeQLResult::Query(qr) => {
            assert_eq!(qr.op, "find_usages");
            assert!(
                !qr.results.is_empty(),
                "encenderMotor should have usage sites"
            );
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

// -----------------------------------------------------------------------
// SHOW body
// -----------------------------------------------------------------------

#[test]
fn show_body_returns_lines() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(&mut engine, &sid, "SHOW body OF 'encenderMotor'");
    match result {
        ForgeQLResult::Show(sr) => {
            assert_eq!(sr.op, "show_body");
            assert_eq!(sr.symbol.as_deref(), Some("encenderMotor"));
        }
        other => panic!("expected Show, got: {other:?}"),
    }
}

/// Phase 4: SHOW body response includes `start_line` and `end_line` covering
/// the full function span — `encenderMotor` spans lines 48–63 in the fixture.
#[test]
fn show_body_result_includes_start_and_end_line() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(&mut engine, &sid, "SHOW body OF 'encenderMotor'");
    match result {
        ForgeQLResult::Show(sr) => {
            let start = sr.start_line.expect("start_line should be populated");
            let end = sr.end_line.expect("end_line should be populated");
            assert!(start > 0, "start_line must be 1-based: {start}");
            assert!(
                end >= start,
                "end_line ({end}) must be >= start_line ({start})"
            );
            // encenderMotor is a multi-line function — span must cover > 1 line.
            assert!(
                end > start,
                "show_body span must cover multiple lines for encenderMotor"
            );
        }
        other => panic!("expected Show, got: {other:?}"),
    }
}

/// Phase 4: SHOW body DEPTH 0 returns signature lines only, but
/// `start_line`/`end_line` must still cover the full function span.
#[test]
fn show_body_depth_zero_is_default_and_signature_only() {
    let (mut engine, sid, _dir) = engine_with_session();
    // No DEPTH and explicit DEPTH 0 must behave identically (signature only).
    let no_depth = execute_fql(&mut engine, &sid, "SHOW body OF 'encenderMotor'");
    let depth0 = execute_fql(&mut engine, &sid, "SHOW body OF 'encenderMotor' DEPTH 0");
    let depth1 = execute_fql(&mut engine, &sid, "SHOW body OF 'encenderMotor' DEPTH 1");

    fn line_count(r: &ForgeQLResult) -> usize {
        match r {
            ForgeQLResult::Show(sr) => match &sr.content {
                forgeql_core::result::ShowContent::Lines { lines, .. } => lines.len(),
                _ => panic!("expected Lines content"),
            },
            other => panic!("expected Show, got: {other:?}"),
        }
    }

    let lines_no_depth = line_count(&no_depth);
    let lines_depth0 = line_count(&depth0);
    let lines_depth1 = line_count(&depth1);

    assert_eq!(
        lines_no_depth, lines_depth0,
        "omitting DEPTH must behave identically to DEPTH 0"
    );
    assert!(
        lines_depth0 < lines_depth1,
        "DEPTH 0 (signature only) must return fewer lines than DEPTH 1 ({lines_depth0} vs {lines_depth1})"
    );
    // start_line / end_line must still cover the full function span at DEPTH 0.
    let (d0_start, d0_end) = match &depth0 {
        ForgeQLResult::Show(sr) => (sr.start_line.unwrap(), sr.end_line.unwrap()),
        other => panic!("expected Show(depth0), got: {other:?}"),
    };
    let (d1_start, d1_end) = match &depth1 {
        ForgeQLResult::Show(sr) => (sr.start_line.unwrap(), sr.end_line.unwrap()),
        other => panic!("expected Show(depth1), got: {other:?}"),
    };
    assert_eq!(
        d0_start, d1_start,
        "start_line must match regardless of depth"
    );
    assert_eq!(
        d0_end, d1_end,
        "end_line must cover full span regardless of depth"
    );
}

/// Phase 4: SHOW context response includes `start_line` and `end_line`.
#[test]
fn show_context_result_includes_start_and_end_line() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(&mut engine, &sid, "SHOW context OF 'encenderMotor'");
    match result {
        ForgeQLResult::Show(sr) => {
            assert_eq!(sr.op, "show_context");
            let start = sr
                .start_line
                .expect("start_line should be populated for show_context");
            let end = sr
                .end_line
                .expect("end_line should be populated for show_context");
            assert!(start > 0, "start_line must be 1-based");
            assert!(end >= start, "end_line must be >= start_line");
        }
        other => panic!("expected Show, got: {other:?}"),
    }
}

// -----------------------------------------------------------------------
// SHOW outline
// -----------------------------------------------------------------------

#[test]
fn show_outline_returns_entries() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(&mut engine, &sid, "SHOW outline OF 'motor_control.h'");
    match result {
        ForgeQLResult::Show(sr) => {
            assert_eq!(sr.op, "show_outline");
        }
        other => panic!("expected Show, got: {other:?}"),
    }
}

#[test]
fn show_outline_while_entries_include_node_id() {
    let (mut engine, sid, _dir) =
        engine_with_session_with_extra_files(&["enrichment_patterns.cpp"]);
    let result = execute_fql(
        &mut engine,
        &sid,
        "SHOW outline OF 'enrichment_patterns.cpp' WHERE fql_kind = 'while'",
    );

    match result {
        ForgeQLResult::Show(sr) => {
            assert_eq!(sr.op, "show_outline");
            let ShowContent::Outline { entries } = sr.content else {
                panic!("expected outline content");
            };
            assert!(
                !entries.is_empty(),
                "expected at least one while entry in enrichment_patterns.cpp"
            );
            assert!(
                entries.iter().all(|entry| entry.node_id.is_some()),
                "all while entries should carry node_id after reindex"
            );
        }
        other => panic!("expected Show, got: {other:?}"),
    }
}

#[test]
fn show_outline_number_entries_do_not_include_node_id() {
    let (mut engine, sid, _dir) =
        engine_with_session_with_extra_files(&["enrichment_patterns.cpp"]);
    let result = execute_fql(
        &mut engine,
        &sid,
        "SHOW outline OF 'enrichment_patterns.cpp' WHERE fql_kind = 'number'",
    );

    match result {
        ForgeQLResult::Show(sr) => {
            assert_eq!(sr.op, "show_outline");
            let ShowContent::Outline { entries } = sr.content else {
                panic!("expected outline content");
            };
            assert!(
                !entries.is_empty(),
                "expected at least one number entry in enrichment_patterns.cpp"
            );
            assert!(
                entries.iter().all(|entry| entry.node_id.is_none()),
                "analysis-only number entries must not carry node_id"
            );
        }
        other => panic!("expected Show, got: {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Mutation: CHANGE FILE RENAME symbol
// -----------------------------------------------------------------------

#[test]
fn change_rename_applies_and_mutates_file() {
    let (mut engine, sid, dir) = engine_with_session();

    let result = execute_fql(
        &mut engine,
        &sid,
        "CHANGE FILE 'motor_control.cpp' MATCHING 'void encenderMotor' WITH 'void startMotor'",
    );
    match result {
        ForgeQLResult::Mutation(mr) => {
            assert!(mr.applied, "mutation should be applied");
            assert!(mr.edit_count > 0, "should have edits");
        }
        other => panic!("expected Mutation, got: {other:?}"),
    }

    // Verify file on disk.
    let cpp = fs::read_to_string(dir.path().join("motor_control.cpp")).unwrap();
    assert!(cpp.contains("startMotor"), "new name should appear in .cpp");
}

// -----------------------------------------------------------------------
// Mutation: CHANGE FILE LINES trailing newline
// -----------------------------------------------------------------------

#[test]
fn change_lines_auto_appends_trailing_newline() {
    let (mut engine, sid, dir) = engine_with_session();

    let cpp_path = dir.path().join("motor_control.cpp");
    let original = fs::read_to_string(&cpp_path).unwrap();
    let original_lines: Vec<&str> = original.lines().collect();

    // Replace line 2 with text that has NO trailing newline.
    let replacement = "// replaced line";
    let fql = format!("CHANGE FILE 'motor_control.cpp' LINES 2-2 WITH '{replacement}'");
    let result = execute_fql(&mut engine, &sid, &fql);

    match &result {
        ForgeQLResult::Mutation(mr) => {
            assert!(mr.applied);
            assert!(mr.edit_count > 0);
        }
        other => panic!("expected Mutation, got: {other:?}"),
    }

    // Line 2 should be the replacement, line 3 should still be the original line 3.
    let modified = fs::read_to_string(&cpp_path).unwrap();
    let modified_lines: Vec<&str> = modified.lines().collect();
    assert_eq!(
        modified_lines[1], replacement,
        "line 2 should be the replacement"
    );
    assert_eq!(
        modified_lines[2], original_lines[2],
        "line 3 must NOT merge with replacement — trailing newline was missing"
    );
}

// -----------------------------------------------------------------------
// Mutation: CHANGE response includes diff preview
// -----------------------------------------------------------------------

#[test]
fn change_mutation_includes_diff() {
    let (mut engine, sid, _dir) = engine_with_session();

    let result = execute_fql(
        &mut engine,
        &sid,
        "CHANGE FILE 'motor_control.cpp' MATCHING 'encenderMotor' WITH 'startMotor'",
    );
    match result {
        ForgeQLResult::Mutation(mr) => {
            assert!(mr.applied);
            let diff = mr.diff.expect("mutation should include a diff preview");
            assert!(
                diff.contains("── "),
                "compact preview should have ── header: {diff}"
            );
            assert!(
                diff.contains("motor_control.cpp"),
                "compact preview should name the file: {diff}"
            );
            assert!(
                diff.contains("startMotor"),
                "compact preview should show the new text: {diff}"
            );
        }
        other => panic!("expected Mutation, got: {other:?}"),
    }
}

/// A mutation that leaves a structured file unparseable reports it immediately:
/// the file was valid JSON before the edit and is not after, so the result
/// carries a `StructuralError` naming the file and the parser's diagnostic. A
/// well-formed edit reports nothing; editing an already-broken file flags the
/// breakage as pre-existing rather than caused by this edit.
#[test]
fn mutation_reports_structural_error_when_it_breaks_json() {
    let (mut engine, sid, dir) = engine_with_session();
    let path = dir.path().join("cfg.json");
    fs::write(&path, "{ \"a\": 1 }\n").unwrap();
    let handle = common::path_handle("cfg.json");

    // valid -> valid: nothing to report.
    let rev = node_rev(&mut engine, &sid, &handle);
    match execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{handle}' IF REV '{rev}' WITH '{{ \"a\": 2 }}'"),
    ) {
        ForgeQLResult::Mutation(mr) => assert!(
            mr.structural_errors.is_empty(),
            "a valid JSON edit must not report a structural error: {:?}",
            mr.structural_errors
        ),
        other => panic!("expected Mutation, got: {other:?}"),
    }

    // valid -> broken: reported, and attributed to this edit.
    let rev = node_rev(&mut engine, &sid, &handle);
    match execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{handle}' IF REV '{rev}' WITH '{{ \"a\": 2 '"),
    ) {
        ForgeQLResult::Mutation(mr) => {
            let se = mr
                .structural_errors
                .first()
                .expect("breaking JSON must report a structural error");
            assert!(se.path.ends_with("cfg.json"), "path: {}", se.path.display());
            assert_eq!(se.valid_before, Some(true), "this edit caused the break");
            assert!(!se.message.is_empty(), "carries the parser diagnostic");
        }
        other => panic!("expected Mutation, got: {other:?}"),
    }

    // broken -> broken: still reported, now flagged as pre-existing.
    let rev = node_rev(&mut engine, &sid, &handle);
    match execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{handle}' IF REV '{rev}' WITH '{{ \"a\": 3 '"),
    ) {
        ForgeQLResult::Mutation(mr) => {
            let se = mr.structural_errors.first().expect("still broken");
            assert_eq!(se.valid_before, Some(false), "breakage predates this edit");
        }
        other => panic!("expected Mutation, got: {other:?}"),
    }
}

/// The JSON plugin serves `.jsonc`, whose comments and trailing commas a strict
/// RFC-8259 parser rejects. Those must never be reported as structural errors —
/// the strict validator opts the dialect out.
#[test]
fn jsonc_dialect_is_not_strictly_validated() {
    let (mut engine, sid, dir) = engine_with_session();
    let path = dir.path().join("cfg.jsonc");
    fs::write(&path, "{ \"a\": 1 }\n").unwrap();
    let handle = common::path_handle("cfg.jsonc");

    // A trailing comma is legal JSONC but not strict JSON; it must not be flagged.
    let rev = node_rev(&mut engine, &sid, &handle);
    match execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{handle}' IF REV '{rev}' WITH '{{ \"a\": 2, }}'"),
    ) {
        ForgeQLResult::Mutation(mr) => assert!(
            mr.structural_errors.is_empty(),
            "JSONC dialect must not be strictly validated: {:?}",
            mr.structural_errors
        ),
        other => panic!("expected Mutation, got: {other:?}"),
    }
}

/// The strict validators cover YAML, TOML and XML too: an edit that leaves any
/// of them unparseable is reported, while a well-formed edit is not. Each broken
/// form is one a strict parser rejects but the error-tolerant tree-sitter grammar
/// would recover from.
#[test]
fn mutation_reports_structural_errors_for_yaml_toml_and_xml() {
    let cases = [
        ("cfg.yaml", "{a: 1}", "{a: 1"),
        ("cfg.toml", "a = 1", "a ="),
        ("cfg.xml", "<r><a/></r>", "<r><a></r>"),
    ];
    for (name, valid, broken) in cases {
        let (mut engine, sid, dir) = engine_with_session();
        let path = dir.path().join(name);
        fs::write(&path, valid).unwrap();
        let handle = common::path_handle(name);

        let rev = node_rev(&mut engine, &sid, &handle);
        match execute_fql(
            &mut engine,
            &sid,
            &format!("CHANGE NODE '{handle}' IF REV '{rev}' WITH '{valid}'"),
        ) {
            ForgeQLResult::Mutation(mr) => assert!(
                mr.structural_errors.is_empty(),
                "{name}: valid content must not be flagged: {:?}",
                mr.structural_errors
            ),
            other => panic!("{name}: expected Mutation, got: {other:?}"),
        }

        let rev = node_rev(&mut engine, &sid, &handle);
        match execute_fql(
            &mut engine,
            &sid,
            &format!("CHANGE NODE '{handle}' IF REV '{rev}' WITH '{broken}'"),
        ) {
            ForgeQLResult::Mutation(mr) => {
                let Some(se) = mr.structural_errors.first() else {
                    panic!("{name}: broken content must be reported");
                };
                assert!(
                    se.path.ends_with(name),
                    "{name}: path {}",
                    se.path.display()
                );
                assert!(!se.message.is_empty(), "{name}: carries a diagnostic");
            }
            other => panic!("{name}: expected Mutation, got: {other:?}"),
        }
    }
}

// -----------------------------------------------------------------------
// Error cases
// -----------------------------------------------------------------------

#[test]
fn find_symbols_without_session_fails() {
    let tmp = tempdir().unwrap();
    let mut engine =
        ForgeQLEngine::new(tmp.path().to_path_buf(), common::make_registry_real()).unwrap();
    let op = ForgeQLIR::FindSymbols {
        backend: Backend::default(),
        clauses: Clauses::default(),
    };
    assert!(
        engine
            .execute(auth(AuthContext::Tester), None, &op)
            .result
            .is_err()
    );
}

// (disconnect_unknown_session_fails removed — DISCONNECT command eliminated)

// -----------------------------------------------------------------------
// Result serialization round-trip
// -----------------------------------------------------------------------

#[test]
fn result_json_contains_projected_fields() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name LIKE 'encender%'",
    );

    // Verify projected JSON structure.
    let json = result.to_json();
    let v: serde_json::Value = serde_json::from_str(&json).expect("JSON must be valid");
    assert_eq!(v["op"], "find_symbols");
    let rows = v["results"].as_array().expect("results array");
    assert!(!rows.is_empty());
    // Each row must have projected fields, not raw SymbolMatch.
    for row in rows {
        assert!(!row["name"].as_str().unwrap_or("").is_empty());
        assert!(row.get("fields").is_none(), "fields must not leak");
    }
}

// -----------------------------------------------------------------------
// Display output
// -----------------------------------------------------------------------

#[test]
fn display_output_contains_symbol_names() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name LIKE 'encender%'",
    );
    let output = format!("{result}");
    assert!(
        output.contains("encenderMotor"),
        "display should show encenderMotor: {output}"
    );
}

// -----------------------------------------------------------------------
// Phase 7: v2 architecture validation
// -----------------------------------------------------------------------

/// FIND symbols WHERE fql_kind = 'function' returns only functions.
#[rustfmt::skip]
#[test]
fn find_symbols_filters_by_fql_kind() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE fql_kind = 'function'",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            assert!(
                !qr.results.is_empty(),
                "should find function rows"
            );
            // All returned rows must have fql_kind = function.
            for row in &qr.results {
                let kind = row.fql_kind.as_deref().unwrap_or("");
                assert_eq!(
                    kind, "function",
                    "unexpected fql_kind '{kind}' for row '{}'",
                    row.name
                );
            }
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

/// All SymbolMatch results carry a populated `fql_kind` field.
#[test]
fn find_symbols_result_has_fql_kind_populated() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(&mut engine, &sid, "FIND symbols WHERE name LIKE '%'");
    match result {
        ForgeQLResult::Query(qr) => {
            assert!(
                !qr.results.is_empty(),
                "fixture workspace must have symbols"
            );
            for row in &qr.results {
                assert!(
                    row.fql_kind.is_some(),
                    "every SymbolMatch must have fql_kind set (missing on '{}')",
                    row.name
                );
            }
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

/// All SymbolMatch results carry a populated `line` field (1-based definition line).
#[test]
fn find_symbols_result_has_line_populated() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(&mut engine, &sid, "FIND symbols WHERE name LIKE '%'");
    match result {
        ForgeQLResult::Query(qr) => {
            assert!(
                !qr.results.is_empty(),
                "fixture workspace must have symbols"
            );
            for row in &qr.results {
                let line = row.line.unwrap_or(0);
                assert!(
                    line > 0,
                    "every SymbolMatch must have line > 0 (was {line} for '{}')",
                    row.name
                );
            }
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

/// FIND usages GROUP BY file deduplicates: each unique path appears at most once.
#[test]
fn find_usages_group_by_file_deduplicates() {
    let (mut engine, sid, _dir) = engine_with_session();
    // encenderMotor is called in motor_control.cpp — there must be a usage.
    let all_result = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let grouped_result = execute_fql(
        &mut engine,
        &sid,
        "FIND usages OF 'encenderMotor' GROUP BY file",
    );

    let all_count = match &all_result {
        ForgeQLResult::Query(qr) => qr.results.len(),
        other => panic!("expected Query(all), got: {other:?}"),
    };
    let grouped_count = match &grouped_result {
        ForgeQLResult::Query(qr) => {
            // Every path should be unique.
            let paths: Vec<_> = qr
                .results
                .iter()
                .filter_map(|r| r.path.as_deref())
                .collect();
            let unique_paths: std::collections::HashSet<_> = paths.iter().collect();
            assert_eq!(
                paths.len(),
                unique_paths.len(),
                "GROUP BY file must yield unique paths"
            );
            // Every grouped row must carry a non-zero count.
            for row in &qr.results {
                let c = row.count.expect("GROUP BY file must populate .count");
                assert!(c >= 1, "per-file count must be >= 1");
            }
            // The sum of per-file counts must equal the total ungrouped usages.
            let total_from_counts: usize = qr.results.iter().filter_map(|r| r.count).sum();
            assert_eq!(
                total_from_counts, all_count,
                "sum of per-file counts ({total_from_counts}) must equal total usages ({all_count})"
            );
            qr.results.len()
        }
        other => panic!("expected Query(grouped), got: {other:?}"),
    };
    assert!(
        grouped_count <= all_count,
        "grouped count ({grouped_count}) must be ≤ total usages ({all_count})"
    );
}

/// LIMIT + OFFSET pagination: OFFSET 1 skips the first result.
#[test]
fn find_symbols_offset_pagination() {
    let (mut engine, sid, _dir) = engine_with_session();
    // Use explicit LIMIT to bypass the implicit cap and get all symbols.
    let all = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name LIKE '%' ORDER BY name ASC LIMIT 1000",
    );
    // Skip the first result (explicit LIMIT required here too).
    let paged = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name LIKE '%' ORDER BY name ASC LIMIT 1000 OFFSET 1",
    );

    let all_names: Vec<String> = match all {
        ForgeQLResult::Query(qr) => qr.results.into_iter().map(|r| r.name).collect(),
        other => panic!("expected Query(all), got: {other:?}"),
    };
    let paged_names: Vec<String> = match paged {
        ForgeQLResult::Query(qr) => qr.results.into_iter().map(|r| r.name).collect(),
        other => panic!("expected Query(paged), got: {other:?}"),
    };

    assert!(
        all_names.len() > 1,
        "need at least 2 indexed symbols for pagination test"
    );
    assert_eq!(
        paged_names.len(),
        all_names.len() - 1,
        "OFFSET 1 must skip exactly one result"
    );
    assert_eq!(
        paged_names[0], all_names[1],
        "OFFSET 1 first result must be second result of unfiltered list"
    );
}

/// `FIND symbols` without `LIMIT` is capped at `DEFAULT_QUERY_LIMIT` rows.
/// `total` must reflect the full pre-cap count so callers can detect truncation.
#[test]
fn find_symbols_implicit_cap_signals_more_rows() {
    let (mut engine, sid, _dir) = engine_with_session();
    // Retrieve everything to know the true count.
    let all = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name LIKE '%' LIMIT 1000",
    );
    let full_count = match &all {
        ForgeQLResult::Query(qr) => qr.total,
        other => panic!("expected Query, got: {other:?}"),
    };

    // If the fixture has more symbols than the implicit cap the uncapped query
    // must be truncated and `total` must still report the full count.
    if full_count > 20 {
        let capped = execute_fql(&mut engine, &sid, "FIND symbols WHERE name LIKE '%'");
        match capped {
            ForgeQLResult::Query(qr) => {
                assert_eq!(
                    qr.results.len(),
                    20,
                    "implicit cap must return exactly 20 rows"
                );
                assert_eq!(
                    qr.total, full_count,
                    "total must reflect full pre-cap count so caller knows more rows exist"
                );
            }
            other => panic!("expected Query, got: {other:?}"),
        }
    }
}

/// WHERE usages = 0 returns symbols with no references (dead code detection).
#[test]
fn find_symbols_where_usages_eq_zero() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(&mut engine, &sid, "FIND symbols WHERE usages = 0");
    match result {
        ForgeQLResult::Query(qr) => {
            // Every returned symbol must have 0 usages.
            for row in &qr.results {
                let usages = row.usages_count.unwrap_or(0);
                assert_eq!(
                    usages, 0,
                    "WHERE usages = 0 returned row '{}' with usages = {usages}",
                    row.name
                );
            }
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

/// FIND symbols WHERE fql_kind = 'macro' finds macros/includes.
#[rustfmt::skip]
#[test]
fn find_symbols_fql_kind_macro_and_import() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE fql_kind = 'macro'",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            // motor_control.h uses #include and likely #define directives.
            assert!(
                !qr.results.is_empty(),
                "fixture must have macro nodes (#define directives)"
            );
            for row in &qr.results {
                let kind = row.fql_kind.as_deref().unwrap_or("");
                assert!(
                    kind == "macro",
                    "unexpected fql_kind '{kind}' for row '{}' — expected macro",
                    row.name
                );
            }
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

// -----------------------------------------------------------------------
// SHOW members / SHOW outline — LIMIT / OFFSET
// -----------------------------------------------------------------------

#[test]
fn show_members_limit_is_respected() {
    let (mut engine, sid, _dir) = engine_with_session();

    // ErrorMotor has 3 enumerators: OK, TIMEOUT, FALLO.
    let full = execute_fql(&mut engine, &sid, "SHOW members OF 'ErrorMotor'");
    let full_count = match &full {
        ForgeQLResult::Show(sr) => match &sr.content {
            ShowContent::Members { members, .. } => members.len(),
            other => panic!("expected Members, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    };
    assert!(
        full_count >= 2,
        "fixture ErrorMotor must have at least 2 members"
    );

    let limited = execute_fql(&mut engine, &sid, "SHOW members OF 'ErrorMotor' LIMIT 1");
    match &limited {
        ForgeQLResult::Show(sr) => match &sr.content {
            ShowContent::Members { members, .. } => {
                assert_eq!(members.len(), 1, "LIMIT 1 must return exactly 1 member");
            }
            other => panic!("expected Members, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    }
}

#[test]
fn show_outline_limit_is_respected() {
    let (mut engine, sid, _dir) = engine_with_session();

    // motor_control.h has many variables; full list should exceed 2.
    let full = execute_fql(&mut engine, &sid, "SHOW outline OF 'motor_control.h' ALL");
    let full_count = match &full {
        ForgeQLResult::Show(sr) => match &sr.content {
            ShowContent::Outline { entries } => entries.len(),
            other => panic!("expected Outline, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    };
    assert!(
        full_count >= 2,
        "fixture motor_control.h must have at least 2 outline entries"
    );

    let limited = execute_fql(
        &mut engine,
        &sid,
        "SHOW outline OF 'motor_control.h' ALL LIMIT 2",
    );
    match &limited {
        ForgeQLResult::Show(sr) => match &sr.content {
            ShowContent::Outline { entries } => {
                assert_eq!(
                    entries.len(),
                    2,
                    "LIMIT 2 must return exactly 2 outline entries"
                );
            }
            other => panic!("expected Outline, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// BUG #4: ORDER BY line — ASC and DESC must differ
// -----------------------------------------------------------------------

#[test]
fn find_symbols_order_by_line_asc_vs_desc_differ() {
    let (mut engine, sid, _dir) = engine_with_session();

    // motor_control.cpp has functions spanning lines 48–217.
    // ASC should return the earliest-defined function first; DESC the latest.
    let asc = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' \
         IN 'motor_control.cpp' ORDER BY line ASC LIMIT 1",
    );
    let desc = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' \
         IN 'motor_control.cpp' ORDER BY line DESC LIMIT 1",
    );

    let first_name = |r: &ForgeQLResult| match r {
        ForgeQLResult::Query(qr) => qr
            .results
            .first()
            .map(|s| s.name.clone())
            .unwrap_or_default(),
        other => panic!("expected Query, got {other:?}"),
    };

    let asc_name = first_name(&asc);
    let desc_name = first_name(&desc);
    assert_ne!(
        asc_name, desc_name,
        "ORDER BY line ASC and DESC must return different first results \
         (got '{asc_name}' for both — ORDER BY line is not working)"
    );
    // encenderMotor is on line 48 — must be first for ASC.
    assert_eq!(
        asc_name, "encenderMotor",
        "ASC should return the function with the lowest line number first"
    );
    // calcularPotencia is on line 232 — must be first for DESC.
    assert_eq!(
        desc_name, "calcularPotencia",
        "DESC should return the function with the highest line number first"
    );
}

// -----------------------------------------------------------------------
// BUG #2+#5: No duplicate rows in FIND symbols / FIND usages
// -----------------------------------------------------------------------

#[test]
fn find_symbols_no_duplicate_rows() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' IN 'motor_control.cpp'",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            // Each function in motor_control.cpp must appear exactly once.
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for row in &qr.results {
                let key = format!(
                    "{}::{}",
                    row.name,
                    row.path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default()
                );
                assert!(
                    seen.insert(key.clone()),
                    "duplicate symbol row detected: {key}"
                );
            }
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// FIND usages JSON — line field must be present
// -----------------------------------------------------------------------

#[test]
fn find_usages_json_line_field_is_present() {
    let (mut engine, sid, _dir) = engine_with_session();

    // 'encenderMotor' appears in comments, macro bodies, and calls in the .cpp.
    let result = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let json = result.to_json();
    let v: serde_json::Value = serde_json::from_str(&json).expect("JSON must be valid");
    let rows = v["results"].as_array().expect("results array");
    // Every row must have a name and line.
    for row in rows {
        assert!(
            !row["name"].as_str().unwrap_or("").is_empty(),
            "name must not be empty: {json}"
        );
        assert!(
            row["line"].as_u64().is_some(),
            "line must be present: {json}"
        );
    }
}

// -----------------------------------------------------------------------
// Declaration indexing (FIND globals / WHERE fql_kind = 'variable')
// -----------------------------------------------------------------------

/// FIND globals returns file-scope variable nodes (variable variables).
#[test]
fn find_globals_returns_variables() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(&mut engine, &sid, "FIND globals LIMIT 200");
    match result {
        ForgeQLResult::Query(qr) => {
            assert!(!qr.results.is_empty(), "FIND globals should return results");
            for row in &qr.results {
                assert_eq!(
                    row.fql_kind.as_deref(),
                    Some("variable"),
                    "FIND globals must only return variable nodes, got {:?} for '{}'",
                    row.fql_kind,
                    row.name,
                );
                assert_eq!(
                    row.fields.get("scope").map(String::as_str),
                    Some("file"),
                    "FIND globals must only return file-scope decls, got scope={:?} for '{}'",
                    row.fields.get("scope"),
                    row.name,
                );
            }
            let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
            assert!(
                names.contains(&"motorPrincipal"),
                "expected motorPrincipal in {names:?}"
            );
            // Local variables must NOT appear.
            for local in ["vel", "velocidad"] {
                assert!(
                    !names.contains(&local),
                    "local variable '{local}' must NOT appear in FIND globals; got: {names:?}"
                );
            }
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

/// FIND symbols WHERE fql_kind = 'variable' returns ALL variables (file + local).
#[test]
fn find_symbols_where_fql_kind_variable() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE fql_kind = 'variable' LIMIT 200",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            assert!(!qr.results.is_empty(), "should return variable nodes");
            let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
            // File-scope variables.
            for expected in ["motorPrincipal", "motorSecundario", "gCallbackEncendido"] {
                assert!(
                    names.contains(&expected),
                    "expected '{expected}' in variables; got: {names:?}",
                );
            }
            // Local variables should also appear (unlike FIND globals).
            let has_local = qr
                .results
                .iter()
                .any(|r| r.fields.get("scope").map(String::as_str) == Some("local"));
            assert!(
                has_local,
                "WHERE fql_kind='variable' should include local variables"
            );
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

/// FIND symbols GROUP BY fql_kind returns one row per fql_kind with counts.
#[test]
fn find_symbols_group_by_fql_kind() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols GROUP BY fql_kind ORDER BY count DESC LIMIT 50",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            assert!(
                !qr.results.is_empty(),
                "GROUP BY fql_kind should return groups"
            );
            // Every row must have a count > 0.
            for row in &qr.results {
                assert!(
                    row.count.unwrap_or(0) > 0,
                    "each group must have count > 0, got {:?} for {:?}",
                    row.count,
                    row.fql_kind,
                );
            }
            // "variable" must now appear as a group.
            let kinds: Vec<&str> = qr
                .results
                .iter()
                .filter_map(|r| r.fql_kind.as_deref())
                .collect();
            assert!(
                kinds.contains(&"variable"),
                "variable must appear in GROUP BY fql_kind results; got: {kinds:?}",
            );
            assert!(
                kinds.contains(&"function"),
                "function must appear in GROUP BY fql_kind results; got: {kinds:?}",
            );
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

/// Scope and storage dynamic fields can be filtered via WHERE clauses.
#[test]
fn find_variables_filter_by_scope_and_storage() {
    let (mut engine, sid, _dir) = engine_with_session();

    // File-scope variables only (same as FIND globals).
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE fql_kind = 'variable' WHERE scope = 'file' LIMIT 200",
    );
    let file_names: Vec<String> = match result {
        ForgeQLResult::Query(qr) => qr.results.iter().map(|r| r.name.clone()).collect(),
        other => panic!("expected Query, got: {other:?}"),
    };
    assert!(
        file_names.contains(&"motorPrincipal".to_string()),
        "file-scope filter should include motorPrincipal; got: {file_names:?}"
    );

    // Storage = 'static' filter.
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE fql_kind = 'variable' WHERE storage = 'static' LIMIT 200",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            for row in &qr.results {
                assert_eq!(
                    row.fields.get("storage").map(String::as_str),
                    Some("static"),
                    "storage filter should only return static variables, got {:?} for '{}'",
                    row.fields.get("storage"),
                    row.name,
                );
            }
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

// -----------------------------------------------------------------------
// SHOW outline / SHOW members — WHERE clause filtering
// -----------------------------------------------------------------------

#[test]
fn show_outline_where_filters_by_kind() {
    let (mut engine, sid, _dir) = engine_with_session();

    // motor_control.h has macro entries AND other kinds (enums, comments, etc.).
    // WHERE kind = 'macro' must return only macro entries.
    let result = execute_fql(
        &mut engine,
        &sid,
        "SHOW outline OF 'motor_control.h' WHERE fql_kind = 'macro'",
    );
    match &result {
        ForgeQLResult::Show(sr) => match &sr.content {
            ShowContent::Outline { entries } => {
                assert!(
                    !entries.is_empty(),
                    "motor_control.h must have macro entries"
                );
                for entry in entries {
                    assert_eq!(
                        entry.fql_kind, "macro",
                        "WHERE fql_kind = 'macro' returned '{}' with kind '{}'",
                        entry.name, entry.fql_kind
                    );
                }
            }
            other => panic!("expected Outline, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    }

    // Unfiltered outline must have MORE entries (other kinds exist).
    let unfiltered = execute_fql(&mut engine, &sid, "SHOW outline OF 'motor_control.h'");
    let unfiltered_count = match &unfiltered {
        ForgeQLResult::Show(sr) => match &sr.content {
            ShowContent::Outline { entries } => entries.len(),
            other => panic!("expected Outline, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    };
    let filtered_count = match &result {
        ForgeQLResult::Show(sr) => match &sr.content {
            ShowContent::Outline { entries } => entries.len(),
            _ => unreachable!(),
        },
        _ => unreachable!(),
    };
    assert!(
        filtered_count < unfiltered_count,
        "WHERE must reduce the result set ({filtered_count} < {unfiltered_count})"
    );
}

#[test]
fn show_outline_where_name_like_filters() {
    let (mut engine, sid, _dir) = engine_with_session();

    let result = execute_fql(
        &mut engine,
        &sid,
        "SHOW outline OF 'motor_control.h' WHERE name LIKE 'VELOCIDAD%'",
    );
    match &result {
        ForgeQLResult::Show(sr) => match &sr.content {
            ShowContent::Outline { entries } => {
                assert!(
                    !entries.is_empty(),
                    "motor_control.h must have entries matching 'VELOCIDAD%'"
                );
                for entry in entries {
                    assert!(
                        entry.name.to_ascii_uppercase().starts_with("VELOCIDAD"),
                        "WHERE name LIKE 'VELOCIDAD%' returned unexpected entry '{}'",
                        entry.name
                    );
                }
            }
            other => panic!("expected Outline, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    }
}

#[test]
fn show_outline_where_with_limit_applies_both() {
    let (mut engine, sid, _dir) = engine_with_session();

    let result = execute_fql(
        &mut engine,
        &sid,
        "SHOW outline OF 'motor_control.h' WHERE fql_kind = 'macro' LIMIT 2",
    );
    match &result {
        ForgeQLResult::Show(sr) => match &sr.content {
            ShowContent::Outline { entries } => {
                assert_eq!(
                    entries.len(),
                    2,
                    "WHERE + LIMIT 2 must return exactly 2 entries"
                );
                for entry in entries {
                    assert_eq!(
                        entry.fql_kind, "macro",
                        "WHERE filter must still apply with LIMIT"
                    );
                }
            }
            other => panic!("expected Outline, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    }
}

#[test]
fn show_members_where_filters_by_kind() {
    let (mut engine, sid, _dir) = engine_with_session();

    // ErrorMotor has enumerator members.  Filtering by kind must include them.
    let result = execute_fql(
        &mut engine,
        &sid,
        "SHOW members OF \"ErrorMotor\" WHERE fql_kind = \"enumerator\"",
    );
    match &result {
        ForgeQLResult::Show(sr) => match &sr.content {
            ShowContent::Members { members, .. } => {
                assert!(
                    !members.is_empty(),
                    "ErrorMotor must have enumerator members"
                );
                for m in members {
                    assert_eq!(
                        m.fql_kind, "enumerator",
                        "kind filter returned member with kind {}",
                        m.fql_kind
                    );
                }
            }
            other => panic!("expected Members, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Member variable → body resolution (regression: field)
// -----------------------------------------------------------------------

/// Create a temp workspace with a header declaring a class method and a
/// .cpp file providing the out-of-line definition.
fn engine_with_class_method() -> (ForgeQLEngine, String, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");

    fs::write(
        dir.path().join("widget.hpp"),
        "\
class Widget {
  public:
    void render(int flags);
    int  width() const;
};
",
    )
    .expect("write header");

    fs::write(
        dir.path().join("widget.cpp"),
        "\
#include \"widget.hpp\"

void Widget::render(int flags) {
    if (flags & 1) {
        // draw
    }
}

int Widget::width() const {
    return 42;
}
",
    )
    .expect("write cpp");

    let data_dir = dir.path().join("data");
    let mut engine = ForgeQLEngine::new(data_dir, common::make_registry_real()).expect("engine");
    let sid = engine
        .register_local_session(dir.path())
        .expect("register session");
    (engine, sid, dir)
}

#[test]
fn show_body_resolves_bare_member_name() {
    let (mut engine, sid, _dir) = engine_with_class_method();

    // Bare name should follow body_symbol → Widget::render
    let result = execute_fql(&mut engine, &sid, "SHOW body OF 'render'");
    match &result {
        ForgeQLResult::Show(sr) => match &sr.content {
            ShowContent::Lines { lines, .. } => {
                assert!(!lines.is_empty(), "SHOW body OF 'render' must return lines");
                let full_text: String = lines
                    .iter()
                    .map(|l| l.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    full_text.contains("Widget::render"),
                    "body must come from the qualified definition, got: {full_text}"
                );
            }
            other => panic!("expected Lines, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    }
}

#[test]
fn show_body_qualified_name_still_works() {
    let (mut engine, sid, _dir) = engine_with_class_method();

    // Fully qualified name should still work directly.
    let result = execute_fql(&mut engine, &sid, "SHOW body OF 'Widget::render'");
    match &result {
        ForgeQLResult::Show(sr) => match &sr.content {
            ShowContent::Lines { lines, .. } => {
                assert!(
                    !lines.is_empty(),
                    "SHOW body OF 'Widget::render' must return lines"
                );
            }
            other => panic!("expected Lines, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    }
}

#[rustfmt::skip]
#[test]
fn member_variable_has_body_symbol_field() {
    let (mut engine, sid, _dir) = engine_with_class_method();

    // The field for 'render' should carry body_symbol = "Widget::render"
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name = 'render' WHERE fql_kind = 'field'",
    );
    match &result {
        ForgeQLResult::Query(qr) => {
            assert_eq!(
                qr.results.len(),
                1,
                "exactly one field for render"
            );
            let row = &qr.results[0];
            let body_sym = row.fields.get("body_symbol").map(String::as_str);
            assert_eq!(
                body_sym,
                Some("Widget::render"),
                "body_symbol must point to the qualified definition"
            );
        }
        other => panic!("expected Query, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Line-budget integration tests
// -----------------------------------------------------------------------

const fn budget_config() -> forgeql_core::config::LineBudgetConfig {
    forgeql_core::config::LineBudgetConfig {
        initial: 50,
        ceiling: 200,
        recovery_base: 10,
        recovery_window_secs: 60,
        warning_threshold: 20,
        critical_threshold: 10,
        critical_max_lines: 5,
        idle_reset_secs: 300,
    }
}

#[test]
fn budget_deducts_on_show_lines() {
    let (mut engine, sid, _dir) = engine_with_session();
    engine.init_session_budget(&sid, &budget_config());

    // Confirm budget starts at initial.
    let snap = engine.budget_status(&sid).expect("budget active");
    assert_eq!(snap.remaining, 50);

    // SHOW LINES returns source lines — budget should decrease.
    let result = execute_fql(
        &mut engine,
        &sid,
        "SHOW LINES 1-5 OF 'motor_control.h' LIMIT 5",
    );
    let lines_returned = result.source_lines_count();
    assert!(lines_returned > 0, "should return lines");

    let snap = engine.budget_status(&sid).expect("budget active");
    // No recovery on SHOW LINES — pure deduction.
    assert_eq!(snap.remaining, 50 - lines_returned);
}

#[test]
fn budget_not_deducted_on_find_symbols() {
    let (mut engine, sid, _dir) = engine_with_session();
    engine.init_session_budget(&sid, &budget_config());

    // FIND symbols returns structured data, not source lines.
    let _ = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE fql_kind = 'function' LIMIT 5",
    );

    let snap = engine.budget_status(&sid).expect("budget active");
    // Recovery may increase it, but it should not go below initial.
    assert!(snap.remaining >= 50, "budget should not decrease for FIND");
}

#[test]
fn budget_critical_caps_show_lines() {
    let (mut engine, sid, _dir) = engine_with_session();
    let mut cfg = budget_config();
    cfg.initial = 5;
    cfg.critical_threshold = 10; // start below critical
    cfg.critical_max_lines = 3;
    engine.init_session_budget(&sid, &cfg);

    // Request 10 lines — should be capped to critical_max_lines (3).
    let result = execute_fql(
        &mut engine,
        &sid,
        "SHOW LINES 1-10 OF 'motor_control.h' LIMIT 10",
    );
    let lines_returned = result.source_lines_count();
    assert!(
        lines_returned <= 3,
        "critical state should cap to 3 lines, got {lines_returned}"
    );

    // Verify hint mentions budget.
    if let ForgeQLResult::Show(ref sr) = result {
        assert!(
            sr.hint
                .as_ref()
                .is_some_and(|h| h.contains("Budget critical")),
            "hint should mention budget: {:?}",
            sr.hint
        );
    }
}

#[test]
fn budget_absent_without_config() {
    let (engine, sid, _dir) = engine_with_session();
    // No init_session_budget call — budget should be None.
    assert!(engine.budget_status(&sid).is_none());
    assert!(engine.budget_status(&sid).is_none());
}

// -----------------------------------------------------------------------
// Regression: multiple WHERE name LIKE predicates — all must be applied
// -----------------------------------------------------------------------

/// Two `WHERE name LIKE` clauses must both be evaluated.  Before the fix,
/// only the first was applied (the second was silently stripped from
/// `non_usages_preds`), so `encenderSistema` leaked through even though it
/// doesn't contain "Motor".
#[test]
fn find_symbols_multiple_name_like_all_applied() {
    let (mut engine, sid, _dir) = engine_with_session();

    // Only `encenderMotor` contains both "encender" AND "Motor".
    // `encenderSistema` has "encender" but NOT "Motor" — must be excluded.
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name LIKE '%encender%' WHERE name LIKE '%Motor%'",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
            assert!(
                names.contains(&"encenderMotor"),
                "encenderMotor should be in results: {names:?}"
            );
            assert!(
                !names.contains(&"encenderSistema"),
                "encenderSistema must NOT be in results (lacks 'Motor'): {names:?}"
            );
            // Sanity: every returned name contains both required substrings.
            for name in &names {
                assert!(
                    name.contains("encender") && name.contains("Motor"),
                    "result '{name}' does not satisfy both LIKE predicates"
                );
            }
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Regression: fql_kind predicate must not be stripped when trigram is
// the candidate source (not fql_kind_index)
// -----------------------------------------------------------------------

/// `WHERE name LIKE '%Motor%' WHERE fql_kind = 'function'` must return only
/// functions.  Before the fix, the trigram candidate source was used (because
/// a LIKE predicate was present), but the `fql_kind` predicate was
/// incorrectly stripped from `non_usages_preds` — so struct/enum/comment
/// rows also appeared in the output.
#[test]
fn find_symbols_like_with_fql_kind_filter_respected() {
    let (mut engine, sid, _dir) = engine_with_session();

    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name LIKE '%Motor%' WHERE fql_kind = 'function'",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            // Must include the two known motor functions.
            let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
            assert!(
                names.contains(&"encenderMotor"),
                "missing encenderMotor: {names:?}"
            );
            assert!(
                names.contains(&"apagarMotor"),
                "missing apagarMotor: {names:?}"
            );
            // Every result must be fql_kind = 'function'.
            for r in &qr.results {
                assert_eq!(
                    r.fql_kind.as_deref(),
                    Some("function"),
                    "non-function leaked through fql_kind filter: {} ({:?})",
                    r.name,
                    r.fql_kind
                );
            }
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

/// Same guard for combined multi-LIKE + fql_kind: only `encenderMotor`
/// matches both `%encender%` AND `%Motor%` AND `fql_kind = 'function'`.
#[test]
fn find_symbols_multi_like_with_fql_kind_filter_respected() {
    let (mut engine, sid, _dir) = engine_with_session();

    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name LIKE '%encender%' WHERE name LIKE '%Motor%' WHERE fql_kind = 'function'",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
            assert!(
                names.contains(&"encenderMotor"),
                "missing encenderMotor: {names:?}"
            );
            assert!(
                !names.contains(&"encenderSistema"),
                "encenderSistema must be excluded: {names:?}"
            );
            for r in &qr.results {
                assert_eq!(
                    r.fql_kind.as_deref(),
                    Some("function"),
                    "non-function leaked through fql_kind filter: {} ({:?})",
                    r.name,
                    r.fql_kind
                );
                assert!(
                    r.name.contains("encender") && r.name.contains("Motor"),
                    "result '{}' does not satisfy both LIKE predicates",
                    r.name
                );
            }
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Regression: trigram pre-filter must respect like_match's ASCII
// case-insensitive semantics
// -----------------------------------------------------------------------

/// `LIKE '%MOTOR%'` (uppercase pattern) must match `encenderMotor` and
/// `apagarMotor` exactly like `LIKE '%Motor%'`, because `like_match` is
/// ASCII case-insensitive.  Before the fix the trigram index was built
/// over original-case bytes, so the uppercase trigrams `MOT`/`OTO`/`TOR`
/// were never found and the candidate set was empty.
#[test]
fn find_symbols_like_uppercase_pattern_matches_mixed_case_names() {
    let (mut engine, sid, _dir) = engine_with_session();

    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name LIKE '%MOTOR%' WHERE fql_kind = 'function'",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
            assert!(
                names.contains(&"encenderMotor"),
                "missing encenderMotor: {names:?}"
            );
            assert!(
                names.contains(&"apagarMotor"),
                "missing apagarMotor: {names:?}"
            );
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

/// Symmetric guard: an `(?i)` regex with a mixed-case literal must
/// still find both upper- and lower-case names.  The trigram pre-filter
/// uses the literal verbatim — making the index case-insensitive at
/// build time keeps this correct.
#[test]
fn find_symbols_matches_case_insensitive_flag_returns_all_matches() {
    let (mut engine, sid, _dir) = engine_with_session();

    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name MATCHES '(?i)Motor' WHERE fql_kind = 'function'",
    );
    match result {
        ForgeQLResult::Query(qr) => {
            let names: Vec<&str> = qr.results.iter().map(|r| r.name.as_str()).collect();
            assert!(
                names.contains(&"encenderMotor"),
                "missing encenderMotor: {names:?}"
            );
            assert!(
                names.contains(&"apagarMotor"),
                "missing apagarMotor: {names:?}"
            );
        }
        other => panic!("expected Query, got: {other:?}"),
    }
}

/// BUG-016 residual: a purely numeric `TO` destination is rejected with
/// guidance instead of silently creating a file named after the number.
#[test]
fn move_lines_to_numeric_dest_rejected() {
    let (mut engine, session_id, dir) = engine_with_session();
    let ops = parser::parse("MOVE LINES 1-2 OF 'motor_control.cpp' TO 3").expect("parse");
    let coords = SessionCoords::from_session_id(&session_id).expect("valid session_id");
    let err = engine
        .execute(auth(AuthContext::Tester), Some(&coords), &ops[0])
        .result
        .expect_err("numeric TO destination must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("must be a file path, not a number"),
        "unexpected error: {msg}"
    );
    assert!(
        !dir.path().join("3").exists(),
        "no file named '3' may be created"
    );
}

/// BUG-014 residual: whole-file deletion (`CHANGE FILE … WITH NOTHING`) is
/// exempt from the indexed-file gate — a ForgeQL-created file can be removed
/// from within ForgeQL.
#[test]
fn change_file_with_nothing_deletes_indexed_file() {
    let (mut engine, session_id, dir) = engine_with_session();
    let _ = execute_fql(
        &mut engine,
        &session_id,
        "COPY LINES 1-2 OF 'motor_control.cpp' TO '_scratch_delete_me.rs'",
    );
    assert!(dir.path().join("_scratch_delete_me.rs").exists());
    let _ = execute_fql(
        &mut engine,
        &session_id,
        "CHANGE FILE '_scratch_delete_me.rs' WITH NOTHING",
    );
    assert!(
        !dir.path().join("_scratch_delete_me.rs").exists(),
        "WITH NOTHING must delete the file even though it is indexed"
    );
}

/// BUG-006 U2: `FIND usages OF` must return usage SITES read from the
/// segment usage postings, not just definition rows. `encenderMotor` is
/// assigned to a function pointer at motor_control.cpp:34 — an occurrence
/// with no call parentheses that grep-style discovery misses.
#[test]
fn find_usages_returns_usage_sites_not_definitions() {
    let (mut engine, session_id, _dir) = engine_with_session();
    let result = execute_fql(&mut engine, &session_id, "FIND usages OF 'encenderMotor'");
    let ForgeQLResult::Query(qr) = result else {
        panic!("expected Query result from FIND usages");
    };
    let lines: Vec<usize> = qr.results.iter().filter_map(|r| r.line).collect();
    assert!(
        lines.contains(&34),
        "function-pointer assignment site (line 34) missing from usages: {lines:?}"
    );
    assert!(
        qr.results.len() >= 2,
        "expected multiple usage sites, got {}",
        qr.results.len()
    );
}

/// BUG-006 U3: `FIND symbols` rows carry a real `usages_count` stamped from
/// the overlay usages aggregate (it was perpetually 0), and `WHERE usages`
/// predicates are no longer pruned to empty by the stale all-zeros zone map.
#[test]
fn find_symbols_usages_count_is_real() {
    let (mut engine, session_id, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &session_id,
        "FIND symbols WHERE name = 'encenderMotor' WHERE fql_kind = 'function'",
    );
    let ForgeQLResult::Query(qr) = result else {
        panic!("expected Query result");
    };
    let row = qr.results.first().expect("encenderMotor row");
    assert!(
        row.usages_count.unwrap_or(0) >= 2,
        "usages_count must come from the usages aggregate, got {:?}",
        row.usages_count
    );

    let filtered = execute_fql(
        &mut engine,
        &session_id,
        "FIND symbols WHERE name = 'encenderMotor' WHERE usages > 0",
    );
    let ForgeQLResult::Query(fq) = filtered else {
        panic!("expected Query result");
    };
    assert!(
        !fq.results.is_empty(),
        "WHERE usages > 0 must not be pruned by the stale zone map"
    );
}

/// A WHERE field that no row carries is refused outright with an error
/// naming the field. A valid enrichment field that merely has no matches
/// must NOT hint.
#[test]
fn unknown_where_field_gets_hint() {
    let (mut engine, sid, _dir) = engine_with_session();

    // The columnar unknown-field guard refuses the query outright, naming
    // the field; the legacy backend answered with an empty result plus a
    // hint. Production serves reads from columnar, so the refusal is the
    // contract this test pins.
    let err = fql_err(&mut engine, &sid, "FIND symbols WHERE nmae = \"x\"");
    assert!(
        err.contains("nmae"),
        "the refusal must name the unknown field: {err}"
    );

    let r = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE has_todo = \"true\" WHERE name = \"no_such_fn_zz\"",
    );
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    assert!(
        qr.hint.is_none(),
        "a valid enrichment field must not hint: {:?}",
        qr.hint
    );
}

/// The mechanical rename sweep: FIND aims at the usage sites, CHANGE NODES
/// LAST sweeps the replacement across exactly those lines. Occurrences in
/// string literals and comments are NOT usage sites, so they survive — the
/// fixture's classic sed-trap lines prove the precision.
#[test]
fn rename_sweep_via_find_then_change_nodes_found() {
    let (mut engine, sid, dir) = engine_with_session();

    let r = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    assert!(!qr.results.is_empty(), "usage sites expected");
    let rev = qr.found_rev.expect("a complete FIND issues a master rev");

    let r = execute_fql(
        &mut engine,
        &sid,
        &format!(
            "CHANGE NODES FOUND IF REV '{rev}' MATCHING WORD 'encenderMotor' WITH 'startMotor'"
        ),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(mr.applied);
    assert!(
        mr.edit_count >= 2,
        "multiple sites swept: {}",
        mr.edit_count
    );

    let cpp = fs::read_to_string(dir.path().join("motor_control.cpp")).expect("read cpp");
    assert!(cpp.contains("startMotor"), "rename applied");
    assert!(
        cpp.contains("encenderMotor: velocidad"),
        "string-literal occurrence must survive (not a usage site)"
    );
}

/// `CHANGE NODES FOUND` without a previous FIND fails with guidance.
#[test]
fn change_nodes_found_without_find_errors() {
    let (mut engine, sid, _dir) = engine_with_session();
    let ops = parser::parse("CHANGE NODES FOUND MATCHING 'a' WITH 'b'").expect("parse");
    let coords = SessionCoords::from_session_id(&sid).expect("valid session_id");
    let err = engine
        .execute(auth(AuthContext::Tester), Some(&coords), &ops[0])
        .result
        .expect_err("must fail without a previous FIND")
        .to_string();
    assert!(
        err.contains("no FIND result is armed"),
        "guidance expected: {err}"
    );
    assert!(
        err.contains(r#""error":"no_found_set""#),
        "the refusal is a structured self-healing rejection: {err}"
    );
}

/// A complete FIND issues a master rev; quoting it back runs the sweep.
#[test]
fn found_rev_gates_the_sweep() {
    let (mut engine, sid, dir) = engine_with_session();

    let r = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    let rev = qr
        .found_rev
        .expect("a complete FIND must issue a master rev");

    let r = execute_fql(
        &mut engine,
        &sid,
        &format!(
            "CHANGE NODES FOUND IF REV '{rev}' MATCHING WORD 'encenderMotor' WITH 'startMotor'"
        ),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(mr.applied);

    let cpp = fs::read_to_string(dir.path().join("motor_control.cpp")).expect("read cpp");
    assert!(cpp.contains("startMotor"), "rename applied under the gate");
}

/// A master rev that no longer matches the live members refuses the mutation —
/// and hands back no replacement rev, so the only way on is to look again.
#[test]
fn stale_found_rev_is_refused() {
    let (mut engine, sid, _dir) = engine_with_session();

    let _ = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let err = fql_err(
        &mut engine,
        &sid,
        "CHANGE NODES FOUND IF REV 'hdeadbeefdeadbeef' MATCHING 'encenderMotor' WITH 'x'",
    );
    assert!(err.contains("rev_mismatch"), "gate must fire: {err}");
    assert!(
        !err.contains("\"current\""),
        "a set-level mismatch must not hand back a fresh rev to blindly retry: {err}"
    );
}

/// A GROUP BY row is a count with a filename on it. It must clear LAST rather
/// than arm a set that no verb can act on.
#[test]
fn group_by_result_clears_found_set() {
    let (mut engine, sid, _dir) = engine_with_session();

    let _ = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let r = execute_fql(&mut engine, &sid, "FIND symbols GROUP BY file");
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    assert!(
        qr.found_rev.is_none(),
        "an aggregate addresses nothing — no master rev"
    );

    let err = fql_err(
        &mut engine,
        &sid,
        "CHANGE NODES FOUND MATCHING 'encenderMotor' WITH 'x'",
    );
    assert!(
        err.contains("no FIND result is armed"),
        "the aggregate must clear LAST, not leave the previous set armed: {err}"
    );
}

/// The destructive bulk verbs will not run ungated.
#[test]
fn delete_node_last_requires_if_rev() {
    let (mut engine, sid, _dir) = engine_with_session();

    let _ = execute_fql(&mut engine, &sid, "FIND files");
    let err = fql_err(&mut engine, &sid, "DELETE NODES FOUND");
    assert!(
        err.contains("requires IF REV"),
        "a bulk delete must demand the gate: {err}"
    );
    assert!(
        err.contains(r#""error":"found_refused""#),
        "the refusal is a structured self-healing rejection: {err}"
    );
}

/// `FIND usages` rows are call sites, not nodes: they cannot be deleted or moved.
#[test]
fn bulk_delete_refuses_a_set_of_usage_sites() {
    let (mut engine, sid, _dir) = engine_with_session();

    let r = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    let rev = qr.found_rev.expect("master rev");

    let err = fql_err(
        &mut engine,
        &sid,
        &format!("DELETE NODES FOUND IF REV '{rev}'"),
    );
    assert!(
        err.contains("addressable nodes"),
        "a site is not a node — say so: {err}"
    );
}

/// A session outlives the process. An agent may FIND, hand the session on (or
/// wait out a restart), and only then sweep — so the set has to come back.
#[test]
fn found_set_survives_a_restart() {
    let (mut engine, sid, dir) = engine_with_session();

    let r = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    let rev = qr.found_rev.expect("master rev");
    drop(engine); // the server goes away between the FIND and the sweep

    let mut restarted =
        ForgeQLEngine::new(dir.path().join("data"), common::make_registry_real()).expect("engine");
    let sid2 = restarted
        .register_local_session(dir.path())
        .expect("register session");

    let r = execute_fql(
        &mut restarted,
        &sid2,
        &format!(
            "CHANGE NODES FOUND IF REV '{rev}' MATCHING WORD 'encenderMotor' WITH 'startMotor'"
        ),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(
        mr.applied,
        "a set armed before the restart is still the set"
    );

    let cpp = fs::read_to_string(dir.path().join("motor_control.cpp")).expect("read cpp");
    assert!(cpp.contains("startMotor"));
}

/// ...but a mutation clears the persisted copy too. Resurrecting a set whose
/// members the mutation just moved would hand stale spans to the next sweep —
/// and a rev quoted from before the mutation must no longer authorise a sweep.
#[test]
fn a_mutation_clears_the_set_on_disk_too() {
    let (mut engine, sid, dir) = engine_with_session();

    let r = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let ForgeQLResult::Query(qr) = r else {
        panic!("expected Query result");
    };
    let rev = qr.found_rev.expect("master rev");
    let _ = execute_fql(
        &mut engine,
        &sid,
        &format!(
            "CHANGE NODES FOUND IF REV '{rev}' MATCHING WORD 'encenderMotor' WITH 'startMotor'"
        ),
    );
    drop(engine);

    let mut restarted =
        ForgeQLEngine::new(dir.path().join("data"), common::make_registry_real()).expect("engine");
    let sid2 = restarted
        .register_local_session(dir.path())
        .expect("register session");

    let err = fql_err(
        &mut restarted,
        &sid2,
        "CHANGE NODES FOUND MATCHING 'startMotor' WITH 'x'",
    );
    assert!(
        err.contains("no FIND result is armed"),
        "the mutation must have cleared the on-disk set, not left it to be restored: {err}"
    );
}

/// Every verb that names an existing node demands the gate.
#[test]
fn existing_node_verbs_require_if_rev() {
    let (mut engine, sid, _dir) = engine_with_session();

    let (id, rev) = file_handle(&mut engine, &sid, "motor_control.cpp");
    assert!(
        rev.starts_with('h'),
        "the rev travels with the handle: {rev}"
    );

    for fql in [
        format!("CHANGE NODE '{id}' WITH 'void x() {{}}'"),
        format!("DELETE NODE '{id}'"),
        format!("MOVE NODE '{id}' TO 'moved.cpp'"),
    ] {
        let err = fql_err(&mut engine, &sid, &fql);
        assert!(
            err.contains("requires IF REV"),
            "an ungated mutation on an existing node must be refused: {fql} → {err}"
        );
    }
}

/// The scenario the gate exists for: an agent carries a handle across other
/// commands, the code under it moves, and it comes back with the rev it read
/// first. The handle still resolves — handles are stable — so nothing but the
/// rev can tell it that the thing it remembers is not the thing that is there.
#[test]
fn a_stale_rev_cannot_overwrite() {
    // Known divergence: on columnar, the rev handed out by FIND files does
    // not match the rev the mutation layer computes for the same file, so
    // the IF REV round-trip fails. Legacy-pinned until both derivations agree.
    let (mut engine, sid, _dir) = engine_with_session_legacy();

    let (id, rev) = file_handle(&mut engine, &sid, "motor_control.cpp");

    let r = execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{id}' IF REV '{rev}' MATCHING 'encenderMotor' WITH 'startMotor'"),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(mr.applied);
    let new_rev = mr.new_rev.expect("a mutation hands back the new rev");
    assert_ne!(new_rev, rev, "the edit moved the node's rev");

    // The agent still remembers the rev it read before that edit.
    let err = fql_err(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{id}' IF REV '{rev}' WITH '// clobber'"),
    );
    assert!(
        err.contains("rev_mismatch"),
        "a stale rev must not be allowed to overwrite: {err}"
    );

    // The rev the mutation handed back works, with no re-read in between.
    let r = execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{id}' IF REV '{new_rev}' MATCHING 'startMotor' WITH 'runMotor'"),
    );
    let ForgeQLResult::Mutation(mr) = r else {
        panic!("expected Mutation result");
    };
    assert!(mr.applied, "the post-edit rev must be usable straight away");
}

/// The handle and rev of a file, as `FIND files` hands them out together.
fn file_handle(engine: &mut ForgeQLEngine, sid: &str, name: &str) -> (String, String) {
    let r = execute_fql(engine, sid, &format!("FIND files WHERE name = '{name}'"));
    let ForgeQLResult::Show(show) = r else {
        panic!("expected Show result");
    };
    let ShowContent::FileList { files, .. } = show.content else {
        panic!("expected FileList");
    };
    let f = files.first().expect("a file row");
    (
        f.node_id.clone().expect("handle"),
        f.rev.clone().expect("rev"),
    )
}

/// COPY LINES reports the addressed range length, not the payload's text-line
/// count: the line model treats the position after a final newline as an
/// addressable zero-byte line, so a whole-file copy `1-<count>` used to say
/// one line fewer than requested and read like data loss.
#[test]
fn copy_lines_reports_addressed_range_length() {
    let (mut engine, session_id, dir) = engine_with_session();
    let content =
        std::fs::read_to_string(dir.path().join("motor_control.cpp")).expect("read fixture");
    // The engine's line-addressing model: a trailing newline opens a final
    // addressable empty line, so the line count is split('\n').count().
    let model_lines = content.split('\n').count();

    let result = execute_fql(
        &mut engine,
        &session_id,
        &format!("COPY LINES 1-{model_lines} OF 'motor_control.cpp' TO 'dupes/copy.cpp'"),
    );
    let ForgeQLResult::Mutation(m) = result else {
        panic!("expected Mutation result from COPY LINES");
    };
    assert_eq!(
        m.lines_written, model_lines,
        "whole-file copy must report the addressed range length"
    );

    // Byte-for-byte identical copy — the count difference was presentation only.
    let copied = std::fs::read_to_string(dir.path().join("dupes/copy.cpp")).expect("read copy");
    assert_eq!(copied, content, "copied bytes must equal the source");
}

/// MOVE LINES reports the same addressed range length on both counters — a
/// clean move must never look like net line loss.
#[test]
fn move_lines_reports_symmetric_range_counts() {
    let (mut engine, session_id, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &session_id,
        "MOVE LINES 1-3 OF 'motor_control.cpp' TO 'dupes/moved.cpp'",
    );
    let ForgeQLResult::Mutation(m) = result else {
        panic!("expected Mutation result from MOVE LINES");
    };
    assert_eq!(m.lines_written, 3, "moved range length");
    assert_eq!(
        m.lines_removed, m.lines_written,
        "a clean move reports written == removed"
    );
}
