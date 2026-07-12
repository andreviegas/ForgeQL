//! Test-only in-crate language implementations for C++, Rust, and Python.
//!
//! The production implementations live in the `forgeql-lang-*` crates.
//! These inline duplicates exist so that `forgeql-core`'s own unit and
//! integration tests can build a [`super::LanguageRegistry`] without
//! depending on the external crates.
use std::sync::OnceLock;

use super::LanguageConfig;
use super::LanguageSupport;
// -----------------------------------------------------------------------
// CppLanguageInline — test-only in-crate C++ implementation
//
// The production C++ support lives in `forgeql-lang-cpp`.  This inline
// duplicate stays here behind `#[cfg(any(test, feature = "test-helpers"))]`
// so that forgeql-core's own unit and integration tests can build a
// LanguageRegistry without depending on the external crate.
// -----------------------------------------------------------------------

#[cfg(any(test, feature = "test-helpers"))]
static CPP_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

#[cfg(any(test, feature = "test-helpers"))]
#[expect(
    clippy::expect_used,
    reason = "test-helper: embedded cpp.json is always valid"
)]
#[allow(clippy::missing_panics_doc)]
pub fn cpp_config() -> &'static LanguageConfig {
    CPP_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../../../../forgeql-lang-cpp/config/cpp.json");
        let json_config = crate::ast::lang_json::LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded cpp.json must be valid");
        json_config.into_language_config()
    })
}

/// Test-only inline C++ language support.
///
/// For production use, depend on `forgeql-lang-cpp::CppLanguage` instead.
#[cfg(any(test, feature = "test-helpers"))]
pub struct CppLanguageInline;

#[cfg(any(test, feature = "test-helpers"))]
impl LanguageSupport for CppLanguageInline {
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

        // A class/struct/union/enum reference or forward declaration
        // (`struct Foo *p;`, `class Foo;`) exposes a `name` field but no
        // `body`: it is a use, not a definition. Skip it so only the
        // definition — which carries the members — is indexed under the name.
        if matches!(
            node.kind(),
            "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier"
        ) && node.child_by_field_name("body").is_none()
        {
            return None;
        }
        // Universal: most grammars expose a "name" field on definition nodes.
        if let Some(name_node) = node.child_by_field_name("name") {
            let text = cpp_node_text(source, name_node);
            if !text.is_empty() {
                return Some(text);
            }
        }

        match node.kind() {
            "function_definition" => node
                .child_by_field_name("declarator")
                .and_then(cpp_find_function_name)
                .map(|n| cpp_node_text(source, n))
                .filter(|s| !s.is_empty()),

            "preproc_include" => node
                .child_by_field_name("path")
                .map(|n| {
                    cpp_node_text(source, n)
                        .trim_matches(|c: char| c == '"' || c == '<' || c == '>')
                        .to_string()
                })
                .filter(|s| !s.is_empty()),

            "declaration" => {
                let decl = node.child_by_field_name("declarator")?;
                if cpp_contains_function_declarator(decl) {
                    return None;
                }
                cpp_find_function_name(decl)
                    .map(|n| cpp_node_text(source, n))
                    .filter(|s| !s.is_empty())
            }

            "field_declaration" => node
                .child_by_field_name("declarator")
                .and_then(cpp_find_function_name)
                .map(|n| cpp_node_text(source, n))
                .filter(|s| !s.is_empty()),

            "parameter_declaration" => node
                .child_by_field_name("declarator")
                .and_then(cpp_find_function_name)
                .map(|n| cpp_node_text(source, n))
                .filter(|s| !s.is_empty()),

            "comment" => {
                let text = cpp_node_text(source, node);
                if text.is_empty() { None } else { Some(text) }
            }

            // macro_invocation: extract the macro name via the "macro" field.
            // NOTE: tree-sitter-cpp 0.23.x rarely produces macro_invocation nodes
            // in practice — see forgeql-lang-cpp for details.
            "macro_invocation" => node
                .child_by_field_name("macro")
                .map(|n| cpp_node_text(source, n))
                .filter(|s| !s.is_empty()),

            "type_definition" => node
                .child_by_field_name("declarator")
                .and_then(cpp_find_type_alias_name)
                .map(|n| cpp_node_text(source, n))
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
}

// -----------------------------------------------------------------------
// C++ helper functions (test-only — production impl in forgeql-lang-cpp)
// -----------------------------------------------------------------------

#[cfg(any(test, feature = "test-helpers"))]
fn cpp_node_text(source: &[u8], node: tree_sitter::Node<'_>) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

#[cfg(any(test, feature = "test-helpers"))]
fn cpp_find_function_name(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
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
            .and_then(cpp_find_function_name),
        _ => {
            for i in 0..node.named_child_count() {
                if let Some(found) = node.named_child(i).and_then(cpp_find_function_name) {
                    return Some(found);
                }
            }
            None
        }
    }
}

/// Find the name node a `type_definition` introduces (typedef alias). Kept
/// separate from `cpp_find_function_name` so the typedef case never perturbs
/// variable / parameter / field name extraction.
fn cpp_find_type_alias_name(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    if matches!(node.kind(), "type_identifier" | "identifier") {
        return Some(node);
    }
    if let Some(found) = node
        .child_by_field_name("declarator")
        .and_then(cpp_find_type_alias_name)
    {
        return Some(found);
    }
    for i in 0..node.named_child_count() {
        if let Some(found) = node.named_child(i).and_then(cpp_find_type_alias_name) {
            return Some(found);
        }
    }
    None
}

#[cfg(any(test, feature = "test-helpers"))]
fn cpp_contains_function_declarator(node: tree_sitter::Node<'_>) -> bool {
    if node.kind() == "function_declarator" {
        return true;
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i)
            && cpp_contains_function_declarator(child)
        {
            return true;
        }
    }
    false
}

// -----------------------------------------------------------------------
// RustLanguageInline — test-only in-crate Rust implementation
//
// The production Rust support lives in `forgeql-lang-rust`.  This inline
// duplicate stays here behind `#[cfg(any(test, feature = "test-helpers"))]`
// so that forgeql-core's own unit and integration tests can build a
// LanguageRegistry without depending on the external crate.
// -----------------------------------------------------------------------

/// Test-only inline Rust language support.
///
/// For production use, depend on `forgeql-lang-rust::RustLanguage` instead.
#[cfg(any(test, feature = "test-helpers"))]
static RUST_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

#[cfg(any(test, feature = "test-helpers"))]
#[expect(
    clippy::expect_used,
    reason = "test-helper: embedded rust.json is always valid"
)]
#[allow(clippy::missing_panics_doc)]
pub fn rust_config() -> &'static LanguageConfig {
    RUST_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../../../../forgeql-lang-rust/config/rust.json");
        let json_config = crate::ast::lang_json::LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded rust.json must be valid");
        json_config.into_language_config()
    })
}

#[cfg(any(test, feature = "test-helpers"))]
pub struct RustLanguageInline;

#[cfg(any(test, feature = "test-helpers"))]
impl LanguageSupport for RustLanguageInline {
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
        // scoped_identifier nodes are references, not definitions.
        if node.kind() == "scoped_identifier" {
            return None;
        }

        if let Some(name_node) = node.child_by_field_name("name") {
            let text = rust_node_text(source, name_node);
            if !text.is_empty() {
                return Some(text);
            }
        }

        match node.kind() {
            "impl_item" => node
                .child_by_field_name("type")
                .map(|n| rust_node_text(source, n))
                .filter(|s| !s.is_empty()),

            "use_declaration" => node
                .child_by_field_name("argument")
                .map(|n| rust_node_text(source, n))
                .filter(|s| !s.is_empty()),

            "line_comment" | "block_comment" => {
                let text = rust_node_text(source, node);
                if text.is_empty() { None } else { Some(text) }
            }

            "let_declaration" => node
                .child_by_field_name("pattern")
                .map(|n| rust_node_text(source, n))
                .filter(|s| !s.is_empty()),

            // macro invocations: extract the macro path (field "macro")
            "macro_invocation" => node
                .child_by_field_name("macro")
                .map(|n| rust_node_text(source, n))
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

    /// Mirrors the production `RustLanguage::block_group_key`: split a comment
    /// run by its style so `///` doc runs and `//` line runs form separate
    /// blocks. Kept in sync deliberately — `comment_block_splits_on_style`
    /// indexes through this fixture, so a divergence here silently weakens the
    /// coverage of the real thing.
    fn block_group_key(
        &self,
        node: tree_sitter::Node<'_>,
        source: &[u8],
        attr: Option<&str>,
    ) -> String {
        match attr {
            Some("comment_style") => rust_config()
                .detect_comment_style(&rust_node_text(source, node))
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        }
    }
}

#[cfg(any(test, feature = "test-helpers"))]
fn rust_node_text(source: &[u8], node: tree_sitter::Node<'_>) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

// -----------------------------------------------------------------------
// PythonLanguageInline — test-only in-crate Python implementation
//
// The production Python support lives in `forgeql-lang-python`.  This inline
// duplicate stays here behind `#[cfg(any(test, feature = "test-helpers"))]`
// so that forgeql-core's own unit and integration tests can build a
// LanguageRegistry without depending on the external crate.
// -----------------------------------------------------------------------

/// Test-only inline Python language support.
///
/// For production use, depend on `forgeql-lang-python::PythonLanguage` instead.
#[cfg(any(test, feature = "test-helpers"))]
static PYTHON_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

#[cfg(any(test, feature = "test-helpers"))]
#[expect(
    clippy::expect_used,
    reason = "test-helper: embedded python.json is always valid"
)]
#[allow(clippy::missing_panics_doc)]
pub fn python_config() -> &'static LanguageConfig {
    PYTHON_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../../../../forgeql-lang-python/config/python.json");
        let json_config = crate::ast::lang_json::LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded python.json must be valid");
        json_config.into_language_config()
    })
}

#[cfg(any(test, feature = "test-helpers"))]
pub struct PythonLanguageInline;

#[cfg(any(test, feature = "test-helpers"))]
impl LanguageSupport for PythonLanguageInline {
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
        if let Some(name_node) = node.child_by_field_name("name") {
            let text = python_node_text(source, name_node);
            if !text.is_empty() {
                return Some(text);
            }
        }

        match node.kind() {
            "decorated_definition" => node
                .child_by_field_name("definition")
                .and_then(|def| def.child_by_field_name("name"))
                .map(|n| python_node_text(source, n))
                .filter(|s| !s.is_empty()),

            "import_statement" => {
                let mut names = Vec::new();
                for i in 0..node.named_child_count() {
                    if let Some(child) = node.named_child(i) {
                        match child.kind() {
                            "dotted_name" | "aliased_import" => {
                                let text = python_node_text(source, child);
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

            "import_from_statement" => node
                .child_by_field_name("module_name")
                .map(|n| python_node_text(source, n))
                .filter(|s| !s.is_empty()),

            "assignment" => node
                .child_by_field_name("left")
                .map(|n| python_node_text(source, n))
                .filter(|s| !s.is_empty()),

            "for_statement" => node
                .child_by_field_name("left")
                .map(|n| python_node_text(source, n))
                .filter(|s| !s.is_empty()),

            "comment" => {
                let text = python_node_text(source, node);
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

#[cfg(any(test, feature = "test-helpers"))]
fn python_node_text(source: &[u8], node: tree_sitter::Node<'_>) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}
