//! C/C++ language support for ForgeQL.
//!
//! This crate implements [`LanguageSupport`] for C and C++ grammars using
//! `tree-sitter-cpp`.  Register an instance with [`LanguageRegistry`] at
//! application startup to enable indexing and analysis of C/C++ source files.
//!
//! ```ignore
//! use forgeql_lang_cpp::CppLanguage;
//! use forgeql_core::ast::lang::LanguageRegistry;
//!
//! let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage)]);
//! ```

#![allow(
    // False positive: re-export of FQL kind constants for convenience.
    clippy::module_name_repetitions,
    // Doc comments for this module are fine as-is.
    clippy::doc_markdown,
)]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, MacroExpander};
use forgeql_core::ast::lang_json::LanguageConfigJson;

pub(crate) mod macro_expand;

/// C/C++ language support for ForgeQL.
pub struct CppLanguage;

/// Static configuration for C/C++.
static CPP_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// Returns the static C/C++ language configuration, loaded from
/// `config/cpp.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `cpp.json` is malformed (should never happen —
/// the file is validated at test time).
#[allow(clippy::expect_used)]
pub fn cpp_config() -> &'static LanguageConfig {
    CPP_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/cpp.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded cpp.json must be valid");
        json_config.into_language_config()
    })
}

impl LanguageSupport for CppLanguage {
    fn name(&self) -> &'static str {
        "cpp"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["cpp", "c", "cc", "cxx", "h", "hpp", "hxx", "ino"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_cpp::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        // Structural nodes that are part of a declarator tree should never
        // produce their own index rows.
        if node.kind() == "qualified_identifier" {
            return None;
        }

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

            // macro_invocation: extract the macro name via the "macro" field.
            //
            // NOTE: tree-sitter-cpp 0.23.x rarely produces macro_invocation nodes
            // in practice — both statement-position and declaration-position macro calls
            // are parsed as expression_statement(call_expression(...)) instead.
            // See macro_expand.rs tests `call_expr_macro_structure` and
            // `decl_position_macro_is_also_call_expression` for confirmation.
            // Full C/C++ macro-call indexing requires the two-pass pipeline (Task 4.2).
            "macro_invocation" => node
                .child_by_field_name("macro")
                .map(|n| node_text(source, n))
                .filter(|s| !s.is_empty()),

            _ => None,
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        cpp_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        cpp_config()
    }

    fn macro_expander(&self) -> Option<&dyn MacroExpander> {
        static EXPANDER: macro_expand::CppMacroExpander = macro_expand::CppMacroExpander;
        Some(&EXPANDER)
    }
}

// -----------------------------------------------------------------------
// C++ helper functions
// -----------------------------------------------------------------------

fn node_text(source: &[u8], node: tree_sitter::Node<'_>) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

fn find_function_name(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    match node.kind() {
        "identifier"
        | "field_identifier"
        | "destructor_name"
        | "operator_name"
        | "qualified_identifier" => Some(node),
        "function_declarator"
        | "pointer_declarator"
        | "reference_declarator"
        | "abstract_function_declarator" => node
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
// Convenience: build a default C++ registry
// -----------------------------------------------------------------------

/// Build a [`LanguageRegistry`] containing only C/C++ support.
#[must_use]
pub fn cpp_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(CppLanguage)])
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn map_kind_covers_all_definition_kinds() {
        let lang = CppLanguage;
        assert_eq!(lang.map_kind("function_definition"), Some("function"));
        assert_eq!(lang.map_kind("class_specifier"), Some("class"));
        assert_eq!(lang.map_kind("struct_specifier"), Some("struct"));
        assert_eq!(lang.map_kind("enum_specifier"), Some("enum"));
        assert_eq!(lang.map_kind("declaration"), Some("variable"));
        assert_eq!(lang.map_kind("field_declaration"), Some("field"));
        assert_eq!(lang.map_kind("comment"), Some("comment"));
        assert_eq!(lang.map_kind("preproc_include"), Some("import"));
        assert_eq!(lang.map_kind("preproc_def"), Some("macro"));
        assert_eq!(lang.map_kind("type_definition"), Some("type_alias"));
        assert_eq!(lang.map_kind("namespace_definition"), Some("namespace"));
    }

    #[test]
    fn map_kind_covers_expression_kinds() {
        let lang = CppLanguage;
        assert_eq!(lang.map_kind("number_literal"), Some("number"));
        assert_eq!(lang.map_kind("cast_expression"), Some("cast"));
        assert_eq!(lang.map_kind("static_cast_expression"), Some("cast"));
        assert_eq!(lang.map_kind("update_expression"), Some("increment"));
    }

    #[test]
    fn map_kind_covers_control_flow_kinds() {
        let lang = CppLanguage;
        assert_eq!(lang.map_kind("if_statement"), Some("if"));
        assert_eq!(lang.map_kind("while_statement"), Some("while"));
        assert_eq!(lang.map_kind("for_statement"), Some("for"));
        assert_eq!(lang.map_kind("for_range_loop"), Some("for"));
        assert_eq!(lang.map_kind("switch_statement"), Some("switch"));
        assert_eq!(lang.map_kind("do_statement"), Some("do"));
    }

    #[test]
    fn map_kind_returns_none_for_unknown() {
        let lang = CppLanguage;
        assert_eq!(lang.map_kind("translation_unit"), None);
        assert_eq!(lang.map_kind("compound_statement"), None);
    }

    #[test]
    fn registry_resolves_cpp_extensions() {
        let registry = cpp_registry();

        for ext in ["cpp", "c", "cc", "cxx", "h", "hpp", "hxx", "ino"] {
            let path = std::path::PathBuf::from(format!("test.{ext}"));
            let lang = registry.language_for_path(&path);
            assert!(lang.is_some(), "extension {ext} should resolve");
            assert_eq!(lang.as_ref().map(|l| l.name()), Some("cpp"));
        }
    }

    #[test]
    fn registry_returns_none_for_unknown_extension() {
        let registry = cpp_registry();
        let path = std::path::PathBuf::from("test.rs");
        assert!(registry.language_for_path(&path).is_none());
    }

    #[test]
    fn config_is_consistent() {
        let config = CppLanguage.config();
        assert_eq!(config.scope_sep(), "::");
        assert!(!config.function_kinds().is_empty());
        assert!(!config.type_kinds().is_empty());
    }

    #[test]
    fn extract_name_via_trait() {
        let lang = CppLanguage;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&lang.tree_sitter_language())
            .expect("set language");

        let source = b"void processSignal(int speed) { return; }";
        let tree = parser.parse(source, None).expect("parse");
        let root = tree.root_node();

        let func_node = root.child(0).expect("function_definition");
        assert_eq!(func_node.kind(), "function_definition");

        let name = lang.extract_name(func_node, source);
        assert_eq!(name.as_deref(), Some("processSignal"));
    }
}
