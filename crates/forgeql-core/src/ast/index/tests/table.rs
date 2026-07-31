//! The symbol table itself, tested without going through the indexer.
//!
//! Rows are pushed by hand so each assertion isolates one thing: that the
//! secondary indexes stay in step with the rows, that lookup and purge agree
//! with them, and that the intern pools hold one entry per distinct value.

use std::collections::HashMap;
use std::path::Path;

use crate::ast::enrich::default_enrichers;
use crate::ast::index::*;
use crate::ast::lang::{CppLanguageInline, LanguageRegistry};
fn two_row_table() -> SymbolTable {
    let mut table = SymbolTable::default();
    table.push_row_strings(
        "foo",
        "function_definition",
        "",
        "",
        Path::new("a.cpp"),
        0..30,
        1,
        HashMap::new(),
    );
    table.push_row_strings(
        "bar",
        "function_definition",
        "",
        "",
        Path::new("b.cpp"),
        0..30,
        1,
        HashMap::new(),
    );
    table.add_usage("foo".to_string(), Path::new("a.cpp"), 0..3, 1);
    table.add_usage("foo".to_string(), Path::new("b.cpp"), 10..13, 1);
    table
}

#[test]
fn push_row_updates_secondary_indexes() {
    let mut table = SymbolTable::default();
    table.push_row_strings(
        "alpha",
        "function_definition",
        "",
        "",
        Path::new("src/alpha.cpp"),
        0..10,
        1,
        HashMap::new(),
    );
    assert_eq!(table.rows.len(), 1);
    let name_id = table.strings.names.get_id("alpha").unwrap();
    let kind_id = table
        .strings
        .node_kinds
        .get_id("function_definition")
        .unwrap();
    assert_eq!(table.name_index[&name_id], vec![0u32]);
    assert_eq!(table.kind_index[&kind_id], vec![0u32]);
}

#[test]
fn find_def_returns_last_row_for_name() {
    let mut table = SymbolTable::default();
    table.push_row_strings(
        "foo",
        "declaration",
        "",
        "",
        Path::new("inc/foo.h"),
        0..10,
        1,
        HashMap::new(),
    );
    table.push_row_strings(
        "foo",
        "function_definition",
        "",
        "",
        Path::new("src/foo.cpp"),
        0..50,
        1,
        HashMap::new(),
    );
    let def = table.find_def("foo").expect("should find foo");
    assert_eq!(table.node_kind_of(def), "function_definition");
}

#[test]
fn rows_by_kind_returns_correct_subset() {
    let table = two_row_table();
    let fns: Vec<&IndexRow> = table.rows_by_kind("function_definition").collect();
    assert_eq!(fns.len(), 2);
    assert!(fns.iter().any(|r| table.name_of(r) == "foo"));
    assert!(fns.iter().any(|r| table.name_of(r) == "bar"));
}

#[test]
fn purge_file_removes_rows_and_usage_sites() {
    let mut table = two_row_table();
    table.purge_file(Path::new("a.cpp"));

    assert!(table.find_def("foo").is_none());
    assert!(table.find_def("bar").is_some());

    let foo_sites = table.find_usages("foo");
    assert_eq!(foo_sites.len(), 1);
    assert_eq!(
        table.strings.paths.get(foo_sites[0].path_id),
        Path::new("b.cpp")
    );
}

#[test]
fn purge_file_removes_empty_usage_keys() {
    let mut table = SymbolTable::default();
    table.add_usage("only_here".to_string(), Path::new("x.cpp"), 0..5, 1);
    table.purge_file(Path::new("x.cpp"));
    assert!(!table.usages.contains_key("only_here"));
}

#[test]
fn purge_file_rebuilds_index_stats() {
    // Two rows in two files, both contributing to fql_kind / language stats.
    let mut table = SymbolTable::default();
    table.push_row_strings(
        "f1",
        "function_definition",
        "function",
        "cpp",
        Path::new("a.cpp"),
        0..10,
        1,
        HashMap::new(),
    );
    table.push_row_strings(
        "f2",
        "function_definition",
        "function",
        "cpp",
        Path::new("b.cpp"),
        0..10,
        1,
        HashMap::new(),
    );
    // IndexStats now keys by interned u32 — resolve to strings for assertion.
    assert_eq!(
        table
            .stats
            .resolved_by_fql_kind(&table.strings)
            .get("function"),
        Some(&2)
    );
    assert_eq!(
        table.stats.resolved_by_language(&table.strings).get("cpp"),
        Some(&2)
    );

    // Purge one file — stats must reflect only the surviving row.
    table.purge_file(Path::new("a.cpp"));
    assert_eq!(
        table
            .stats
            .resolved_by_fql_kind(&table.strings)
            .get("function"),
        Some(&1)
    );
    assert_eq!(
        table.stats.resolved_by_language(&table.strings).get("cpp"),
        Some(&1)
    );

    // Purge the other — stats must be empty (key removed entirely).
    table.purge_file(Path::new("b.cpp"));
    assert!(table.stats.by_fql_kind.is_empty());
    assert!(table.stats.by_language.is_empty());
}

#[test]
fn reindex_files_refreshes_content() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("test.cpp");
    std::fs::write(&file, "void alpha() {}").unwrap();

    let mut table = SymbolTable::default();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_cpp::LANGUAGE.into())
        .unwrap();
    let enrichers = default_enrichers();
    {
        let mut ctx = IndexContext {
            path: &file,
            language: &CppLanguageInline,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: None,
            table: &mut table,
            workspace_root: None,
        };
        index_file(&mut parser, &mut ctx, None).unwrap();
    }
    assert!(table.find_def("alpha").is_some());

    std::fs::write(&file, "void beta() {}").unwrap();
    let registry = LanguageRegistry::new(vec![std::sync::Arc::new(CppLanguageInline)]);
    table.reindex_files(&[file], &registry, None).unwrap();

    assert!(
        table.find_def("alpha").is_none(),
        "stale entry should be purged"
    );
    assert!(table.find_def("beta").is_some(), "new entry should exist");
}

// -- find_all_defs ---------------------------------------------------
#[test]
fn find_all_defs_empty_for_unknown_name() {
    let table = two_row_table();
    let defs = table.find_all_defs("nonexistent");
    assert!(defs.is_empty(), "unknown symbol must return empty vec");
}

#[test]
fn find_all_defs_returns_all_matching_rows() {
    // Push the same name into two files to simulate a multi-file workspace.
    let mut table = SymbolTable::default();
    table.push_row_strings(
        "shared",
        "function_definition",
        "",
        "",
        Path::new("src/a.cpp"),
        0..10,
        1,
        HashMap::new(),
    );
    table.push_row_strings(
        "shared",
        "function_definition",
        "",
        "",
        Path::new("src/b.cpp"),
        0..10,
        1,
        HashMap::new(),
    );

    let defs = table.find_all_defs("shared");
    assert_eq!(defs.len(), 2, "both rows must be returned");
}

#[test]
fn find_all_defs_single_result() {
    let table = two_row_table();
    let defs = table.find_all_defs("foo");
    assert_eq!(defs.len(), 1);
    assert_eq!(table.name_of(defs[0]), "foo");
}

// -- suggest_similar -------------------------------------------------

// -- suggest_similar -------------------------------------------------

#[test]
fn suggest_similar_prefix_match() {
    let table = two_row_table(); // has "foo" and "bar"
    let suggestions = table.suggest_similar("fo", 10);
    assert!(suggestions.contains(&"foo"), "prefix 'fo' must match 'foo'");
}

#[test]
fn suggest_similar_substring_match() {
    let table = two_row_table(); // has "foo" and "bar"
    let suggestions = table.suggest_similar("oo", 10);
    assert!(
        suggestions.contains(&"foo"),
        "substring 'oo' must match 'foo'"
    );
}

#[test]
fn suggest_similar_case_insensitive() {
    let table = two_row_table(); // has "foo"
    let suggestions = table.suggest_similar("FOO", 10);
    assert!(
        suggestions.contains(&"foo"),
        "uppercase query must match lowercase name"
    );
}

#[test]
fn suggest_similar_no_match_returns_empty() {
    let table = two_row_table();
    let suggestions = table.suggest_similar("zzz_nonexistent", 10);
    assert!(suggestions.is_empty(), "no match must return empty vec");
}

#[test]
fn suggest_similar_respects_max_limit() {
    // Build a table with 5 symbols that all start with "sym".
    let mut table = SymbolTable::default();
    for i in 0..5_usize {
        table.push_row_strings(
            &format!("sym_{i}"),
            "function_definition",
            "",
            "",
            Path::new("src/lib.cpp"),
            0..10,
            1,
            HashMap::new(),
        );
    }
    let suggestions = table.suggest_similar("sym", 3);
    assert!(
        suggestions.len() <= 3,
        "result must not exceed max limit of 3"
    );
}

// -- intern-pool correctness -----------------------------------------

/// Verify that the five accessor methods return the expected strings
/// after rows are pushed via `push_row_strings`.
#[test]
fn accessors_match_string_fields() {
    let mut table = SymbolTable::default();
    let data = [
        (
            "alpha",
            "function_definition",
            "function",
            "cpp",
            "src/a.cpp",
            0..10usize,
            1usize,
        ),
        (
            "beta",
            "struct_specifier",
            "struct",
            "cpp",
            "src/a.cpp",
            10..20,
            5,
        ),
        (
            "gamma",
            "function_definition",
            "function",
            "rust",
            "src/b.rs",
            0..15,
            1,
        ),
    ];
    for &(name, nk, fql, lang, path, ref br, line) in &data {
        table.push_row_strings(
            name,
            nk,
            fql,
            lang,
            Path::new(path),
            br.clone(),
            line,
            HashMap::new(),
        );
    }
    for (row, &(exp_name, exp_nk, exp_fql, exp_lang, exp_path, _, _)) in
        table.rows.iter().zip(data.iter())
    {
        assert_eq!(table.name_of(row), exp_name, "name_of");
        assert_eq!(table.node_kind_of(row), exp_nk, "node_kind_of");
        assert_eq!(table.fql_kind_of(row), exp_fql, "fql_kind_of");
        assert_eq!(table.language_of(row), exp_lang, "language_of");
        assert_eq!(table.path_of(row), Path::new(exp_path), "path_of");
    }
}

/// Rows with the same low-cardinality fields must share pool slots, keeping
/// pool sizes bounded by unique-value cardinality rather than row count.
#[test]
fn intern_pool_sizes_reflect_unique_values() {
    let mut table = SymbolTable::default();
    // 100 rows: unique names, shared node_kind/fql_kind/language/path.
    for i in 0..100_usize {
        table.push_row_strings(
            &format!("fn_{i}"),
            "function_definition",
            "function",
            "cpp",
            Path::new("src/big.cpp"),
            0..10,
            i + 1,
            HashMap::new(),
        );
    }
    assert_eq!(table.strings.names.len(), 100, "100 unique names");
    assert_eq!(table.strings.node_kinds.len(), 1, "one node_kind");
    assert_eq!(table.strings.fql_kinds.len(), 1, "one fql_kind");
    assert_eq!(table.strings.languages.len(), 1, "one language");
    assert_eq!(table.strings.paths.len(), 1, "one path");
}
