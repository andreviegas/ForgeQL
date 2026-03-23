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

use std::sync::Arc;

use forgeql_core::ast::lang::{
    FQL_CAST, FQL_CLASS, FQL_COMMENT, FQL_DO, FQL_ENUM, FQL_FIELD, FQL_FOR, FQL_FUNCTION, FQL_IF,
    FQL_IMPORT, FQL_INCREMENT, FQL_MACRO, FQL_NAMESPACE, FQL_NUMBER, FQL_STRUCT, FQL_SWITCH,
    FQL_TYPE_ALIAS, FQL_VARIABLE, FQL_WHILE, LanguageConfig, LanguageRegistry, LanguageSupport,
};

/// C/C++ language support for ForgeQL.
pub struct CppLanguage;

/// Static configuration for C/C++.
pub static CPP_CONFIG: LanguageConfig = LanguageConfig {
    root_node_kind: "translation_unit",
    scope_separator: "::",

    function_raw_kinds: &["function_definition"],
    type_raw_kinds: &["class_specifier", "struct_specifier", "enum_specifier"],
    definition_raw_kinds: &[
        "function_definition",
        "class_specifier",
        "struct_specifier",
        "enum_specifier",
    ],
    declaration_raw_kinds: &["declaration"],
    field_raw_kinds: &["field_declaration"],
    parameter_raw_kind: "parameter_declaration",
    member_body_raw_kind: "field_declaration_list",
    member_raw_kinds: &["field_declaration"],
    comment_raw_kind: "comment",

    number_literal_raw_kinds: &["number_literal"],
    digit_separator: Some('\''),
    number_suffixes: &[
        ("ull", "unsigned_long_long"),
        ("ull", "unsigned_long_long"),
        ("ul", "unsigned_long"),
        ("ll", "long_long"),
        ("uz", "unsigned_size"),
        ("u", "unsigned"),
        ("l", "long"),
        ("z", "size"),
        ("f", "float"),
    ],

    control_flow_raw_kinds: &[
        "if_statement",
        "while_statement",
        "for_statement",
        "for_range_loop",
        "switch_statement",
        "do_statement",
    ],
    switch_raw_kinds: &["switch_statement"],

    null_literals: &["nullptr", "NULL", "0"],
    boolean_literals: &["true", "false"],

    doc_comment_prefixes: &[
        ("/**", "doc_block"),
        ("///", "doc_line"),
        ("/*", "block"),
        ("//", "line"),
    ],

    modifier_map: &[
        ("const", "is_const"),
        ("static", "is_static"),
        ("virtual", "is_virtual"),
        ("inline", "is_inline"),
        ("extern", "is_extern"),
        ("volatile", "is_volatile"),
        ("mutable", "is_mutable"),
        ("constexpr", "is_constexpr"),
        ("explicit", "is_explicit"),
        ("override", "is_override"),
        ("final", "is_final"),
    ],
    modifier_node_kinds: &[
        "type_qualifier",
        "storage_class_specifier",
        "virtual_specifier",
    ],
    visibility_keywords: &[
        ("public", "public"),
        ("private", "private"),
        ("protected", "protected"),
    ],
    visibility_default_by_type: &[
        ("class_specifier", "private"),
        ("struct_specifier", "public"),
    ],

    cast_kinds: &[
        ("cast_expression", "c_style", "unsafe"),
        ("static_cast_expression", "static_cast", "safe"),
        ("reinterpret_cast_expression", "reinterpret_cast", "unsafe"),
        ("const_cast_expression", "const_cast", "moderate"),
        ("dynamic_cast_expression", "dynamic_cast", "safe"),
    ],

    has_goto: true,
    has_increment_decrement: true,
    has_implicit_truthiness: true,
    decorator_raw_kind: None,
    skip_node_kinds: &["preproc_else", "preproc_elif"],
    usage_node_kinds: &["identifier", "field_identifier", "type_identifier"],
    declarator_field_name: "declarator",
    function_declarator_kind: "function_declarator",

    parameter_list_raw_kind: "parameter_list",
    identifier_raw_kind: "identifier",
    assignment_raw_kinds: &["assignment_expression"],
    update_raw_kinds: &["update_expression"],
    init_declarator_raw_kind: "init_declarator",
    block_raw_kind: "compound_statement",

    return_statement_raw_kind: "return_statement",
    address_of_expression_raw_kind: "pointer_expression",
    address_of_operator: "&",
    array_declarator_raw_kind: "array_declarator",
    static_storage_keywords: &["static"],

    case_statement_raw_kind: "case_statement",
    break_statement_raw_kind: "break_statement",
};

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

            _ => None,
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        match raw_kind {
            // Definition kinds
            "function_definition" => Some(FQL_FUNCTION),
            "class_specifier" => Some(FQL_CLASS),
            "struct_specifier" => Some(FQL_STRUCT),
            "enum_specifier" => Some(FQL_ENUM),
            "declaration" | "parameter_declaration" => Some(FQL_VARIABLE),
            "field_declaration" => Some(FQL_FIELD),
            "comment" => Some(FQL_COMMENT),
            "preproc_include" => Some(FQL_IMPORT),
            "preproc_def" | "preproc_function_def" => Some(FQL_MACRO),
            "type_definition" | "alias_declaration" => Some(FQL_TYPE_ALIAS),
            "namespace_definition" => Some(FQL_NAMESPACE),

            // Expression/literal kinds (from enricher extra_rows)
            "number_literal" => Some(FQL_NUMBER),
            "cast_expression"
            | "static_cast_expression"
            | "reinterpret_cast_expression"
            | "const_cast_expression"
            | "dynamic_cast_expression" => Some(FQL_CAST),
            "update_expression" => Some(FQL_INCREMENT),

            // Control flow kinds (from enricher extra_rows)
            "if_statement" => Some(FQL_IF),
            "while_statement" => Some(FQL_WHILE),
            "for_statement" | "for_range_loop" => Some(FQL_FOR),
            "switch_statement" => Some(FQL_SWITCH),
            "do_statement" => Some(FQL_DO),

            _ => None,
        }
    }

    fn config(&self) -> &'static LanguageConfig {
        &CPP_CONFIG
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
        assert_eq!(config.root_node_kind, "translation_unit");
        assert_eq!(config.scope_separator, "::");
        assert!(!config.function_raw_kinds.is_empty());
        assert!(!config.type_raw_kinds.is_empty());
        assert!(!config.skip_node_kinds.is_empty());
        assert!(!config.usage_node_kinds.is_empty());
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
