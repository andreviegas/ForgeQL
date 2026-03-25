//! Rust language support for ForgeQL.
//!
//! This crate implements [`LanguageSupport`] for Rust using
//! `tree-sitter-rust`.  Register an instance with [`LanguageRegistry`]
//! at application startup to enable indexing and analysis of Rust source
//! files.
//!
//! ```ignore
//! use forgeql_lang_rust::RustLanguage;
//! use forgeql_core::ast::lang::LanguageRegistry;
//!
//! let registry = LanguageRegistry::new(vec![Arc::new(RustLanguage)]);
//! ```

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// Rust language support for ForgeQL.
pub struct RustLanguage;

/// Static configuration for Rust.
static RUST_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// Returns the static Rust language configuration, loaded from
/// `config/rust.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `rust.json` is malformed (should never happen —
/// the file is validated at test time).
#[allow(clippy::expect_used)]
pub fn rust_config() -> &'static LanguageConfig {
    RUST_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/rust.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded rust.json must be valid");
        json_config.into_language_config()
    })
}

impl LanguageSupport for RustLanguage {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_rust::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        // Most Rust definition nodes have a `name` field.
        if let Some(name_node) = node.child_by_field_name("name") {
            let text = node_text(source, name_node);
            if !text.is_empty() {
                return Some(text);
            }
        }

        match node.kind() {
            // impl blocks: extract the type being implemented.
            // `impl Trait for Type` → name is "Type"
            // `impl Type` → name is "Type"
            "impl_item" => {
                // Try `type` field first (present in both `impl T` and `impl Tr for T`)
                node.child_by_field_name("type")
                    .map(|n| node_text(source, n))
                    .filter(|s| !s.is_empty())
            }

            // `use` declarations: extract the whole path text
            "use_declaration" => node
                .child_by_field_name("argument")
                .map(|n| node_text(source, n))
                .filter(|s| !s.is_empty()),

            // Comments: extract raw text
            "line_comment" | "block_comment" => {
                let text = node_text(source, node);
                if text.is_empty() { None } else { Some(text) }
            }

            // let bindings: extract the pattern
            "let_declaration" => node
                .child_by_field_name("pattern")
                .map(|n| node_text(source, n))
                .filter(|s| !s.is_empty()),

            _ => None,
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        rust_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        rust_config()
    }
}

// -----------------------------------------------------------------------
// Rust helper functions
// -----------------------------------------------------------------------

fn node_text(source: &[u8], node: tree_sitter::Node<'_>) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

// -----------------------------------------------------------------------
// Convenience: build a default Rust registry
// -----------------------------------------------------------------------

/// Build a [`LanguageRegistry`] containing only Rust support.
#[must_use]
pub fn rust_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(RustLanguage)])
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
        let lang = RustLanguage;
        assert_eq!(lang.map_kind("function_item"), Some("function"));
        assert_eq!(lang.map_kind("struct_item"), Some("struct"));
        assert_eq!(lang.map_kind("enum_item"), Some("enum"));
        assert_eq!(lang.map_kind("trait_item"), Some("interface"));
        assert_eq!(lang.map_kind("impl_item"), Some("class"));
        assert_eq!(lang.map_kind("mod_item"), Some("namespace"));
        assert_eq!(lang.map_kind("let_declaration"), Some("variable"));
        assert_eq!(lang.map_kind("const_item"), Some("variable"));
        assert_eq!(lang.map_kind("static_item"), Some("variable"));
        assert_eq!(lang.map_kind("type_item"), Some("type_alias"));
        assert_eq!(lang.map_kind("use_declaration"), Some("import"));
        assert_eq!(lang.map_kind("macro_definition"), Some("macro"));
        assert_eq!(lang.map_kind("field_declaration"), Some("field"));
        assert_eq!(lang.map_kind("line_comment"), Some("comment"));
        assert_eq!(lang.map_kind("block_comment"), Some("comment"));
    }

    #[test]
    fn map_kind_covers_expression_kinds() {
        let lang = RustLanguage;
        assert_eq!(lang.map_kind("integer_literal"), Some("number"));
        assert_eq!(lang.map_kind("float_literal"), Some("number"));
        assert_eq!(lang.map_kind("type_cast_expression"), Some("cast"));
    }

    #[test]
    fn map_kind_covers_control_flow_kinds() {
        let lang = RustLanguage;
        assert_eq!(lang.map_kind("if_expression"), Some("if"));
        assert_eq!(lang.map_kind("while_expression"), Some("while"));
        assert_eq!(lang.map_kind("for_expression"), Some("for"));
        assert_eq!(lang.map_kind("loop_expression"), Some("while"));
        assert_eq!(lang.map_kind("match_expression"), Some("switch"));
    }

    #[test]
    fn map_kind_returns_none_for_unknown() {
        let lang = RustLanguage;
        assert_eq!(lang.map_kind("source_file"), None);
        assert_eq!(lang.map_kind("block"), None);
    }

    #[test]
    fn registry_resolves_rs_extension() {
        let registry = rust_registry();
        let path = std::path::PathBuf::from("test.rs");
        let lang = registry.language_for_path(&path);
        assert!(lang.is_some(), "extension rs should resolve");
        assert_eq!(lang.as_ref().map(|l| l.name()), Some("rust"));
    }

    #[test]
    fn registry_returns_none_for_unknown_extension() {
        let registry = rust_registry();
        let path = std::path::PathBuf::from("test.cpp");
        assert!(registry.language_for_path(&path).is_none());
    }

    #[test]
    fn config_is_consistent() {
        let config = RustLanguage.config();
        assert_eq!(config.scope_sep(), "::");
        assert!(!config.function_kinds().is_empty());
        assert!(!config.type_kinds().is_empty());
    }

    #[test]
    fn extract_name_function() {
        let lang = RustLanguage;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&lang.tree_sitter_language())
            .expect("set language");

        let source = b"fn process_signal(speed: i32) -> i32 { speed }";
        let tree = parser.parse(source, None).expect("parse");
        let root = tree.root_node();

        let func_node = root.child(0).expect("function_item");
        assert_eq!(func_node.kind(), "function_item");

        let name = lang.extract_name(func_node, source);
        assert_eq!(name.as_deref(), Some("process_signal"));
    }

    #[test]
    fn extract_name_struct() {
        let lang = RustLanguage;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&lang.tree_sitter_language())
            .expect("set language");

        let source = b"struct Motor { speed: f64 }";
        let tree = parser.parse(source, None).expect("parse");
        let root = tree.root_node();

        let struct_node = root.child(0).expect("struct_item");
        assert_eq!(struct_node.kind(), "struct_item");

        let name = lang.extract_name(struct_node, source);
        assert_eq!(name.as_deref(), Some("Motor"));
    }

    #[test]
    fn extract_name_impl() {
        let lang = RustLanguage;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&lang.tree_sitter_language())
            .expect("set language");

        let source = b"impl Motor { fn new() -> Self { Motor { speed: 0.0 } } }";
        let tree = parser.parse(source, None).expect("parse");
        let root = tree.root_node();

        let impl_node = root.child(0).expect("impl_item");
        assert_eq!(impl_node.kind(), "impl_item");

        let name = lang.extract_name(impl_node, source);
        assert_eq!(name.as_deref(), Some("Motor"));
    }

    #[test]
    fn extract_name_enum() {
        let lang = RustLanguage;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&lang.tree_sitter_language())
            .expect("set language");

        let source = b"enum State { Idle, Running, Stopped }";
        let tree = parser.parse(source, None).expect("parse");
        let root = tree.root_node();

        let enum_node = root.child(0).expect("enum_item");
        assert_eq!(enum_node.kind(), "enum_item");

        let name = lang.extract_name(enum_node, source);
        assert_eq!(name.as_deref(), Some("State"));
    }
}
