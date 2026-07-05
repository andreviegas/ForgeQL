//! Integration coverage for CMake indexing.
//!
//! Exercised against the production indexer to prove the behaviors a live
//! `run_fql` session relies on: command calls are addressable, function
//! definitions resolve, and `if()`/`foreach()` blocks emit nested
//! control-flow rows (the gap found while exercising 0.91.1 on zephyr —
//! `kind_map` entries alone do not index control flow; the `control_flow`
//! config section does).

#![allow(
    clippy::panic,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::doc_markdown
)]

use std::path::PathBuf;

use forgeql_core::ast::enrich::default_enrichers;
use forgeql_core::ast::index::{IndexContext, SymbolTable, index_file};
use forgeql_core::ast::lang::LanguageSupport;
use forgeql_lang_text::CmakeLanguage;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Index a CMake fixture through the production indexer.
fn index_fixture(name: &str) -> SymbolTable {
    let lang = CmakeLanguage;
    let path = fixture_path(name);
    assert!(path.exists(), "fixture missing: {}", path.display());

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.tree_sitter_language())
        .expect("set_language");

    let enrichers = default_enrichers();
    let mut table = SymbolTable::default();
    {
        let mut ctx = IndexContext {
            path: &path,
            language: &lang,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: None,
            table: &mut table,
        };
        let count = index_file(&mut parser, &mut ctx, None).expect("index_file should succeed");
        assert!(count > 0, "expected at least one indexed row");
    }
    table
}

/// Every row of a given fql kind, with its ordinal.
fn rows_of_kind(table: &SymbolTable, kind: &str) -> Vec<Option<u32>> {
    table.rows_by_fql_kind(kind).map(|r| r.ordinal).collect()
}

/// Command calls are addressable objects named by the command identifier.
#[test]
fn commands_are_addressable() {
    let table = index_fixture("build_options.cmake");
    let rows = table.find_all_defs("set");
    let row = rows
        .iter()
        .find(|r| table.fql_kind_of(r) == "call_statement")
        .expect("'set' should be indexed as a call_statement");
    assert!(row.ordinal.is_some(), "'set' must carry an ordinal");
}

/// Function definitions are addressable and named by their first argument.
#[test]
fn function_defs_are_addressable() {
    let table = index_fixture("build_options.cmake");
    let rows = table.find_all_defs("register_test");
    let row = rows
        .iter()
        .find(|r| table.fql_kind_of(r) == "function")
        .expect("'register_test' should be indexed as a function");
    assert!(row.ordinal.is_some());
}

/// `if()` and `foreach()` blocks emit addressable control-flow rows —
/// driven by the `control_flow` config section, not the kind map.
#[test]
fn control_flow_blocks_are_addressable() {
    let table = index_fixture("build_options.cmake");
    let ifs = rows_of_kind(&table, "if");
    assert!(!ifs.is_empty(), "if(BUILD_TESTS) block should be indexed");
    assert!(ifs.iter().all(Option::is_some), "if rows need ordinals");
    let fors = rows_of_kind(&table, "for");
    assert!(!fors.is_empty(), "foreach() block should be indexed");
    assert!(fors.iter().all(Option::is_some), "for rows need ordinals");
}

/// Control-flow rows are FINDable by name: the CMake grammar has no
/// `condition` field, so the enricher names them by the construct's first
/// line (the nameless-row gap found live on zephyr in 0.91.1).
#[test]
fn control_flow_rows_are_named_by_first_line() {
    let table = index_fixture("build_options.cmake");
    let rows = table.find_all_defs("if(BUILD_TESTS)");
    assert!(
        rows.iter().any(|r| table.fql_kind_of(r) == "if"),
        "the if block should be named 'if(BUILD_TESTS)'"
    );
    let rows = table.find_all_defs("foreach(t IN LISTS TEST_LIST)");
    assert!(
        rows.iter().any(|r| table.fql_kind_of(r) == "for"),
        "the foreach block should be named by its first line"
    );
}
