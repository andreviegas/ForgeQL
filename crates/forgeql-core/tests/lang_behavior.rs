//! C++ language-behaviour tests, relocated from `src/ast/lang.rs`.
//!
//! These exercise the real `forgeql-lang-cpp` plugin — `map_kind`,
//! `extract_name`, `config`, and registry resolution. They live under `tests/`
//! because the in-crate `#[cfg(test)]` build cannot link a real
//! `forgeql-lang-*` plugin (Cargo reports "multiple different versions of crate
//! `forgeql_core`"); the integration-test crate can. The assertions are
//! unchanged from the in-crate versions that ran against the inline clone.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
use forgeql_lang_cpp::CppLanguage;
mod tests {
    use super::*;

    #[test]
    fn cpp_map_kind_covers_all_definition_kinds() {
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
    fn cpp_map_kind_covers_expression_kinds() {
        let lang = CppLanguage;
        assert_eq!(lang.map_kind("number_literal"), Some("number"));
        assert_eq!(lang.map_kind("cast_expression"), Some("cast"));
        assert_eq!(lang.map_kind("static_cast_expression"), Some("cast"));
        assert_eq!(lang.map_kind("update_expression"), Some("increment"));
    }

    #[test]
    fn cpp_map_kind_covers_control_flow_kinds() {
        let lang = CppLanguage;
        assert_eq!(lang.map_kind("if_statement"), Some("if"));
        assert_eq!(lang.map_kind("while_statement"), Some("while"));
        assert_eq!(lang.map_kind("for_statement"), Some("for"));
        assert_eq!(lang.map_kind("for_range_loop"), Some("for"));
        assert_eq!(lang.map_kind("switch_statement"), Some("switch"));
        assert_eq!(lang.map_kind("do_statement"), Some("do"));
    }

    #[test]
    fn cpp_map_kind_returns_none_for_unknown() {
        let lang = CppLanguage;
        assert_eq!(lang.map_kind("translation_unit"), None);
        assert_eq!(lang.map_kind("compound_statement"), None);
    }

    #[test]
    fn registry_resolves_cpp_extensions() {
        let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage)]);

        for ext in ["cpp", "cc", "cxx", "h", "hpp", "hxx", "ino"] {
            let path = std::path::PathBuf::from(format!("test.{ext}"));
            let lang = registry.language_for_path(&path);
            assert!(lang.is_some(), "extension {ext} should resolve");
            assert_eq!(lang.as_ref().map(|l| l.name()), Some("cpp"));
        }
    }

    #[test]
    fn registry_returns_none_for_unknown_extension() {
        let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage)]);
        let path = std::path::PathBuf::from("test.rs");
        assert!(registry.language_for_path(&path).is_none());
    }

    #[test]
    fn registry_falls_back_to_file_name_for_extensionless_paths() {
        // A registered key doubles as a well-known file name: extensionless
        // paths match by lowercased name with any leading dot stripped.
        let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage)]);
        for name in ["cpp", ".cpp", "CPP", "dir/.cpp"] {
            let path = std::path::PathBuf::from(name);
            assert!(
                registry.language_for_path(&path).is_some(),
                "extensionless '{name}' should resolve by file name"
            );
        }
        // An unclaimed extension falls back to the (unclaimed) full name → None.
        let path = std::path::PathBuf::from("other.unknown");
        assert!(registry.language_for_path(&path).is_none());
    }

    #[test]
    fn registry_falls_back_to_full_file_name_when_extension_is_unclaimed() {
        // A key containing a dot claims a well-known full file name (the
        // CMakeLists.txt case) without claiming the bare extension.
        let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage)]);
        // CppLanguage claims "cpp"; "weird.cpp" resolves via extension,
        // and a full-name key would resolve via the fallback — prove the
        // fallback consults the same table by using a claimed key as a name.
        let path = std::path::PathBuf::from("dir/CPP.unclaimed-ext");
        assert!(
            registry.language_for_path(&path).is_none(),
            "unclaimed extension + unclaimed full name must stay None"
        );
        let path = std::path::PathBuf::from("dir/cpp");
        assert!(registry.language_for_path(&path).is_some());
    }

    #[test]
    fn registry_language_for_extension() {
        let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage)]);
        assert!(registry.language_for_extension("cpp").is_some());
        assert!(registry.language_for_extension("py").is_none());
    }

    #[test]
    fn registry_languages_deduplicates() {
        let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage)]);
        let languages = registry.languages();
        assert_eq!(languages.len(), 1);
        assert_eq!(languages[0].name(), "cpp");
    }

    #[test]
    fn query_methods_kind_membership() {
        let cfg = CppLanguage.config();
        // slice-based membership
        assert!(cfg.is_function_kind("function_definition"));
        assert!(!cfg.is_function_kind("class_specifier"));
        assert!(cfg.is_type_kind("class_specifier"));
        assert!(cfg.is_type_kind("struct_specifier"));
        assert!(cfg.is_definition_kind("function_definition"));
        assert!(cfg.is_declaration_kind("declaration"));
        assert!(cfg.is_field_kind("field_declaration"));
        assert!(cfg.is_member_kind("field_declaration"));
        assert!(cfg.is_number_literal_kind("number_literal"));
        assert!(cfg.is_control_flow_kind("if_statement"));
        assert!(cfg.is_switch_kind("switch_statement"));
        assert!(cfg.is_modifier_node_kind("type_qualifier"));
        assert!(cfg.is_assignment_kind("assignment_expression"));
        assert!(cfg.is_update_kind("update_expression"));
        assert!(cfg.is_string_literal_kind("string_literal"));
        assert!(!cfg.is_skip_kind("preproc_else")); // now traversed as a guard branch
        assert!(cfg.is_usage_node_kind("identifier"));
        assert!(cfg.is_shift_expression_kind("shift_expression"));
        assert!(cfg.is_template_misparse_kind("template_function"));
        assert!(cfg.is_null_literal("nullptr"));
        assert!(cfg.is_boolean_literal("true"));
        assert!(cfg.is_static_storage_keyword("static"));
        // negative cases
        assert!(!cfg.is_skip_kind("function_definition"));
        assert!(!cfg.is_null_literal("42"));
    }

    #[test]
    fn query_methods_single_kind() {
        let cfg = CppLanguage.config();
        assert!(cfg.is_root_kind("translation_unit"));
        assert!(cfg.is_parameter_kind("parameter_declaration"));
        assert!(cfg.is_member_body_kind("field_declaration_list"));
        assert!(cfg.is_comment_kind("comment"));
        assert!(cfg.is_block_kind("compound_statement"));
        assert!(cfg.is_identifier_kind("identifier"));
        assert!(cfg.is_init_declarator_kind("init_declarator"));
        assert!(cfg.is_return_statement_kind("return_statement"));
        assert!(cfg.is_address_of_expression_kind("pointer_expression"));
        assert!(cfg.is_case_statement_kind("case_statement"));
        assert!(cfg.is_break_statement_kind("break_statement"));
        assert!(cfg.is_call_expression_kind("call_expression"));
        assert!(cfg.is_goto_statement_kind("goto_statement"));
        assert!(cfg.is_throw_statement_kind("throw_statement"));
        assert!(cfg.is_template_declaration_kind("template_declaration"));
        assert!(cfg.is_enumerator_kind("enumerator"));
        assert!(cfg.is_binary_expression_kind("binary_expression"));
        assert!(cfg.is_logical_expression_kind("logical_expression"));
        assert!(cfg.is_parameter_list_kind("parameter_list"));
        assert!(cfg.is_char_literal_kind("char_literal"));
        // negative
        assert!(!cfg.is_root_kind("program"));
        assert!(!cfg.is_block_kind("block"));
    }

    #[test]
    fn query_methods_accessors() {
        let cfg = CppLanguage.config();
        assert_eq!(cfg.scope_sep(), "::");
        assert_eq!(cfg.declarator_field(), "declarator");
        assert_eq!(cfg.function_declarator(), "function_declarator");
        assert_eq!(cfg.address_of_op(), "&");
    }

    #[test]
    fn query_methods_lookups() {
        let cfg = CppLanguage.config();
        // cast_info (direct node-kind casts)
        assert_eq!(
            cfg.cast_info("cast_expression"),
            Some(("c_style", "unsafe"))
        );
        assert_eq!(
            cfg.cast_info("static_cast_expression"),
            Some(("static_cast", "safe"))
        );
        assert_eq!(cfg.cast_info("unknown"), None);
        // named_cast_info (keyword-based, for tree-sitter-cpp 0.23 call_expression style)
        assert_eq!(
            cfg.named_cast_info("static_cast"),
            Some(("static_cast", "safe"))
        );
        assert_eq!(
            cfg.named_cast_info("dynamic_cast"),
            Some(("dynamic_cast", "safe"))
        );
        assert_eq!(
            cfg.named_cast_info("const_cast"),
            Some(("const_cast", "moderate"))
        );
        assert_eq!(
            cfg.named_cast_info("reinterpret_cast"),
            Some(("reinterpret_cast", "unsafe"))
        );
        assert_eq!(cfg.named_cast_info("unknown_cast"), None);
        // for_style
        assert_eq!(cfg.for_style("for_statement"), Some("traditional"));
        assert_eq!(cfg.for_style("for_range_loop"), Some("range"));
        assert_eq!(cfg.for_style("while_statement"), None);
        // modifier_field_for
        assert_eq!(cfg.modifier_field_for("const"), Some("is_const"));
        assert_eq!(cfg.modifier_field_for("virtual"), Some("is_virtual"));
        assert_eq!(cfg.modifier_field_for("unknown"), None);
        // visibility
        assert_eq!(cfg.visibility_for_keyword("public"), Some("public"));
        assert_eq!(cfg.visibility_for_keyword("private"), Some("private"));
        assert_eq!(cfg.visibility_for_keyword("unknown"), None);
        // default visibility for type
        assert_eq!(
            cfg.default_visibility_for_type("class_specifier"),
            Some("private")
        );
        assert_eq!(
            cfg.default_visibility_for_type("struct_specifier"),
            Some("public")
        );
        // comment style
        assert_eq!(cfg.detect_comment_style("/** doc */"), Some("doc_block"));
        assert_eq!(cfg.detect_comment_style("/// doc"), Some("doc_line"));
        assert_eq!(cfg.detect_comment_style("/* block */"), Some("block"));
        assert_eq!(cfg.detect_comment_style("// line"), Some("line"));
        // number suffix
        assert_eq!(cfg.number_suffix_meaning("f"), Some("float"));
        assert_eq!(cfg.number_suffix_meaning("ull"), Some("unsigned_long_long"));
        assert_eq!(cfg.number_suffix_meaning("xyz"), None);
    }

    #[test]
    fn cpp_extract_name_via_trait() {
        let lang = CppLanguage;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&lang.tree_sitter_language())
            .expect("set language");

        let source = b"void processSignal(int speed) { return; }";
        let tree = parser.parse(source, None).expect("parse");
        let root = tree.root_node();

        // Walk to find the function_definition node.
        let func_node = root.child(0).expect("function_definition");
        assert_eq!(func_node.kind(), "function_definition");

        let name = lang.extract_name(func_node, source);
        assert_eq!(name.as_deref(), Some("processSignal"));
    }
}
