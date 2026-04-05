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
use std::path::PathBuf;
use std::sync::Arc;

use forgeql_core::ast::lang::{CppLanguageInline, LanguageRegistry};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::ir::{Clauses, ForgeQLIR};
use forgeql_core::parser;
use forgeql_core::result::{ForgeQLResult, ShowContent};
use tempfile::tempdir;

fn make_registry() -> Arc<LanguageRegistry> {
    Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]))
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/fixtures")
}

/// Create a temp workspace with motor_control fixtures and boot an engine
/// with a local session pointing at that workspace.
///
/// Returns `(engine, session_id, TempDir)`.  `TempDir` must stay alive.
fn engine_with_session() -> (ForgeQLEngine, String, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let src = fixtures_dir();

    // Copy fixtures into the temp workspace.
    let _ = fs::copy(
        src.join("motor_control.h"),
        dir.path().join("motor_control.h"),
    )
    .expect("copy .h");
    let _ = fs::copy(
        src.join("motor_control.cpp"),
        dir.path().join("motor_control.cpp"),
    )
    .expect("copy .cpp");

    // Create an engine with a data_dir inside the temp dir.
    let data_dir = dir.path().join("data");
    let mut engine = ForgeQLEngine::new(data_dir, make_registry()).expect("engine");

    // Register a local session.  The engine doesn't have a direct method
    // for this — we use the internal test helper.
    let session_id = engine
        .register_local_session(dir.path())
        .expect("register session");

    (engine, session_id, dir)
}

/// Parse FQL and execute the first op against the engine.
fn execute_fql(engine: &mut ForgeQLEngine, session_id: &str, fql: &str) -> ForgeQLResult {
    let ops = parser::parse(fql).expect("parse");
    let op = ops.first().expect("at least one op");
    engine.execute(Some(session_id), op).expect("execute")
}

// -----------------------------------------------------------------------
// Engine lifecycle
// -----------------------------------------------------------------------

#[test]
fn engine_starts_with_zero_state() {
    let tmp = tempdir().unwrap();
    let engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
    assert_eq!(engine.session_count(), 0);
    assert_eq!(engine.source_count(), 0);
    assert_eq!(engine.commands_served(), 0);
}

#[test]
fn show_sources_on_empty_engine() {
    let tmp = tempdir().unwrap();
    let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
    let result = engine.execute(None, &ForgeQLIR::ShowSources).unwrap();
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

// -----------------------------------------------------------------------
// Error cases
// -----------------------------------------------------------------------

#[test]
fn find_symbols_without_session_fails() {
    let tmp = tempdir().unwrap();
    let mut engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
    let op = ForgeQLIR::FindSymbols {
        clauses: Clauses::default(),
    };
    assert!(engine.execute(None, &op).is_err());
}

// (disconnect_unknown_session_fails removed — DISCONNECT command eliminated)

// -----------------------------------------------------------------------
// Result serialization round-trip
// -----------------------------------------------------------------------

#[test]
fn result_round_trips_through_json() {
    let (mut engine, sid, _dir) = engine_with_session();
    let result = execute_fql(
        &mut engine,
        &sid,
        "FIND symbols WHERE name LIKE 'encender%'",
    );

    // Serialize → deserialize and verify structure is preserved.
    let json = result.to_json();
    let deserialized: ForgeQLResult = serde_json::from_str(&json).expect("deserialize");
    match deserialized {
        ForgeQLResult::Query(qr) => {
            assert_eq!(qr.op, "find_symbols");
            assert!(!qr.results.is_empty());
        }
        other => panic!("expected Query, got: {other:?}"),
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
    let full = execute_fql(&mut engine, &sid, "SHOW outline OF 'motor_control.h'");
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
        "SHOW outline OF 'motor_control.h' LIMIT 2",
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
// BUG #3: FIND usages without GROUP BY — count column must be non-empty
// -----------------------------------------------------------------------

#[test]
fn find_usages_csv_count_column_is_non_empty() {
    let (mut engine, sid, _dir) = engine_with_session();

    // 'encenderMotor' appears in comments, macro bodies, and calls in the .cpp.
    // Without GROUP BY each usage site is a separate row; the count column
    // falls back to the 1-based line number so agents can distinguish rows.
    let result = execute_fql(&mut engine, &sid, "FIND usages OF 'encenderMotor'");
    let csv = result.to_csv();
    let v: serde_json::Value = serde_json::from_str(&csv).expect("CSV must be valid JSON");
    let rows = v["results"].as_array().expect("results array");
    // Skip header row (index 0); every data row must have a non-empty 4th column.
    for row in rows.iter().skip(1) {
        let col4 = row[3].as_str().unwrap_or("");
        assert!(
            !col4.is_empty(),
            "count/line column must not be empty in FIND usages CSV rows: {csv}"
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

    // ErrorMotor has enumerator members.  WHERE kind = 'enumerator' must include them.
    let result = execute_fql(
        &mut engine,
        &sid,
        "SHOW members OF 'ErrorMotor' WHERE fql_kind = 'enumerator'",
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
                        "WHERE fql_kind = 'enumerator' returned member with kind '{}'",
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
    let mut engine = ForgeQLEngine::new(data_dir, make_registry()).expect("engine");
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
