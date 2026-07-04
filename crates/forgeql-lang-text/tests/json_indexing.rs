//! Integration coverage for JSON indexing.
//!
//! The primary motivation for JSON support is making large data files (such
//! as the golden-test corpus) navigable and editable by stable `node_id`.
//! A row is addressable — i.e. it carries an ordinal that the `node_id`
//! handle is derived from — only when its `fql_kind` is registered as
//! addressable in core.  These tests index a golden-style fixture through
//! the real indexer and assert that every entry receives such an ordinal.

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
use forgeql_lang_text::JsonLanguage;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Index a JSON fixture through the production indexer.
fn index_fixture(name: &str) -> SymbolTable {
    let lang = JsonLanguage;
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

/// A `pair` is indexed under its key and is addressable (has an ordinal).
#[test]
fn pair_keys_are_addressable() {
    let table = index_fixture("golden_sample.json");
    let rows = table.find_all_defs("fql");
    let pair = rows
        .iter()
        .find(|r| table.fql_kind_of(r) == "pair")
        .expect("'fql' member should be indexed as a pair");
    assert!(
        pair.ordinal.is_some(),
        "JSON pair rows must carry an ordinal so a stable node_id can be derived"
    );
}

/// An object named by its `name` member (a golden-test case) is addressable.
#[test]
fn named_objects_are_addressable() {
    let table = index_fixture("golden_sample.json");
    let rows = table.find_all_defs("G2_kernel_sched_first_5_functions");
    let object = rows
        .iter()
        .find(|r| table.fql_kind_of(r) == "object")
        .expect("the test-case object should be named by its 'name' member");
    assert!(
        object.ordinal.is_some(),
        "named JSON object rows must carry an ordinal"
    );
}

/// Repeated keys (every entry has a `name`) get distinct, non-colliding
/// ordinals — the basis for unique node_ids across the whole file.
#[test]
fn repeated_keys_get_distinct_ordinals() {
    let table = index_fixture("golden_sample.json");
    // "name" appears once at the top level and once in each nested row.
    let rows = table.find_all_defs("name");
    let ordinals: Vec<u32> = rows
        .iter()
        .filter(|r| table.fql_kind_of(r) == "pair")
        .filter_map(|r| r.ordinal)
        .collect();
    assert!(
        ordinals.len() >= 3,
        "expected the top-level and two nested 'name' members, got {}",
        ordinals.len()
    );
    let mut unique = ordinals.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        ordinals.len(),
        "node ordinals must be unique per entry (no collisions across repeated keys)"
    );
}
