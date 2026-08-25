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

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, node_text};
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
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
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

        // `decorated_definition` is deliberately absent from the arms below, so
        // it falls through to `None` and produces no row of its own. The
        // definition it wraps already produces one, and that row's span is
        // folded back to the leading decorator, so naming the wrapper gave two
        // rows agreeing on name, kind, path and line — indistinguishable to the
        // dedupe, which kept the wrapper. The wrapper is not in
        // `function_kinds`, so the surviving row answered no function metric at
        // all: 36,978 of 141,233 Python `function` rows on one corpus, 26%.
        // Naming it also made a decorated CLASS a second `function` row.
        match node.kind() {
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

            // For-loop variable: `for x in ...` → name is "x"
            "for_statement" => node
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
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn map_kind_covers_all_definition_kinds() {
        let lang = PythonLanguage;
        assert_eq!(lang.map_kind("function_definition"), Some("function"));
        assert_eq!(lang.map_kind("class_definition"), Some("class"));
        // The wrapper maps to nothing on purpose: the definition it wraps is
        // the row, and mapping the wrapper to `function` made a decorated
        // CLASS answer as a function as well as a class. Paired with
        // `extract_name` returning `None` for it — either one alone leaves a
        // row behind, kindless in one direction and unnamed in the other.
        assert_eq!(lang.map_kind("decorated_definition"), None);
    }

    /// The wrapper is not named, so it produces no row of its own.
    ///
    /// This is the half that matters for the metrics: a named wrapper became a
    /// row indistinguishable from the definition's own under the dedupe key,
    /// and the survivor was the wrapper, which no function enricher visits.
    #[test]
    fn a_decorated_definition_is_not_named() {
        let source = b"@deco\ndef inner(a):\n    return a\n";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .expect("python grammar");
        let tree = parser.parse(source.as_slice(), None).expect("parse");
        let root = tree.root_node();
        let wrapper = root.named_child(0).expect("decorated_definition");
        assert_eq!(wrapper.kind(), "decorated_definition");
        assert_eq!(
            PythonLanguage.extract_name(wrapper, source.as_slice()),
            None,
            "naming the wrapper is what produced the shadow row"
        );
        let inner = wrapper
            .child_by_field_name("definition")
            .expect("inner definition");
        assert_eq!(
            PythonLanguage
                .extract_name(inner, source.as_slice())
                .as_deref(),
            Some("inner"),
            "the definition it wraps must still be named"
        );
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

    /// The bundled config must survive strict deserialization: a key the schema
    /// does not declare is an error, so this fails loudly if the file and the
    /// schema ever drift apart. The crate owns its config, so it owns the check.
    #[test]
    fn bundled_config_deserializes_strictly() {
        let parsed = LanguageConfigJson::from_json_bytes(include_bytes!("../config/python.json"));
        assert!(
            parsed.is_ok(),
            "python.json must deserialize strictly: {:?}",
            parsed.err()
        );
    }
    /// The sibling-run block groups are declared by the shipped config. A typo
    /// in a member kind would not be a parse error — the group would simply
    /// never match — so the declared values are pinned.
    #[test]
    fn python_json_declares_the_sibling_run_block_groups() {
        let groups = PythonLanguage.config().block_groups();
        let find = |block: &str| groups.iter().find(|g| g.block_fql_kind == block);
        let comments = find("comment_block").expect("python.json must declare comment_block");
        assert_eq!(comments.member_fql_kind, "comment");
        let imports = find("import_block").expect("python.json must declare import_block");
        assert_eq!(imports.member_fql_kind, "import");
        assert_eq!(imports.min_run, 2);
    }
}
