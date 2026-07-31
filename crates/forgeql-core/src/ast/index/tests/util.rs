//! Index a snippet and hand back the resulting table.
//!
//! Both helpers write the snippet to a tempdir and run a real index build over
//! it, so a test asserts against what the indexer actually produced rather than
//! a hand-built table.

use crate::ast::enrich::default_enrichers;
use crate::ast::index::*;
use crate::ast::lang::CppLanguageInline;
pub(super) fn index_snippet(code: &str) -> SymbolTable {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("snippet.cpp");
    std::fs::write(&file, code).unwrap();
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
    table
}

pub(super) fn index_rust_snippet(src: &str) -> SymbolTable {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("snippet.rs");
    std::fs::write(&file, src).unwrap();
    let mut table = SymbolTable::default();
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .unwrap();
    let enrichers = default_enrichers();
    {
        let mut ctx = IndexContext {
            path: &file,
            language: &crate::ast::lang::RustLanguageInline,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: None,
            table: &mut table,
            workspace_root: None,
        };
        index_file(&mut parser, &mut ctx, None).unwrap();
    }
    table
}
