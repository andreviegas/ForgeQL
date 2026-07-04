//! Integration coverage for XML indexing.
//!
//! The motivation for XML support is editing automotive configuration files
//! (EB tresos `.xdm`, AUTOSAR `.arxml` ECUC values) by stable `node_id`
//! instead of through a GUI. These tests index realistic fixtures through
//! the production indexer and assert that every nested container receives
//! its own ordinal — the basis of the nested `node_id` handles.

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
use forgeql_lang_xml::XmlLanguage;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Index an XML fixture through the production indexer.
fn index_fixture(name: &str) -> SymbolTable {
    let lang = XmlLanguage;
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

/// The ordinal of the single `object` row named `name`, or a panic with context.
fn object_ordinal(table: &SymbolTable, name: &str) -> u32 {
    let rows = table.find_all_defs(name);
    let row = rows
        .iter()
        .find(|r| table.fql_kind_of(r) == "object")
        .unwrap_or_else(|| panic!("'{name}' should be indexed as an element (object)"));
    row.ordinal
        .unwrap_or_else(|| panic!("'{name}' must carry an ordinal for a stable node_id"))
}

/// tresos .xdm: every named container/var down the nesting chain is
/// addressable, each with its own distinct ordinal.
#[test]
fn tresos_nested_containers_are_each_addressable() {
    let table = index_fixture("adc_config.xdm");
    let chain = [
        "Adc",
        "AdcConfigSet",
        "AdcHwUnit",
        "AdcHwUnit_0",
        "AdcPrescale",
        "AdcPriority",
        "AdcChannel",
        "AdcChannel_0",
        "AdcChannelId",
    ];
    let mut ordinals: Vec<u32> = chain
        .iter()
        .map(|name| object_ordinal(&table, name))
        .collect();
    ordinals.sort_unstable();
    ordinals.dedup();
    assert_eq!(
        ordinals.len(),
        chain.len(),
        "every nesting level must get its own distinct ordinal"
    );
}

/// AUTOSAR .arxml: containers are found by their SHORT-NAME identity.
#[test]
fn arxml_containers_named_by_short_name() {
    let table = index_fixture("adc_ecuc.arxml");
    for name in ["ActiveEcuC", "Adc", "AdcConfigSet", "AdcHwUnit_0"] {
        let _ = object_ordinal(&table, name);
    }
}

/// AUTOSAR .arxml: anonymous structural wrappers fall back to their tag name
/// and stay addressable — they are the INSERT targets for new containers.
#[test]
fn arxml_wrappers_are_addressable_by_tag() {
    let table = index_fixture("adc_ecuc.arxml");
    for name in ["CONTAINERS", "SUB-CONTAINERS", "PARAMETER-VALUES"] {
        let _ = object_ordinal(&table, name);
    }
}

/// Repeated same-tag siblings (two parameter values named by tag fallback)
/// receive distinct ordinals — unique node_ids across the whole file.
#[test]
fn repeated_tags_get_distinct_ordinals() {
    let table = index_fixture("adc_ecuc.arxml");
    let rows = table.find_all_defs("ECUC-NUMERICAL-PARAM-VALUE");
    let ordinals: Vec<u32> = rows
        .iter()
        .filter(|r| table.fql_kind_of(r) == "object")
        .filter_map(|r| r.ordinal)
        .collect();
    assert_eq!(
        ordinals.len(),
        2,
        "both parameter-value elements should be indexed"
    );
    assert_ne!(
        ordinals[0], ordinals[1],
        "sibling elements must not share an ordinal"
    );
}
