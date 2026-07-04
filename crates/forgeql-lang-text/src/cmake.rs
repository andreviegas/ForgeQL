//! CMake language support for ForgeQL.
//!
//! Implements [`LanguageSupport`] for CMake files using `tree-sitter-cmake`.
//! Claims `.cmake` plus the well-known `CMakeLists.txt` file name (matched
//! through the registry's file-name fallback, without claiming `.txt`).
//!
//! `function()`/`macro()` definitions index as `function`/`macro` rows named
//! by their first argument; every other command call (`set`, `add_library`,
//! `target_link_libraries`, …) as a `call_statement` named by the command
//! identifier; `if`/`foreach`/`while` blocks get nested control-flow node
//! ids exactly like code — so a rename sweep over a variable finds every
//! `set(FOO …)` call site by handle.

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]
// Tests use unwrap/expect intentionally — the pedantic lints are for library code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, node_text};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// CMake language support for ForgeQL.
pub struct CmakeLanguage;

/// Static configuration for CMake.
static CMAKE_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// Returns the static CMake language configuration, loaded from
/// `config/cmake.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `cmake.json` is malformed (should never happen —
/// the file is validated at test time).
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
pub fn cmake_config() -> &'static LanguageConfig {
    CMAKE_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/cmake.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded cmake.json must be valid");
        json_config.into_language_config()
    })
}

/// The trimmed text of a node's first child of `kind`, if any.
fn child_of_kind<'t>(node: tree_sitter::Node<'t>, kind: &str) -> Option<tree_sitter::Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

/// The first argument of a `function_command`/`macro_command` header —
/// the definition's name (e.g. `function(my_func ARG)` → `my_func`).
fn first_argument(header: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let args = child_of_kind(header, "argument_list")?;
    let first = child_of_kind(args, "argument")?;
    let text = node_text(source, first).trim().to_string();
    (!text.is_empty()).then_some(text)
}

impl LanguageSupport for CmakeLanguage {
    fn name(&self) -> &'static str {
        "cmake"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["cmake", "cmakelists.txt"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_cmake::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        match node.kind() {
            // `function(name …)` / `macro(name …)` — named by the first
            // argument of the definition header.
            "function_def" => first_argument(child_of_kind(node, "function_command")?, source),
            "macro_def" => first_argument(child_of_kind(node, "macro_command")?, source),
            // `set(FOO …)`, `add_library(app …)` — named by the command
            // identifier so call sites group under one name.
            "normal_command" => {
                let ident = child_of_kind(node, "identifier")?;
                let text = node_text(source, ident).trim().to_string();
                (!text.is_empty()).then_some(text)
            }
            _ => None,
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        cmake_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        cmake_config()
    }
}

/// Convenience registry containing only CMake support.
#[must_use]
pub fn cmake_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(CmakeLanguage)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `src` with the real CMake grammar and return every
    /// `(node_kind, extract_name)` where a name is produced.
    fn names(src: &str) -> Vec<(String, String)> {
        let lang = CmakeLanguage;
        let ts_lang = lang.tree_sitter_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let source = src.as_bytes();

        let mut out = Vec::new();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if let Some(name) = lang.extract_name(node, source) {
                out.push((node.kind().to_string(), name));
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
        out
    }

    const SAMPLE: &str = "cmake_minimum_required(VERSION 3.20)\nproject(demo)\n\nset(FOO \"bar\")\n\nfunction(register_test name)\n  add_test(NAME ${name} COMMAND ${name})\nendfunction()\n\nif(BUILD_TESTS)\n  add_subdirectory(tests)\nendif()\n";

    #[test]
    fn embedded_config_is_valid() {
        let cfg = cmake_config();
        assert!(cfg.kind_map_lookup("function_def").is_some());
        assert!(cfg.kind_map_lookup("normal_command").is_some());
    }

    #[test]
    fn map_kind_covers_structural_kinds() {
        let lang = CmakeLanguage;
        assert_eq!(lang.map_kind("function_def"), Some("function"));
        assert_eq!(lang.map_kind("macro_def"), Some("macro"));
        assert_eq!(lang.map_kind("normal_command"), Some("call_statement"));
        assert_eq!(lang.map_kind("if_condition"), Some("if"));
        assert_eq!(lang.map_kind("foreach_loop"), Some("for"));
        assert_eq!(lang.map_kind("line_comment"), Some("comment"));
        assert_eq!(lang.map_kind("argument_list"), None);
    }

    #[test]
    fn registry_resolves_cmake_names() {
        let registry = cmake_registry();
        for file in ["helpers.cmake", "CMakeLists.txt"] {
            let lang = registry.language_for_path(std::path::Path::new(file));
            assert!(lang.is_some(), "no language for {file}");
            assert_eq!(lang.unwrap().name(), "cmake");
        }
        // The plain txt extension stays unclaimed.
        assert!(
            registry
                .language_for_path(std::path::Path::new("notes.txt"))
                .is_none()
        );
    }

    #[test]
    fn function_defs_named_by_first_argument() {
        let got = names(SAMPLE);
        assert!(
            got.contains(&("function_def".to_string(), "register_test".to_string())),
            "names: {got:?}"
        );
    }

    #[test]
    fn commands_named_by_identifier() {
        let got = names(SAMPLE);
        for cmd in ["project", "set", "add_subdirectory"] {
            assert!(
                got.contains(&("normal_command".to_string(), cmd.to_string())),
                "missing '{cmd}' in: {got:?}"
            );
        }
    }
}
