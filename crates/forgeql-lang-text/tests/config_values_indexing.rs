//! Integration coverage for config-value occurrences.
//!
//! The corpus goldens pin the counts; these cases pin the two shapes that are
//! easy to get wrong and cheap to get wrong silently — a key nested inside a
//! value, and a sequence entry that carries no field label of its own.

#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use forgeql_core::ast::enrich::default_enrichers;
use forgeql_core::ast::index::{IndexContext, SymbolTable, index_file};
use forgeql_core::ast::lang::LanguageSupport;
use forgeql_lang_text::YamlLanguage;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Index a YAML fixture through the production indexer.
fn index_fixture(name: &str) -> SymbolTable {
    let lang = YamlLanguage;
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
            workspace_root: None,
        };
        let _ = index_file(&mut parser, &mut ctx, None).expect("index_file should succeed");
    }
    table
}

#[test]
fn values_are_config_occurrences_and_keys_are_not() {
    let table = index_fixture("config_values.yaml");
    let config = table
        .mentions
        .get("config")
        .expect("yaml values produce config mentions");

    for token in ["HYPHEN-VALUE", "SEQITEM", "NESTEDVALUE"] {
        let sites = config
            .get(token)
            .unwrap_or_else(|| panic!("{token} should be a config occurrence"));
        assert_eq!(
            sites.len(),
            1,
            "{token} should occur exactly once, got {}",
            sites.len()
        );
    }

    // `innerkey` is the one that matters: it sits inside `nestedkey`'s value,
    // so a rule that stayed armed for the whole subtree would tag it.
    for token in ["outerkey", "listkey", "nestedkey", "innerkey"] {
        assert!(
            !config.contains_key(token),
            "{token} is a key and must not be a config occurrence"
        );
    }
}
