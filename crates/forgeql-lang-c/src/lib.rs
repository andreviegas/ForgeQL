//! C language support for ForgeQL.
//!
//! This crate implements [`LanguageSupport`] for C using `tree-sitter-c`.
//! Register an instance with [`LanguageRegistry`] at application startup to
//! enable indexing and analysis of C source files.
//!
//! C is treated as a separate language from C++ (`forgeql-lang-cpp`).
//! `.c` files use the C grammar; `.cpp`, `.cc`, `.cxx`, `.h`, `.hpp`,
//! `.hxx`, and `.ino` files use the C++ grammar.
//!
//! ```ignore
//! use forgeql_lang_c::CLanguage;
//! use forgeql_core::ast::lang::LanguageRegistry;
//!
//! let registry = LanguageRegistry::new(vec![Arc::new(CLanguage)]);
//! ```

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{
    LanguageConfig, LanguageRegistry, LanguageSupport, MacroExpander, node_text,
};
use forgeql_core::ast::lang_json::LanguageConfigJson;

pub(crate) mod macro_expand;

/// C language support for ForgeQL.
pub struct CLanguage;

/// Static configuration for C.
static C_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// Returns the static C language configuration, loaded from
/// `config/c.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `c.json` is malformed (should never happen —
/// the file is validated at test time).
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
pub fn c_config() -> &'static LanguageConfig {
    C_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/c.json");
        let json_config =
            LanguageConfigJson::from_json_bytes(json_bytes).expect("embedded c.json must be valid");
        json_config.into_language_config()
    })
}

impl LanguageSupport for CLanguage {
    fn name(&self) -> &'static str {
        "c"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["c"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_c::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        // Universal: most grammars expose a "name" field on definition nodes.
        if let Some(name_node) = node.child_by_field_name("name") {
            let text = node_text(source, name_node);
            if !text.is_empty() {
                return Some(text);
            }
        }

        match node.kind() {
            "function_definition" => node
                .child_by_field_name("declarator")
                .and_then(find_function_name)
                .map(|n| node_text(source, n))
                .filter(|s| !s.is_empty()),

            "preproc_include" => node
                .child_by_field_name("path")
                .map(|n| {
                    node_text(source, n)
                        .trim_matches(|c: char| c == '"' || c == '<' || c == '>')
                        .to_string()
                })
                .filter(|s| !s.is_empty()),

            "declaration" => {
                let decl = node.child_by_field_name("declarator")?;
                if contains_function_declarator(decl) {
                    return None;
                }
                find_function_name(decl)
                    .map(|n| node_text(source, n))
                    .filter(|s| !s.is_empty())
            }

            "field_declaration" | "parameter_declaration" => node
                .child_by_field_name("declarator")
                .and_then(find_function_name)
                .map(|n| node_text(source, n))
                .filter(|s| !s.is_empty()),

            "comment" => {
                let text = node_text(source, node);
                if text.is_empty() { None } else { Some(text) }
            }

            "macro_invocation" => node
                .child_by_field_name("macro")
                .map(|n| node_text(source, n))
                .filter(|s| !s.is_empty()),

            "type_definition" => node
                .child_by_field_name("declarator")
                .and_then(find_type_alias_name)
                .map(|n| node_text(source, n))
                .filter(|s| !s.is_empty()),

            _ => None,
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        c_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        c_config()
    }

    fn macro_expander(&self) -> Option<&dyn MacroExpander> {
        static EXPANDER: macro_expand::CMacroExpander = macro_expand::CMacroExpander;
        Some(&EXPANDER)
    }
}

// -----------------------------------------------------------------------
// C helper functions
// -----------------------------------------------------------------------

fn find_function_name(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    match node.kind() {
        "identifier" | "field_identifier" => Some(node),
        "function_declarator" | "pointer_declarator" | "reference_declarator" => node
            .child_by_field_name("declarator")
            .and_then(find_function_name),
        _ => {
            for i in 0..node.named_child_count() {
                if let Some(found) = node.named_child(i).and_then(find_function_name) {
                    return Some(found);
                }
            }
            None
        }
    }
}

/// Find the name node a `type_definition` introduces. Unlike a variable
/// declarator (whose name is an `identifier`), a typedef's new name is a
/// `type_identifier`, optionally wrapped in pointer / array / function
/// declarators (function-pointer and array typedefs). Kept separate from
/// `find_function_name` so the typedef case never perturbs variable /
/// parameter / field name extraction.
fn find_type_alias_name(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    if matches!(node.kind(), "type_identifier" | "identifier") {
        return Some(node);
    }
    if let Some(found) = node
        .child_by_field_name("declarator")
        .and_then(find_type_alias_name)
    {
        return Some(found);
    }
    for i in 0..node.named_child_count() {
        if let Some(found) = node.named_child(i).and_then(find_type_alias_name) {
            return Some(found);
        }
    }
    None
}

fn contains_function_declarator(node: tree_sitter::Node<'_>) -> bool {
    if node.kind() == "function_declarator" {
        return true;
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i)
            && contains_function_declarator(child)
        {
            return true;
        }
    }
    false
}

// -----------------------------------------------------------------------
// Convenience: build a default C registry
// -----------------------------------------------------------------------

/// Build a [`LanguageRegistry`] containing only C support.
#[must_use]
pub fn c_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(CLanguage)])
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn map_kind_covers_all_definition_kinds() {
        let lang = CLanguage;
        assert_eq!(lang.map_kind("function_definition"), Some("function"));
        assert_eq!(lang.map_kind("struct_specifier"), Some("struct"));
        assert_eq!(lang.map_kind("enum_specifier"), Some("enum"));
        assert_eq!(lang.map_kind("union_specifier"), Some("union"));
        assert_eq!(lang.map_kind("enumerator"), Some("enumerator"));
        assert_eq!(lang.map_kind("declaration"), Some("variable"));
        assert_eq!(lang.map_kind("field_declaration"), Some("field"));
        assert_eq!(lang.map_kind("comment"), Some("comment"));
        assert_eq!(lang.map_kind("preproc_include"), Some("import"));
        assert_eq!(lang.map_kind("preproc_def"), Some("macro"));
        assert_eq!(lang.map_kind("type_definition"), Some("type_alias"));
    }

    #[test]
    fn map_kind_covers_expression_kinds() {
        let lang = CLanguage;
        assert_eq!(lang.map_kind("number_literal"), Some("number"));
        assert_eq!(lang.map_kind("cast_expression"), Some("cast"));
        assert_eq!(lang.map_kind("update_expression"), Some("increment"));
        assert_eq!(
            lang.map_kind("compound_assignment"),
            Some("compound_assignment")
        );
    }

    #[test]
    fn map_kind_covers_control_flow_kinds() {
        let lang = CLanguage;
        assert_eq!(lang.map_kind("if_statement"), Some("if"));
        assert_eq!(lang.map_kind("while_statement"), Some("while"));
        assert_eq!(lang.map_kind("for_statement"), Some("for"));
        assert_eq!(lang.map_kind("switch_statement"), Some("switch"));
        assert_eq!(lang.map_kind("do_statement"), Some("do"));
    }

    #[test]
    fn map_kind_does_not_map_cpp_only_kinds() {
        let lang = CLanguage;
        // C has no classes, namespaces, or C++ casts.
        assert_eq!(lang.map_kind("class_specifier"), None);
        assert_eq!(lang.map_kind("namespace_definition"), None);
        assert_eq!(lang.map_kind("static_cast_expression"), None);
        assert_eq!(lang.map_kind("for_range_loop"), None);
    }

    #[test]
    fn map_kind_returns_none_for_unknown() {
        let lang = CLanguage;
        assert_eq!(lang.map_kind("translation_unit"), None);
        assert_eq!(lang.map_kind("compound_statement"), None);
    }

    #[test]
    fn registry_resolves_c_extensions() {
        let registry = c_registry();
        let path = std::path::PathBuf::from("test.c");
        let lang = registry.language_for_path(&path);
        assert!(lang.is_some(), "extension c should resolve");
        assert_eq!(lang.as_ref().map(|l| l.name()), Some("c"));
    }

    #[test]
    fn registry_does_not_resolve_cpp_extensions() {
        let registry = c_registry();
        for ext in ["cpp", "cc", "cxx", "h", "hpp", "hxx"] {
            let path = std::path::PathBuf::from(format!("test.{ext}"));
            assert!(
                registry.language_for_path(&path).is_none(),
                "extension {ext} should not resolve in C-only registry"
            );
        }
    }

    #[test]
    fn extract_name_typedef_alias() {
        let lang = CLanguage;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&lang.tree_sitter_language())
            .expect("set language");

        let source = b"typedef unsigned int paddr_t;";
        let tree = parser.parse(source, None).expect("parse");
        let root = tree.root_node();

        let td = root.child(0).expect("type_definition");
        assert_eq!(td.kind(), "type_definition");
        assert_eq!(lang.extract_name(td, source).as_deref(), Some("paddr_t"));
    }
}
