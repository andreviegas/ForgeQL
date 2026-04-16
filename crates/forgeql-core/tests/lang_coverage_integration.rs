//! Tier 2 — Language Index Coverage
//!
//! Indexes each canonical fixture through [`index_file`] and asserts that
//! every universal construct is found with the correct `fql_kind` and line
//! number, regardless of source language.

#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use forgeql_core::ast::enrich::default_enrichers;
use forgeql_core::ast::index::{SymbolTable, index_file};
use forgeql_core::ast::lang::{CppLanguageInline, LanguageSupport, RustLanguageInline};

// ---------------------------------------------------------------------------
// Canonical line contract (see tests/fixtures/canonical/CONTRACT.md)
// ---------------------------------------------------------------------------

const FOO_LINE: usize = 1;
const MOTOR_LINE: usize = 5;
const SPEED_LINE: usize = 6;
const STATE_LINE: usize = 9;
const BAR_LINE: usize = 16;
const COUNT_LINE: usize = 23;

struct ExpectedDef {
    name: &'static str,
    fql_kind: &'static str,
    line: usize,
}

macro_rules! def {
    ($name:expr, $kind:expr, $line:expr) => {
        ExpectedDef {
            name: $name,
            fql_kind: $kind,
            line: $line,
        }
    };
}

const UNIVERSAL_DEFS: &[ExpectedDef] = &[
    def!("foo", "function", FOO_LINE),
    def!("Motor", "struct", MOTOR_LINE),
    def!("speed", "field", SPEED_LINE),
    def!("State", "enum", STATE_LINE),
    def!("bar", "function", BAR_LINE),
    def!("count", "variable", COUNT_LINE),
];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/canonical")
}

/// Index a canonical fixture and return the populated [`SymbolTable`].
fn index_canonical(lang: &dyn LanguageSupport, filename: &str) -> SymbolTable {
    let path = fixtures_dir().join(filename);
    assert!(path.exists(), "fixture missing: {}", path.display());

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.tree_sitter_language())
        .expect("set_language");

    let enrichers = default_enrichers();
    let mut table = SymbolTable::default();

    let count = index_file(&mut parser, &path, &mut table, &enrichers, lang, None)
        .expect("index_file should succeed");

    assert!(count > 0, "expected at least one indexed row");
    table
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Verify every universal definition for a single language.
fn assert_universal_defs(lang_name: &str, table: &SymbolTable) {
    for expected in UNIVERSAL_DEFS {
        let row = table.find_def(expected.name).unwrap_or_else(|| {
            panic!(
                "[{}] symbol '{}' not found in index",
                lang_name, expected.name
            )
        });

        assert_eq!(
            row.fql_kind, expected.fql_kind,
            "[{}] symbol '{}': expected fql_kind='{}', got='{}'",
            lang_name, expected.name, expected.fql_kind, row.fql_kind
        );

        assert_eq!(
            row.line, expected.line,
            "[{}] symbol '{}': expected line={}, got={}",
            lang_name, expected.name, expected.line, row.line
        );
    }
}

#[test]
fn cpp_canonical_universal_defs() {
    let table = index_canonical(&CppLanguageInline, "canonical.cpp");
    assert_universal_defs("cpp", &table);
}

#[test]
fn rust_canonical_universal_defs() {
    let table = index_canonical(&RustLanguageInline, "canonical.rs");
    assert_universal_defs("rust", &table);
}

#[test]
fn cpp_bar_has_doc() {
    let table = index_canonical(&CppLanguageInline, "canonical.cpp");
    let bar = table.find_def("bar").expect("bar not found");
    assert_eq!(
        bar.fields.get("has_doc").map(String::as_str),
        Some("true"),
        "[cpp] bar should have has_doc=true"
    );
}

#[test]
fn rust_bar_has_doc() {
    let table = index_canonical(&RustLanguageInline, "canonical.rs");
    let bar = table.find_def("bar").expect("bar not found");
    assert_eq!(
        bar.fields.get("has_doc").map(String::as_str),
        Some("true"),
        "[rust] bar should have has_doc=true"
    );
}

#[test]
fn cpp_language_field_populated() {
    let table = index_canonical(&CppLanguageInline, "canonical.cpp");
    for row in &table.rows {
        assert_eq!(
            row.language, "cpp",
            "every row should have language='cpp', got='{}'",
            row.language
        );
    }
}

#[test]
fn rust_language_field_populated() {
    let table = index_canonical(&RustLanguageInline, "canonical.rs");
    for row in &table.rows {
        assert_eq!(
            row.language, "rust",
            "every row should have language='rust', got='{}'",
            row.language
        );
    }
}

fn walk_for_macros(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    out: &mut Vec<(usize, Option<String>)>,
) {
    if node.kind() == "macro_invocation" {
        let name = node.child_by_field_name("macro").map(|n| {
            std::str::from_utf8(&source[n.byte_range()])
                .unwrap_or("?")
                .to_string()
        });
        out.push((node.start_position().row + 1, name));
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            walk_for_macros(child, source, out);
        }
    }
}

#[test]
fn rust_macro_invocation_indexed_as_macro_call() {
    let table = index_canonical(&RustLanguageInline, "canonical.rs");

    // Debug: parse the file and walk looking for macro_invocation nodes
    let path = fixtures_dir().join("canonical.rs");
    let source = std::fs::read(&path).expect("read fixture");
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&RustLanguageInline.tree_sitter_language())
        .expect("set_language");
    let tree = parser.parse(&source, None).expect("parse");

    let mut macro_nodes = Vec::new();
    walk_for_macros(tree.root_node(), &source, &mut macro_nodes);

    let has_macro_calls = table.rows.iter().any(|r| r.fql_kind == "macro_call");

    // Report both what tree-sitter found and what index_file produced
    assert!(
        has_macro_calls,
        "expected macro_call rows; got 0.\n\
         macro_invocation AST nodes found: {:?}\n\
         All indexed rows: {:#?}",
        macro_nodes,
        table
            .rows
            .iter()
            .map(|r| (&r.name, &r.node_kind, &r.fql_kind, r.line))
            .collect::<Vec<_>>()
    );
}

#[test]
fn rust_cfg_attribute_ast_structure() {
    // Verify tree-sitter-rust AST structure for `#[cfg(test)] fn guarded_fn() {}`
    let source = b"#[cfg(test)]\nfn guarded_fn() {}\n";
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&RustLanguageInline.tree_sitter_language())
        .expect("set_language");
    let tree = parser.parse(&source[..], None).expect("parse");
    let root = tree.root_node();

    // Collect all named children of root with their kind
    let mut kinds = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        kinds.push((child.kind().to_string(), child.start_position().row + 1));
    }

    // tree-sitter-rust should produce attribute_item as a SIBLING of function_item
    assert!(
        kinds.iter().any(|(k, _)| k == "attribute_item"),
        "attribute_item should be a root-level sibling. Got: {kinds:?}",
    );
    assert!(
        kinds.iter().any(|(k, _)| k == "function_item"),
        "function_item should be a root-level sibling. Got: {kinds:?}",
    );

    // Verify prev_named_sibling relationship (this is what collect_attribute_guard_frames uses)
    let fn_node = root
        .named_children(&mut root.walk())
        .find(|c| c.kind() == "function_item")
        .expect("function_item node");
    let prev_sib = fn_node
        .prev_named_sibling()
        .expect("should have prev sibling");
    assert_eq!(
        prev_sib.kind(),
        "attribute_item",
        "prev_named_sibling of function_item should be attribute_item",
    );
}

#[test]
fn rust_cfg_attribute_guard_indexed() {
    // Write a temp Rust file with #[cfg(test)] guarded function
    let source = "#[cfg(test)]\nfn guarded_fn() {}\n\nfn unguarded_fn() {}\n";
    let dir = std::env::temp_dir().join("forgeql_test_guard");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("test_guard.rs");
    std::fs::write(&path, source).unwrap();

    let lang = RustLanguageInline;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.tree_sitter_language())
        .expect("set_language");
    let enrichers = default_enrichers();
    let mut table = SymbolTable::default();
    let _count = index_file(&mut parser, &path, &mut table, &enrichers, &lang, None)
        .expect("index_file should succeed");

    // Find the guarded function
    let guarded = table
        .rows
        .iter()
        .find(|r| r.name == "guarded_fn" && r.fql_kind == "function")
        .unwrap_or_else(|| {
            panic!(
                "guarded_fn not found. Rows: {:?}",
                table
                    .rows
                    .iter()
                    .map(|r| (&r.name, &r.fql_kind, r.line, &r.fields))
                    .collect::<Vec<_>>()
            )
        });

    // Check guard_kind field
    let guard_kind = guarded.fields.get("guard_kind").map(String::as_str);
    assert_eq!(
        guard_kind,
        Some("attribute"),
        "guarded_fn should have guard_kind=attribute. Fields: {:?}",
        guarded.fields,
    );

    // Check guard text (field name is "guard", not "guard_text")
    let guard = guarded.fields.get("guard").map(String::as_str);
    assert_eq!(
        guard,
        Some("test"),
        "guarded_fn should have guard=test. Fields: {:?}",
        guarded.fields,
    );

    // The unguarded function should NOT have guard_kind
    let unguarded = table
        .rows
        .iter()
        .find(|r| r.name == "unguarded_fn" && r.fql_kind == "function")
        .expect("unguarded_fn should be indexed");
    assert!(
        !unguarded.fields.contains_key("guard_kind"),
        "unguarded_fn should not have guard_kind. Fields: {:?}",
        unguarded.fields,
    );

    // Cleanup
    let _ = std::fs::remove_dir_all(&dir);
}
