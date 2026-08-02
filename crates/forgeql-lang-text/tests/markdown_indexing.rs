//! Integration coverage for Markdown prose occurrences.
//!
//! The doc role is pinned on reStructuredText by the golden corpus; these cases
//! cover the Markdown half, which no corpus case reaches.

#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use forgeql_core::ast::enrich::default_enrichers;
use forgeql_core::ast::index::{IndexContext, SymbolTable, index_file};
use forgeql_core::ast::lang::LanguageSupport;
use forgeql_lang_text::MarkdownLanguage;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Index a Markdown fixture through the production indexer.
fn index_fixture(name: &str) -> SymbolTable {
    let lang = MarkdownLanguage;
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
        let count = index_file(&mut parser, &mut ctx, None).expect("index_file should succeed");
        assert!(count > 0, "expected at least one indexed row");
    }
    table
}

/// Every prose token is one occurrence, never two.
///
/// This is the property the mapping is chosen for. A setext heading holds its
/// text in a child `paragraph` node, and a list item holds its text in a
/// paragraph too, so mapping either container *alongside* `paragraph` would
/// scan the same span twice and report every token in it twice. Only
/// `paragraph` and `atx_heading` are mapped: an atx heading keeps its text in an
/// `inline` node, so it cannot overlap a paragraph.
#[test]
fn each_prose_token_is_a_single_doc_occurrence() {
    let table = index_fixture("prose_headings.md");
    let doc = table
        .mentions
        .get("doc")
        .expect("markdown prose produces doc mentions");

    for token in ["ATXTOKEN", "PARATOKEN", "SETEXTTOKEN", "LISTTOKEN"] {
        let sites = doc
            .get(token)
            .unwrap_or_else(|| panic!("{token} should be a doc occurrence"));
        assert_eq!(
            sites.len(),
            1,
            "{token} must occur exactly once, got {} sites — a container that \
             encloses a paragraph was mapped alongside it",
            sites.len()
        );
    }
}
