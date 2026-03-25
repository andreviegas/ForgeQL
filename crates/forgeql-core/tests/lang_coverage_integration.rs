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

    let count = index_file(&mut parser, &path, &mut table, &enrichers, lang)
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
