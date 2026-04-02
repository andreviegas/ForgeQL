//! Python language support for ForgeQL.
//!
//! This crate implements [`LanguageSupport`] for Python using
//! `tree-sitter-python`.  Register an instance with [`LanguageRegistry`]
//! at application startup to enable indexing and analysis of Python source
//! files.
//!
//! ```ignore
//! use forgeql_lang_python::PythonLanguage;
//! use forgeql_core::ast::lang::LanguageRegistry;
//!
//! let registry = LanguageRegistry::new(vec![Arc::new(PythonLanguage)]);
//! ```

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// Python language support for ForgeQL.
pub struct PythonLanguage;

/// Static configuration for Python.
static PYTHON_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// Returns the static Python language configuration, loaded from
/// `config/python.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `python.json` is malformed (should never happen —
/// the file is validated at test time).
#[allow(clippy::expect_used)]
pub fn python_config() -> &'static LanguageConfig {
    PYTHON_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/python.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded python.json must be valid");
        json_config.into_language_config()
    })
}

impl LanguageSupport for PythonLanguage {
    fn name(&self) -> &'static str {
        "python"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["py", "pyi"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_python::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        // Most Python definition nodes have a `name` field.
        if let Some(name_node) = node.child_by_field_name("name") {
            let text = node_text(source, name_node);
            if !text.is_empty() {
                return Some(text);
            }
        }

        match node.kind() {
            // Decorated definitions: delegate to the inner definition.
            "decorated_definition" => node
                .child_by_field_name("definition")
                .and_then(|def| def.child_by_field_name("name"))
                .map(|n| node_text(source, n))
                .filter(|s| !s.is_empty()),

            // import X, import X as Y
            "import_statement" => {
                let mut names = Vec::new();
                for i in 0..node.named_child_count() {
                    if let Some(child) = node.named_child(i) {
                        match child.kind() {
                            "dotted_name" | "aliased_import" => {
                                let text = node_text(source, child);
                                if !text.is_empty() {
                                    names.push(text);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if names.is_empty() {
                    None
                } else {
                    Some(names.join(", "))
                }
            }

            // from X import Y
            "import_from_statement" => node
                .child_by_field_name("module_name")
                .map(|n| node_text(source, n))
                .filter(|s| !s.is_empty()),

            // Simple assignments: `x = ...`
            "assignment" => node
                .child_by_field_name("left")
                .map(|n| node_text(source, n))
                .filter(|s| !s.is_empty()),

            // Comments: extract raw text
            "comment" => {
                let text = node_text(source, node);
                if text.is_empty() { None } else { Some(text) }
            }

            _ => None,
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        python_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        python_config()
    }
}

// -----------------------------------------------------------------------
// Python helper functions
// -----------------------------------------------------------------------

fn node_text(source: &[u8], node: tree_sitter::Node<'_>) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

// -----------------------------------------------------------------------
// Convenience: build a default Python registry
// -----------------------------------------------------------------------

/// Build a [`LanguageRegistry`] containing only Python support.
#[must_use]
pub fn python_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(PythonLanguage)])
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
        let lang = PythonLanguage;
        assert_eq!(lang.map_kind("function_definition"), Some("function"));
        assert_eq!(lang.map_kind("class_definition"), Some("class"));
        assert_eq!(lang.map_kind("decorated_definition"), Some("function"));
    }

    #[test]
    fn map_kind_covers_expression_kinds() {
        let lang = PythonLanguage;
        assert_eq!(lang.map_kind("assignment"), Some("variable"));
        assert_eq!(lang.map_kind("import_statement"), Some("import"));
        assert_eq!(lang.map_kind("import_from_statement"), Some("import"));
    }

    #[test]
    fn map_kind_covers_control_flow_kinds() {
        let lang = PythonLanguage;
        assert_eq!(lang.map_kind("if_statement"), Some("if"));
        assert_eq!(lang.map_kind("while_statement"), Some("while"));
        assert_eq!(lang.map_kind("for_statement"), Some("for"));
        assert_eq!(lang.map_kind("match_statement"), Some("switch"));
    }

    #[test]
    fn map_kind_returns_none_for_unknown() {
        let lang = PythonLanguage;
        assert_eq!(lang.map_kind("unknown_node_type_xyz"), None);
    }

    #[test]
    fn registry_resolves_py_extension() {
        let registry = python_registry();
        let path = std::path::Path::new("example.py");
        let lang = registry.language_for_path(path);
        assert!(lang.is_some());
        assert_eq!(lang.unwrap().name(), "python");
    }

    #[test]
    fn registry_resolves_pyi_extension() {
        let registry = python_registry();
        let path = std::path::Path::new("stubs.pyi");
        let lang = registry.language_for_path(path);
        assert!(lang.is_some());
        assert_eq!(lang.unwrap().name(), "python");
    }

    #[test]
    fn registry_returns_none_for_unknown_extension() {
        let registry = python_registry();
        let path = std::path::Path::new("readme.txt");
        assert!(registry.language_for_path(path).is_none());
    }

    #[test]
    fn config_is_consistent() {
        let config = PythonLanguage.config();
        assert_eq!(config.scope_sep(), ".");
        assert!(!config.function_kinds().is_empty());
        assert!(!config.type_kinds().is_empty());
    }

    #[test]
    fn extract_name_function() {
        let lang = PythonLanguage;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let source = b"def hello_world(x, y):\n    return x + y\n";
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();
        let func_node = root.named_child(0).unwrap();
        let name = lang.extract_name(func_node, source);
        assert_eq!(name.as_deref(), Some("hello_world"));
    }

    #[test]
    fn extract_name_class() {
        let lang = PythonLanguage;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let source = b"class MyClass:\n    pass\n";
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();
        let class_node = root.named_child(0).unwrap();
        let name = lang.extract_name(class_node, source);
        assert_eq!(name.as_deref(), Some("MyClass"));
    }

    #[test]
    fn extract_name_import() {
        let lang = PythonLanguage;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let source = b"import os\n";
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();
        let import_node = root.named_child(0).unwrap();
        let name = lang.extract_name(import_node, source);
        assert_eq!(name.as_deref(), Some("os"));
    }

    #[test]
    fn extract_name_assignment() {
        let lang = PythonLanguage;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let source = b"x = 42\n";
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();
        // assignment is inside expression_statement
        let expr_stmt = root.named_child(0).unwrap();
        // The expression_statement's child is the assignment
        let assign_node = if expr_stmt.kind() == "expression_statement" {
            expr_stmt.named_child(0).unwrap()
        } else {
            expr_stmt
        };
        let name = lang.extract_name(assign_node, source);
        assert_eq!(name.as_deref(), Some("x"));
    }
}
