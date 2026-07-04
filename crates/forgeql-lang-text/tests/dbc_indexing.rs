//! Integration coverage for DBC (Vector CAN database) indexing.
//!
//! A `.dbc` file describes the messages and signals on a CAN bus. These
//! tests index a realistic fixture through the production indexer and
//! assert that messages, their nested signals, and value tables all
//! receive their own ordinals — the basis of the nested `node_id`
//! handles an agent uses to edit one signal without touching the file.

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
use forgeql_lang_text::DbcLanguage;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Index a DBC fixture through the production indexer.
fn index_fixture(name: &str) -> SymbolTable {
    let lang = DbcLanguage;
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

/// The ordinal of the single row named `name` with fql kind `kind`.
fn ordinal_of(table: &SymbolTable, name: &str, kind: &str) -> u32 {
    let rows = table.find_all_defs(name);
    let row = rows
        .iter()
        .find(|r| table.fql_kind_of(r) == kind)
        .unwrap_or_else(|| panic!("'{name}' should be indexed as a {kind}"));
    row.ordinal
        .unwrap_or_else(|| panic!("'{name}' must carry an ordinal for a stable node_id"))
}

/// BO_ messages are addressable objects named by their message name.
#[test]
fn messages_are_addressable() {
    let table = index_fixture("powertrain.dbc");
    let _ = ordinal_of(&table, "EngineData", "object");
    let _ = ordinal_of(&table, "TransmissionData", "object");
}

/// SG_ signals nested inside each message are individually addressable,
/// with distinct ordinals across the whole file.
#[test]
fn signals_are_addressable_with_distinct_ordinals() {
    let table = index_fixture("powertrain.dbc");
    let signals = [
        "EngineSpeed",
        "EngineTemp",
        "ThrottlePos",
        "CurrentGear",
        "OutputSpeed",
    ];
    let mut ordinals: Vec<u32> = signals
        .iter()
        .map(|name| ordinal_of(&table, name, "field"))
        .collect();
    ordinals.sort_unstable();
    ordinals.dedup();
    assert_eq!(
        ordinals.len(),
        signals.len(),
        "every signal must get its own distinct ordinal"
    );
}

/// VAL_TABLE_ and VAL_ enumerations are addressable as enums.
#[test]
fn value_tables_are_addressable() {
    let table = index_fixture("powertrain.dbc");
    let _ = ordinal_of(&table, "GearTable", "enum");
    // The VAL_ entry for CurrentGear is a second, distinct enum row.
    let _ = ordinal_of(&table, "CurrentGear", "enum");
}
