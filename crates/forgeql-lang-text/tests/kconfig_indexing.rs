//! Kconfig indexing: the shapes the corpus goldens do not reach.
//!
//! The grammar's kind names are pinned here as well as used, because an
//! unmapped kind never reaches the index — a renamed node would silently stop
//! producing rows rather than fail, and a flag's definition site would simply
//! go missing again.

#![allow(clippy::expect_used)]

use forgeql_core::ast::lang::LanguageSupport;
use forgeql_lang_text::KconfigLanguage;

/// Parse a Kconfig fragment with the bundled grammar.
fn parse(source: &[u8]) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&KconfigLanguage.tree_sitter_language())
        .expect("set kconfig language");
    parser.parse(source, None).expect("parse kconfig")
}

/// `config X` is a `config` node carrying a `name` child, which is what
/// `extract_name` reads and what the `kind_map` keys on.
#[test]
fn a_config_entry_is_named_by_its_name_child() {
    let source: &[u8] = b"config PM_DEVICE_RUNTIME\n\tbool \"Runtime PM\"\n";
    let tree = parse(source);
    let root = tree.root_node();
    assert_eq!(root.kind(), "configuration");

    let entry = root.child(0).expect("one entry");
    assert_eq!(entry.kind(), "config");
    assert_eq!(
        KconfigLanguage.extract_name(entry, source).as_deref(),
        Some("PM_DEVICE_RUNTIME")
    );
    assert_eq!(KconfigLanguage.map_kind("config"), Some("macro"));
    assert_eq!(KconfigLanguage.map_kind("menuconfig"), Some("macro"));
}

/// Every flag reference is a `symbol`, whatever keyword introduces it — which
/// is why one `usage_node_kinds` entry covers `depends on`, `select` and `if`.
#[test]
fn every_flag_reference_is_a_symbol_node() {
    let source: &[u8] =
        b"config A\n\tdepends on DEP_FLAG\n\tselect SEL_FLAG\n\nif IF_FLAG\nconfig B\n\tbool\nendif\n";
    let tree = parse(source);

    let mut found = Vec::new();
    let mut cursor = tree.walk();
    let mut descend = true;
    loop {
        if descend {
            if cursor.node().kind() == "symbol" {
                let text = std::str::from_utf8(&source[cursor.node().byte_range()]).expect("utf8");
                found.push(text.to_owned());
            }
            if cursor.goto_first_child() {
                continue;
            }
        }
        if cursor.goto_next_sibling() {
            descend = true;
            continue;
        }
        if !cursor.goto_parent() {
            break;
        }
        descend = false;
    }

    for flag in ["DEP_FLAG", "SEL_FLAG", "IF_FLAG"] {
        assert!(
            found.iter().any(|s| s == flag),
            "{flag} must be a symbol node so it becomes a usage site; found {found:?}"
        );
    }
    assert!(
        KconfigLanguage.config().is_usage_node_kind("symbol"),
        "kconfig.json must claim `symbol` as a usage node kind"
    );
}

/// `if` stays unmapped on purpose: its condition is a bare `symbol`, so a named
/// mapping would emit a second row carrying the flag's own name at the guard's
/// line and break the one-definition-row guarantee.
#[test]
fn an_if_guard_is_not_a_named_row() {
    assert_eq!(KconfigLanguage.map_kind("if"), None);
}
